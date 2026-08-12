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
                        if n == INSERTS as u64 {
                            healed += 1;
                        } else {
                            lost += 1;
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
