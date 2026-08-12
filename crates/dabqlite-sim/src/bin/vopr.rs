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
use dabqlite_sim::{run_lifetime, LifetimeConfig};
use rand::{rngs::OsRng, Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Derive the swarm configuration for a seed. Deterministic: the explicit
/// seed path uses the same derivation, so `vopr -- <seed>` reproduces the
/// exact lifetime, config included.
fn config_for(seed: u64) -> LifetimeConfig {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x5741_524D_5445_5354); // "SWARMTEST"
    let rows = *[4u64, 16, 64, 256, 1024]
        .get(rng.gen_range(0..5))
        .expect("in range");
    LifetimeConfig {
        cycles: rng.gen_range(8..=96),
        max_inserts_per_cycle: rng.gen_range(1..=12),
        caps: Capacities { rows },
        recovery_crash_p: rng.gen_range(0.0..0.4),
        io_fail_p: rng.gen_range(0.0..0.4),
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

    let mut total_commits: u64 = 0;
    let mut total_crashes: u64 = 0;
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
        let stats = run_lifetime(seed, &cfg);
        total_commits += stats.commits;
        total_crashes += stats.crashes;
        i += 1;
        if i.is_multiple_of(100) {
            println!(
                "vopr: {i} lifetimes ok ({total_commits} commits, {total_crashes} crashes survived)"
            );
        }
    }
    println!(
        "vopr: all {i} lifetimes ok ({total_commits} commits, {total_crashes} crashes survived)"
    );
}
