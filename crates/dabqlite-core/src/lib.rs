//! # dabqlite-core
//!
//! The pure state-machine core of DABQLite (docs/DESIGN.md §4.1):
//!
//! ```text
//! fn tick(&mut self, input: Input) -> Output
//! ```
//!
//! No I/O, no clock, no randomness, no allocation after init. I/O is
//! *returned* as a request; the host performs it and feeds the result back as
//! another input. This crate is `#![no_std]` (plus `alloc` for the one arena
//! allocation per zone at open) and must always build for
//! `wasm32-unknown-unknown` — that target has no clock, no randomness, and no
//! filesystem, so ambient nondeterminism fails to link rather than misbehave.
//!
//! ## The vertical slice (docs/DESIGN.md §9 step 1)
//!
//! One table (`records`), one fixed-width value field, insert and get-by-id.
//! Arena allocated at open, superblock durability with a checksummed copy
//! set, and a commit protocol whose single atomicity point is the superblock
//! generation flip:
//!
//! ```text
//! Insert -> write row slot -> fsync rows -> write stale superblock copy
//!        -> fsync superblock -> committed
//! ```
//!
//! A crash anywhere in that sequence recovers to either the previous state
//! (N) or, if the superblock write survived, the fully-consistent next state
//! (N+1) — never in between. That property is exercised exhaustively by the
//! simulator in `dabqlite-sim` (docs/DESIGN.md §7.3).

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod blob;
pub mod btree;
pub mod crc32;
pub mod engine;
pub mod generated;
pub mod layout;
pub mod migration;

pub use blob::{BlobAllocator, BlobError, BlobHandle, BlobStats, BLOB_HARD_MAX};
pub use engine::{
    Capacities, DbError, Engine, FileId, Input, Output, RangePage, RecoveryReport, WriteBuf,
    RANGE_PAGE,
};
pub use layout::{ROW_SIZE, SB_COPIES, SB_COPY_SIZE, SB_ZONE_SIZE, SCHEMA_HASH, VALUE_LEN};
