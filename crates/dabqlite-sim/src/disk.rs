//! A simulated disk with crash, I/O-failure, and media fault models.
//!
//! Each file tracks two views:
//!
//! - `current`: what reads observe (the OS page cache view). This survives a
//!   *process* restart — the page cache is OS state — which is exactly what
//!   makes visible-but-not-durable bugs expressible.
//! - `durable`: what survives a *machine* crash (media state as of the last
//!   fsync, plus whatever unsynced writes happened to persist).
//!
//! Writes land in `current` immediately and are queued unsynced. `fsync`
//! promotes everything. At a crash, every unsynced write independently gets
//! a [`WriteFate`]: fully persisted, vanished, torn to a prefix, torn to an
//! arbitrary subset of sectors, or torn with a garbage sector (a sector that
//! was being written when power failed can contain anything). Dropping an
//! earlier write while keeping a later one models write reordering.
//!
//! Fates can be chosen by seeded RNG ([`SimDisk::crash`], variable) or
//! supplied explicitly ([`SimDisk::settle_with`], for exhaustive
//! enumeration of every persistence combination — predictable).
//!
//! At-rest faults: [`SimDisk::corrupt`] (bit rot), [`SimDisk::truncate_at_rest`]
//! (lost tail), [`SimDisk::extend_at_rest`] (garbage tail).

use dabqlite_core::FileId;
use rand::{Rng, RngCore};
use rand_chacha::ChaCha8Rng;

/// Tear granularity for subset fates: real disks tear at sector boundaries;
/// we use a deliberately small unit so tears land *inside* fields.
pub const SECTOR: usize = 8;

/// What becomes of one unsynced write at a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFate {
    /// Fully persisted despite the missing fsync.
    Keep,
    /// Never reached the platter.
    Drop,
    /// Only the first `n` bytes persisted (byte-granular: stricter than
    /// sector tearing).
    Prefix(usize),
    /// Only the sectors whose bits are set persisted (arbitrary subset —
    /// covers suffix-only and holes, which prefix tears cannot express).
    Subset(u64),
    /// Subset persisted, and additionally the `garbage_sector`-th sector of
    /// the write's range was persisted with arbitrary bytes: a sector in
    /// flight at power loss can contain anything.
    SubsetGarbage {
        mask: u64,
        garbage_sector: u8,
        garbage: [u8; SECTOR],
    },
}

#[derive(Default, Clone)]
pub struct SimFile {
    durable: Vec<u8>,
    current: Vec<u8>,
    unsynced: Vec<(u64, Vec<u8>)>,
}

