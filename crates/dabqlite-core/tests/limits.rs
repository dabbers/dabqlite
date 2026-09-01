//! Capacity-declaration guards, pinned by name.
//!
//! `Engine::new` sizes every arena from the declared capacity with
//! checked arithmetic (docs/DESIGN.md §4.2): a capacity whose arena
//! cannot be expressed panics AT CONSTRUCTION with a named message — it
//! never wraps into a small allocation that would corrupt addressing
//! later. These tests pin the guards so a mutant that swaps
//! `checked_mul` for a wrapping one dies here, not in production.
//!
//! (A capacity that is representable but merely huge is the OOM case —
//! covered by `dabqlite-host/tests/oom.rs` against a real `RLIMIT_AS`.)

use dabqlite_core::migration::MigrationEngine;
use dabqlite_core::{Capacities, Engine};

fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let hook = std::panic::take_hook();
    // Silence the expected panic's backtrace noise in test output.
    std::panic::set_hook(Box::new(|_| {}));
    let err = std::panic::catch_unwind(f).expect_err("must panic");
    std::panic::set_hook(hook);
    err.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| err.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

#[test]
fn zero_capacity_is_refused_at_construction() {
    let msg = panic_message(|| {
        let _ = Engine::new(Capacities { rows: 0 });
    });
    assert!(
        msg.contains("capacity must be positive"),
        "wrong guard: {msg:?}"
    );
    let msg = panic_message(|| {
        let _ = MigrationEngine::new(Capacities { rows: 0 });
    });
    assert!(
        msg.contains("capacity must be positive"),
        "wrong guard: {msg:?}"
    );
}

#[test]
fn unrepresentable_capacity_panics_named_never_wraps() {
    // rows * ROW_SIZE overflows usize: the checked multiply must refuse
    // loudly. A wrapping multiply would "succeed" with a tiny arena and
    // corrupt row addressing forever after.
    let msg = panic_message(|| {
        let _ = Engine::new(Capacities { rows: u64::MAX });
    });
    assert!(msg.contains("overflows arena size"), "wrong guard: {msg:?}");
    // The largest power-of-two capacity is just as unrepresentable —
    // no boundary value sneaks past the guard.
    let msg = panic_message(|| {
        let _ = Engine::new(Capacities { rows: 1 << 62 });
    });
    assert!(msg.contains("overflows arena size"), "wrong guard: {msg:?}");
}
