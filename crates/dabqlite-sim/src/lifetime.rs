//! Whole-lifetime simulation: a database living through many crash/recover
//! cycles, in the spirit of TigerBeetle's VOPR.
//!
//! One `run_lifetime(seed)` call simulates: open fresh → insert under a
//! randomly-placed crash → settle unsynced writes (survive/vanish/tear) →
//! recover (sometimes crashing *during recovery* too) → verify the entire
//! database against the oracle → keep inserting → crash again → … The oracle
//! is the certainly-committed set; after each recovery the one in-flight
//! insert is resolved to committed or not and folded in.
//!
//! Everything derives from the single `u64` seed. A failure panics with the
//! seed and cycle in the message; rerunning with that seed reproduces it
//! bit-for-bit (docs/DESIGN.md §7.2).

use std::collections::BTreeMap;

use dabqlite_core::{Capacities, Output, VALUE_LEN};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::disk::SimDisk;
use crate::host::{ClientOp, Driven, SimHost};

#[derive(Debug, Clone, Copy)]
pub struct LifetimeConfig {
    /// Crash/recover cycles per lifetime.
    pub cycles: usize,
    /// Maximum inserts attempted per cycle.
    pub max_inserts_per_cycle: usize,
    /// Row capacity. Small enough that lifetimes can hit the Full wall.
    pub caps: Capacities,
    /// Probability that a recovery itself is crashed and re-recovered.
    pub recovery_crash_p: f64,
    /// Probability that a cycle ends in an I/O *failure* (fail-stop, dirty
    /// page cache carries into the restart) instead of a machine crash.
    pub io_fail_p: f64,
}

impl Default for LifetimeConfig {
    fn default() -> Self {
        LifetimeConfig {
            cycles: 8,
            max_inserts_per_cycle: 6,
            caps: Capacities { rows: 64 },
            recovery_crash_p: 0.25,
            io_fail_p: 0.2,
        }
    }
}

/// Statistics from one lifetime, for soak-run reporting. `Eq` so two runs
/// of the same seed can be compared for determinism.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LifetimeStats {
    pub cycles: usize,
    pub commits: u64,
    pub crashes: u64,
    pub io_failures: u64,
    pub recovery_crashes: u64,
    pub in_flight_committed: u64,
    pub in_flight_lost: u64,
    pub full_rejections: u64,
    /// I/O ops performed across every incarnation of this lifetime, for
    /// simulated-time accounting (docs/FAULTS.md).
    pub reads: u64,
    pub writes: u64,
    pub fsyncs: u64,
}

/// Fold a dying incarnation's I/O counters into the lifetime totals.
fn absorb_io(stats: &mut LifetimeStats, host: &SimHost) {
    stats.reads += host.n_reads;
    stats.writes += host.n_writes;
    stats.fsyncs += host.n_fsyncs;
}

