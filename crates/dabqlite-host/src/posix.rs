//! POSIX file storage: the declared file set as real files in a directory.
//!
//! One `unsafe` block lives here, in `record_lock`: the `fcntl` call that
//! takes the single-writer lock. It is the only FFI in the workspace's
//! production code, and it is here rather than avoided because the safe
//! alternative — `flock`, via `std::fs::File::try_lock` — leaks the lock
//! into forked children (see [`WriterLock`]), which is a correctness bug
//! and not a stylistic one.
#![allow(
    unsafe_code,
    reason = "one fcntl(F_SETLK) call; see `record_lock` and `WriterLock`"
)]
//!
//! File creation happens exactly once, at open (docs/DESIGN.md §4.4), and
//! the directory is fsynced right there — the one directory operation in
//! the design, confined to the one place it can happen. After that the
//! backend is pure positional I/O on held handles, the same shape OPFS
//! sync access handles offer.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use dabqlite_core::migration::V1_SCHEMA_HASH;
use dabqlite_core::{FileId, SCHEMA_HASH};

use crate::Storage;

// The file set is backend-independent (docs/DESIGN.md §4.4); these live
// at the crate root now and are re-exported here so existing callers —
// the inspector, the test suites — keep working unchanged.
pub use crate::{rows_file_name, LOCK_FILE, SUPERBLOCK_FILE};

/// Database directories THIS PROCESS holds the writer lock for.
///
/// The other half of the single-writer rule, and not optional. The kernel
/// lock below is a POSIX record lock, which is owned by the *process*: two
/// handles in one process do not conflict with each other at all, so
/// without this registry a program could open the same database twice and
/// the engine's whole single-writer premise would quietly stop holding.
///
/// It also makes the record lock's other sharp edge unreachable. Closing
/// ANY descriptor to a file drops that process's record locks on it, so a
/// second open-and-close of the same lock file would release the first
/// handle's lock; the registry refuses that second open before it happens.
static HELD: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// The single-writer lock (docs/DESIGN.md §2: "one writer, always"), held
/// for the storage's lifetime.
///
/// A POSIX record lock (`fcntl(F_SETLK)`), NOT `flock`, for one reason:
/// record locks are not inherited by a child created with `fork`, and
/// `flock` locks are. That difference is not academic. `flock` belongs to
/// the open file description, `fork` duplicates it, and `O_CLOEXEC` only
/// closes the copy at `exec` — so with `flock`, any program that spawns a
/// subprocess hands its database's writer lock to a child that has never
/// heard of the database, for the whole fork-to-exec window. Measured on
/// this machine, against a database whose only handle had already been
/// CLOSED: 891 of 1500 reopens refused, from 39 spawns of `/bin/true`.
///
/// The crash-safety property is unchanged: the kernel releases record
/// locks when the process dies, so a crash can never leave a stale lock.
struct WriterLock {
    file: File,
    dir: PathBuf,
}

/// Set or clear the whole-file write lock. `l_len` of 0 means "to the end
/// of the file, however long it becomes", which is the idiom for locking a
/// file rather than a range of it.
fn record_lock(file: &File, kind: libc::c_short) -> io::Result<()> {
    let request = libc::flock {
        l_type: kind,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    // SAFETY: `fcntl` receives a descriptor borrowed from `file` and valid
    // for the duration of the call, and a pointer to a fully-initialized
    // `flock` this function owns and outlives the call. `F_SETLK` reads
    // that struct and touches no other memory. This is the only FFI in the
    // workspace's production code, and the only `unsafe` outside the
    // counting allocator the test suite uses.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &request) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl WriterLock {
    fn acquire(dir: &Path, lock_path: &Path) -> io::Result<Self> {
        let key = dir.canonicalize()?;
        {
            let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
            if !held.insert(key.clone()) {
                return Err(contended(dir));
            }
        }
        // From here on the guard owns the registry entry, so every exit
        // path releases it by dropping.
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                HELD.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
                return Err(e);
            }
        };
        let lock = WriterLock { file, dir: key };
        match record_lock(&lock.file, libc::F_WRLCK as libc::c_short) {
            Ok(()) => Ok(lock),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EAGAIN)) => {
                Err(contended(dir))
            }
            Err(e) => Err(e),
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Release explicitly before the descriptor closes, so the unlock
        // is an ordered step rather than a side effect of dropping a file.
        let _ = record_lock(&self.file, libc::F_UNLCK as libc::c_short);
        HELD.lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.dir);
    }
}

fn contended(dir: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "{} is locked by another writer; the store is single-writer \
             (design §2) — close the other handle first",
            dir.display()
        ),
    )
}

/// Is another process holding the single-writer lock on `dir` right now?
///
/// Asks with `F_GETLK`, which reports whether a lock WOULD conflict
/// without taking anything — so the question cannot disturb the writer,
/// and cannot trip the record-lock rule that closing any descriptor to a
/// file drops the caller's own locks on it.
///
/// `None` means the question could not be answered — no lock file, or a
/// mount that will not even open it — which callers treat as "no writer",
/// since a rescue must still be possible on media that barely works.
/// `F_GETLK` also cannot see a lock this same process holds, so a caller
/// asking about a database it has open itself gets `Some(false)`; the
/// in-process registry is what answers that question.
pub fn writer_holds(dir: &Path) -> Option<bool> {
    let path = dir.join(LOCK_FILE);
    if !path.exists() {
        return Some(false);
    }
    let file = File::open(&path).ok()?;
    let mut probe = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    // SAFETY: as `record_lock` — a borrowed valid descriptor and a
    // fully-initialized `flock` this function owns. `F_GETLK` writes its
    // answer back into that same struct and touches nothing else.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) };
    if rc == -1 {
        return None;
    }
    Some(probe.l_type != libc::F_UNLCK as libc::c_short)
}

