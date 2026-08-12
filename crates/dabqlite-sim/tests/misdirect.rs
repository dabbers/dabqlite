//! Misdirected writes: the device reports success but the bytes landed at
//! the wrong offset, or in the wrong file entirely (firmware faults;
//! TigerBeetle's fault model includes these for good reason).
//!
//! A misdirected write can silently destroy an acked commit — that is
//! beyond any single-disk durability guarantee. What the engine MUST
//! provide is the detection guarantee (docs/DESIGN.md §5): re-opening a
//! database that suffered a misdirected write yields either
//!
//! - `Ok`, where every row served is exactly correct and the row count
//!   never exceeds what was acked, or
//! - a loud error (`Corrupt`, …).
//!
//! Never a silently wrong answer. Swept over every write in the run and a
//! grid of shift patterns, plus cross-file misdirection.

use std::collections::BTreeMap;

use dabqlite_core::{Capacities, DbError, Output, ROW_SIZE, SB_COPY_SIZE, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, Misdirect, SimDisk, SimHost};

const SEEDS: u64 = 6;
const INSERTS: usize = 6;
const CAPS: Capacities = Capacities { rows: 16 };

fn shifts() -> Vec<Misdirect> {
    vec![
        Misdirect::Shift(-(SB_COPY_SIZE as i64)),
        Misdirect::Shift(SB_COPY_SIZE as i64),
        Misdirect::Shift(-(ROW_SIZE as i64)),
        Misdirect::Shift(ROW_SIZE as i64),
        Misdirect::Shift(8 * ROW_SIZE as i64),
        Misdirect::Shift(-(2 * SB_COPY_SIZE as i64)),
        Misdirect::CrossFile,
    ]
}

#[test]
fn misdirected_writes_are_never_silently_wrong() {
    let mut hit_runs = 0u64;
    let mut ok_outcomes = 0u64;
    let mut detected_outcomes = 0u64;

    for seed in 0..SEEDS {
        let ops = gen_workload(seed, INSERTS);
        let oracle: BTreeMap<u64, [u8; VALUE_LEN]> = ops.iter().copied().collect();

        let total_io = {
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.open();
            for &(id, value) in &ops {
                host.run(ClientOp::Insert { id, value });
            }
            host.io_count
        };

        for io_index in 0..total_io {
            for kind in shifts() {
                let ctx = format!("seed={seed} io_index={io_index} kind={kind:?}");
                let mut host = SimHost::new(CAPS, SimDisk::new(), None);
                host.misdirect_at = Some((io_index, kind));

                // The run itself proceeds obliviously: the device lied, so
                // every op completes and every insert acks.
                assert!(matches!(
                    host.open(),
                    Driven::Done(Output::OpenDone { result: Ok(0) })
                ));
                for &(id, value) in &ops {
                    let out = host.run(ClientOp::Insert { id, value });
                    assert!(
                        matches!(out, Driven::Done(Output::InsertDone { result: Ok(()), .. })),
                        "[{ctx}] insert did not ack: {out:?}"
                    );
                }
                if host.misdirected == 0 {
                    continue; // op at io_index wasn't a write, or shift < 0
                }
                hit_runs += 1;

                // Restart and check the detection guarantee.
                let disk = std::mem::take(&mut host.disk);
                let mut reopened = SimHost::new(CAPS, disk, None);
                match reopened.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        ok_outcomes += 1;
                        assert!(
                            n <= INSERTS as u64,
                            "[{ctx}] recovered {n} rows from {INSERTS} inserts"
                        );
                        // Every row served must be exactly right; a missing
                        // row is permitted (destroyed by the fault, but
                        // honestly absent), a wrong value never is.
                        for (&id, &value) in &oracle {
                            match reopened.get(id) {
                                None => {}
                                Some(got) => assert_eq!(
                                    got, value,
                                    "[{ctx}] id={id} served with wrong bytes"
                                ),
                            }
                        }
                        // Negative space: ids never inserted stay absent.
                        for probe in [u64::MAX, u64::MAX - 1] {
                            if !oracle.contains_key(&probe) {
                                assert_eq!(reopened.get(probe), None, "[{ctx}] phantom row");
                            }
                        }
                    }
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. } | DbError::CapacityBelowData { .. }),
                    }) => {
                        detected_outcomes += 1;
                    }
                    other => panic!("[{ctx}] unexpected open outcome: {other:?}"),
                }
            }
        }
    }

    // Self-checks on sweep coverage: the grid must actually misdirect
    // writes, and both outcome classes must occur — if every case recovered
    // Ok the corruption paths went untested, and vice versa.
    assert!(
        hit_runs > 100,
        "only {hit_runs} misdirected runs; grid degenerated"
    );
    assert!(ok_outcomes > 0, "no survivable misdirection observed");
    assert!(detected_outcomes > 0, "no detected corruption observed");
}