/// Run one full lifetime. Panics (with seed context) on any divergence.
pub fn run_lifetime(seed: u64, cfg: &LifetimeConfig) -> LifetimeStats {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut stats = LifetimeStats::default();

    // The oracle: everything certainly committed (§7.2 technique 3).
    let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();

    // Clean first open.
    let mut host = SimHost::new(cfg.caps, SimDisk::new(), None);
    match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(0) }) => {}
        other => panic!("[seed={seed}] fresh open failed: {other:?}"),
    }

    for cycle in 0..cfg.cycles {
        let ctx = format!("seed={seed} cycle={cycle}");
        stats.cycles = cycle + 1;

        // Plan this cycle: some inserts, ended by a machine crash, an I/O
        // failure (fail-stop, dirty cache survives into the restart), or a
        // clean restart. All three restart paths matter.
        let inserts = rng.gen_range(1..=cfg.max_inserts_per_cycle);
        // ~5 I/O ops per insert; sometimes the boundary lands past the end,
        // meaning this cycle completes without incident.
        let fault_delta = rng.gen_range(1..=(inserts as u64) * 5 + 3);
        let io_fail_cycle = rng.gen_bool(cfg.io_fail_p);
        if io_fail_cycle {
            host.fail_after = Some(host.io_count + fault_delta);
        } else {
            host.crash_after = Some(host.io_count + fault_delta);
        }

        let mut in_flight: Option<(u64, [u8; VALUE_LEN])> = None;
        let mut crashed = false;
        let mut io_failed = false;
        for _ in 0..inserts {
            let id: u64 = rng.gen();
            let mut value = [0u8; VALUE_LEN];
            rng.fill_bytes(&mut value);
            match host.run(ClientOp::Insert { id, value }) {
                Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                    oracle.insert(id, value);
                    stats.commits += 1;
                }
                Driven::Done(Output::InsertDone {
                    result: Err(dabqlite_core::DbError::Full { .. }),
                    ..
                }) => {
                    // Legitimate at capacity; verify and carry on.
                    assert_eq!(
                        oracle.len() as u64,
                        cfg.caps.rows,
                        "[{ctx}] Full below capacity"
                    );
                    stats.full_rejections += 1;
                }
                Driven::Done(Output::InsertDone {
                    result: Err(dabqlite_core::DbError::IoFailed { .. }),
                    ..
                }) => {
                    // Fail-stop: the failed insert is the in-flight one.
                    in_flight = Some((id, value));
                    io_failed = true;
                    break;
                }
                Driven::Done(other) => panic!("[{ctx}] unexpected insert result: {other:?}"),
                Driven::Crashed => {
                    in_flight = Some((id, value));
                    crashed = true;
                    break;
                }
            }
        }

        absorb_io(&mut stats, &host);
        if crashed {
            stats.crashes += 1;
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut rng);
            host = recover(&ctx, cfg, disk, &mut rng, &mut stats);
        } else if io_failed {
            // Process restart WITHOUT machine crash: the dirty page cache
            // carries over unsettled. A later cycle's crash will settle it.
            stats.io_failures += 1;
            let disk = std::mem::take(&mut host.disk);
            host = recover(&ctx, cfg, disk, &mut rng, &mut stats);
        } else {
            // No fault this cycle: restart cleanly instead (also a path
            // worth exercising — clean shutdown must obviously recover).
            host.crash_after = None;
            host.fail_after = None;
            let disk = std::mem::take(&mut host.disk);
            host = recover(&ctx, cfg, disk, &mut rng, &mut stats);
        }

        // Resolve the in-flight insert: committed or vanished, atomically.
        let (used, _) = host.engine.usage();
        let expected = oracle.len() as u64;
        if used == expected + 1 {
            let (id, value) =
                in_flight.unwrap_or_else(|| panic!("[{ctx}] extra row with none in flight"));
            assert_eq!(
                host.get(id),
                Some(value),
                "[{ctx}] in-flight insert committed but corrupted"
            );
            oracle.insert(id, value);
            stats.in_flight_committed += 1;
        } else {
            assert_eq!(
                used, expected,
                "[{ctx}] recovered count diverged from oracle"
            );
            if let Some((id, _)) = in_flight {
                assert_eq!(host.get(id), None, "[{ctx}] uncommitted insert visible");
                stats.in_flight_lost += 1;
            }
        }

        // Full-database verification against the oracle, every cycle.
        for (&id, &value) in &oracle {
            assert_eq!(host.get(id), Some(value), "[{ctx}] committed id={id} lost");
        }
        // Negative space: ids never inserted must be absent.
        for _ in 0..4 {
            let absent: u64 = rng.gen();
            if !oracle.contains_key(&absent) {
                assert_eq!(host.get(absent), None, "[{ctx}] phantom row id={absent}");
            }
        }
    }
    absorb_io(&mut stats, &host);
    stats
}

/// Open the given disk, sometimes crashing mid-recovery and recovering
/// again. Returns a Ready host. Recovery must always succeed: our fault
/// model (crash + torn unsynced writes) never produces an unopenable disk.
fn recover(
    ctx: &str,
    cfg: &LifetimeConfig,
    mut disk: SimDisk,
    rng: &mut ChaCha8Rng,
    stats: &mut LifetimeStats,
) -> SimHost {
    if rng.gen_bool(cfg.recovery_crash_p) {
        // Crash during the recovery itself, then settle and try again.
        let boundary = rng.gen_range(0..4);
        let mut host = SimHost::new(cfg.caps, disk, Some(boundary));
        match host.open() {
            Driven::Crashed => {
                stats.recovery_crashes += 1;
                absorb_io(stats, &host);
                disk = std::mem::take(&mut host.disk);
                disk.crash(rng);
            }
            Driven::Done(Output::OpenDone { result: Ok(_) }) => {
                // Recovery finished before the boundary (few I/O ops).
                host.crash_after = None;
                return host;
            }
            other => panic!("[{ctx}] recovery failed: {other:?}"),
        }
    }
    let mut host = SimHost::new(cfg.caps, disk, None);
    match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(_) }) => host,
        other => panic!("[{ctx}] recovery failed: {other:?}"),
    }
}
