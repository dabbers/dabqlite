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

pub const SUPERBLOCK_FILE: &str = "superblock.dabq";
pub const LOCK_FILE: &str = "lock.dabq";

/// Rows files are NAMED by the schema hash that wrote them, so the
/// superblock's stored hash is also the name of the live rows file. After
/// a migration flips the superblock, the legacy file is an orphan the
/// manifest no longer names — inert by construction (docs/DESIGN.md §4.4).
pub fn rows_file_name(schema_hash: u64) -> String {
    format!("rows-{schema_hash:016x}.dabq")
}

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
