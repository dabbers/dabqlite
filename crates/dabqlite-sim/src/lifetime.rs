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
//! (crash/EIO retries until the two-worlds protocol converges); writes
//! are single ops and ATOMIC BATCHES, carrying values from empty to four
//! slots long; every cycle verifies point gets, the full ordered scan,
//! substring search against the insertion-order oracle, negative space,
//! AND the inspector's independent verdict against the engine's recovery
//! report.
//!
//! Batches and long values are not a second code path here. Every commit
//! is modelled as a list of steps landing under one generation flip, and
//! a single insert is the one-step case — which is exactly what the
//! engine does, so the harness reconciles a faulted batch of four
//! multi-slot values by the same rule it reconciles a faulted insert.

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
    /// Commits issued as an ATOMIC BATCH rather than a single op.
    pub batches: u64,
    /// Steps inside those batches.
    pub batch_steps: u64,
    /// Committed values that did not fit one row slot.
    pub long_values: u64,
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

/// A value a lifetime writes.
///
/// Half of them fill a row exactly — the shape the fixed-slot single-op
/// write path produces, and the one the harness has always covered — and
/// the rest are drawn across the multi-slot range, empty values included:
/// a zero-length value is still a value and still needs a row to say so.
fn gen_value(rng: &mut ChaCha8Rng) -> Vec<u8> {
    let len = if rng.gen_bool(0.5) {
        VALUE_LEN
    } else {
        rng.gen_range(0..=4 * VALUE_LEN)
    };
    let mut value = vec![0u8; len];
    rng.fill_bytes(&mut value);
    value
}

/// Row slots a value consumes: one per row-width of bytes, at least one.
/// The engine's rule (`on_batch`), restated here so the oracle counts
/// slots independently of the code it is checking.
fn slots_for(len: usize) -> u64 {
    (len.div_ceil(VALUE_LEN).max(1)) as u64
}

/// One write inside a commit.
///
/// All three kinds reconcile identically — appended slots, one generation
/// — which is the whole reason deletes, updates, long values and batches
/// needed no new recovery machinery.
#[derive(Debug, Clone)]
enum Step {
    Insert { id: u64, value: Vec<u8> },
    Update { id: u64, value: Vec<u8> },
    Delete { id: u64 },
}

impl Step {
    /// Slots this step appends. A value takes one per row-width; a
    /// deletion takes the one its tombstone lives in.
    fn slots(&self) -> u64 {
        match self {
            Step::Insert { value, .. } | Step::Update { value, .. } => slots_for(value.len()),
            Step::Delete { .. } => 1,
        }
    }

    fn to_op(&self) -> dabqlite_core::BatchOp<'_> {
        match self {
            Step::Insert { id, value } => dabqlite_core::BatchOp::Insert { id: *id, value },
            Step::Update { id, value } => dabqlite_core::BatchOp::Update { id: *id, value },
            Step::Delete { id } => dabqlite_core::BatchOp::Delete { id: *id },
        }
    }

    /// True when this step can also be issued through the single-op input
    /// path, which carries a fixed-width value and no batch framing.
    fn fits_single_op(&self) -> bool {
        match self {
            Step::Insert { value, .. } | Step::Update { value, .. } => value.len() == VALUE_LEN,
            Step::Delete { .. } => true,
        }
    }

    /// Apply to the model. `log` mirrors ROW order, which is what
    /// substring search returns: an update moves its row to the end.
    fn apply(
        &self,
        oracle: &mut BTreeMap<u64, Vec<u8>>,
        log: &mut Vec<(u64, Vec<u8>)>,
        history: &mut Vec<(u64, Vec<u8>)>,
    ) {
        match self {
            Step::Insert { id, value } => {
                history.push((*id, value.clone()));
                oracle.insert(*id, value.clone());
                log.push((*id, value.clone()));
            }
            Step::Update { id, value } => {
                history.push((*id, value.clone()));
                oracle.insert(*id, value.clone());
                log.retain(|(k, _)| k != id);
                log.push((*id, value.clone()));
            }
            Step::Delete { id } => {
                oracle.remove(id);
                log.retain(|(k, _)| k != id);
            }
        }
    }

    /// Apply to a projection, for planning a batch against the state its
    /// predecessors in the same commit would leave — exactly the rule the
    /// engine validates by.
    fn project(&self, state: &mut BTreeMap<u64, Vec<u8>>) {
        match self {
            Step::Insert { id, value } | Step::Update { id, value } => {
                state.insert(*id, value.clone());
            }
            Step::Delete { id } => {
                state.remove(id);
            }
        }
    }

    fn id(&self) -> u64 {
        match self {
            Step::Insert { id, .. } | Step::Update { id, .. } | Step::Delete { id } => *id,
        }
    }
}

