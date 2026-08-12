//! A simulated disk with a crash model.
//!
//! Each file tracks two views:
//!
//! - `current`: what reads observe (the OS page cache view).
//! - `durable`: what survives a crash (media state as of the last fsync).
//!
//! Writes land in `current` immediately and are queued as unsynced. `fsync`
//! promotes everything to `durable`. On `crash`, each unsynced write
//! independently either survives, vanishes, or is torn to a prefix
//! (docs/DESIGN.md §7.2 technique 4: torn writes, reordered writes, writes
//! that succeed then vanish). Dropping an earlier write while keeping a
//! later one models reordering.

use dabqlite_core::FileId;
use rand::Rng;
use rand_chacha::ChaCha8Rng;

#[derive(Default, Clone)]
pub struct SimFile {
    durable: Vec<u8>,
    current: Vec<u8>,
    unsynced: Vec<(u64, Vec<u8>)>,
}

fn apply(buf: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let end = offset as usize + data.len();
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[offset as usize..end].copy_from_slice(data);
}

impl SimFile {
    fn read(&self, offset: u64, len: u64) -> Vec<u8> {
        let start = (offset as usize).min(self.current.len());
        let end = ((offset + len) as usize).min(self.current.len());
        self.current[start..end].to_vec()
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        apply(&mut self.current, offset, data);
        self.unsynced.push((offset, data.to_vec()));
    }

    fn fsync(&mut self) {
        self.durable = self.current.clone();
        self.unsynced.clear();
    }

    fn crash(&mut self, rng: &mut ChaCha8Rng) {
        let pending = std::mem::take(&mut self.unsynced);
        for (offset, data) in pending {
            match rng.gen_range(0u8..3) {
                // The write never reached the platter.
                0 => {}
                // The write made it despite the missing fsync.
                1 => apply(&mut self.durable, offset, &data),
                // Torn: an arbitrary prefix reached the platter.
                2 => {
                    let n = rng.gen_range(0..=data.len());
                    apply(&mut self.durable, offset, &data[..n]);
                }
                _ => unreachable!(),
            }
        }
        self.current = self.durable.clone();
    }
}

/// The declared file set (one file per zone, docs/DESIGN.md §4.4).
#[derive(Default, Clone)]
pub struct SimDisk {
    superblock: SimFile,
    rows: SimFile,
}

impl SimDisk {
    pub fn new() -> Self {
        Self::default()
    }

    fn file(&self, id: FileId) -> &SimFile {
        match id {
            FileId::Superblock => &self.superblock,
            FileId::Rows => &self.rows,
        }
    }

    fn file_mut(&mut self, id: FileId) -> &mut SimFile {
        match id {
            FileId::Superblock => &mut self.superblock,
            FileId::Rows => &mut self.rows,
        }
    }

    pub fn len(&self, id: FileId) -> u64 {
        self.file(id).current.len() as u64
    }

    pub fn is_empty(&self, id: FileId) -> bool {
        self.len(id) == 0
    }

    pub fn read(&self, id: FileId, offset: u64, len: u64) -> Vec<u8> {
        self.file(id).read(offset, len)
    }

    pub fn write(&mut self, id: FileId, offset: u64, data: &[u8]) {
        self.file_mut(id).write(offset, data);
    }

    pub fn fsync(&mut self, id: FileId) {
        self.file_mut(id).fsync();
    }

    /// Simulate a process/machine crash: every unsynced write independently
    /// survives, vanishes, or tears. Reads afterwards see only what survived.
    pub fn crash(&mut self, rng: &mut ChaCha8Rng) {
        self.superblock.crash(rng);
        self.rows.crash(rng);
    }
}
