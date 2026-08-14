//! Lying fsyncs (fsyncgate): the syscall reports success but persists
//! nothing. On a single disk this is the one fault where acked-durability
//! genuinely cannot be upheld — TigerBeetle survives it by repairing from
//! other replicas; a single-node store cannot. What CAN and MUST hold:
//!
//! - **Prefix consistency**: after a lie and a later power loss, the
//!   recovered state is exactly the first N acked commits for some N —
//!   in order, no holes, no reordering, correct bytes.
//! - **Detection where possible**: if the lie leaves the superblock
//!   referencing a row that never persisted, open fails `Corrupt` rather
//!   than serving garbage.
//! - **Self-healing**: a lie whose file gets a later honest fsync loses
//!   nothing.
//!
//! Never: silently wrong data, phantom rows, or out-of-order survival.

use dabqlite_core::{Capacities, DbError, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const SEEDS: u64 = 6;
const INSERTS: usize = 6;
const CAPS: Capacities = Capacities { rows: 16 };
const SETTLES: u64 = 4;

#[test]
fn lying_fsync_at_every_index_preserves_prefix_consistency() {
    let mut healed = 0u64;
    let mut lost = 0u64;
    let mut detected = 0u64;

    for seed in 0..SEEDS {
        let ops = gen_workload(seed, INSERTS);
        let total_io = {
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.open();
            for &(id, value) in &ops {
                host.run(ClientOp::Insert { id, value });
            }
            host.io_count
        };

        for lie_at in 0..total_io {
            // Run the whole workload; the lie is invisible, everything acks.
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.lie_fsync_at = Some(lie_at);
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
            if host.fsyncs_lied == 0 {
                continue; // index wasn't an fsync
            }
            let snapshot = std::mem::take(&mut host.disk);

            // Power loss now, under several settle outcomes.
            for settle in 0..SETTLES {
                let ctx = format!("seed={seed} lie_at={lie_at} settle={settle}");
                let mut disk = snapshot.clone();
                disk.crash(&mut crash_rng(seed ^ (lie_at << 8), settle));

                let mut recovered = SimHost::new(CAPS, disk, None);
                match recovered.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        assert!(n <= INSERTS as u64, "[{ctx}] impossible count {n}");
                        // Bounded loss: a SINGLE lie can only leave the
                        // final commit unsynced — every earlier lie is
                        // healed by a later honest fsync of the same file.
                        // Loss deeper than one commit means the healing
                        // property broke.
                        assert!(
                            n + 1 >= INSERTS as u64,
                            "[{ctx}] single lie lost {} commits; only 1 is possible",
                            INSERTS as u64 - n
                        );
                        if n == INSERTS as u64 {
                            healed += 1;
                        } else {
                            lost += 1;
                            // The rolled-back commit's row was durably
                            // fsynced (honestly) before the lying superblock
                            // fsync — its evidence must have been seen.
                            let report = recovered.engine.recovery_report();
                            assert!(
                                report.orphan_valid_rows >= 1,
                                "[{ctx}] rollback left no surviving evidence \
                                 despite an honest rows fsync"
                            );
                        }
                        // Exact prefix: the first n acked inserts present
                        // and correct, everything after absent. No holes,
                        // no reordering, no phantoms.
                        for (i, &(id, value)) in ops.iter().enumerate() {
                            if (i as u64) < n {
                                assert_eq!(
                                    recovered.get(id),
                                    Some(value),
                                    "[{ctx}] prefix row {i} lost or wrong"
                                );
                            } else {
                                assert_eq!(
                                    recovered.get(id),
                                    None,
                                    "[{ctx}] non-prefix row {i} survived out of order"
                                );
                            }
                        }
                        // The survivor must still be a working database.
                        let out = recovered.run(ClientOp::Insert {
                            id: u64::MAX,
                            value: [0xEE; VALUE_LEN],
                        });
                        assert!(
                            matches!(out, Driven::Done(Output::InsertDone { result: Ok(()), .. })),
                            "[{ctx}] recovered database not writable: {out:?}"
                        );
                    }
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. }),
                    }) => {
                        // The superblock outlived a row its lie unsynced:
                        // detected, not served. Honest failure.
                        detected += 1;
                    }
                    other => panic!("[{ctx}] unexpected outcome: {other:?}"),
                }
            }
        }
    }

    // All three outcome classes must be exercised or the sweep is weaker
    // than it claims: early lies heal via later honest fsyncs of the same
    // file; a lie at the final superblock fsync can lose the last commit;
    // a lie at the final rows fsync can leave the superblock pointing at a
    // row that vanished (detected).
    assert!(healed > 0, "no self-healed lie observed");
    assert!(lost > 0, "no honest loss observed; loss path untested");
    assert!(detected > 0, "no detected dangling-superblock observed");
}

