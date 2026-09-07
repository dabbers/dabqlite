//! Atomic batches, held to the standard every other commit is held to.
//!
//! A batch is not a transaction log and not a second commit protocol: it
//! is the SAME commit protocol with more rows before the fsync. That is
//! the whole design argument — append `n` rows, fsync once, flip the
//! superblock once — so a crash inside a batch has to resolve
//! all-or-nothing by exactly the mechanism that makes a crash inside a
//! single insert safe.
//!
//! What this suite proves:
//!
//! - a batch is durable, and every op in it is visible after restart;
//! - crashing or failing I/O at EVERY boundary of a batch leaves it
//!   entirely applied or entirely absent — never half — for batch lengths
//!   from 1 to the format's maximum;
//! - a refused batch performs literally ZERO I/O and changes nothing, for
//!   every reason a batch can be refused;
//! - ops inside a batch see the batch's own earlier ops, so `insert 5;
//!   delete 5; insert 5` means what it reads as;
//! - the fsync count of a batch does not grow with its length, which is
//!   the performance claim, checked rather than asserted in prose;
//! - a long interleaved workload of batches and single writes matches a
//!   BTreeMap exactly, across crashes;
//! - recovery still distinguishes an interrupted batch from evidence that
//!   an acknowledged commit was rolled back, at every batch length.

use std::collections::BTreeMap;

use dabqlite_core::{
    BatchOp, Capacities, DbError, FileId, Output, MAX_COMMIT_ROWS, ROW_SIZE, VALUE_LEN,
};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 512 };

/// A distinctive value for `n`, leaked so it can be borrowed by a batch
/// op written inline.
///
/// Deliberate: batch ops borrow their payloads, and a suite that had to
/// keep every payload alive by hand would read as a test about lifetimes
/// rather than a test about batches. A test binary exits; these bytes are
/// never reclaimed and never need to be.
fn val(n: u64) -> &'static [u8] {
    let mut v = [0u8; VALUE_LEN];
    v[..8].copy_from_slice(&n.to_le_bytes());
    v[8..].copy_from_slice(&(n.wrapping_mul(0x9E37_79B9)).to_le_bytes());
    Box::leak(Box::new(v))
}

/// A value of exactly `len` bytes, deterministic in `n`, leaked like
/// `val`. Used to build values that span several row slots.
fn long_val(n: u64, len: usize) -> &'static [u8] {
    let mut v = vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (n.wrapping_mul(0x9E37_79B9).wrapping_add(i as u64) & 0xFF) as u8;
    }
    Box::leak(v.into_boxed_slice())
}

fn fresh() -> SimHost {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    host
}

/// The fixed-slot write path still takes a full-width array; this is the
/// bridge from the byte-slice values the batch path uses.
fn arr(v: &[u8]) -> [u8; VALUE_LEN] {
    <[u8; VALUE_LEN]>::try_from(v).expect("a full-width value")
}

fn got(host: &mut SimHost, id: u64) -> Option<Vec<u8>> {
    host.get_bytes(id)
}

fn open(disk: SimDisk) -> (SimHost, u64) {
    let mut host = SimHost::new(CAPS, disk, None);
    let n = match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
        other => panic!("open: {other:?}"),
    };
    (host, n)
}