/// A commit that may or may not have landed when a fault hit: one step or
/// several, all-or-nothing either way.
///
/// A batch is not a special case here. It is the general one: a single
/// insert is a one-step commit, and the reconciliation below has exactly
/// one rule because the engine has exactly one commit protocol.
#[derive(Debug, Clone)]
struct InFlight {
    steps: Vec<Step>,
    /// Every id the commit touches, as it was BEFORE — the exact state a
    /// commit that did not land must have left behind. Recorded per id
    /// rather than per step, because a batch may touch one id twice and
    /// only the state at its edges is observable.
    before: BTreeMap<u64, Option<Vec<u8>>>,
    /// The same ids as they must be if the commit DID land.
    after: BTreeMap<u64, Option<Vec<u8>>>,
    /// Issued as an atomic batch rather than through the single-op path.
    batched: bool,
}

impl InFlight {
    /// Slots the whole commit appends.
    fn slots(&self) -> u64 {
        self.steps.iter().map(Step::slots).sum()
    }

    fn apply(
        &self,
        oracle: &mut BTreeMap<u64, Vec<u8>>,
        log: &mut Vec<(u64, Vec<u8>)>,
        history: &mut Vec<(u64, Vec<u8>)>,
    ) {
        for step in &self.steps {
            step.apply(oracle, log, history);
        }
    }

    fn assert_committed(&self, host: &mut SimHost, ctx: &str) {
        for (&id, want) in &self.after {
            assert_eq!(
                host.get_bytes(id).as_deref(),
                want.as_deref(),
                "[{ctx}] commit landed but id={id} does not read back as it should"
            );
        }
    }

    fn assert_not_committed(&self, host: &mut SimHost, ctx: &str) {
        // A half-applied commit is the failure this whole design exists to
        // prevent: every id the commit touched must be EXACTLY as it was,
        // not merely present.
        for (&id, want) in &self.before {
            assert_eq!(
                host.get_bytes(id).as_deref(),
                want.as_deref(),
                "[{ctx}] commit did not land but id={id} changed anyway"
            );
        }
    }
}

/// What one commit attempt did — read identically off the single-op and
/// the batch paths, because the outcomes ARE identical: a commit lands
/// whole, is refused before any I/O, or is left unresolved by a fault.
#[derive(Debug, Clone, Copy)]
enum Landed {
    /// Durably committed. `rows` is the slot count the batch path
    /// reports; the single-op path does not carry one.
    Committed {
        rows: Option<u64>,
    },
    /// Refused at the capacity wall, before any I/O.
    Full,
    /// Fail-stop: the commit is unresolved until recovery says.
    IoFailed,
    /// The machine died mid-commit: likewise unresolved.
    Crashed,
    Unexpected,
}

fn classify(driven: Driven) -> Landed {
    let out = match driven {
        Driven::Crashed => return Landed::Crashed,
        Driven::Done(out) => out,
    };
    let error = match out {
        Output::InsertDone { result: Ok(()), .. }
        | Output::UpdateDone { result: Ok(()), .. }
        | Output::DeleteDone { result: Ok(()), .. } => return Landed::Committed { rows: None },
        Output::BatchDone {
            rows,
            result: Ok(()),
        } => return Landed::Committed { rows: Some(rows) },
        Output::InsertDone { result: Err(e), .. }
        | Output::UpdateDone { result: Err(e), .. }
        | Output::DeleteDone { result: Err(e), .. }
        | Output::BatchDone {
            result: Err(dabqlite_core::BatchReject { error: e, .. }),
            ..
        } => e,
        _ => return Landed::Unexpected,
    };
    match error {
        DbError::Full { .. } => Landed::Full,
        DbError::IoFailed { .. } => Landed::IoFailed,
        _ => Landed::Unexpected,
    }
}

