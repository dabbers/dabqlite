//! The trigram index against its free oracle (docs/DESIGN.md §4.6): naive
//! substring match over the same values. EXACT equality, always — that
//! oracle being exact is precisely why trigram won the "vector or
//! trigram" open decision (§10). Results are in insertion (row) order,
//! which is what the naive oracle produces by construction.

use dabqlite_core::{BatchOp, Capacities, DbError, Input, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 32 };

fn contains(hay: &[u8; VALUE_LEN], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// The oracle: insertion-ordered naive scan.
fn oracle(ops: &[(u64, [u8; VALUE_LEN])], needle: &[u8]) -> Vec<(u64, [u8; VALUE_LEN])> {
    ops.iter()
        .filter(|(_, v)| contains(v, needle))
        .copied()
        .collect()
}

fn find_input(needle: &[u8], after: Option<dabqlite_core::FindCursor>) -> Input<'static> {
    let mut padded = [0u8; VALUE_LEN];
    padded[..needle.len()].copy_from_slice(needle);
    Input::Find {
        needle: padded,
        needle_len: needle.len() as u8,
        after,
    }
}

#[test]
fn every_needle_length_matches_the_oracle_on_random_workloads() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, 24);
        let mut host = SimHost::new(CAPS, SimDisk::new(), None);
        host.open();
        for &(id, value) in &ops {
            host.run(ClientOp::Insert { id, value });
        }
        // Needles that MUST hit: every-length prefixes and infixes of
        // stored values. Needles that mostly miss: seeded noise.
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for &(_, v) in ops.iter().take(4) {
            for len in 0..=VALUE_LEN {
                needles.push(v[..len].to_vec());
            }
            for len in [1usize, 2, 3, 5, 13] {
                needles.push(v[VALUE_LEN - len..].to_vec());
                needles.push(v[3..3 + len.min(VALUE_LEN - 3)].to_vec());
            }
        }
        for i in 0..16u8 {
            needles.push(vec![i.wrapping_mul(37) ^ seed as u8; 3]);
            needles.push(vec![i, i ^ 0xFF, 42, 7]);
        }
        let io_before = host.io_count;
        for needle in &needles {
            assert_eq!(
                host.find_all(needle),
                oracle(&ops, needle),
                "seed={seed} needle={needle:?}"
            );
        }
        // Reads never touch the disk: zero opportunity to fault or stall.
        assert_eq!(host.io_count, io_before, "find performed I/O");
    }
}

#[test]
fn paging_walks_large_results_exactly() {
    // 24 rows sharing a common infix: 3 full pages of 8, and the
    // continuation contract (a full page sets `next`; the last
    // continuation may return an empty page) must reassemble exactly.
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    let mut ops = Vec::new();
    for i in 0..24u64 {
        let mut value = [0u8; VALUE_LEN];
        value[..6].copy_from_slice(b"needle");
        value[6..14].copy_from_slice(&i.to_le_bytes());
        ops.push((i * 7 + 1, value));
        host.run(ClientOp::Insert {
            id: i * 7 + 1,
            value,
        });
    }
    assert_eq!(host.find_all(b"needle"), oracle(&ops, b"needle"));
    assert_eq!(host.find_all(b"needle").len(), 24);

    // Single pages are bounded and NEWEST FIRST — the order a paged
    // search box wants, and the order that makes a continuation resume
    // where it stopped instead of walking the chain again.
    match host.run_input(find_input(b"needle", None)) {
        Driven::Done(Output::FindDone { result: Ok(page) }) => {
            assert_eq!(page.count, 8);
            assert!(page.next.is_some());
            let ids: Vec<u64> = page.items[..8].iter().map(|r| r.id).collect();
            assert_eq!(
                ids,
                (16..24u64).rev().map(|i| i * 7 + 1).collect::<Vec<_>>()
            );
        }
        other => panic!("{other:?}"),
    }
}

