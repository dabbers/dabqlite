//! Seed-driven workload generation. Same seed, same workload, always
//! (docs/DESIGN.md §7.2 technique 1).

use std::collections::BTreeSet;

use dabqlite_core::VALUE_LEN;
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

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
