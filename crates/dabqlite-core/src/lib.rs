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
/// Verdict labels shared by the engine and the inspector.
///
/// The inspector is a deliberately independent second implementation of the
/// recovery rules (cross-checked by agreement tests), but the human-readable
/// LABEL for a defect is not a rule — duplicating it only invites drift that
/// the agreement test would report as a disagreement it is not.
pub mod defect {
    /// A committed row failed checksum or padding validation.
    pub const ROW_CHECKSUM: &str =
        "committed row failed checksum (reopen in salvage mode to read the rest)";
    /// A deletion refers to an id that holds no live record at that point
    /// in the commit order.
    pub const ORPHAN_TOMBSTONE: &str =
        "deletion of a row that was not live (reopen in salvage mode to read the rest)";
    /// An update refers to an id that holds no live record at that point
    /// in the commit order.
    pub const ORPHAN_UPDATE: &str =
        "update of a row that was not live (reopen in salvage mode to read the rest)";
    /// A continuation row with no value in front of it to continue. The
    /// engine only ever writes a chunk immediately after the row it
    /// belongs to, in the same commit, so a stranded chunk means a
    /// misdirected write, a truncated file, or a file we did not write.
    pub const ORPHAN_CHUNK: &str =
        "value continuation with nothing to continue (reopen in salvage mode to read the rest)";
    /// A value's continuations do not run to the end it promised: the
    /// head says the value continues, and what follows is not a readable
    /// continuation of it. Serving the head alone would serve a value
    /// silently cut short, so the whole value goes instead.
    pub const TRUNCATED_VALUE: &str =
        "value continuation missing or damaged (reopen in salvage mode to read the rest)";
    /// Two committed rows claim the same primary key.
    pub const DUPLICATE_ID: &str =
        "duplicate id among committed rows (reopen in salvage mode to read the rest)";
}

pub mod inspect;
pub mod layout;
pub mod migration;
pub mod trigram;

pub use blob::{BlobAllocator, BlobError, BlobHandle, BlobStats, BLOB_HARD_MAX};
pub use engine::{
    BatchOp, BatchReject, Capacities, DbError, Engine, FileId, FindPage, Input, Match, Output,
    RangePage, RecoveryReport, RowRef, ValueWindow, WriteBuf, FIND_PAGE, MAX_VALUE_LEN, RANGE_PAGE,
    WINDOW_LEN,
};
pub use layout::{
    MAX_COMMIT_ROWS, ROW_SIZE, SB_COPIES, SB_COPY_SIZE, SB_ZONE_SIZE, SCHEMA_HASH, VALUE_LEN,
};
pub use trigram::FindCursor;
