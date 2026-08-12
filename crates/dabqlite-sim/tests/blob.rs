//! Blob allocator oracle fuzz (docs/DESIGN.md §7.5): the oracle is a map of
//! live handles to their exact contents. Random alloc/free/rewrite traffic,
//! diffed continuously. Invariants asserted:
//!
//! - No two live handles overlap (checked geometrically, and implicitly by
//!   content verification: an overlap would smear one payload into another).
//! - Free-byte accounting is exact (conservation below the high-water mark).
//! - A full alloc-then-free cycle returns to the initial state, zero leak.

use std::collections::BTreeMap;

use dabqlite_core::blob::NUM_CLASSES;
use dabqlite_core::{BlobAllocator, BlobError, BlobHandle, BLOB_HARD_MAX};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

const SEEDS: u64 = 16;
const OPS: usize = 3000;
const ARENA: u64 = 4 << 20; // 4 MiB: small enough to hit Full regularly

/// Deterministic payload for a nonce: cheap to regenerate for verification.
fn pattern(nonce: u64, len: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (nonce.wrapping_mul(0x9E37_79B9).wrapping_add(i as u64) >> 3) as u8)
        .collect()
}

/// Log-uniform length: exercises every size class instead of only the top.
fn random_len(rng: &mut ChaCha8Rng) -> u32 {
    let log2 = rng.gen_range(0..=18u32);
    let base = 1u32 << log2;
    rng.gen_range(0..=base).min(BLOB_HARD_MAX)
}

fn check_disjoint_and_verify(
    a: &BlobAllocator,
    live: &BTreeMap<u64, (BlobHandle, u32)>,
    ctx: &str,
) {
    // Every live payload must read back exactly.
    for (&nonce, &(handle, len)) in live {
        assert_eq!(
            a.data(handle),
            pattern(nonce, len).as_slice(),
            "[{ctx}] payload for nonce={nonce} corrupted"
        );
    }
    // Geometric disjointness of live blocks...
    let mut ranges: Vec<(u32, u32)> = live
        .values()
        .map(|&(h, _)| (h.offset, h.offset + (64u32 << h.class)))
        .collect();
    // ...and of free blocks against live blocks and each other.
    a.for_each_free_block(|offset, class| ranges.push((offset, offset + (64u32 << class))));
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        assert!(
            pair[0].1 <= pair[1].0,
            "[{ctx}] blocks overlap: {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }
    // Conservation, from the outside: stats must balance exactly.
    let s = a.stats();
    assert_eq!(
        s.live_block_bytes + s.free_list_bytes,
        s.high_water,
        "[{ctx}] byte accounting leaked"
    );
    assert_eq!(
        s.live_count,
        live.len() as u64,
        "[{ctx}] live count diverged"
    );
    let payload: u64 = live.values().map(|&(_, len)| len as u64).sum();
    assert_eq!(
        s.live_payload_bytes, payload,
        "[{ctx}] payload bytes diverged"
    );
}

#[test]
fn allocator_matches_oracle_under_random_traffic() {
    for seed in 0..SEEDS {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut a = BlobAllocator::new(ARENA);
        let mut live: BTreeMap<u64, (BlobHandle, u32)> = BTreeMap::new();
        let mut nonce: u64 = 0;
        let mut fulls = 0u64;

        for step in 0..OPS {
            let ctx = format!("seed={seed} step={step}");
            let do_alloc = live.is_empty() || rng.gen_bool(0.55);
            if do_alloc {
                let len = random_len(&mut rng);
                match a.alloc(len) {
                    Ok(handle) => {
                        nonce += 1;
                        a.data_mut(handle).copy_from_slice(&pattern(nonce, len));
                        live.insert(nonce, (handle, len));
                    }
                    Err(BlobError::Full { block_bytes, .. }) => {
                        assert!(
                            block_bytes >= len,
                            "[{ctx}] Full block smaller than request"
                        );
                        fulls += 1;
                    }
                    Err(e) => panic!("[{ctx}] unexpected error: {e:?}"),
                }
            } else {
                // Free a pseudo-random live handle (BTreeMap order is
                // deterministic, so this replays exactly).
                let idx = rng.gen_range(0..live.len());
                let &key = live.keys().nth(idx).expect("non-empty");
                let (handle, len) = live.remove(&key).expect("present");
                // Verify contents before releasing: a corrupted block must
                // be caught while we still know who owned it.
                assert_eq!(a.data(handle), pattern(key, len).as_slice(), "[{ctx}]");
                a.free(handle);
            }

            if step.is_multiple_of(250) {
                check_disjoint_and_verify(&a, &live, &ctx);
            }
        }
        check_disjoint_and_verify(&a, &live, &format!("seed={seed} end"));
        assert!(fulls > 0, "seed={seed}: arena never filled; weaken ARENA");

        // Drain everything: the zero-leak cycle (§7.5).
        let keys: Vec<u64> = live.keys().copied().collect();
        for key in keys {
            let (handle, len) = live.remove(&key).expect("present");
            assert_eq!(a.data(handle), pattern(key, len).as_slice());
            a.free(handle);
        }
        let s = a.stats();
        assert_eq!(s.live_count, 0, "seed={seed}");
        assert_eq!(s.live_block_bytes, 0, "seed={seed}");
        assert_eq!(
            s.free_list_bytes, s.high_water,
            "seed={seed}: bytes leaked over the lifetime"
        );

        // And the drained allocator must be fully reusable.
        let h = a
            .alloc(BLOB_HARD_MAX.min((ARENA / 2) as u32))
            .expect("reusable");
        a.free(h);
    }
}

#[test]
fn every_size_class_is_exercised() {
    // Meta-test on the generator: if random_len drifts, the fuzz above
    // silently stops covering classes. Fail loudly instead.
    let mut rng = ChaCha8Rng::seed_from_u64(0);
    let mut hit = [false; NUM_CLASSES];
    for _ in 0..10_000 {
        let mut a = BlobAllocator::new(1 << 19);
        if let Ok(h) = a.alloc(random_len(&mut rng)) {
            hit[h.class as usize] = true;
        }
    }
    // Classes above the small arena can't allocate; check the reachable ones.
    for (class, hit) in hit.iter().enumerate().take(13) {
        if (64u64 << class) <= (1 << 19) {
            assert!(hit, "size class {class} never exercised by the generator");
        }
    }
}
