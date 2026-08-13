//! The storm: every IN-BUDGET fault class layered into one lifetime, with
//! the strictest possible invariant — **zero loss, zero drift, always**.
//!
//! The declared fault budget (docs/FAULTS.md): machine crashes with
//! arbitrary settle of unsynced writes, EIO fail-stops with dirty page
//! caches, crashes during recovery, at most one unrepaired superblock
//! media fault per two generations, and transient corruption of the
//! superblock read. Within that budget the store claims perfection: every
//! acknowledged insert survives bit-exact, every read is exactly right,
//! recovery never fails, and no rollback evidence ever appears.
//!
//! Individual suites test each fault class in isolation; the storm tests
//! their *interactions* — a media fault discovered by a recovery that gets
//! crashed and re-run on a cache dirtied by an earlier EIO is exactly the
//! kind of compound state where silent bugs live. Any loss here, however
//! small, is a test failure. There is no "acceptable" outcome class other
//! than perfection.

use std::collections::BTreeMap;

use dabqlite_core::{Capacities, DbError, FileId, Output, SB_ZONE_SIZE, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{Driven, SimDisk, SimHost};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

const SEEDS: u64 = 24;
const CYCLES: usize = 12;
const CAPS: Capacities = Capacities { rows: 96 };

struct StormStats {
    crashes: u64,
    io_fails: u64,
    recovery_crashes: u64,
    media_faults: u64,
    read_corruptions: u64,
}

#[test]
fn all_in_budget_faults_together_zero_loss_zero_drift() {
    let mut totals = StormStats {
        crashes: 0,
        io_fails: 0,
        recovery_crashes: 0,
        media_faults: 0,
        read_corruptions: 0,
    };

    for seed in 0..SEEDS {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
        // Budget guard: a superblock media fault is only injected once the
        // generation has advanced 2 past the previous one, guaranteeing
        // both slot pairs were rewritten and the earlier damage healed.
        // Two live faults at once would exceed the single-fault budget.
        let mut last_media_gen: u64 = 0;

        let mut host = SimHost::new(CAPS, SimDisk::new(), None);
        match host.open() {
            Driven::Done(Output::OpenDone { result: Ok(0) }) => {}
            other => panic!("[seed={seed}] fresh open: {other:?}"),
        }

        for cycle in 0..CYCLES {
            let ctx = format!("seed={seed} cycle={cycle}");

            // --- fault plan for this cycle ------------------------------
            let inserts = rng.gen_range(1..=6);
            let fault_delta = rng.gen_range(1..=(inserts as u64) * 5 + 3);
            let kind = rng.gen_range(0u8..10);
            let (crash_cycle, io_fail_cycle) = (kind < 5, (5..8).contains(&kind));
            if crash_cycle {
                host.crash_after = Some(host.io_count + fault_delta);
            } else if io_fail_cycle {
                host.fail_after = Some(host.io_count + fault_delta);
            }

            // --- run the cycle's inserts --------------------------------
            let mut in_flight: Option<(u64, [u8; VALUE_LEN])> = None;
            let mut crashed = false;
            for _ in 0..inserts {
                let id: u64 = rng.gen();
                let mut value = [0u8; VALUE_LEN];
                rng.fill_bytes(&mut value);
                match host.run(ClientOp::Insert { id, value }) {
                    Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                        oracle.insert(id, value);
                    }
                    Driven::Done(Output::InsertDone {
                        result: Err(DbError::Full { .. }),
                        ..
                    }) => {
                        assert_eq!(oracle.len() as u64, CAPS.rows, "[{ctx}] early Full");
                    }
                    Driven::Done(Output::InsertDone {
                        result: Err(DbError::IoFailed { .. }),
                        ..
                    }) => {
                        in_flight = Some((id, value));
                        totals.io_fails += 1;
                        break;
                    }
                    Driven::Done(other) => panic!("[{ctx}] unexpected: {other:?}"),
                    Driven::Crashed => {
                        in_flight = Some((id, value));
                        crashed = true;
                        totals.crashes += 1;
                        break;
                    }
                }
            }

            // --- settle + optional in-budget media fault -----------------
            let mut disk = std::mem::take(&mut host.disk);
            let mut media_faulted = false;
            if crashed {
                disk.crash(&mut rng);
                // Media fault: one byte, one bit, anywhere in the
                // superblock zone — but only when the budget guard allows.
                let gen_now = host.engine.generation();
                if gen_now >= last_media_gen + 2
                    && rng.gen_bool(0.5)
                    && disk.len(FileId::Superblock) >= SB_ZONE_SIZE as u64
                {
                    let offset = rng.gen_range(0..SB_ZONE_SIZE as u64);
                    disk.corrupt(FileId::Superblock, offset, 1 << rng.gen_range(0..8));
                    last_media_gen = gen_now;
                    media_faulted = true;
                    totals.media_faults += 1;
                }
            }
            // (After an EIO fail-stop the cache is dirty — unsynced writes
            // carry into the restart, and a later cycle's crash settles
            // them. That interaction is the point.)

            // --- recovery, itself under fire -----------------------------
            // Budget: a durable media fault and a transient read fault in
            // the same recovery could damage both copies of one pair —
            // that's a double fault, out of budget. One or the other.
            host = storm_recover(&ctx, disk, &mut rng, !media_faulted, &mut totals);

            // --- the perfection invariant --------------------------------
            let (used, _) = host.engine.usage();
            let expected = oracle.len() as u64;
            if used == expected + 1 {
                let (id, value) =
                    in_flight.unwrap_or_else(|| panic!("[{ctx}] extra row, none in flight"));
                assert_eq!(host.get(id), Some(value), "[{ctx}] in-flight torn");
                oracle.insert(id, value);
            } else {
                assert_eq!(
                    used, expected,
                    "[{ctx}] LOSS: {expected} acked, {used} found"
                );
                if let Some((id, _)) = in_flight {
                    assert_eq!(host.get(id), None, "[{ctx}] partial in-flight visible");
                }
            }
            for (&id, &value) in &oracle {
                assert_eq!(host.get(id), Some(value), "[{ctx}] DRIFT on id={id}");
            }
            for _ in 0..3 {
                let absent: u64 = rng.gen();
                if !oracle.contains_key(&absent) {
                    assert_eq!(host.get(absent), None, "[{ctx}] phantom id={absent}");
                }
            }
            let report = host.engine.recovery_report();
            assert!(
                report.orphan_valid_rows <= 1,
                "[{ctx}] {} orphan rows from in-budget faults",
                report.orphan_valid_rows
            );
            assert!(
                !report.rollback_evidence,
                "[{ctx}] rollback evidence from in-budget faults"
            );
        }
    }

    // Coverage floors: the storm must actually storm.
    assert!(totals.crashes > 40, "only {} crashes", totals.crashes);
    assert!(
        totals.io_fails > 15,
        "only {} EIO fail-stops",
        totals.io_fails
    );
    assert!(
        totals.recovery_crashes > 10,
        "only {} recovery crashes",
        totals.recovery_crashes
    );
    assert!(
        totals.media_faults > 15,
        "only {} media faults",
        totals.media_faults
    );
    assert!(
        totals.read_corruptions > 15,
        "only {} read corruptions",
        totals.read_corruptions
    );
}

