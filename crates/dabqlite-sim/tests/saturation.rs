//! Pegged-CPU validation: scheduling pressure cannot touch correctness.
//!
//! The core is clockless by construction (docs/DESIGN.md §7.1 — the wasm
//! gate makes ambient time and threads unlinkable), so CPU starvation has
//! no seam to enter through: a starved engine computes the same bytes,
//! later. These tests pin that structural claim EMPIRICALLY:
//!
//! - full fault-schedule lifetimes produce bit-identical statistics on a
//!   machine whose every core is pegged by spinner threads;
//! - the same seed run simultaneously from many threads agrees with
//!   itself exactly — no hidden global, no shared mutable state, nothing
//!   scheduling-order-dependent anywhere in engine, simulator, oracle,
//!   or inspector.
//!
//! The limit case — a writer PAUSED indefinitely mid-commit and resumed —
//! is a real-process test (`dabqlite-host/tests/sigstop.rs`): SIGSTOP is
//! the "infinitely pegged CPU".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dabqlite_sim::{run_lifetime, LifetimeConfig, LifetimeStats};

/// The full-surface configuration: crashes, EIO, recovery crashes,
/// disk-full episodes, and a legacy migration under fire — everything the
/// soak runs, so the determinism pin covers the whole feature surface.
fn config() -> LifetimeConfig {
    LifetimeConfig {
        legacy_rows_max: 16,
        ..LifetimeConfig::default()
    }
}

/// Spinners pegging every core (twice over) until dropped.
struct Saturation {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Saturation {
    fn start() -> Self {
        let n = std::thread::available_parallelism().map_or(4, |n| n.get()) * 2;
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..n)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        Saturation { stop, threads }
    }
}

impl Drop for Saturation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            t.join().expect("spinner");
        }
    }
}

#[test]
fn lifetimes_are_bit_identical_under_saturated_cores() {
    let cfg = config();
    let seeds: Vec<u64> = (0..12).collect();

    // Baseline on an unloaded machine.
    let baseline: Vec<LifetimeStats> = seeds.iter().map(|&s| run_lifetime(s, &cfg)).collect();
    // Sanity: the schedule actually does things worth comparing.
    let commits: u64 = baseline.iter().map(|s| s.commits).sum();
    let crashes: u64 = baseline.iter().map(|s| s.crashes).sum();
    assert!(commits > 100, "baseline too quiet: {commits} commits");
    assert!(crashes > 20, "baseline too quiet: {crashes} crashes");

    // Same seeds with every core pegged. Not one bit may differ.
    let _load = Saturation::start();
    for (&seed, expect) in seeds.iter().zip(&baseline) {
        let stats = run_lifetime(seed, &cfg);
        assert_eq!(stats, *expect, "seed {seed} diverged under CPU saturation");
    }
}

#[test]
fn concurrent_runs_of_one_seed_agree_exactly() {
    // Eight threads race the SAME lifetime while spinners peg the rest of
    // the machine: any hidden shared state, ambient randomness, or
    // scheduling dependence shows up as disagreement.
    let cfg = config();
    let expect = run_lifetime(0xC0DE, &cfg);
    let _load = Saturation::start();
    let runs: Vec<std::thread::JoinHandle<LifetimeStats>> = (0..8)
        .map(|_| std::thread::spawn(move || run_lifetime(0xC0DE, &config())))
        .collect();
    for (i, run) in runs.into_iter().enumerate() {
        let stats = run.join().expect("lifetime thread");
        assert_eq!(stats, expect, "concurrent run {i} diverged");
    }
}
