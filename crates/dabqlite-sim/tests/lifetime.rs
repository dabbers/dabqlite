//! Multi-crash lifetime testing: a database living through repeated
//! crash/recover/continue cycles, verified against the oracle after every
//! recovery. See `dabqlite_sim::lifetime` for the mechanics.

use dabqlite_sim::{run_lifetime, LifetimeConfig};

#[test]
fn lifetimes_survive_repeated_crashes() {
    let cfg = LifetimeConfig::default();
    let mut crashes = 0;
    let mut io_failures = 0;
    let mut recovery_crashes = 0;
    let mut in_flight_committed = 0;
    for seed in 0..48u64 {
        let stats = run_lifetime(seed, &cfg);
        crashes += stats.crashes;
        io_failures += stats.io_failures;
        recovery_crashes += stats.recovery_crashes;
        in_flight_committed += stats.in_flight_committed;
    }
    // The sweep must actually exercise the interesting paths, or a passing
    // run proves nothing. These bounds fail if the generator drifts.
    assert!(crashes > 100, "only {crashes} crashes across the sweep");
    assert!(io_failures > 20, "only {io_failures} fail-stop restarts");
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
fn every_cycle_verifies_the_whole_feature_surface() {
    // The lifetime is the "everything in one pass" harness: every cycle
    // must run substring-search oracle checks AND inspector agreements,
    // or the soak's coverage claim is hollow. Floors, not hopes.
    let cfg = LifetimeConfig::default();
    let mut find_checks = 0;
    let mut inspections = 0;
    let mut cycles = 0;
    for seed in 0..16u64 {
        let stats = run_lifetime(seed, &cfg);
        find_checks += stats.find_checks;
        inspections += stats.inspections;
        cycles += stats.cycles as u64;
    }
    assert_eq!(
        inspections, cycles,
        "inspector agreement must run EVERY cycle"
    );
    assert!(
        find_checks >= cycles * 3,
        "only {find_checks} substring checks across {cycles} cycles"
    );
}

#[test]
fn full_disk_episodes_actually_happen_in_the_soak() {
    let cfg = LifetimeConfig {
        disk_full_p: 0.5,
        ..LifetimeConfig::default()
    };
    let mut episodes = 0;
    for seed in 0..12u64 {
        episodes += run_lifetime(seed, &cfg).disk_full_recoveries;
    }
    assert!(
        episodes > 20,
        "only {episodes} full-disk recovery episodes; the ENOSPC regime \
         is under-exercised"
    );
}

#[test]
fn legacy_lifetimes_migrate_under_fire_and_then_live_normally() {
    // A lifetime that BEGINS as a v1 database: migrated under the fault
    // schedule (crash/EIO retries until the two-worlds protocol
    // converges), then lives through the usual crash cycles with the
    // migrated rows folded into every oracle check.
    let cfg = LifetimeConfig {
        legacy_rows_max: 24,
        ..LifetimeConfig::default()
    };
    let mut migrations = 0;
    let mut attempts = 0;
    for seed in 0..24u64 {
        let stats = run_lifetime(seed, &cfg);
        migrations += stats.migrations;
        attempts += stats.migration_attempts;
    }
    assert_eq!(migrations, 24, "every legacy lifetime must converge");
    assert!(
        attempts > migrations + 10,
        "only {attempts} attempts for {migrations} migrations; \
         the faulted-migration retry path is under-exercised"
    );
}

#[test]
fn legacy_lifetimes_are_deterministic_too() {
    let cfg = LifetimeConfig {
        legacy_rows_max: 16,
        ..LifetimeConfig::default()
    };
    for seed in 0..6u64 {
        assert_eq!(
            run_lifetime(seed, &cfg),
            run_lifetime(seed, &cfg),
            "seed={seed} legacy lifetime diverged"
        );
    }
}

#[test]
fn same_seed_same_lifetime_bit_for_bit() {
    // The seed is the only input (docs/DESIGN.md §7.2). If two runs of one
    // seed diverge, some nondeterminism leaked into the simulator and every
    // "reproducible from an integer" claim is void.
    let cfg = LifetimeConfig::default();
    for seed in 0..12u64 {
        let a = run_lifetime(seed, &cfg);
        let b = run_lifetime(seed, &cfg);
        assert_eq!(a, b, "seed={seed} produced two different lifetimes");
    }
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