fn batch(host: &mut SimHost, ops: &[BatchOp]) -> Result<u64, (u16, DbError)> {
    match host.batch(ops) {
        Driven::Done(Output::BatchDone {
            rows,
            result: Ok(()),
        }) => Ok(rows),
        Driven::Done(Output::BatchDone {
            result: Err(reject),
            rows,
        }) => {
            assert_eq!(rows, 0, "a refused batch must report committing no rows");
            Err((reject.at, reject.error))
        }
        other => panic!("batch: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// The basic promise
// ---------------------------------------------------------------------

#[test]
fn every_op_in_a_batch_is_visible_together_and_survives_restart() {
    let mut host = fresh();
    let ops: Vec<BatchOp> = (0..8)
        .map(|i| BatchOp::Insert {
            id: i,
            value: val(i),
        })
        .collect();
    assert_eq!(batch(&mut host, &ops), Ok(8));
    for i in 0..8 {
        assert_eq!(
            got(&mut host, i),
            Some(val(i).to_vec()),
            "id {i} not visible after commit"
        );
    }

    let disk = std::mem::take(&mut host.disk);
    let (mut host, n) = open(disk);
    assert_eq!(n, 8);
    for i in 0..8 {
        assert_eq!(
            got(&mut host, i),
            Some(val(i).to_vec()),
            "id {i} lost across restart"
        );
    }
}

#[test]
fn a_batch_mixes_inserts_updates_and_deletes_in_one_commit() {
    let mut host = fresh();
    for i in 0..4 {
        host.run(ClientOp::Insert {
            id: i,
            value: arr(val(i)),
        });
    }
    assert_eq!(
        batch(
            &mut host,
            &[
                BatchOp::Update {
                    id: 0,
                    value: val(100)
                },
                BatchOp::Delete { id: 1 },
                BatchOp::Insert {
                    id: 9,
                    value: val(9)
                },
                BatchOp::Put {
                    id: 2,
                    value: val(200)
                },
                BatchOp::Put {
                    id: 10,
                    value: val(10)
                },
            ]
        ),
        Ok(5)
    );
    assert_eq!(
        got(&mut host, 0),
        Some(val(100).to_vec()),
        "update did not apply"
    );
    assert_eq!(got(&mut host, 1), None, "delete did not apply");
    assert_eq!(
        got(&mut host, 2),
        Some(val(200).to_vec()),
        "put over a live row"
    );
    assert_eq!(
        got(&mut host, 3),
        Some(val(3).to_vec()),
        "untouched row moved"
    );
    assert_eq!(
        got(&mut host, 9),
        Some(val(9).to_vec()),
        "insert did not apply"
    );
    assert_eq!(
        got(&mut host, 10),
        Some(val(10).to_vec()),
        "put of a new row"
    );

    // The same picture after a restart, from the file alone.
    let disk = std::mem::take(&mut host.disk);
    let (mut host, _) = open(disk);
    assert_eq!(got(&mut host, 0), Some(val(100).to_vec()));
    assert_eq!(got(&mut host, 1), None);
    assert_eq!(got(&mut host, 2), Some(val(200).to_vec()));
    assert_eq!(got(&mut host, 3), Some(val(3).to_vec()));
    assert_eq!(got(&mut host, 9), Some(val(9).to_vec()));
    assert_eq!(got(&mut host, 10), Some(val(10).to_vec()));
}

/// A batch's ops see the batch's own earlier ops. This is the difference
/// between a batch and a bag: `insert 5; delete 5; insert 5` has to be
/// legal and has to end with the LAST value, or the ordering means nothing.
#[test]
fn ops_inside_a_batch_see_the_batch_that_precedes_them() {
    let mut host = fresh();
    assert_eq!(
        batch(
            &mut host,
            &[
                BatchOp::Insert {
                    id: 5,
                    value: val(1)
                },
                BatchOp::Delete { id: 5 },
                BatchOp::Insert {
                    id: 5,
                    value: val(2)
                },
                BatchOp::Update {
                    id: 5,
                    value: val(3)
                },
            ]
        ),
        Ok(4)
    );
    assert_eq!(
        got(&mut host, 5),
        Some(val(3).to_vec()),
        "the last word must win"
    );
    // Four slots consumed: two records, a tombstone, and a superseding
    // update. Nothing was elided just because it cancelled out.
    assert_eq!(host.engine.live_count(), 1);
    assert_eq!(host.engine.dead_slots(), 3);

    let disk = std::mem::take(&mut host.disk);
    let (mut host, _) = open(disk);
    assert_eq!(
        got(&mut host, 5),
        Some(val(3).to_vec()),
        "replay disagreed with the live engine about the last word"
    );
}

/// `Remove` is the forgiving delete, and it stages no row when there is
/// nothing to remove — so a batch is not lost to one target that had
/// already gone, and does not burn a slot on a tombstone for nothing.
#[test]
fn remove_of_an_absent_row_costs_nothing_and_does_not_sink_the_batch() {
    let mut host = fresh();
    host.run(ClientOp::Insert {
        id: 1,
        value: arr(val(1)),
    });
    let before = host.engine.usage().0;
    assert_eq!(
        batch(
            &mut host,
            &[
                BatchOp::Remove { id: 1 },
                BatchOp::Remove { id: 777 },
                BatchOp::Remove { id: 888 },
            ]
        ),
        Ok(1),
        "only the row that existed should be staged"
    );
    assert_eq!(host.engine.usage().0, before + 1);
    assert_eq!(got(&mut host, 1), None);

    // A batch of nothing but absent removes is a no-op, and performs no
    // I/O at all: there is no generation to flip.
    let io_before = host.io_count;
    assert_eq!(
        batch(&mut host, &[BatchOp::Remove { id: 777 }]),
        Ok(0),
        "a batch with nothing to do must commit nothing"
    );
    assert_eq!(
        host.io_count, io_before,
        "a no-op batch performed I/O; it has nothing to make durable"
    );
}

#[test]
fn an_empty_batch_is_a_no_op_with_no_io_and_no_generation_flip() {
    let mut host = fresh();
    host.run(ClientOp::Insert {
        id: 1,
        value: arr(val(1)),
    });
    let io_before = host.io_count;
    assert_eq!(batch(&mut host, &[]), Ok(0));
    assert_eq!(host.io_count, io_before, "an empty batch performed I/O");
    assert_eq!(got(&mut host, 1), Some(val(1).to_vec()));
}

// ---------------------------------------------------------------------
// The performance claim, checked
// ---------------------------------------------------------------------

/// The reason to batch at all: `n` writes cost the same TWO fsyncs as one
/// write, not `2n`. Stated as an assertion rather than a README sentence.
#[test]
fn a_batch_costs_two_fsyncs_no_matter_how_long_it_is() {
    for n in [1usize, 2, 8, 32, MAX_COMMIT_ROWS] {
        let mut host = fresh();
        let ops: Vec<BatchOp> = (0..n as u64)
            .map(|i| BatchOp::Insert {
                id: i,
                value: val(i),
            })
            .collect();
        let fsyncs_before = host.n_fsyncs;
        let writes_before = host.n_writes;
        assert_eq!(batch(&mut host, &ops), Ok(n as u64));
        assert_eq!(
            host.n_fsyncs - fsyncs_before,
            2,
            "batch of {n} did not cost exactly two fsyncs"
        );
        // One write per row, plus the two superblock copies. That is the
        // whole cost model, and it is linear in rows with no hidden term.
        assert_eq!(
            host.n_writes - writes_before,
            n as u64 + 2,
            "batch of {n} wrote something other than n rows plus two \
             superblock copies"
        );
    }
}

/// And the same work done singly costs `2n` fsyncs — so the saving is
/// real, measured against the alternative rather than asserted alone.
#[test]
fn the_same_writes_done_singly_cost_two_fsyncs_each() {
    let n = 16u64;
    let mut host = fresh();
    let fsyncs_before = host.n_fsyncs;
    for i in 0..n {
        host.run(ClientOp::Insert {
            id: i,
            value: arr(val(i)),
        });
    }
    assert_eq!(host.n_fsyncs - fsyncs_before, 2 * n);
}

// ---------------------------------------------------------------------
// Refusal: whole, and free
// ---------------------------------------------------------------------

/// Every reason a batch can be refused, each checked for the same two
/// things: the refusal names the right operation, and it costs ZERO I/O.
/// A refusal that wrote something would be a partial batch by another
/// name.
#[test]
fn every_refusal_is_whole_and_performs_no_io() {
    let mut base = fresh();
    for i in 0..4u64 {
        base.run(ClientOp::Insert {
            id: i,
            value: arr(val(i)),
        });
    }
    let disk = base.disk.clone();

    let cases: Vec<(&str, Vec<BatchOp>, u16, DbError)> = vec![
        (
            "duplicate against a committed row",
            vec![
                BatchOp::Insert {
                    id: 50,
                    value: val(50),
                },
                BatchOp::Insert {
                    id: 2,
                    value: val(2),
                },
            ],
            1,
            DbError::DuplicateId { id: 2 },
        ),
        (
            "duplicate against the batch's own earlier insert",
            vec![
                BatchOp::Insert {
                    id: 50,
                    value: val(50),
                },
                BatchOp::Insert {
                    id: 50,
                    value: val(51),
                },
            ],
            1,
            DbError::DuplicateId { id: 50 },
        ),
        (
            "update of an absent row",
            vec![BatchOp::Update {
                id: 99,
                value: val(99),
            }],
            0,
            DbError::NotFound { id: 99 },
        ),
        (
            "delete of an absent row",
            vec![
                BatchOp::Insert {
                    id: 60,
                    value: val(60),
                },
                BatchOp::Delete { id: 99 },
            ],
            1,
            DbError::NotFound { id: 99 },
        ),
        (
            "update of a row the batch itself deleted",
            vec![
                BatchOp::Delete { id: 1 },
                BatchOp::Update {
                    id: 1,
                    value: val(1),
                },
            ],
            1,
            DbError::NotFound { id: 1 },
        ),
        (
            "longer than the format can describe",
            (0..MAX_COMMIT_ROWS as u64 + 1)
                .map(|i| BatchOp::Insert {
                    id: 1000 + i,
                    value: val(i),
                })
                .collect(),
            MAX_COMMIT_ROWS as u16,
            DbError::BatchTooLong {
                rows: MAX_COMMIT_ROWS as u64 + 1,
                max: MAX_COMMIT_ROWS as u64,
            },
        ),
    ];

    for (name, ops, at, error) in cases {
        let mut host = SimHost::new(CAPS, disk.clone(), None);
        host.open();
        let io_before = host.io_count;
        assert_eq!(
            batch(&mut host, &ops),
            Err((at, error)),
            "[{name}] wrong refusal"
        );
        assert_eq!(
            host.io_count, io_before,
            "[{name}] a refused batch performed I/O"
        );
        // And the database is untouched and still usable.
        for i in 0..4u64 {
            assert_eq!(
                got(&mut host, i),
                Some(val(i).to_vec()),
                "[{name}] id {i} changed"
            );
        }
        assert!(
            matches!(
                host.run(ClientOp::Insert {
                    id: 4242,
                    value: arr(val(4242))
                }),
                Driven::Done(Output::InsertDone { result: Ok(()), .. })
            ),
            "[{name}] database unusable after a refused batch"
        );
    }
}

/// A batch that would run past the declared capacity is refused whole and
/// early, naming the op that would not have fit — not accepted and
/// truncated, which would be a silent partial commit.
#[test]
fn a_batch_that_overruns_capacity_is_refused_naming_the_op_that_does_not_fit() {
    const SMALL: Capacities = Capacities { rows: 6 };
    let mut host = SimHost::new(SMALL, SimDisk::new(), None);
    host.open();
    for i in 0..4u64 {
        host.run(ClientOp::Insert {
            id: i,
            value: arr(val(i)),
        });
    }
    // Two slots left; ask for four.
    let ops: Vec<BatchOp> = (10..14u64)
        .map(|i| BatchOp::Insert {
            id: i,
            value: val(i),
        })
        .collect();
    let io_before = host.io_count;
    assert_eq!(
        batch(&mut host, &ops),
        Err((
            2,
            DbError::Full {
                entity: "records",
                capacity: 6,
                dead: 0,
            }
        ))
    );
    assert_eq!(host.io_count, io_before, "an overrunning batch wrote rows");
    // The two that WOULD have fit did not sneak in.
    for i in 10..14u64 {
        assert_eq!(
            got(&mut host, i),
            None,
            "id {i} was written by a refused batch"
        );
    }
    // A batch that exactly fills the remaining room is accepted.
    assert_eq!(
        batch(
            &mut host,
            &[
                BatchOp::Insert {
                    id: 10,
                    value: val(10)
                },
                BatchOp::Insert {
                    id: 11,
                    value: val(11)
                },
            ]
        ),
        Ok(2)
    );
    assert_eq!(host.engine.usage(), (6, 6));
}

// ---------------------------------------------------------------------
// Crash and I/O failure at every boundary, at every length
// ---------------------------------------------------------------------

/// THE property. Crash at every I/O boundary of a batch, at every batch
/// length up to the format's maximum, and settle the disk every way the
/// model allows: the batch is either entirely there or entirely absent.
#[test]
fn a_crash_at_every_boundary_of_a_batch_is_all_or_nothing() {
    for n in [1usize, 2, 3, 8, 17, MAX_COMMIT_ROWS] {
        // A base with rows to update and delete, so the batch is not all
        // inserts: a half-applied mixed batch would be visible in more
        // ways than a half-applied set of inserts.
        let mut base_host = fresh();
        for i in 0..4u64 {
            base_host.run(ClientOp::Insert {
                id: i,
                value: arr(val(i)),
            });
        }
        let base = std::mem::take(&mut base_host.disk);

        let mut ops: Vec<BatchOp> = Vec::with_capacity(n);
        ops.push(BatchOp::Update {
            id: 0,
            value: val(1000),
        });
        if n >= 2 {
            ops.push(BatchOp::Delete { id: 1 });
        }
        for i in ops.len()..n {
            ops.push(BatchOp::Insert {
                id: 100 + i as u64,
                value: val(100 + i as u64),
            });
        }

        // Boundaries: every write and fsync the batch performs, plus a
        // couple past the end so "crashed after everything" is covered.
        let boundaries = n as u64 + 4;
        for boundary in 0..boundaries {
            for settle in 0..3u64 {
                let ctx = format!("n={n} boundary={boundary} settle={settle}");
                let mut host = SimHost::new(CAPS, base.clone(), None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                let _ = host.batch(&ops);

                let mut disk = std::mem::take(&mut host.disk);
                let mut rng = crash_rng(0xBA7C4, settle);
                disk.crash(&mut rng);

                let (mut host, live) = open(disk);
                // Did the batch land? Every op must agree on the answer.
                let applied = got(&mut host, 0) == Some(val(1000).to_vec());
                for (k, op) in ops.iter().enumerate() {
                    let agrees = match *op {
                        BatchOp::Update { id, value } => {
                            let want = if applied { value } else { val(id) };
                            got(&mut host, id) == Some(want.to_vec())
                        }
                        BatchOp::Delete { id } => got(&mut host, id).is_none() == applied,
                        BatchOp::Insert { id, value } => {
                            got(&mut host, id) == applied.then(|| value.to_vec())
                        }
                        BatchOp::Put { .. } | BatchOp::Remove { .. } => true,
                    };
                    assert!(
                        agrees,
                        "[{ctx}] op {k} ({op:?}) disagrees with the rest of the \
                         batch (batch applied = {applied}) — a batch landed HALF"
                    );
                }
                // `open` reports LIVE records, not slots. The batch's
                // update replaces one, its delete removes one, and the
                // rest are inserts.
                let inserts = ops
                    .iter()
                    .filter(|o| matches!(o, BatchOp::Insert { .. }))
                    .count() as u64;
                let deletes = ops
                    .iter()
                    .filter(|o| matches!(o, BatchOp::Delete { .. }))
                    .count() as u64;
                assert_eq!(
                    live,
                    if applied { 4 + inserts - deletes } else { 4 },
                    "[{ctx}] live count disagrees with what is readable"
                );
                // Rows the batch never mentioned are untouched either way.
                for i in 2..4u64 {
                    assert_eq!(
                        got(&mut host, i),
                        Some(val(i).to_vec()),
                        "[{ctx}] neighbour {i}"
                    );
                }
                // And the database still takes writes.
                assert!(matches!(
                    host.run(ClientOp::Insert {
                        id: 900_001,
                        value: arr(val(7))
                    }),
                    Driven::Done(Output::InsertDone { result: Ok(()), .. })
                ));
            }
        }
    }
}

/// I/O failure at every boundary of a batch: fail-stop, nothing applied,
/// and the restart resolves all-or-nothing exactly like the crash case.
#[test]
fn an_io_failure_at_every_boundary_of_a_batch_fail_stops_cleanly() {
    for n in [1usize, 4, 9] {
        let mut base_host = fresh();
        for i in 0..3u64 {
            base_host.run(ClientOp::Insert {
                id: i,
                value: arr(val(i)),
            });
        }
        let base = std::mem::take(&mut base_host.disk);
        let ops: Vec<BatchOp> = (0..n as u64)
            .map(|i| BatchOp::Insert {
                id: 200 + i,
                value: val(200 + i),
            })
            .collect();

        for fail_at in 0..(n as u64 + 4) {
            let ctx = format!("n={n} fail_at={fail_at}");
            let mut host = SimHost::new(CAPS, base.clone(), None);
            host.open();
            host.fail_after = Some(host.io_count + fail_at);
            match host.batch(&ops) {
                Driven::Done(Output::BatchDone {
                    rows: 0,
                    result: Err(reject),
                }) => assert!(
                    matches!(reject.error, DbError::IoFailed { .. }),
                    "[{ctx}] wrong failure: {reject:?}"
                ),
                Driven::Done(Output::BatchDone {
                    result: Ok(()),
                    rows,
                }) => assert_eq!(rows, n as u64, "[{ctx}]"),
                other => panic!("[{ctx}] batch must fail-stop or commit: {other:?}"),
            }

            let disk = std::mem::take(&mut host.disk);
            let (mut host, live) = open(disk);
            let applied = host.get(200).is_some();
            for i in 0..n as u64 {
                assert_eq!(
                    got(&mut host, 200 + i),
                    applied.then(|| val(200 + i).to_vec()),
                    "[{ctx}] op {i} disagrees with the batch"
                );
            }
            assert_eq!(live, if applied { 3 + n as u64 } else { 3 }, "[{ctx}]");

            for i in 0..3u64 {
                assert_eq!(
                    got(&mut host, i),
                    Some(val(i).to_vec()),
                    "[{ctx}] neighbour {i}"
                );
            }
        }
    }
}

/// A fail-stop mid-batch must not leave staged effects behind for the next
/// batch to apply twice. The engine's own invariant checks this, but only
/// a restart-free reuse of the same engine would catch a leak, so ask
/// directly: after a failure, every operation is refused, and nothing the
/// failed batch intended shows up.
#[test]
fn a_failed_batch_leaves_no_staged_effects_behind() {
    let mut host = fresh();
    host.run(ClientOp::Insert {
        id: 1,
        value: arr(val(1)),
    });
    host.fail_after = Some(host.io_count + 1);
    let ops = [
        BatchOp::Insert {
            id: 2,
            value: val(2),
        },
        BatchOp::Delete { id: 1 },
    ];
    match host.batch(&ops) {
        Driven::Done(Output::BatchDone {
            rows: 0,
            result: Err(_),
        }) => {}
        other => panic!("expected a fail-stop: {other:?}"),
    }
    // Fail-stopped: everything is refused, including another batch.
    match host.batch(&[BatchOp::Insert {
        id: 3,
        value: val(3),
    }]) {
        Driven::Done(Output::BatchDone {
            rows: 0,
            result: Err(reject),
        }) => assert!(matches!(reject.error, DbError::IoFailed { .. })),
        other => panic!("expected refusal after fail-stop: {other:?}"),
    }
    // And on reopen, the failed batch's delete did not happen.
    let disk = std::mem::take(&mut host.disk);
    let (mut host, _) = open(disk);
    assert_eq!(
        got(&mut host, 1),
        Some(val(1).to_vec()),
        "a failed batch deleted a row"
    );
    assert_eq!(got(&mut host, 2), None, "a failed batch inserted a row");
}

// ---------------------------------------------------------------------
// Recovery still tells an interrupted batch from lost acknowledged data
// ---------------------------------------------------------------------

/// The reason the span byte exists. Interrupt a batch of `n` at every
/// point: the rows left past the manifest are exactly what one commit
/// group can explain, so recovery must NOT report rollback evidence.
#[test]
fn an_interrupted_batch_is_never_mistaken_for_lost_acknowledged_data() {
    for n in [1usize, 2, 5, 16, MAX_COMMIT_ROWS] {
        let mut base_host = fresh();
        base_host.run(ClientOp::Insert {
            id: 0,
            value: arr(val(0)),
        });
        let base = std::mem::take(&mut base_host.disk);
        let ops: Vec<BatchOp> = (0..n as u64)
            .map(|i| BatchOp::Insert {
                id: 300 + i,
                value: val(i),
            })
            .collect();

        for boundary in 0..(n as u64 + 3) {
            for settle in 0..3u64 {
                let ctx = format!("n={n} boundary={boundary} settle={settle}");
                let mut host = SimHost::new(CAPS, base.clone(), None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                let _ = host.batch(&ops);
                let mut disk = std::mem::take(&mut host.disk);
                let mut rng = crash_rng(0x5A17_5EED, settle);
                disk.crash(&mut rng);

                let (host, _) = open(disk);
                let report = host.engine.recovery_report();
                assert!(
                    !report.rollback_evidence,
                    "[{ctx}] an interrupted batch was reported as LOST \
                     ACKNOWLEDGED DATA ({} orphan rows) — the span byte is \
                     not doing its job",
                    report.orphan_valid_rows
                );
            }
        }
    }
}

/// And the detector still fires when it should: hand-build a rows file
/// whose slots past the manifest cannot come from ONE commit — two
/// complete single-row commits — and recovery must say so.
#[test]
fn two_complete_commits_past_the_manifest_are_still_reported_as_rollback() {
    let mut host = fresh();
    for i in 0..4u64 {
        host.run(ClientOp::Insert {
            id: i,
            value: arr(val(i)),
        });
    }
    // Roll the superblock back to generation 1 / 2 rows by replaying the
    // first two inserts into a fresh disk, then splicing in the longer
    // rows file. Two acknowledged commits are then stranded past the
    // manifest, which is precisely what a lying fsync leaves behind.
    let full_rows = host.disk.contents(FileId::Rows);
    let mut short = fresh();
    for i in 0..2u64 {
        short.run(ClientOp::Insert {
            id: i,
            value: arr(val(i)),
        });
    }
    let mut disk = std::mem::take(&mut short.disk);
    disk.write(FileId::Rows, 0, &full_rows);

    let (host, live) = open(disk);
    assert_eq!(live, 2, "the manifest still names two rows");
    let report = host.engine.recovery_report();
    assert_eq!(report.orphan_valid_rows, 2);
    assert!(
        report.rollback_evidence,
        "two acknowledged commits were stranded past the manifest and \
         recovery stayed quiet"
    );
}

// ---------------------------------------------------------------------
// The long game: an oracle
// ---------------------------------------------------------------------

/// A long interleaved workload of batches and single writes, checked
/// against a BTreeMap after every step and after every restart. If any
/// batch is ever half-applied, the oracle diverges.
/// A long interleaved workload of batches and single writes, over values
/// of every length from empty to several slots, checked against a
/// BTreeMap after every step and after every restart. If a batch is ever
/// half-applied, or a long value ever comes back a slot short, the oracle
/// diverges.
#[test]
fn batches_and_single_writes_track_a_btreemap_exactly_across_restarts() {
    for seed in 0..8u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // Lengths that straddle every interesting boundary: empty, inside
        // one slot, exactly one slot, one byte over, and several slots.
        let lengths = [0usize, 1, 15, 16, 17, 31, 32, 33, 100];
        let mut oracle: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut host = fresh();

        let cost = |len: usize| (len.div_ceil(VALUE_LEN).max(1)) as u64;

        for step in 0..40 {
            let ctx = format!("seed={seed} step={step}");
            if next() % 3 == 0 {
                // A batch. Build it against the oracle so it is legal by
                // construction, then apply it to both.
                let n = 1 + (next() % 6) as usize;
                let mut ops = Vec::with_capacity(n);
                let mut shadow = oracle.clone();
                let mut rows = 0u64;
                for _ in 0..n {
                    let id = next() % 24;
                    let len = lengths[(next() % lengths.len() as u64) as usize];
                    let v = long_val(next(), len);
                    match next() % 4 {
                        0 if shadow.contains_key(&id) => {
                            ops.push(BatchOp::Delete { id });
                            shadow.remove(&id);
                            rows += 1;
                        }
                        1 if shadow.contains_key(&id) => {
                            ops.push(BatchOp::Update { id, value: v });
                            shadow.insert(id, v.to_vec());
                            rows += cost(len);
                        }
                        _ => {
                            ops.push(BatchOp::Put { id, value: v });
                            shadow.insert(id, v.to_vec());
                            rows += cost(len);
                        }
                    }
                }
                if host.engine.usage().0 + rows > CAPS.rows || rows as usize > MAX_COMMIT_ROWS {
                    continue;
                }
                assert_eq!(batch(&mut host, &ops), Ok(rows), "[{ctx}]");
                oracle = shadow;
            } else {
                let id = next() % 24;
                let len = lengths[(next() % lengths.len() as u64) as usize];
                let v = long_val(next(), len);
                if host.engine.usage().0 + cost(len) > CAPS.rows {
                    continue;
                }
                let op = if oracle.contains_key(&id) {
                    BatchOp::Update { id, value: v }
                } else {
                    BatchOp::Insert { id, value: v }
                };
                assert_eq!(batch(&mut host, &[op]), Ok(cost(len)), "[{ctx}]");
                oracle.insert(id, v.to_vec());
            }

            for id in 0..24u64 {
                assert_eq!(
                    got(&mut host, id),
                    oracle.get(&id).cloned(),
                    "[{ctx}] id {id} diverged from the oracle"
                );
            }

            // Restart every few steps: the file alone must reproduce the
            // oracle, not just the in-memory engine.
            if step % 7 == 6 {
                let disk = std::mem::take(&mut host.disk);
                let (reopened, live) = open(disk);
                host = reopened;
                assert_eq!(host.engine.live_count(), oracle.len() as u64, "[{ctx}]");
                assert!(live >= oracle.len() as u64);
                for id in 0..24u64 {
                    assert_eq!(
                        got(&mut host, id),
                        oracle.get(&id).cloned(),
                        "[{ctx}] id {id} diverged after restart"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Residue must not accumulate
// ---------------------------------------------------------------------

/// A crash loop must not be able to fake evidence of lost data.
///
/// Rows past the manifest are the residue of commits that were never
/// acknowledged. Left in place, residue from SEVERAL incarnations piles
/// up — and a wide interrupted commit followed by a narrow one leaves
/// rows claiming two different commit sizes, which is EXACTLY what one
/// acknowledged commit rolled back by a lying fsync looks like. Recovery
/// would then report data loss on a database that lost nothing, on every
/// open, forever, and a host that treats the flag as fatal (as the
/// documentation asks) would refuse to start a healthy database.
///
/// So recovery drops the residue: what lies past the manifest always
/// belongs to at most one commit — this incarnation's.
#[test]
fn a_wide_torn_commit_then_a_narrow_one_is_not_mistaken_for_lost_data() {
    for settle in 0..3u64 {
        let ctx = format!("settle={settle}");
        let mut base = fresh();
        for i in 0..4u64 {
            base.run(ClientOp::Insert {
                id: i,
                value: arr(val(i)),
            });
        }
        let disk = std::mem::take(&mut base.disk);

        // Life one: a wide batch, interrupted after its rows are written
        // but before the superblock flips.
        let wide: Vec<BatchOp> = (100..140u64)
            .map(|i| BatchOp::Insert {
                id: i,
                value: val(i),
            })
            .collect();
        let mut host = SimHost::new(CAPS, disk, None);
        host.open();
        host.crash_after = Some(host.io_count + wide.len() as u64);
        let _ = host.batch(&wide);
        let mut disk = std::mem::take(&mut host.disk);
        disk.crash(&mut crash_rng(0xD1DE_5EED, settle));

        // Life two: reopen (which is where the residue is dealt with),
        // then a NARROW batch, also interrupted.
        let (mut host, _) = open(disk);
        let narrow = [
            BatchOp::Insert {
                id: 200,
                value: val(200),
            },
            BatchOp::Insert {
                id: 201,
                value: val(201),
            },
        ];
        host.crash_after = Some(host.io_count + 2);
        let _ = host.batch(&narrow);
        let mut disk = std::mem::take(&mut host.disk);
        disk.crash(&mut crash_rng(0xA1AA_5EED, settle));

        // Life three: the verdict.
        let (mut host, live) = open(disk);
        let report = host.engine.recovery_report();
        assert!(
            !report.rollback_evidence,
            "[{ctx}] two INTERRUPTED commits of different widths were reported \
             as lost acknowledged data ({} orphan rows) — a crash loop can \
             brick a healthy database",
            report.orphan_valid_rows
        );
        assert_eq!(live, 4, "[{ctx}] the committed rows are all that survives");
        for i in 0..4u64 {
            assert_eq!(got(&mut host, i), Some(val(i).to_vec()), "[{ctx}] row {i}");
        }
        // And the database is still writable, and still quiet on reopen.
        assert!(matches!(
            host.batch(&[BatchOp::Insert {
                id: 300,
                value: val(300)
            }]),
            Driven::Done(Output::BatchDone { result: Ok(()), .. })
        ));
        let disk = std::mem::take(&mut host.disk);
        let (host, _) = open(disk);
        assert!(
            !host.engine.recovery_report().rollback_evidence,
            "[{ctx}] the alarm came back on a later open"
        );
    }
}

/// The mechanism, stated directly: an open leaves nothing past the
/// manifest, so `orphan_valid_rows` always describes THIS incarnation.
#[test]
fn an_open_leaves_no_residue_past_the_manifest() {
    let mut base = fresh();
    base.run(ClientOp::Insert {
        id: 1,
        value: arr(val(1)),
    });
    let disk = std::mem::take(&mut base.disk);

    let ops: Vec<BatchOp> = (10..30u64)
        .map(|i| BatchOp::Insert {
            id: i,
            value: val(i),
        })
        .collect();
    let mut host = SimHost::new(CAPS, disk, None);
    host.open();
    host.crash_after = Some(host.io_count + ops.len() as u64);
    let _ = host.batch(&ops);
    let mut disk = std::mem::take(&mut host.disk);
    disk.crash(&mut crash_rng(0x5C4A_9EED, 1));
    let before = disk.contents(FileId::Rows).len();
    assert!(
        before > ROW_SIZE,
        "the setup should leave residue to clear: {before} bytes"
    );

    let (mut host, live) = open(disk);
    // The first open reports what it found, then clears it.
    let first = host.engine.recovery_report();
    assert_eq!(live, 1);
    // Force the truncate to become durable the way a real open does, then
    // look at the file.
    let disk = std::mem::take(&mut host.disk);
    assert_eq!(
        disk.contents(FileId::Rows).len(),
        ROW_SIZE,
        "the rows file still holds residue past the manifest ({} orphans found)",
        first.orphan_valid_rows
    );

    let (host, _) = open(disk);
    assert_eq!(
        host.engine.recovery_report().orphan_valid_rows,
        0,
        "a second open still sees residue the first one should have cleared"
    );
}