/// Recover, possibly under a transient superblock-read corruption (when the
/// budget allows) and/or a crash mid-recovery. In-budget recovery MUST
/// succeed; anything else panics.
fn storm_recover(
    ctx: &str,
    mut disk: SimDisk,
    rng: &mut ChaCha8Rng,
    allow_read_corrupt: bool,
    totals: &mut StormStats,
) -> SimHost {
    let crash_recovery = rng.gen_bool(0.3);

    for attempt in 0..8 {
        let mut host = SimHost::new(CAPS, std::mem::take(&mut disk), None);
        if crash_recovery && attempt == 0 {
            host.crash_after = Some(rng.gen_range(0..6));
        }
        // Transient read fault: one bit of the superblock read, in flight.
        // The twin copy is in the same buffer, so this is survivable alone;
        // it is only excluded when a durable media fault is already live.
        if allow_read_corrupt && rng.gen_bool(0.4) {
            host.read_corrupt_at =
                Some((0, rng.gen_range(0..SB_ZONE_SIZE), 1 << rng.gen_range(0..8)));
        }
        match host.open() {
            Driven::Done(Output::OpenDone { result: Ok(_) }) => {
                totals.read_corruptions += host.reads_corrupted;
                host.crash_after = None;
                return host;
            }
            Driven::Crashed => {
                totals.recovery_crashes += 1;
                disk = std::mem::take(&mut host.disk);
                disk.crash(rng);
            }
            other => panic!("[{ctx}] in-budget recovery failed: {other:?}"),
        }
    }
    panic!("[{ctx}] recovery did not converge in 8 attempts");
}