/// Paging is LINEAR in the number of matches, not quadratic.
///
/// The old cursor was a row number, and since a trigram's chain descends,
/// resuming "above row N" meant walking the chain from its head and
/// verifying nearly every match again on every page. A bookmark store
/// measured the consequence: a needle matching all 50,000 rows took 31
/// SECONDS, against 58 ms for a brute-force scan of the same data — an
/// index 538x slower than no index.
///
/// Counting verifications is the honest way to test that: it is the work
/// the old shape repeated, and it does not depend on how fast this
/// machine happens to be.
#[test]
fn paging_a_common_needle_verifies_each_row_about_once() {
    const N: u64 = 400;
    let caps = Capacities { rows: 1024 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for i in 0..N {
        let mut value = [0u8; VALUE_LEN];
        value[..6].copy_from_slice(b"needle");
        value[6..14].copy_from_slice(&i.to_le_bytes());
        host.run(ClientOp::Insert { id: i, value });
    }

    let hits = host.find_all(b"needle");
    assert_eq!(hits.len(), N as usize, "every row matches this needle");

    // Every row is verified about once across the whole paged walk. The
    // quadratic shape would be ~N^2/8 = 20,000 verifications for N=400;
    // the bound below is generous enough to be stable and tight enough
    // that the old shape fails it by two orders of magnitude.
    let verifications = host.engine.find_verifications();
    assert!(
        verifications <= 3 * N,
        "paging {N} matches cost {verifications} row verifications; a page \
         is re-walking the chain instead of resuming in it"
    );
}

/// One long value must not turn the index off for the whole database.
///
/// A posting is filed under the row its window STARTS in, so a match past
/// the first slot of a multi-row value is filed under a continuation row.
/// While that mapping was not inverted, the chain was not a superset of
/// the answer and the engine compensated by scanning every row whenever
/// `long_values > 0` — meaning ONE 17-byte value anywhere put every
/// subsequent search on the scan path, for the life of the database. A
/// bookmark store measured the consequence at 618 ns before and 61.7 ms
/// after inserting a single long row.
///
/// Counting verifications is the honest way to test that: it is the work
/// the scan path does and the chain does not, and it does not depend on
/// how fast this machine happens to be.
#[test]
fn one_long_value_does_not_put_the_whole_database_on_the_scan_path() {
    const N: u64 = 400;
    let caps = Capacities { rows: 1024 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for i in 0..N {
        let mut value = [0u8; VALUE_LEN];
        value[..6].copy_from_slice(b"filler");
        value[6..14].copy_from_slice(&i.to_le_bytes());
        host.run(ClientOp::Insert { id: i, value });
    }
    // Three rows and a bit, with the needle wholly inside the THIRD slot
    // — the posting the old mapping could not resolve — and a second one
    // straddling the first seam.
    let mut long = vec![b'.'; 3 * VALUE_LEN + 5];
    long[40..46].copy_from_slice(b"needle");
    long[14..20].copy_from_slice(b"seamed");
    assert!(matches!(
        host.batch(&[BatchOp::Insert {
            id: 9999,
            value: &long,
        }]),
        Driven::Done(Output::BatchDone { result: Ok(()), .. })
    ));

    for needle in [&b"needle"[..], b"seamed"] {
        let before = host.engine.find_verifications();
        let hits = host.find_all_bytes(needle);
        let cost = host.engine.find_verifications() - before;
        assert_eq!(hits.len(), 1, "{needle:?} should match exactly one value");
        assert_eq!(hits[0].0, 9999);
        assert_eq!(hits[0].1, long, "the whole value comes back, not a slot");
        assert!(
            cost < N / 4,
            "{needle:?} verified {cost} rows across {N}+ rows: the chain is \
             not being used, so one long value has turned the index off"
        );
    }

    // Short needles have no trigram to look up and still scan — that is
    // the documented exception, not a regression.
    let before = host.engine.find_verifications();
    assert_eq!(host.find_all_bytes(b"ne").len(), 1);
    assert!(host.engine.find_verifications() - before >= N);
}

/// Exactness over long values, against the naive oracle, with matches at
/// every position of a multi-row value — including the seams, where a
/// window belongs to two slots at once.
#[test]
fn long_values_match_the_oracle_at_every_offset() {
    let caps = Capacities { rows: 256 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    let mut stored: Vec<(u64, Vec<u8>)> = Vec::new();
    for i in 0..12u64 {
        let len = (i as usize * 7) % (4 * VALUE_LEN) + 1;
        let mut value = vec![0u8; len];
        for (k, b) in value.iter_mut().enumerate() {
            *b = ((i as u8).wrapping_mul(31)).wrapping_add(k as u8);
        }
        assert!(matches!(
            host.batch(&[BatchOp::Insert {
                id: i,
                value: &value
            }]),
            Driven::Done(Output::BatchDone { result: Ok(()), .. })
        ));
        stored.push((i, value));
    }
    for (_, v) in &stored {
        for off in 0..v.len().saturating_sub(2) {
            for len in [3usize, 4, 9, VALUE_LEN] {
                if off + len > v.len() {
                    continue;
                }
                let needle = &v[off..off + len];
                let want: Vec<(u64, Vec<u8>)> = stored
                    .iter()
                    .filter(|(_, s)| s.windows(len).any(|w| w == needle))
                    .cloned()
                    .collect();
                assert_eq!(
                    host.find_all_bytes(needle),
                    want,
                    "needle={needle:?} off={off}"
                );
            }
        }
    }
}

/// Paging over long values: the cursor tracks the chain by POSTING row
/// while a page carries HEAD rows, so a resume has to land in the right
/// place without repeating or skipping a value. Thirty multi-row values
/// sharing a needle is four pages of that.
#[test]
fn paging_long_values_neither_repeats_nor_skips() {
    const N: u64 = 30;
    let caps = Capacities { rows: 512 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    let mut stored: Vec<(u64, Vec<u8>)> = Vec::new();
    for i in 0..N {
        // The needle sits past the first slot, so every posting that
        // finds it is filed under a continuation row.
        let mut value = vec![b'-'; 3 * VALUE_LEN];
        value[36..42].copy_from_slice(b"needle");
        value[0..8].copy_from_slice(&i.to_le_bytes());
        assert!(matches!(
            host.batch(&[BatchOp::Insert {
                id: i * 3 + 1,
                value: &value,
            }]),
            Driven::Done(Output::BatchDone { result: Ok(()), .. })
        ));
        stored.push((i * 3 + 1, value));
    }
    assert_eq!(host.find_all_bytes(b"needle"), stored);

    // And it is linear: paging all N matches verifies each value a
    // bounded number of times, not once per page.
    let before = host.engine.find_verifications();
    let _ = host.find_all_bytes(b"needle");
    let cost = host.engine.find_verifications() - before;
    assert!(
        cost <= 3 * N,
        "paging {N} long-value matches cost {cost} verifications"
    );
}

#[test]
fn find_survives_every_crash_boundary_like_everything_else() {
    for seed in 0..4u64 {
        let ops = gen_workload(seed, 6);
        let extra = (u64::MAX - seed, [0xC3; VALUE_LEN]);
        for boundary in 0..5u64 {
            for settle in 0..2u64 {
                let mut host = SimHost::new(CAPS, SimDisk::new(), None);
                host.open();
                for &(id, value) in &ops {
                    host.run(ClientOp::Insert { id, value });
                }
                host.crash_after = Some(host.io_count + boundary);
                assert!(matches!(
                    host.run(ClientOp::Insert {
                        id: extra.0,
                        value: extra.1
                    }),
                    Driven::Crashed
                ));
                let mut disk = std::mem::take(&mut host.disk);
                disk.crash(&mut crash_rng(0x7161, seed * 100 + boundary * 10 + settle));

                let mut host = SimHost::new(CAPS, disk, None);
                let n = match host.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                    other => panic!("seed={seed} b={boundary}: {other:?}"),
                };
                // The rebuilt trigram index answers over EXACTLY the
                // recovered prefix — all-or-nothing, like every index.
                let mut committed: Vec<(u64, [u8; VALUE_LEN])> = ops.clone();
                committed.push(extra);
                committed.truncate(n as usize);
                for needle in [
                    &committed
                        .first()
                        .map_or([0u8; 3], |(_, v)| [v[0], v[1], v[2]])[..],
                    &[0xC3, 0xC3, 0xC3],
                    b"",
                ] {
                    assert_eq!(
                        host.find_all(needle),
                        oracle(&committed, needle),
                        "seed={seed} b={boundary} settle={settle} needle={needle:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn find_respects_the_engine_lifecycle() {
    // Before open: refused, not answered from nothing.
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    assert!(matches!(
        host.run_input(find_input(b"abc", None)),
        Driven::Done(Output::FindDone {
            result: Err(DbError::NotOpen)
        })
    ));
    // After fail-stop: the original error, every time.
    host.open();
    host.fail_after = Some(host.io_count);
    host.run(ClientOp::Insert {
        id: 1,
        value: [1; VALUE_LEN],
    });
    assert!(matches!(
        host.run_input(find_input(b"abc", None)),
        Driven::Done(Output::FindDone {
            result: Err(DbError::IoFailed { .. })
        })
    ));
}

#[test]
fn find_reads_committed_state_only() {
    // The trigram index updates at the COMMIT POINT: a value whose
    // insert never committed must never appear, even though its row may
    // sit in the arena as an orphan.
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    host.run(ClientOp::Insert {
        id: 1,
        value: *b"before-the-crash",
    });
    host.crash_after = Some(host.io_count + 2); // row durable, sb not
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: 2,
            value: *b"never-committed!",
        }),
        Driven::Crashed
    ));
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(1) })
    ));
    assert_eq!(host.find_all(b"never"), vec![]);
    assert_eq!(host.find_all(b"before").len(), 1);
}

