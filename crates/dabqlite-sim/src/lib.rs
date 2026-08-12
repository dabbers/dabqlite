//! # dabqlite-sim
//!
//! The deterministic simulation harness (docs/DESIGN.md §7). The core is a
//! pure state machine; this crate supplies everything nondeterministic —
//! disk, faults, crashes — in simulated, seed-reproducible form.
//!
//! The contract: **the seed is the only input** (§7.2). Every failure a test
//! reports includes the seed (and crash boundary) that reproduces it; pasting
//! that integer back reproduces the failure exactly, on any machine.

pub mod disk;
pub mod host;
pub mod workload;

pub use disk::SimDisk;
pub use host::{Driven, SimHost};
pub use workload::gen_workload;
