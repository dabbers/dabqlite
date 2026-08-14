//! B+tree oracle fuzz (docs/DESIGN.md §7.5): the oracle is `BTreeMap`,
//! the obviously-correct ordered map. Random inserts and range scans,
//! diffed exactly; deep structural invariants checked continuously; the
//! node-pool bound verified never to be approached.

use std::collections::BTreeMap;

use dabqlite_core::btree::{pool_nodes_for, BTreeIndex};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

const SEEDS: u64 = 24;
const INSERTS: usize = 2000;

/// Key generators with different shapes: uniform, clustered, sequential,
/// reverse, ends-interleaved. Structure bugs hide in shapes.
fn key_for(shape: u64, i: usize, rng: &mut ChaCha8Rng) -> u64 {
    match shape % 5 {
        0 => rng.gen(),
        1 => rng.gen_range(0..4096), // dense collisions region
        2 => i as u64,               // ascending
        3 => (u64::MAX / 2).wrapping_sub(i as u64), // descending
        _ => {
            if i.is_multiple_of(2) {
                i as u64
            } else {
                u64::MAX - i as u64
            }
        }
    }
}

#[test]
fn btree_matches_btreemap_under_random_traffic() {
    for seed in 0..SEEDS {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut tree = BTreeIndex::new(INSERTS as u64);
        let mut oracle: BTreeMap<u64, u64> = BTreeMap::new();

        for i in 0..INSERTS {
            let key = key_for(seed, i, &mut rng);
            if oracle.contains_key(&key) {
                continue; // engine never feeds duplicates
            }
            oracle.insert(key, i as u64);
            tree.insert(key, i as u64);

            if i.is_multiple_of(200) {
                tree.check_invariants();
            }
            // Continuous spot diffs: a random range per insert.
            let a: u64 = rng.gen();
            let b: u64 = rng.gen();
            let (lo, hi) = (a.min(b), a.max(b));
            let mut got = Vec::new();
            tree.for_each_from(lo, |k, v| {
                if k > hi {
                    return false;
                }
                got.push((k, v));
                true
            });
            let want: Vec<(u64, u64)> = oracle.range(lo..=hi).map(|(&k, &v)| (k, v)).collect();
            assert_eq!(got, want, "seed={seed} i={i}: range [{lo},{hi}] diverged");
        }

        tree.check_invariants();
        assert_eq!(tree.len(), oracle.len() as u64, "seed={seed}");

        // Full-scan equality, in order.
        let mut got = Vec::new();
        tree.for_each_from(0, |k, v| {
            got.push((k, v));
            true
        });
        let want: Vec<(u64, u64)> = oracle.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(got, want, "seed={seed}: full scan diverged");

        // Degenerate ranges: empty (lo>hi handled by caller), singleton,
        // off-by-one around present keys.
        for (&k, &v) in oracle.iter().take(50) {
            let mut hit = None;
            tree.for_each_from(k, |kk, vv| {
                hit = Some((kk, vv));
                false
            });
            assert_eq!(hit, Some((k, v)), "seed={seed}: singleton at {k}");
            if k > 0 {
                let mut first = None;
                tree.for_each_from(k - 1, |kk, _| {
                    first = Some(kk);
                    false
                });
                let want = oracle.range(k - 1..).next().map(|(&kk, _)| kk);
                assert_eq!(first, want, "seed={seed}: start at {}-1", k);
            }
        }

        // The pool bound must hold with real headroom, not by luck.
        assert!(
            (tree.nodes_used() as u64) < pool_nodes_for(INSERTS as u64),
            "seed={seed}: node usage {} at bound {}",
            tree.nodes_used(),
            pool_nodes_for(INSERTS as u64)
        );
    }
}
