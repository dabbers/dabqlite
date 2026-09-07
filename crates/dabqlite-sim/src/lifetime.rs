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
//!
//! The lifetime covers the ENTIRE feature surface, so one soak pass
//! exercises everything the database can do: lifetimes may START as a
//! legacy v1 database and migrate under the same fault schedule
//! (crash/EIO retries until the two-worlds protocol converges); every
//! cycle verifies point gets, the full ordered scan, substring search
//! against the insertion-order oracle, negative space, AND the
//! inspector's independent verdict against the engine's recovery report.

use std::collections::BTreeMap;

use dabqlite_core::inspect::{inspect, Verdict};
use dabqlite_core::migration::V1_VALUE_LEN;
use dabqlite_core::{Capacities, DbError, FileId, Input, Output, VALUE_LEN};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::disk::SimDisk;
use crate::host::{ClientOp, Driven, SimHost};
use crate::workload::build_v1_disk;

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
    /// Start this lifetime as a LEGACY v1 database with up to this many
    /// rows, and migrate it — under the fault schedule — before the first
    /// open. 0 = start fresh (the classic lifetime).
    pub legacy_rows_max: u64,
    /// Probability that a recovery happens during a DISK-FULL episode:
    /// the first attempt(s) run with every write and fsync refused
    /// (reads fine), must fail loudly, and the real recovery follows
    /// once "space frees".
    pub disk_full_p: f64,
}