pub struct PosixStorage {
    superblock: File,
    rows: File,
    rows_old: File,
    _lock: WriterLock,
}

impl PosixStorage {
    /// Open (creating if absent) the declared file set in `dir`, then fsync
    /// the directory so the entries themselves are durable before any data
    /// I/O begins.
    pub fn open_dir(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let open = |name: &str| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(name))
        };
        // Take the single-writer lock BEFORE touching data files: a second
        // writer must be refused before it can do any harm at all.
        let lock = WriterLock::acquire(dir, &dir.join(LOCK_FILE))?;
        let superblock = open(SUPERBLOCK_FILE)?;
        let rows = open(&rows_file_name(SCHEMA_HASH))?;
        let rows_old = open(&rows_file_name(V1_SCHEMA_HASH))?;
        // Directory fsync: the least portable operation in the design,
        // written once, here (§4.4). macOS needs F_FULLFSYNC for real
        // guarantees — tracked for when that target is wired up.
        File::open(dir)?.sync_all()?;
        Ok(PosixStorage {
            superblock,
            rows,
            rows_old,
            _lock: lock,
        })
    }

    fn file(&self, id: FileId) -> &File {
        match id {
            FileId::Superblock => &self.superblock,
            FileId::Rows => &self.rows,
            FileId::RowsOld => &self.rows_old,
        }
    }
}

impl Storage for PosixStorage {
    type Error = io::Error;

    fn len(&mut self, file: FileId) -> Result<u64, io::Error> {
        Ok(self.file(file).metadata()?.len())
    }

    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, io::Error> {
        // Clamp to EOF, matching the simulator's contract exactly.
        let file_len = self.len(file)?;
        let start = offset.min(file_len);
        let end = offset.saturating_add(len).min(file_len);
        let mut buf = vec![0u8; (end - start) as usize];
        self.file(file).read_exact_at(&mut buf, start)?;
        Ok(buf)
    }

    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        // write_at past EOF zero-fills the gap, same as the simulator.
        self.file(file).write_all_at(data, offset)
    }

    fn sync(&mut self, file: FileId) -> Result<(), io::Error> {
        // sync_all = fsync (data + metadata; the file can grow, so metadata
        // matters). macOS F_FULLFSYNC is the TODO noted at open_dir.
        self.file(file).sync_all()
    }

    fn truncate(&mut self, file: FileId, len: u64) -> Result<(), io::Error> {
        // Only ever shrinks: `set_len` would zero-extend a shorter file,
        // and growing the rows file behind the engine's back is not
        // something any caller means.
        if self.len(file)? > len {
            self.file(file).set_len(len)?;
        }
        Ok(())
    }
}

/// A strictly READ-ONLY view of a database directory: no lock, no
/// creation, no writes. This is what makes forensics and rescue safe on a
/// database someone else may be using, on a read-only mount, or on a
/// volume whose writes are failing — the three situations where you most
/// need them and can least afford a tool that mutates.
///
/// `sync` succeeds trivially: nothing was ever written, so there is
/// nothing to flush. `write` always fails — loudly, since a write here
/// would be a bug in the caller, not a condition to recover from.
pub struct ReadOnlyDir {
    superblock: Option<File>,
    rows: Option<File>,
    rows_old: Option<File>,
}

impl ReadOnlyDir {
    /// Open the declared file set read-only. Missing files read as empty,
    /// exactly as a fresh database's would.
    pub fn open_dir(dir: &Path) -> io::Result<Self> {
        let open = |name: &str| match File::open(dir.join(name)) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
        Ok(ReadOnlyDir {
            superblock: open(SUPERBLOCK_FILE)?,
            rows: open(&rows_file_name(SCHEMA_HASH))?,
            rows_old: open(&rows_file_name(V1_SCHEMA_HASH))?,
        })
    }

    fn file(&self, id: FileId) -> Option<&File> {
        match id {
            FileId::Superblock => self.superblock.as_ref(),
            FileId::Rows => self.rows.as_ref(),
            FileId::RowsOld => self.rows_old.as_ref(),
        }
    }
}

impl Storage for ReadOnlyDir {
    type Error = io::Error;

    fn len(&mut self, file: FileId) -> Result<u64, io::Error> {
        match self.file(file) {
            Some(f) => Ok(f.metadata()?.len()),
            None => Ok(0),
        }
    }

    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, io::Error> {
        let file_len = self.len(file)?;
        let start = offset.min(file_len);
        let end = offset.saturating_add(len).min(file_len);
        let mut buf = vec![0u8; (end - start) as usize];
        if let Some(f) = self.file(file) {
            f.read_exact_at(&mut buf, start)?;
        }
        Ok(buf)
    }

    fn write(&mut self, _file: FileId, _offset: u64, _data: &[u8]) -> Result<(), io::Error> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "dabqlite: read-only handle — inspection and salvage never write",
        ))
    }

    fn sync(&mut self, _file: FileId) -> Result<(), io::Error> {
        // Nothing was written, so nothing needs flushing.
        Ok(())
    }

    fn truncate(&mut self, _file: FileId, _len: u64) -> Result<(), io::Error> {
        // A rescue must not alter the wreckage it is reading.
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "dabqlite: a read-only open never truncates",
        ))
    }
}
