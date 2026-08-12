//! Exhaustive crash-outcome enumeration. The random sweeps *sample* the
//! crash space; this test *enumerates* it: at every I/O boundary of a run,
//! for every combination of per-write persistence fates (vanish, survive,
//! every sector-aligned prefix tear, an alternating sector subset), recover
//! and assert the N / N+1 atomicity property.
//!
//! This is possible because the protocol keeps the unsynced window tiny
//! (at most the two superblock copy writes, or one row write) — a designed
//! property this test also pins: if the window ever grows, the combination
//! count explodes and the assertion below fails loudly.

use dabqlite_core::{Capacities, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost, WriteFate, SECTOR};

const SEEDS: u64 = 8;
const INSERTS: usize = 6;
const CAPS: Capacities = Capacities { rows: 16 };
/// The protocol's designed maximum unsynced-window size.
const MAX_WINDOW: usize = 2;

/// Every fate worth enumerating for a write of `len` bytes.
fn fates_for(len: usize) -> Vec<WriteFate> {
    let mut fates = vec![WriteFate::Drop, WriteFate::Keep];
    let mut n = SECTOR;
    while n < len {
        fates.push(WriteFate::Prefix(n));
        n += SECTOR;
    }
    // One non-contiguous subset: holes are outcomes prefix tears can't
    // express (suffix-only persistence, alternating sectors).
    let sectors = len.div_ceil(SECTOR);
    if sectors >= 2 {
        fates.push(WriteFate::Subset(
            0xAAAA_AAAA_AAAA_AAAA & ((1 << sectors) - 1),
        ));
        fates.push(WriteFate::Subset(
            0x5555_5555_5555_5554 & ((1 << sectors) - 1),
        ));
    }
    fates
}

/// Cartesian product of fate choices across the window.
fn fate_combos(window: &[(dabqlite_core::FileId, u64, usize)]) -> Vec<Vec<WriteFate>> {
    let mut combos: Vec<Vec<WriteFate>> = vec![Vec::new()];
    for &(_, _, len) in window {
        let mut next = Vec::new();
        for combo in &combos {
            for &fate in &fates_for(len) {
                let mut c = combo.clone();
                c.push(fate);
                next.push(c);
            }
        }
        combos = next;
    }
    combos
}

#[test]
fn every_boundary_x_every_persistence_combination() {
    let mut scenarios = 0u64;
    for seed in 0..SEEDS {
        let ops = gen_workload(seed, INSERTS);

        // Clean run to learn the I/O count.
        let total_io = {
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            assert!(matches!(
                host.open(),
                Driven::Done(Output::OpenDone { result: Ok(0) })
            ));
            for &(id, value) in &ops {
                let out = host.run(ClientOp::Insert { id, value });
                assert!(matches!(
                    out,
                    Driven::Done(Output::InsertDone { result: Ok(()), .. })
                ));
            }
            host.io_count
        };

        for boundary in 0..total_io {
            // Run to the boundary, tracking acked and in-flight.
            let mut host = SimHost::new(CAPS, SimDisk::new(), Some(boundary));
            let mut acked: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
            let mut in_flight: Option<(u64, [u8; VALUE_LEN])> = None;
            let mut crashed = matches!(host.open(), Driven::Crashed);
            if !crashed {
                for &(id, value) in &ops {
                    match host.run(ClientOp::Insert { id, value }) {
                        Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                            acked.push((id, value))
                        }
                        Driven::Done(other) => panic!("unexpected: {other:?}"),
                        Driven::Crashed => {
                            in_flight = Some((id, value));
                            crashed = true;
                            break;
                        }
                    }
                }
            }
            assert!(crashed);
            let snapshot = std::mem::take(&mut host.disk);

            let window = snapshot.unsynced_writes();
            assert!(
                window.len() <= MAX_WINDOW,
                "seed={seed} boundary={boundary}: unsynced window grew to \
                 {} writes — the protocol's tiny-window property broke",
                window.len()
            );
            // The window's writes must be pairwise disjoint (an engine seam
            // property that makes fates independent).
            for (i, &(fa, oa, la)) in window.iter().enumerate() {
                for &(fb, ob, lb) in window.iter().skip(i + 1) {
                    assert!(
                        fa != fb || oa + la as u64 <= ob || ob + lb as u64 <= oa,
                        "seed={seed} boundary={boundary}: overlapping unsynced writes"
                    );
                }
            }

            for fates in fate_combos(&window) {
                let mut disk = snapshot.clone();
                disk.settle_with(&fates);

                let mut recovered = SimHost::new(CAPS, disk, None);
                let n = match recovered.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                    other => panic!(
                        "seed={seed} boundary={boundary} fates={fates:?}: \
                         recovery failed: {other:?}"
                    ),
                };
                let ctx = format!("seed={seed} boundary={boundary} fates={fates:?}");
                let n_acked = acked.len() as u64;
                assert!(
                    n == n_acked || n == n_acked + 1,
                    "[{ctx}] recovered {n}, acked {n_acked}"
                );
                for &(id, value) in &acked {
                    assert_eq!(recovered.get(id), Some(value), "[{ctx}] acked id={id} lost");
                }
                if n == n_acked + 1 {
                    let (id, value) = in_flight.expect("N+1 requires an in-flight insert");
                    assert_eq!(
                        recovered.get(id),
                        Some(value),
                        "[{ctx}] torn in-flight commit"
                    );
                } else if let Some((id, _)) = in_flight {
                    assert_eq!(recovered.get(id), None, "[{ctx}] partial in-flight visible");
                }
                scenarios += 1;
            }
        }
    }
    // Self-check: the enumeration must be substantial, or the sweep proves
    // little. (~8 seeds x ~33 boundaries x avg combos per window.)
    assert!(
        scenarios > 5_000,
        "only {scenarios} scenarios enumerated; the sweep has degenerated"
    );
}
