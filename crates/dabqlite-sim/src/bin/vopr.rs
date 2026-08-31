//! The VOPR: run randomized database lifetimes until something breaks.
//! Named for TigerBeetle's Viewstamped Operation Replicator; ours replicates
//! the spirit — a seed-driven soak harness where every failure is an integer.
//!
//! ```text
//! cargo run --release -p dabqlite-sim --bin vopr             # soak forever
//! cargo run --release -p dabqlite-sim --bin vopr -- 12345    # one seed
//! cargo run --release -p dabqlite-sim --bin vopr -- --runs 10000
//! ```
//!
//! **Swarm testing**: the lifetime configuration (cycle count, capacity,
//! fault probabilities) is itself derived from the seed, so the soak
//! explores corners of the config space — tiny arenas that live at the
//! capacity wall, fault-storm probabilities, long quiet lifetimes — instead
//! of hammering one operating point. The seed alone still reproduces
//! everything, config included.
//!
//! The seed is printed *before* the run, so a panic always has its
//! reproducer directly above it in the output.

use dabqlite_core::Capacities;
use dabqlite_sim::{run_lifetime, LifetimeConfig, LifetimeStats};
use rand::{rngs::OsRng, Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Simulated-time model (documented in docs/FAULTS.md, "Simulated-time
/// accounting"): what each simulated event would have cost on real
/// hardware. Deliberately conservative — fast NVMe, fast supervisor.
const FSYNC_SECS: f64 = 0.001; // 1 ms: fast NVMe fsync
const RW_SECS: f64 = 0.000_02; // 20 µs: NVMe 4K read/write
const RESTART_SECS: f64 = 5.0; // process crash -> supervisor restart -> open

#[derive(Default)]
struct Totals {
    lifetimes: u64,
    commits: u64,
    reads: u64,
    writes: u64,
    fsyncs: u64,
    crashes: u64,
    io_failures: u64,
    recovery_crashes: u64,
    find_checks: u64,
    inspections: u64,
    migrations: u64,
    migration_attempts: u64,
    disk_full_recoveries: u64,
}

impl Totals {
    fn add(&mut self, s: &LifetimeStats) {
        self.lifetimes += 1;
        self.commits += s.commits;
        self.reads += s.reads;
        self.writes += s.writes;
        self.fsyncs += s.fsyncs;
        self.crashes += s.crashes;
        self.io_failures += s.io_failures;
        self.recovery_crashes += s.recovery_crashes;
        self.find_checks += s.find_checks;
        self.inspections += s.inspections;
        self.migrations += s.migrations;
        self.migration_attempts += s.migration_attempts;
        self.disk_full_recoveries += s.disk_full_recoveries;
    }

    fn report(&self, wall_secs: f64) {
        let io_secs = self.fsyncs as f64 * FSYNC_SECS + (self.reads + self.writes) as f64 * RW_SECS;
        let restarts = self.crashes + self.io_failures + self.recovery_crashes;
        let restart_secs = restarts as f64 * RESTART_SECS;
        let sim_secs = io_secs + restart_secs;
        println!(
            "vopr: {} lifetimes | {} commits | {} reads, {} writes, {} fsyncs",
            self.lifetimes, self.commits, self.reads, self.writes, self.fsyncs
        );
        println!(
            "vopr: faults survived: {} crashes, {} EIO fail-stops, {} crashes mid-recovery",
            self.crashes, self.io_failures, self.recovery_crashes
        );
        println!(
            "vopr: full surface: {} migrations ({} attempts under faults), \
             {} substring-search oracle checks, {} inspector agreements, \
             {} full-disk recovery episodes",
            self.migrations,
            self.migration_attempts,
            self.find_checks,
            self.inspections,
            self.disk_full_recoveries
        );
        println!(
            "vopr: simulated operational time: {:.1} h ({:.1} h of device I/O + {} restart cycles at {}s)",
            sim_secs / 3600.0,
            io_secs / 3600.0,
            restarts,
            RESTART_SECS
        );
        if wall_secs > 0.0 {
            println!(
                "vopr: wall clock {:.1} s -> {:.0}x real time",
                wall_secs,
                sim_secs / wall_secs
            );
        }
    }
}

/// Derive the swarm configuration for a seed. Deterministic: the explicit
/// seed path uses the same derivation, so `vopr -- <seed>` reproduces the
/// exact lifetime, config included.
fn config_for(seed: u64) -> LifetimeConfig {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x5741_524D_5445_5354); // "SWARMTEST"
    let rows = *[4u64, 16, 64, 256, 1024]
        .get(rng.gen_range(0..5))
        .expect("in range");
    // A quarter of all lifetimes begin as legacy v1 databases and migrate
    // under the fault schedule before their first open.
    let legacy_rows_max = if rng.gen_bool(0.25) {
        rng.gen_range(1..=rows.min(64))
    } else {
        0
    };
    LifetimeConfig {
        cycles: rng.gen_range(8..=96),
        max_inserts_per_cycle: rng.gen_range(1..=12),
        caps: Capacities { rows },
        recovery_crash_p: rng.gen_range(0.0..0.4),
        io_fail_p: rng.gen_range(0.0..0.4),
        legacy_rows_max,
        disk_full_p: rng.gen_range(0.0..0.3),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut seed: Option<u64> = None;
    let mut runs: Option<u64> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                let v = args.next().expect("--runs needs a value");
                runs = Some(v.parse().expect("--runs must be a u64"));
            }
            s => seed = Some(s.parse().expect("seed must be a u64")),
        }
    }

    if let Some(seed) = seed {
        let cfg = config_for(seed);
        println!("vopr: seed={seed} (explicit) cfg={cfg:?}");
        let stats = run_lifetime(seed, &cfg);
        println!("vopr: seed={seed} ok: {stats:?}");
        return;
    }

    // Wall-clock measurement of the soak harness itself, outside the
    // deterministic boundary: nothing inside a simulation ever observes it.
    #[allow(clippy::disallowed_methods)]
    let started = std::time::Instant::now();
    let mut totals = Totals::default();
    let mut i: u64 = 0;
    loop {
        if let Some(max) = runs {
            if i == max {
                break;
            }
        }
        // OS entropy picks the seed; determinism starts the moment it's
        // chosen. Never used inside the simulation itself.
        let seed: u64 = OsRng.gen();
        let cfg = config_for(seed);
        println!(
            "vopr: run={i} seed={seed} cycles={} rows={} io_fail_p={:.2}",
            cfg.cycles, cfg.caps.rows, cfg.io_fail_p
        );
        totals.add(&run_lifetime(seed, &cfg));
        i += 1;
        if i.is_multiple_of(500) {
            println!("vopr: {i} lifetimes ok");
        }
    }
    println!("vopr: all {i} lifetimes ok");
    #[allow(clippy::disallowed_methods)]
    totals.report(started.elapsed().as_secs_f64());
}