fn apply(buf: &mut Vec<u8>, offset: u64, data: &[u8]) {
    if data.is_empty() {
        return;
    }
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

    fn settle_one(durable: &mut Vec<u8>, offset: u64, data: &[u8], fate: WriteFate) {
        match fate {
            WriteFate::Keep => apply(durable, offset, data),
            WriteFate::Drop => {}
            WriteFate::Prefix(n) => {
                assert!(n <= data.len(), "prefix fate exceeds write length");
                apply(durable, offset, &data[..n]);
            }
            WriteFate::Subset(mask) => {
                for sector in 0..data.len().div_ceil(SECTOR) {
                    if mask & (1 << sector) != 0 {
                        let lo = sector * SECTOR;
                        let hi = (lo + SECTOR).min(data.len());
                        apply(durable, offset + lo as u64, &data[lo..hi]);
                    }
                }
            }
            WriteFate::SubsetGarbage {
                mask,
                garbage_sector,
                garbage,
            } => {
                Self::settle_one(durable, offset, data, WriteFate::Subset(mask));
                let lo = garbage_sector as usize * SECTOR;
                assert!(lo < data.len(), "garbage sector beyond write");
                let hi = (lo + SECTOR).min(data.len());
                apply(durable, offset + lo as u64, &garbage[..hi - lo]);
            }
        }
    }

    fn random_fate(len: usize, rng: &mut ChaCha8Rng) -> WriteFate {
        let sectors = len.div_ceil(SECTOR) as u32;
        match rng.gen_range(0u8..10) {
            0..=2 => WriteFate::Drop,
            3..=5 => WriteFate::Keep,
            6..=7 => WriteFate::Prefix(rng.gen_range(0..=len)),
            8 => WriteFate::Subset(rng.gen::<u64>() & ((1u64 << sectors) - 1)),
            9 => {
                let mut garbage = [0u8; SECTOR];
                rng.fill_bytes(&mut garbage);
                WriteFate::SubsetGarbage {
                    mask: rng.gen::<u64>() & ((1u64 << sectors) - 1),
                    garbage_sector: rng.gen_range(0..sectors) as u8,
                    garbage,
                }
            }
            _ => unreachable!(),
        }
    }

    fn crash(&mut self, rng: &mut ChaCha8Rng) {
        let pending = std::mem::take(&mut self.unsynced);
        for (offset, data) in pending {
            let fate = Self::random_fate(data.len(), rng);
            Self::settle_one(&mut self.durable, offset, &data, fate);
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

    /// Simulate a machine crash: every unsynced write independently gets a
    /// random [`WriteFate`]. Reads afterwards see only what survived.
    pub fn crash(&mut self, rng: &mut ChaCha8Rng) {
        self.superblock.crash(rng);
        self.rows.crash(rng);
    }

    /// The unsynced-write window, in deterministic order (superblock file
    /// first, then rows; insertion order within each): `(file, offset, len)`
    /// per write. This is the domain over which [`Self::settle_with`]
    /// assigns fates.
    pub fn unsynced_writes(&self) -> Vec<(FileId, u64, usize)> {
        let mut out = Vec::new();
        for (id, file) in [
            (FileId::Superblock, &self.superblock),
            (FileId::Rows, &self.rows),
        ] {
            for (offset, data) in &file.unsynced {
                out.push((id, *offset, data.len()));
            }
        }
        out
    }

    /// Simulate a machine crash with *chosen* fates, one per unsynced write
    /// in [`Self::unsynced_writes`] order. This is what makes the crash
    /// space exhaustively enumerable rather than merely sampled.
    pub fn settle_with(&mut self, fates: &[WriteFate]) {
        let total = self.superblock.unsynced.len() + self.rows.unsynced.len();
        assert_eq!(fates.len(), total, "one fate per unsynced write");
        let mut it = fates.iter();
        for file in [&mut self.superblock, &mut self.rows] {
            let pending = std::mem::take(&mut file.unsynced);
            for (offset, data) in pending {
                SimFile::settle_one(
                    &mut file.durable,
                    offset,
                    &data,
                    *it.next().expect("counted"),
                );
            }
            file.current = file.durable.clone();
        }
    }

    /// Simulate media corruption (bit rot): flip bits in the durable image.
    /// Call on a quiescent disk (no unsynced writes) — this models damage
    /// discovered at the next restart.
    pub fn corrupt(&mut self, id: FileId, offset: u64, mask: u8) {
        assert!(mask != 0, "corruption must change something");
        let f = self.file_mut(id);
        assert!(
            f.unsynced.is_empty(),
            "corrupt() models at-rest damage; settle or fsync first"
        );
        assert!((offset as usize) < f.durable.len(), "corrupting past EOF");
        f.durable[offset as usize] ^= mask;
        f.current = f.durable.clone();
    }

    /// At-rest truncation: the tail of the file is gone (lost extent,
    /// filesystem repair, backup/restore of a shorter version).
    pub fn truncate_at_rest(&mut self, id: FileId, new_len: u64) {
        let f = self.file_mut(id);
        assert!(f.unsynced.is_empty(), "settle or fsync first");
        assert!((new_len as usize) <= f.durable.len(), "truncate grows file");
        f.durable.truncate(new_len as usize);
        f.current = f.durable.clone();
    }

    /// At-rest extension with arbitrary bytes: preallocation debris, a torn
    /// extension, or filesystem garbage past the logical end.
    pub fn extend_at_rest(&mut self, id: FileId, extra: usize, rng: &mut ChaCha8Rng) {
        let f = self.file_mut(id);
        assert!(f.unsynced.is_empty(), "settle or fsync first");
        let mut tail = vec![0u8; extra];
        rng.fill_bytes(&mut tail);
        f.durable.extend_from_slice(&tail);
        f.current = f.durable.clone();
    }
}