impl Default for LifetimeConfig {
    fn default() -> Self {
        LifetimeConfig {
            cycles: 8,
            max_inserts_per_cycle: 6,
            caps: Capacities { rows: 64 },
            recovery_crash_p: 0.25,
            io_fail_p: 0.2,
            legacy_rows_max: 0,
            disk_full_p: 0.15,
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
    /// Substring-search verifications performed (each one an exact
    /// oracle-equality assertion over the whole database).
    pub find_checks: u64,
    /// Inspector-agreement verifications performed.
    pub inspections: u64,
    /// Salvage episodes: a committed row damaged on a COPY of this
    /// lifetime's disk, then read back in salvage mode with every
    /// surviving row verified against the log.
    pub salvage_checks: u64,
    /// Committed deletions.
    pub deletes: u64,
    /// Committed updates.
    pub updates: u64,
    /// Successful legacy→current migrations (0 or 1 per lifetime).
    pub migrations: u64,
    /// Migration attempts, including ones ended by crash or EIO.
    pub migration_attempts: u64,
    /// Recoveries first refused by a full-disk episode, then converged.
    pub disk_full_recoveries: u64,
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

/// A write that may or may not have committed when a fault hit.
///
/// All three kinds reconcile identically — one appended slot, one
/// generation — which is the whole reason deletes and updates needed no
/// new recovery machinery.
#[derive(Debug, Clone, Copy)]
enum InFlight {
    Insert { id: u64, value: [u8; VALUE_LEN] },
    Update { id: u64, value: [u8; VALUE_LEN] },
    Delete { id: u64, old: [u8; VALUE_LEN] },
}

impl InFlight {
    /// Apply to the model. `log` mirrors ROW order, which is what
    /// substring search returns: an update moves its row to the end.
    fn apply(
        &self,
        oracle: &mut BTreeMap<u64, [u8; VALUE_LEN]>,
        log: &mut Vec<(u64, [u8; VALUE_LEN])>,
        history: &mut Vec<(u64, [u8; VALUE_LEN])>,
    ) {
        if let InFlight::Insert { id, value } | InFlight::Update { id, value } = *self {
            history.push((id, value));
        }
        match *self {
            InFlight::Insert { id, value } => {
                oracle.insert(id, value);
                log.push((id, value));
            }
            InFlight::Update { id, value } => {
                oracle.insert(id, value);
                log.retain(|(k, _)| *k != id);
                log.push((id, value));
            }
            InFlight::Delete { id, .. } => {
                oracle.remove(&id);
                log.retain(|(k, _)| *k != id);
            }
        }
    }

    fn assert_committed(&self, host: &mut SimHost, ctx: &str) {
        match *self {
            InFlight::Insert { id, value } | InFlight::Update { id, value } => assert_eq!(
                host.get(id),
                Some(value),
                "[{ctx}] in-flight write committed but is not readable"
            ),
            InFlight::Delete { id, .. } => assert_eq!(
                host.get(id),
                None,
                "[{ctx}] in-flight delete committed but the row is still there"
            ),
        }
    }

    fn assert_not_committed(&self, host: &mut SimHost, ctx: &str) {
        match *self {
            InFlight::Insert { id, .. } => {
                assert_eq!(host.get(id), None, "[{ctx}] uncommitted insert is visible")
            }
            // An uncommitted update or delete must leave the OLD value
            // exactly as it was — a half-applied write is the failure this
            // whole design exists to prevent.
            InFlight::Update { id, .. } => assert!(
                host.get(id).is_some(),
                "[{ctx}] uncommitted update lost the row"
            ),
            InFlight::Delete { id, old } => assert_eq!(
                host.get(id),
                Some(old),
                "[{ctx}] uncommitted delete removed or altered the row"
            ),
        }
    }
}

/// Run one full lifetime. Panics (with seed context) on any divergence.
pub fn run_lifetime(seed: u64, cfg: &LifetimeConfig) -> LifetimeStats {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut stats = LifetimeStats::default();

    // The oracle: everything certainly committed (§7.2 technique 3),
    // plus the same facts in INSERTION order — the substring index answers
    // in row order, so the log is its exact oracle.
    let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
    let mut log: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
    // Slots consumed on disk. Distinct from `oracle.len()` as soon as
    // anything is deleted or updated, because every write appends.
    let mut slots: u64;
    // Every (id, value) this lifetime ever committed. Salvage may serve a
    // value the current oracle no longer holds — quarantining a tombstone
    // loses a deletion — but it must never serve one that was never
    // written at all.
    let mut history: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();

    // Some lifetimes begin as a LEGACY v1 database: migrate it first,
    // under the same fault schedule as everything else. The two-worlds
    // protocol means every failed attempt leaves either the untouched
    // legacy world (retry) or the completed migration (idempotent no-op
    // on retry) — the loop converges because attempts eventually run
    // fault-free.
    let mut disk = SimDisk::new();
    if cfg.legacy_rows_max > 0 {
        let n = rng.gen_range(1..=cfg.legacy_rows_max.min(cfg.caps.rows));
        let (legacy_disk, v1_ops) = build_v1_disk(&mut rng, n);
        disk = legacy_disk;
        let legacy_bytes = disk.contents(FileId::RowsOld);
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            stats.migration_attempts += 1;
            let mut host = SimHost::new(cfg.caps, disk, None);
            // Fault the early attempts; guarantee convergence by running
            // later attempts clean.
            if attempts < 8 && rng.gen_bool(0.6) {
                let delta = rng.gen_range(0..n + 8);
                if rng.gen_bool(cfg.io_fail_p) {
                    host.fail_after = Some(delta);
                } else {
                    host.crash_after = Some(delta);
                }
            }
            match host.run_migration() {
                Driven::Done(Output::MigrateDone { result: Ok(rows) }) => {
                    assert_eq!(rows, n, "[seed={seed}] migration row count");
                    absorb_io(&mut stats, &host);
                    disk = std::mem::take(&mut host.disk);
                    stats.migrations += 1;
                    break;
                }
                Driven::Done(Output::MigrateDone {
                    result: Err(DbError::IoFailed { .. }),
                }) => {
                    // Fail-stop; the dirty page cache carries into the
                    // retry, exactly like an EIO'd insert.
                    absorb_io(&mut stats, &host);
                    disk = std::mem::take(&mut host.disk);
                }
                Driven::Crashed => {
                    absorb_io(&mut stats, &host);
                    disk = std::mem::take(&mut host.disk);
                    disk.crash(&mut rng);
                }
                other => panic!("[seed={seed}] migration attempt {attempts}: {other:?}"),
            }
            // The legacy file is read, never written — through every
            // failed attempt, byte-identical.
            assert_eq!(
                disk.contents(FileId::RowsOld),
                legacy_bytes,
                "[seed={seed}] migration touched the legacy file"
            );
        }
        for &(id, v1) in &v1_ops {
            let mut value = [0u8; VALUE_LEN];
            value[..V1_VALUE_LEN].copy_from_slice(&v1);
            oracle.insert(id, value);
            log.push((id, value));
            history.push((id, value));
        }
    }

    // First open: fresh init, or recovery of the freshly-migrated file.
    let mut host = SimHost::new(cfg.caps, disk, None);
    match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == oracle.len() as u64 => {}
        other => panic!("[seed={seed}] first open failed: {other:?}"),
    }
    // A migrated database arrives with slots already consumed.
    slots = host.engine.usage().0;

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

        let mut in_flight: Option<InFlight> = None;
        let mut crashed = false;
        let mut io_failed = false;
        for _ in 0..inserts {
            // Every write kind takes one slot and one generation, so all
            // three reconcile identically after a fault: the operation is
            // committed or it is not.
            let roll = rng.gen_range(0..100u32);
            let existing = (!oracle.is_empty()).then(|| {
                *oracle
                    .keys()
                    .nth(rng.gen_range(0..oracle.len()))
                    .expect("non-empty")
            });
            let op = match (existing, roll) {
                (Some(id), r) if r < 15 => {
                    let old = oracle[&id];
                    InFlight::Delete { id, old }
                }
                (Some(id), r) if r < 30 => {
                    let mut value = [0u8; VALUE_LEN];
                    rng.fill_bytes(&mut value);
                    InFlight::Update { id, value }
                }
                _ => {
                    let id: u64 = rng.gen();
                    let mut value = [0u8; VALUE_LEN];
                    rng.fill_bytes(&mut value);
                    InFlight::Insert { id, value }
                }
            };
            let driven = match op {
                InFlight::Insert { id, value } => host.run(ClientOp::Insert { id, value }),
                InFlight::Update { id, value } => host.run(ClientOp::Update { id, value }),
                InFlight::Delete { id, .. } => host.run(ClientOp::Delete { id }),
            };
            match driven {
                Driven::Done(
                    Output::InsertDone { result: Ok(()), .. }
                    | Output::UpdateDone { result: Ok(()), .. }
                    | Output::DeleteDone { result: Ok(()), .. },
                ) => {
                    op.apply(&mut oracle, &mut log, &mut history);
                    slots += 1;
                    stats.commits += 1;
                    match op {
                        InFlight::Delete { .. } => stats.deletes += 1,
                        InFlight::Update { .. } => stats.updates += 1,
                        InFlight::Insert { .. } => {}
                    }
                }
                Driven::Done(
                    Output::InsertDone {
                        result: Err(dabqlite_core::DbError::Full { .. }),
                        ..
                    }
                    | Output::UpdateDone {
                        result: Err(dabqlite_core::DbError::Full { .. }),
                        ..
                    }
                    | Output::DeleteDone {
                        result: Err(dabqlite_core::DbError::Full { .. }),
                        ..
                    },
                ) => {
                    // Legitimate at capacity — which counts SLOTS, not
                    // live rows: deletes and updates consume them too.
                    assert_eq!(slots, cfg.caps.rows, "[{ctx}] Full below capacity");
                    stats.full_rejections += 1;
                }
                Driven::Done(
                    Output::InsertDone {
                        result: Err(dabqlite_core::DbError::IoFailed { .. }),
                        ..
                    }
                    | Output::UpdateDone {
                        result: Err(dabqlite_core::DbError::IoFailed { .. }),
                        ..
                    }
                    | Output::DeleteDone {
                        result: Err(dabqlite_core::DbError::IoFailed { .. }),
                        ..
                    },
                ) => {
                    in_flight = Some(op);
                    io_failed = true;
                    break;
                }
                Driven::Done(other) => panic!("[{ctx}] unexpected write result: {other:?}"),
                Driven::Crashed => {
                    in_flight = Some(op);
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

        // Resolve the in-flight write — insert, update or delete alike:
        // it committed or it did not, atomically, and the SLOT count says
        // which.
        let (used, _) = host.engine.usage();
        if used == slots + 1 {
            let op = in_flight.unwrap_or_else(|| panic!("[{ctx}] extra slot with none in flight"));
            op.apply(&mut oracle, &mut log, &mut history);
            slots += 1;
            op.assert_committed(&mut host, &ctx);
            stats.in_flight_committed += 1;
        } else {
            assert_eq!(used, slots, "[{ctx}] recovered slot count diverged");
            if let Some(op) = in_flight {
                op.assert_not_committed(&mut host, &ctx);
                stats.in_flight_lost += 1;
            }
        }

        // Full-database verification against the oracle, every cycle.
        for (&id, &value) in &oracle {
            assert_eq!(host.get(id), Some(value), "[{ctx}] committed id={id} lost");
        }
        // Ordered-scan verification: a full paged range scan must equal the
        // oracle exactly, in key order — the rebuilt B+tree is checked
        // against reality after every recovery, under every fault schedule.
        {
            let mut cursor = 0u64;
            let mut scanned = 0u64;
            let mut oracle_iter = oracle.iter();
            loop {
                let page = match host.run_input(Input::Range {
                    lo: cursor,
                    hi: u64::MAX,
                }) {
                    Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
                    other => panic!("[{ctx}] range scan failed: {other:?}"),
                };
                for item in &page.items[..page.count as usize] {
                    let (k, v) = (
                        item.id,
                        <[u8; VALUE_LEN]>::try_from(
                            item.value().expect("lifetime values fit one slot"),
                        )
                        .expect("full-width value"),
                    );
                    let (&ok, &ov) = oracle_iter
                        .next()
                        .unwrap_or_else(|| panic!("[{ctx}] scan has extra key {k}"));
                    assert_eq!((k, v), (ok, ov), "[{ctx}] ordered scan diverged");
                    scanned += 1;
                }
                match page.next {
                    Some(n) => cursor = n,
                    None => break,
                }
            }
            assert_eq!(
                scanned,
                oracle.len() as u64,
                "[{ctx}] ordered scan missed rows"
            );
        }
        // Negative space: ids never inserted must be absent.
        for _ in 0..4 {
            let absent: u64 = rng.gen();
            if !oracle.contains_key(&absent) {
                assert_eq!(host.get(absent), None, "[{ctx}] phantom row id={absent}");
            }
        }
        // Substring-search verification: the rebuilt trigram index against
        // the insertion-order log — a guaranteed-hit trigram and a full
        // value from a random committed row, seeded noise, and (some
        // cycles) the match-everything empty needle.
        {
            debug_assert_eq!(log.len(), oracle.len(), "[{ctx}] log/oracle drift");
            let mut needles: Vec<Vec<u8>> = Vec::new();
            if !log.is_empty() {
                let (_, v) = log[rng.gen_range(0..log.len())];
                let off = rng.gen_range(0..=VALUE_LEN - 3);
                needles.push(v[off..off + 3].to_vec());
                needles.push(v.to_vec());
            }
            let mut noise = vec![0u8; rng.gen_range(3..=4)];
            rng.fill_bytes(&mut noise);
            needles.push(noise);
            if cycle % 4 == 0 {
                needles.push(Vec::new());
            }
            for needle in &needles {
                let want: Vec<(u64, [u8; VALUE_LEN])> = log
                    .iter()
                    .filter(|(_, v)| {
                        needle.is_empty() || v.windows(needle.len()).any(|w| w == &needle[..])
                    })
                    .copied()
                    .collect();
                assert_eq!(
                    host.find_all(needle),
                    want,
                    "[{ctx}] substring search diverged for {needle:?}"
                );
                stats.find_checks += 1;
            }
        }
        // Inspector agreement: the independent second implementation of
        // the recovery rules must reach the engine's exact conclusion
        // about this disk, every cycle, under every fault schedule.
        {
            let report = inspect(
                &host.disk.contents(FileId::Superblock),
                &host.disk.contents(FileId::Rows),
            );
            let (used, _) = host.engine.usage();
            assert!(
                matches!(report.verdict, Verdict::Recovers { rows } if rows == used),
                "[{ctx}] inspector verdict diverged: {:?} vs {used} rows",
                report.verdict
            );
            // A stronger agreement than the row count: the inspector
            // replays the commit order itself — records, updates and
            // tombstones — so its answer to "what survives" must match the
            // engine's, arrived at independently.
            assert_eq!(
                report.rows.live_records,
                host.engine.live_count(),
                "[{ctx}] inspector and engine disagree about what is LIVE"
            );
            assert_eq!(
                report.rows.live_records,
                oracle.len() as u64,
                "[{ctx}] inspector diverged from the oracle"
            );
            let rr = host.engine.recovery_report();
            assert_eq!(
                report.rollback_evidence, rr.rollback_evidence,
                "[{ctx}] inspector rollback-evidence diverged"
            );
            stats.inspections += 1;
        }

        // Salvage episode: damage a committed row on a COPY of this
        // disk (the lifetime itself continues undamaged) and prove
        // containment holds under whatever fault schedule got us here —
        // strict open refuses, salvage open serves every OTHER logged
        // row exactly, and the damaged one is refused rather than
        // silently reported absent.
        if !log.is_empty() {
            let used = host.engine.usage().0;
            let victim = (rng.next_u64() % used) as usize;
            let mut damaged = host.disk.clone();
            damaged.corrupt(
                FileId::Rows,
                (victim * dabqlite_core::ROW_SIZE) as u64 + 7,
                0x20,
            );

            let mut strict = SimHost::new(cfg.caps, damaged.clone(), None);
            assert!(
                matches!(
                    strict.open(),
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. })
                    })
                ),
                "[{ctx}] strict open accepted a damaged row"
            );

            let live_before = host.engine.live_count();
            let mut rescue = SimHost::new(cfg.caps, damaged, None);
            let salvaged = match rescue.open_salvage() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] salvage open failed: {other:?}"),
            };
            let quarantined = rescue.engine.quarantined();
            assert!(quarantined >= 1, "[{ctx}] damage was not detected");
            assert_eq!(rescue.n_writes, 0, "[{ctx}] salvage wrote to the disk");

            // With deletes and updates in play, damaging ONE slot no
            // longer costs exactly one live row: quarantining a record
            // loses it, but quarantining a TOMBSTONE loses the deletion,
            // so the row it removed reappears. That is the documented
            // cost of containment on a database that has churned, and it
            // is bounded by the damage rather than open-ended.
            let survivors = rescue.range_all(0, u64::MAX);
            assert_eq!(
                survivors.len() as u64,
                salvaged,
                "[{ctx}] scan and live count disagree"
            );
            assert!(
                salvaged + quarantined >= live_before && salvaged <= live_before + quarantined,
                "[{ctx}] salvage lost or invented more than the damage: \
                 {salvaged} live vs {live_before} before, {quarantined} quarantined"
            );
            // The invariant that never bends: nothing served is WRONG.
            // Every surviving row is a value this lifetime actually
            // wrote for that id, and no id appears that was never used.
            for (id, value) in &survivors {
                let known = history.iter().any(|(i, v)| i == id && v == value);
                assert!(
                    known,
                    "[{ctx}] salvage served a value that was never written: id={id}"
                );
            }
            stats.salvage_checks += 1;
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
    if rng.gen_bool(cfg.disk_full_p) {
        // The restart lands on a FULL DISK: recovery must be refused
        // loudly (IoFailed, no ack, no harm), one or more times, until
        // space frees. Reads would still work; recovery's durability
        // fsyncs cannot.
        for _ in 0..rng.gen_range(1..=3) {
            let mut host = SimHost::new(cfg.caps, disk, None);
            host.disk_full_from = Some(0);
            match host.open() {
                Driven::Done(Output::OpenDone {
                    result: Err(dabqlite_core::DbError::IoFailed { .. }),
                }) => {}
                other => panic!("[{ctx}] full-disk recovery must refuse: {other:?}"),
            }
            absorb_io(stats, &host);
            disk = std::mem::take(&mut host.disk);
        }
        stats.disk_full_recoveries += 1;
    }
    if rng.gen_bool(cfg.recovery_crash_p) {
        // Crash during the recovery itself, then settle and try again.
        let boundary = rng.gen_range(0..6); // recovery is 6 ops incl. repair writes
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
