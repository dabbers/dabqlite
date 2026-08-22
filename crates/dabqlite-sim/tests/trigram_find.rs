//! The trigram index against its free oracle (docs/DESIGN.md §4.6): naive
//! substring match over the same values. EXACT equality, always — that
//! oracle being exact is precisely why trigram won the "vector or
//! trigram" open decision (§10). Results are in insertion (row) order,
//! which is what the naive oracle produces by construction.

use dabqlite_core::{Capacities, DbError, Input, Output, VALUE_LEN};
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

fn find_input(needle: &[u8], after: Option<u64>) -> Input<'static> {
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

    // Single pages are bounded and ascending by row.
    match host.run_input(find_input(b"needle", None)) {
        Driven::Done(Output::FindDone { result: Ok(page) }) => {
            assert_eq!(page.count, 8);
            assert!(page.next.is_some());
            let ids: Vec<u64> = page.items[..8].iter().map(|&(id, _)| id).collect();
            assert_eq!(ids, (0..8u64).map(|i| i * 7 + 1).collect::<Vec<_>>());
        }
        other => panic!("{other:?}"),
    }
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