/// Run one full lifetime. Panics (with seed context) on any divergence.
pub fn run_lifetime(seed: u64, cfg: &LifetimeConfig) -> LifetimeStats {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut stats = LifetimeStats::default();

    // The oracle: everything certainly committed (§7.2 technique 3),
    // plus the same facts in INSERTION order — the substring index answers
    // in row order, so the log is its exact oracle.
    let mut oracle: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut log: Vec<(u64, Vec<u8>)> = Vec::new();
    // Slots consumed on disk. Distinct from `oracle.len()` as soon as
    // anything is deleted or updated, because every write appends.
    let mut slots: u64;
    // Every (id, value) this lifetime ever committed. Salvage may serve a
    // value the current oracle no longer holds — quarantining a tombstone
    // loses a deletion — but it must never serve one that was never
    // written at all.
    let mut history: Vec<(u64, Vec<u8>)> = Vec::new();

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
            let mut value = vec![0u8; VALUE_LEN];
            value[..V1_VALUE_LEN].copy_from_slice(&v1);
            oracle.insert(id, value.clone());
            log.push((id, value.clone()));
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

        // Plan this cycle: some commits, ended by a machine crash, an I/O
        // failure (fail-stop, dirty cache survives into the restart), or a
        // clean restart. All three restart paths matter.
        let commits = rng.gen_range(1..=cfg.max_inserts_per_cycle);
        // ~5 I/O ops per commit; sometimes the boundary lands past the end,
        // meaning this cycle completes without incident.
        let fault_delta = rng.gen_range(1..=(commits as u64) * 5 + 3);
        let io_fail_cycle = rng.gen_bool(cfg.io_fail_p);
        if io_fail_cycle {
            host.fail_after = Some(host.io_count + fault_delta);
        } else {
            host.crash_after = Some(host.io_count + fault_delta);
        }

        let mut in_flight: Option<InFlight> = None;
        let mut crashed = false;
        let mut io_failed = false;
        for _ in 0..commits {
            // Plan ONE commit: usually a single write, sometimes a batch
            // of several. Batch ops are validated against the state their
            // predecessors in the same commit would leave, so the plan is
            // built against a projection of the oracle — the same rule,
            // arrived at independently.
            let n_steps = if rng.gen_bool(0.3) {
                rng.gen_range(2..=4usize)
            } else {
                1
            };
            let mut projected = oracle.clone();
            let mut steps: Vec<Step> = Vec::new();
            let mut before: BTreeMap<u64, Option<Vec<u8>>> = BTreeMap::new();
            for _ in 0..n_steps {
                let roll = rng.gen_range(0..100u32);
                let existing = (!projected.is_empty()).then(|| {
                    *projected
                        .keys()
                        .nth(rng.gen_range(0..projected.len()))
                        .expect("non-empty")
                });
                let step = match (existing, roll) {
                    (Some(id), r) if r < 15 => Step::Delete { id },
                    (Some(id), r) if r < 30 => Step::Update {
                        id,
                        value: gen_value(&mut rng),
                    },
                    _ => {
                        let id: u64 = rng.gen();
                        // A 64-bit id colliding inside one batch is not a
                        // case worth generating; skipping keeps every
                        // planned batch legal by construction.
                        if projected.contains_key(&id) {
                            continue;
                        }
                        Step::Insert {
                            id,
                            value: gen_value(&mut rng),
                        }
                    }
                };
                before
                    .entry(step.id())
                    .or_insert_with(|| oracle.get(&step.id()).cloned());
                step.project(&mut projected);
                steps.push(step);
            }
            if steps.is_empty() {
                continue;
            }
            let after: BTreeMap<u64, Option<Vec<u8>>> = before
                .keys()
                .map(|&id| (id, projected.get(&id).cloned()))
                .collect();
            // One step whose value fills a row exactly can go through the
            // single-op input path as well as the batch one; both are real
            // ways to reach the same commit protocol, so both are driven.
            let batched = steps.len() > 1 || !steps[0].fits_single_op() || rng.gen_bool(0.5);
            let op = InFlight {
                steps,
                before,
                after,
                batched,
            };
            let need = op.slots();

            let driven = if batched {
                let ops: Vec<dabqlite_core::BatchOp> = op.steps.iter().map(Step::to_op).collect();
                host.batch(&ops)
            } else {
                match &op.steps[0] {
                    Step::Insert { id, value } => host.run(ClientOp::Insert {
                        id: *id,
                        value: <[u8; VALUE_LEN]>::try_from(&value[..]).expect("row-width value"),
                    }),
                    Step::Update { id, value } => host.run(ClientOp::Update {
                        id: *id,
                        value: <[u8; VALUE_LEN]>::try_from(&value[..]).expect("row-width value"),
                    }),
                    Step::Delete { id } => host.run(ClientOp::Delete { id: *id }),
                }
            };
            match classify(driven) {
                Landed::Committed { rows } => {
                    if let Some(rows) = rows {
                        assert_eq!(rows, need, "[{ctx}] batch committed the wrong slot count");
                    }
                    op.apply(&mut oracle, &mut log, &mut history);
                    slots += need;
                    stats.commits += 1;
                    if op.batched {
                        stats.batches += 1;
                        stats.batch_steps += op.steps.len() as u64;
                    }
                    for step in &op.steps {
                        match step {
                            Step::Delete { .. } => stats.deletes += 1,
                            Step::Update { .. } => stats.updates += 1,
                            Step::Insert { .. } => {}
                        }
                        if step.slots() > 1 {
                            stats.long_values += 1;
                        }
                    }
                }
                Landed::Full => {
                    // Legitimate only when the commit genuinely does not
                    // fit — and capacity counts SLOTS, not live rows:
                    // deletes, updates and every row of a long value
                    // consume them too.
                    assert!(
                        slots + need > cfg.caps.rows,
                        "[{ctx}] Full with room for {need} more slots at {slots}/{}",
                        cfg.caps.rows
                    );
                    stats.full_rejections += 1;
                }
                Landed::IoFailed => {
                    in_flight = Some(op);
                    io_failed = true;
                    break;
                }
                Landed::Crashed => {
                    in_flight = Some(op);
                    crashed = true;
                    break;
                }
                Landed::Unexpected => panic!("[{ctx}] unexpected write result: {driven:?}"),
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

        // Resolve the in-flight commit — one insert or a batch of four
        // multi-slot values, it makes no difference: it committed or it
        // did not, atomically, and the SLOT count says which. There is no
        // third answer to look for, which is the point.
        let (used, _) = host.engine.usage();
        let staged = in_flight.as_ref().map_or(0, InFlight::slots);
        if used == slots + staged && staged > 0 {
            let op = in_flight
                .clone()
                .unwrap_or_else(|| panic!("[{ctx}] extra slot with none in flight"));
            op.apply(&mut oracle, &mut log, &mut history);
            slots += staged;
            op.assert_committed(&mut host, &ctx);
            stats.in_flight_committed += 1;
        } else {
            assert_eq!(used, slots, "[{ctx}] recovered slot count diverged");
            if let Some(op) = &in_flight {
                op.assert_not_committed(&mut host, &ctx);
                stats.in_flight_lost += 1;
            }
        }

        // Full-database verification against the oracle, every cycle.
        // `get_bytes` reassembles a value from the bounded windows the
        // protocol hands back, so a four-slot value is checked byte for
        // byte exactly like a one-slot one.
        for (&id, value) in &oracle {
            assert_eq!(
                host.get_bytes(id).as_deref(),
                Some(value.as_slice()),
                "[{ctx}] committed id={id} lost"
            );
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
                let refs: Vec<dabqlite_core::RowRef> = page.items[..page.count as usize].to_vec();
                for item in refs {
                    // A page carries a head and a length; anything longer
                    // than a row is read the proper way rather than
                    // compared against a prefix.
                    let v = match item.value() {
                        Some(v) => v.to_vec(),
                        None => host
                            .get_bytes(item.id)
                            .expect("a scanned row is readable by id"),
                    };
                    let (&ok, ov) = oracle_iter
                        .next()
                        .unwrap_or_else(|| panic!("[{ctx}] scan has extra key {}", item.id));
                    assert_eq!((item.id, &v), (ok, ov), "[{ctx}] ordered scan diverged");
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
                assert_eq!(
                    host.get_bytes(absent),
                    None,
                    "[{ctx}] phantom row id={absent}"
                );
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
                // A value shorter than a trigram indexes nothing and can
                // seed no needle, so pick from the ones that can: start
                // at random and take the first that is long enough.
                let start = rng.gen_range(0..log.len());
                let pick = (0..log.len())
                    .map(|k| (start + k) % log.len())
                    .find(|&k| log[k].1.len() >= 3);
                if let Some(k) = pick {
                    let v = &log[k].1;
                    let off = rng.gen_range(0..=v.len() - 3);
                    needles.push(v[off..off + 3].to_vec());
                    // The longest needle the protocol carries, taken from
                    // a random window of a random value: on a multi-slot
                    // value that window usually STRADDLES a row boundary,
                    // which is the match an index that only looked at head
                    // slots would quietly miss.
                    let wide = v.len().min(VALUE_LEN);
                    let off = rng.gen_range(0..=v.len() - wide);
                    needles.push(v[off..off + wide].to_vec());
                }
            }
            let mut noise = vec![0u8; rng.gen_range(3..=4)];
            rng.fill_bytes(&mut noise);
            needles.push(noise);
            if cycle % 4 == 0 {
                needles.push(Vec::new());
            }
            for needle in &needles {
                let want: Vec<(u64, Vec<u8>)> = log
                    .iter()
                    .filter(|(_, v)| {
                        needle.is_empty() || v.windows(needle.len()).any(|w| w == &needle[..])
                    })
                    .cloned()
                    .collect();
                assert_eq!(
                    host.find_all_bytes(needle),
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
            // Reclaimable space, worked out twice: the engine tracks it
            // as rows retire, the inspector replays the file and adds it
            // up. It is the number a host watches to decide when to
            // rebuild, and it is in SLOTS — a retired eight-slot value
            // frees eight, not one.
            assert_eq!(
                report.rows.dead_slots(),
                host.engine.dead_slots(),
                "[{ctx}] inspector and engine disagree about reclaimable slots"
            );
            assert_eq!(
                report.rows.live_records + report.rows.dead_slots() + report.rows.chunks
                    - report.rows.dead_chunks,
                used,
                "[{ctx}] the inspector's slots do not add up to the file"
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
            let survivors = rescue.range_all_bytes(0, u64::MAX);
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
