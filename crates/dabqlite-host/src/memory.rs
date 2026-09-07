//! An in-memory backend: the whole declared file set held in RAM.
//!
//! This is the browser's baseline store and the one backend that works
//! everywhere — no filesystem, no OPFS, no permissions, no locks. Safari
//! in private mode has no OPFS at all (docs/DESIGN.md §8.1); a worker
//! that cannot acquire sync access handles can still run the complete
//! engine on this.
//!
//! ## What it does and does not promise
//!
//! Everything ABOVE the storage seam is unchanged: the same commit
//! protocol, the same checksums, the same superblock generations, the
//! same recovery, the same compiled queries and indices. So the
//! consistency properties hold exactly as they do on disk — a torn or
//! half-applied state is impossible here for the same reasons it is
//! impossible there.
//!
//! What is gone is DURABILITY, completely and by construction: the
//! medium is memory, so `sync` has nothing to flush and process death
//! takes everything. That is not a weaker version of the disk story, it
//! is a different one, and it is stated rather than implied.
//!
//! ## Explicit persistence
//!
//! [`MemoryStorage::image`] and [`MemoryStorage::from_images`] move a
//! database in and out of RAM as plain byte arrays — the same bytes the
//! POSIX and OPFS backends write, so an image taken here opens on disk
//! and vice versa. That is the honest persistence story for a browser
//! without OPFS: hold the database in memory, and snapshot it wherever
//! the platform will take bytes (IndexedDB, a download, a server).

use dabqlite_core::FileId;

use crate::Storage;

/// The declared file set in RAM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryStorage {
    superblock: Vec<u8>,
    rows: Vec<u8>,
    rows_old: Vec<u8>,
}

impl MemoryStorage {
    /// An empty store — a fresh database, once opened.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an existing database from its bytes. The images are exactly
    /// what the other backends hold in their files, so this loads a
    /// database captured anywhere.
    pub fn from_images(superblock: Vec<u8>, rows: Vec<u8>, rows_old: Vec<u8>) -> Self {
        MemoryStorage {
            superblock,
            rows,
            rows_old,
        }
    }

    /// One file's bytes, for snapshotting the database out.
    pub fn image(&self, file: FileId) -> &[u8] {
        match file {
            FileId::Superblock => &self.superblock,
            FileId::Rows => &self.rows,
            FileId::RowsOld => &self.rows_old,
        }
    }

    /// All three images, in `FileId` order.
    pub fn snapshot(&self) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            self.superblock.clone(),
            self.rows.clone(),
            self.rows_old.clone(),
        )
    }

    /// Total bytes held — the store's whole footprint below the engine.
    pub fn byte_len(&self) -> usize {
        self.superblock.len() + self.rows.len() + self.rows_old.len()
    }

    fn file_mut(&mut self, file: FileId) -> &mut Vec<u8> {
        match file {
            FileId::Superblock => &mut self.superblock,
            FileId::Rows => &mut self.rows,
            FileId::RowsOld => &mut self.rows_old,
        }
    }
}

/// Memory cannot fail the way a device can: there is no medium to report
/// an error. Errors are still *representable* through the seam — the
/// engine's fail-stop path is driven by the simulator and the OPFS model
/// — they simply never originate here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Never {}

impl Storage for MemoryStorage {
    type Error = Never;

    fn len(&mut self, file: FileId) -> Result<u64, Never> {
        Ok(self.image(file).len() as u64)
    }

    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Never> {
        // Clamp to EOF, exactly as every other backend does: a short or
        // empty result is not an error (see the `Storage` contract).
        let bytes = self.image(file);
        let start = (offset as usize).min(bytes.len());
        let end = (offset.saturating_add(len) as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Never> {
        // Writing past the end extends, zero-filling the gap — the same
        // semantics as POSIX `pwrite` and an OPFS sync access handle.
        let bytes = self.file_mut(file);
        let end = offset as usize + data.len();
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn sync(&mut self, _file: FileId) -> Result<(), Never> {
        // Nothing to flush: the medium IS the cache. This is where the
        // durability story ends, and it ends honestly.
        Ok(())
    }
}