#[test]
fn pathological_chains_all_rows_one_trigram() {
    // Every row the same value: ONE trigram chain of maximum length (the
    // index degenerates to a scan — still exact, still zero I/O), at the
    // capacity wall.
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    let mut ops = Vec::new();
    for i in 0..CAPS.rows {
        let value = [0xAB; VALUE_LEN];
        ops.push((i, value));
        assert!(matches!(
            host.run(ClientOp::Insert { id: i, value }),
            Driven::Done(Output::InsertDone { result: Ok(()), .. })
        ));
    }
    let io = host.io_count;
    for needle in [
        &[0xAB, 0xAB, 0xAB][..],
        &[0xAB; VALUE_LEN][..],
        &[0xAB, 0xAB, 0xAC][..],
    ] {
        assert_eq!(host.find_all(needle), oracle(&ops, needle), "{needle:?}");
    }
    assert_eq!(host.io_count, io, "degenerate chains still cost zero I/O");

    // And the rebuilt index after recovery handles the same degenerate
    // chain identically.
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == CAPS.rows
    ));
    assert_eq!(
        host.find_all(&[0xAB, 0xAB, 0xAB]),
        oracle(&ops, &[0xAB, 0xAB, 0xAB])
    );
}

#[test]
fn maximum_distinct_trigrams_stress_the_table() {
    // The opposite pathology: every window of every row distinct, driving
    // the trigram table toward its worst-case load. Exactness holds.
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    let mut ops = Vec::new();
    for i in 0..CAPS.rows {
        let mut value = [0u8; VALUE_LEN];
        for (k, b) in value.iter_mut().enumerate() {
            *b = (i as u8)
                .wrapping_mul(16)
                .wrapping_add(k as u8)
                .wrapping_mul(7);
        }
        ops.push((i, value));
        host.run(ClientOp::Insert { id: i, value });
    }
    for &(_, v) in ops.iter().take(6) {
        for off in [0usize, 5, VALUE_LEN - 3] {
            let needle = &v[off..off + 3];
            assert_eq!(host.find_all(needle), oracle(&ops, needle), "{needle:?}");
        }
    }
}
