//! POSIX file storage: the declared file set as real files in a directory.
//!
//! File creation happens exactly once, at open (docs/DESIGN.md §4.4), and
//! the directory is fsynced right there — the one directory operation in
//! the design, confined to the one place it can happen. After that the
//! backend is pure positional I/O on held handles, the same shape OPFS
//! sync access handles offer.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use dabqlite_core::FileId;

use crate::Storage;

pub const SUPERBLOCK_FILE: &str = "superblock.dabq";
pub const ROWS_FILE: &str = "rows.dabq";

pub struct PosixStorage {
    superblock: File,
    rows: File,
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
        let superblock = open(SUPERBLOCK_FILE)?;
        let rows = open(ROWS_FILE)?;
        // Directory fsync: the least portable operation in the design,
        // written once, here (§4.4). macOS needs F_FULLFSYNC for real
        // guarantees — tracked for when that target is wired up.
        File::open(dir)?.sync_all()?;
        Ok(PosixStorage { superblock, rows })
    }

    fn file(&self, id: FileId) -> &File {
        match id {
            FileId::Superblock => &self.superblock,
            FileId::Rows => &self.rows,
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
