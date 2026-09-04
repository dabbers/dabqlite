//! POSIX file storage: the declared file set as real files in a directory.
//!
//! File creation happens exactly once, at open (docs/DESIGN.md §4.4), and
//! the directory is fsynced right there — the one directory operation in
//! the design, confined to the one place it can happen. After that the
//! backend is pure positional I/O on held handles, the same shape OPFS
//! sync access handles offer.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use dabqlite_core::migration::V1_SCHEMA_HASH;
use dabqlite_core::{FileId, SCHEMA_HASH};

use crate::Storage;

// The file set is backend-independent (docs/DESIGN.md §4.4); these live
// at the crate root now and are re-exported here so existing callers —
// the inspector, the test suites — keep working unchanged.
pub use crate::{rows_file_name, LOCK_FILE, SUPERBLOCK_FILE};

pub struct PosixStorage {
    superblock: File,
    rows: File,
    rows_old: File,
    /// The single-writer lock (docs/DESIGN.md §2: "one writer, always").
    /// Held for the storage's lifetime; `flock` is released by the kernel
    /// when the process dies, so a crash can never leave a stale lock —
    /// the lock itself is crash-safe by construction.
    _lock: File,
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
        // process must be refused before it can do any harm at all.
        let lock = open(LOCK_FILE)?;
        // Non-blocking exclusive lock via std (`flock(LOCK_EX|LOCK_NB)` on
        // Linux): held by the open file description, released by the
        // kernel when the process dies — crash-safe by construction, and
        // no unsafe FFI anywhere in the workspace's production code.
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "dabqlite: {} is locked by another process; the store is \
                         single-writer (design §2) — close the other handle first",
                        dir.display()
                    ),
                ));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
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
}