/// The full fsyncgate scenario: one swallowed error and every subsequent
/// fsync is a silent no-op, enabling arbitrarily deep rollback. When the
/// row data survives the crash (its writes hit the platter even though the
/// fsyncs lied), recovery MUST see the evidence and flag the rollback:
/// silent loss is only permitted when no distinguishing bit exists on disk.
#[test]
fn deep_rollback_with_surviving_evidence_is_flagged() {
    use dabqlite_sim::WriteFate;

    for seed in 0..6u64 {
        let ops = gen_workload(seed, INSERTS);
        let mut host = SimHost::new(CAPS, SimDisk::new(), None);
        // Fresh init is I/O ops 0..=2 (two copy writes + fsync). Every
        // fsync after init lies: nothing the inserts write is ever synced.
        host.lie_fsync_from = Some(3);
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
        assert!(
            host.fsyncs_lied >= (INSERTS as u64) * 2,
            "lies did not fire"
        );

        // Power loss where the row writes happened to persist but the
        // superblock writes did not — the maximum-evidence outcome.
        let mut disk = std::mem::take(&mut host.disk);
        let fates: Vec<WriteFate> = disk
            .unsynced_writes()
            .iter()
            .map(|&(file, _, _)| match file {
                dabqlite_core::FileId::Superblock => WriteFate::Drop,
                dabqlite_core::FileId::Rows | dabqlite_core::FileId::RowsOld => WriteFate::Keep,
            })
            .collect();
        disk.settle_with(&fates);

        let mut recovered = SimHost::new(CAPS, disk, None);
        match recovered.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                // Rolled all the way back to the empty init generation…
                assert_eq!(n, 0, "seed={seed}: expected full rollback, got {n}");
                let report = recovered.engine.recovery_report();
                // …but every lost commit left its row behind, and the scan
                // must have found and flagged them.
                assert_eq!(
                    report.orphan_valid_rows, INSERTS as u64,
                    "seed={seed}: evidence miscounted"
                );
                assert!(
                    report.rollback_evidence,
                    "seed={seed}: {INSERTS} acked commits vanished with \
                     surviving evidence, and recovery stayed silent"
                );
                // The prefix (empty) is honest: nothing is served wrong.
                for &(id, _) in &ops {
                    assert_eq!(recovered.get(id), None, "seed={seed}: phantom row");
                }
            }
            other => panic!("seed={seed}: recovery failed: {other:?}"),
        }
    }
}

/// Randomized persistent-lie runs: whatever the settle outcome, the result
/// is an exact in-order prefix (or detected corruption), and whenever the
/// evidence flag fires it corresponds to real loss.
#[test]
fn persistent_lies_keep_prefix_consistency_under_random_settles() {
    let mut evidence_flags = 0u64;
    for seed in 0..12u64 {
        let ops = gen_workload(seed, INSERTS);
        for lie_from in [3u64, 8, 13, 18] {
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.lie_fsync_from = Some(lie_from);
            host.open();
            for &(id, value) in &ops {
                host.run(ClientOp::Insert { id, value });
            }
            let snapshot = std::mem::take(&mut host.disk);

            for settle in 0..SETTLES {
                let ctx = format!("seed={seed} lie_from={lie_from} settle={settle}");
                let mut disk = snapshot.clone();
                disk.crash(&mut crash_rng(seed ^ (lie_from << 16), settle));
                let mut recovered = SimHost::new(CAPS, disk, None);
                match recovered.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        assert!(n <= INSERTS as u64, "[{ctx}] impossible count");
                        // Exact in-order prefix, nothing else.
                        for (i, &(id, value)) in ops.iter().enumerate() {
                            let expect = ((i as u64) < n).then_some(value);
                            assert_eq!(
                                recovered.get(id),
                                expect,
                                "[{ctx}] row {i} violates prefix consistency"
                            );
                        }
                        let report = recovered.engine.recovery_report();
                        if report.rollback_evidence {
                            evidence_flags += 1;
                            assert!(
                                n < INSERTS as u64,
                                "[{ctx}] evidence flagged without actual loss"
                            );
                        }
                    }
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. }),
                    }) => {}
                    other => panic!("[{ctx}] unexpected outcome: {other:?}"),
                }
            }
        }
    }
    assert!(
        evidence_flags > 0,
        "no rollback evidence observed across the persistent-lie sweep"
    );
}
