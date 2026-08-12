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
//! The seed is printed *before* the run, so a panic always has its
//! reproducer directly above it in the output.

use dabqlite_sim::{run_lifetime, LifetimeConfig};
use rand::{rngs::OsRng, Rng};

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

    // Heavier than the CI config: longer lifetimes, more chances to break.
    let cfg = LifetimeConfig {
        cycles: 64,
        max_inserts_per_cycle: 8,
        caps: dabqlite_core::Capacities { rows: 256 },
        recovery_crash_p: 0.25,
    };

    if let Some(seed) = seed {
        println!("vopr: seed={seed} (explicit)");
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
        println!("vopr: run={i} seed={seed}");
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
