//! Seed-driven workload generation. Same seed, same workload, always
//! (docs/DESIGN.md §7.2 technique 1).

use std::collections::BTreeSet;

use dabqlite_core::crc32::crc32;
use dabqlite_core::generated::records_v1;
use dabqlite_core::migration::{V1_ROW_SIZE, V1_SCHEMA_HASH, V1_VALUE_LEN};
use dabqlite_core::{FileId, SB_COPY_SIZE, VALUE_LEN};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::disk::SimDisk;

/// Generate `n` inserts with distinct ids and random values.
pub fn gen_workload(seed: u64, n: usize) -> Vec<(u64, [u8; VALUE_LEN])> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut seen = BTreeSet::new();
    let mut ops = Vec::with_capacity(n);
    while ops.len() < n {
        let id: u64 = rng.gen();
        if !seen.insert(id) {
            continue;
        }
        let mut value = [0u8; VALUE_LEN];
        rng.fill_bytes(&mut value);
        ops.push((id, value));
    }
    ops
}

/// Derive an independent RNG stream for a (seed, crash boundary) pair, so
/// crash-time fault decisions don't perturb workload generation.
pub fn crash_rng(seed: u64, boundary: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(seed ^ boundary.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Build the disk a LEGACY (v1-schema) binary would leave behind after
/// `n` committed inserts: v1 rows, a v1 superblock at the protocol's
/// generation for that history, everything fsynced. Returns the disk and
/// the inserted (id, v1 value) pairs in insertion order.
pub fn build_v1_disk(rng: &mut ChaCha8Rng, n: u64) -> (SimDisk, Vec<(u64, [u8; V1_VALUE_LEN])>) {
    let mut disk = SimDisk::new();
    let mut seen = BTreeSet::new();
    let mut ops = Vec::with_capacity(n as usize);
    while (ops.len() as u64) < n {
        let id: u64 = rng.gen();
        if !seen.insert(id) {
            continue;
        }
        let mut value = [0u8; V1_VALUE_LEN];
        rng.fill_bytes(&mut value);
        let row = records_v1::RecordsRow { id, value };
        let mut slot = [0u8; V1_ROW_SIZE];
        records_v1::encode_records_row(&row, &mut slot);
        disk.write(
            FileId::RowsOld,
            ops.len() as u64 * V1_ROW_SIZE as u64,
            &slot,
        );
        ops.push((id, value));
    }
    disk.fsync(FileId::RowsOld);
    let generation = n + 1; // n commits + the initial superblock
    let pair = (generation % 2) * 2;
    let mut copy = [0u8; SB_COPY_SIZE];
    copy[0..8].copy_from_slice(b"DABQSB01");
    copy[8..16].copy_from_slice(&generation.to_le_bytes());
    copy[16..24].copy_from_slice(&n.to_le_bytes());
    copy[24..32].copy_from_slice(&V1_SCHEMA_HASH.to_le_bytes());
    let crc = crc32(&copy[0..32]);
    copy[32..36].copy_from_slice(&crc.to_le_bytes());
    for slot in [pair, pair + 1] {
        disk.write(FileId::Superblock, slot * SB_COPY_SIZE as u64, &copy);
    }
    disk.fsync(FileId::Superblock);
    (disk, ops)
}
