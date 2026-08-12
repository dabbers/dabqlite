//! Multi-crash lifetime testing: a database living through repeated
//! crash/recover/continue cycles, verified against the oracle after every
//! recovery. See `dabqlite_sim::lifetime` for the mechanics.

use dabqlite_sim::{run_lifetime, LifetimeConfig};

#[test]
fn lifetimes_survive_repeated_crashes() {
    let cfg = LifetimeConfig::default();
    let mut crashes = 0;
    let mut recovery_crashes = 0;
    let mut in_flight_committed = 0;
    for seed in 0..48u64 {
        let stats = run_lifetime(seed, &cfg);
        crashes += stats.crashes;
        recovery_crashes += stats.recovery_crashes;
        in_flight_committed += stats.in_flight_committed;
    }
    // The sweep must actually exercise the interesting paths, or a passing
    // run proves nothing. These bounds fail if the generator drifts.
    assert!(crashes > 100, "only {crashes} crashes across the sweep");
    assert!(
        recovery_crashes > 10,
        "only {recovery_crashes} crashes-during-recovery"
    );
    assert!(
        in_flight_committed > 5,
        "only {in_flight_committed} in-flight commits observed; \
         the N+1 recovery path is under-exercised"
    );
}

#[test]
fn lifetimes_hit_the_capacity_wall() {
    // Tiny capacity: lifetimes spend most cycles at Full, exercising
    // capacity rejection interleaved with crashes.
    let cfg = LifetimeConfig {
        caps: dabqlite_core::Capacities { rows: 8 },
        ..LifetimeConfig::default()
    };
    let mut full_rejections = 0;
    for seed in 0..24u64 {
        full_rejections += run_lifetime(seed, &cfg).full_rejections;
    }
    assert!(
        full_rejections > 50,
        "only {full_rejections} Full rejections; the wall is under-exercised"
    );
}
