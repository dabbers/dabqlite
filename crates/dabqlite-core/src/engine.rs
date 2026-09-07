//! The storage engine state machine.
//!
//! `Engine::tick` consumes exactly one [`Input`] and produces exactly one
//! [`Output`]. I/O outputs are requests the host must perform and complete
//! (via `ReadDone`/`WriteDone`/`FsyncDone`) before anything else happens:
//! v1 serializes all access (docs/DESIGN.md §5, isolation).
//!
//! ## Commit protocol
//!
//! ```text
//! state:   Ready ──Insert──▶ InsertWriteRow ──▶ InsertFsyncRows
//!                                                     │
//!          Ready ◀── InsertFsyncSb ◀── InsertWriteSb{0,1} ◀┘
//! ```
//!
//! The row slot is written and fsynced *before* the superblock copies that
//! reference it (docs/DESIGN.md §4.4), so a surviving superblock always
//! names fully-durable data. The generation flip in the superblock is the
//! sole atomicity point.
//!
//! ## Superblock copy-set rotation
//!
//! Generation `g` is written to the two slots of pair `g % 2` (slots 0,1 or
//! 2,3). This buys two properties at once:
//!
//! - **Crash safety**: a commit never touches the previous generation's
//!   pair, so even if every unsynced write tears, the previous generation
//!   survives intact.
//! - **Media-fault tolerance**: every generation exists in two slots, so a
//!   single corrupted copy (bit rot, torn sector discovered later) cannot
//!   lose a committed generation. Recovery takes the highest valid copy
//!   found in either slot.

use alloc::vec;
use alloc::vec::Vec;

use crate::btree::BTreeIndex;
use crate::layout::{
    decode_row, decode_sb, encode_row, encode_sb, RowKind, RowSlot, SbDecodeError, MAX_COMMIT_ROWS,
    ROW_SIZE, SB_COPIES, SB_COPY_SIZE, SB_ZONE_SIZE, SCHEMA_HASH, VALUE_LEN,
};
use crate::trigram::{FindCursor, TrigramIndex};

/// The declared file set (docs/DESIGN.md §4.4): derived from the schema,
/// knowable before the program runs. One file per zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileId {
    /// The superblock copy set: the sole atomicity point.
    Superblock,
    /// Fixed-width row slots for the `records` table, in the schema this
    /// binary was compiled against.
    Rows,
    /// The LEGACY rows file (previous schema). Only the migration engine
    /// ever touches it, and only to READ: migration writes the new rows
    /// file and flips the superblock, leaving this file byte-identical —
    /// an inert orphan after the flip (docs/DESIGN.md §4.4, §4.8). The
    /// row engine itself never emits this id.
    RowsOld,
}

/// Capacities supplied at open (docs/DESIGN.md §4.2). Layout is a
/// compile-time constant; capacity is an open-time argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacities {
    /// Maximum number of rows in the `records` table.
    pub rows: u64,
}

/// User-visible errors. Capacity exhaustion is first-class (docs/DESIGN.md
/// §6): the error carries the entity, the configured capacity, and reads
/// like documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbError {
    /// The zone is at its declared capacity. Raise `Capacities::rows` at
    /// `open()` to make room; `usage()` reports fill level so hosts can
    /// alarm before hitting this.
    ///
    /// `dead` is how many of those slots hold nothing useful — superseded
    /// records, deleted ones, and the tombstones that retired them. A
    /// rebuild reclaims exactly those, and needs no free slot to do it, so
    /// the number is the difference between "run a compaction" and "this
    /// database really is full".
    Full {
        entity: &'static str,
        capacity: u64,
        dead: u64,
    },
    /// A row with this id already exists.
    DuplicateId { id: u64 },
    /// No row with this id exists, so there is nothing to delete. Deleting
    /// an absent row is a caller mistake, not a silent no-op: the engine
    /// says so rather than burning a slot on a tombstone for nothing.
    NotFound { id: u64 },
    /// An operation is already in flight; v1 serializes all access.
    Busy,
    /// The engine has not completed `open()` yet.
    NotOpen,
    /// The file was written by a different schema. Migrate before opening
    /// (docs/DESIGN.md §4.8).
    SchemaMismatch {
        file_schema: u64,
        binary_schema: u64,
    },
    /// The configured capacity is smaller than the committed data already
    /// on disk. Reopen with at least `required` rows.
    CapacityBelowData { required: u64, configured: u64 },
    /// On-disk state violates an invariant the commit protocol guarantees.
    /// A strict `Open` refuses such a file outright; `OpenSalvage` opens it
    /// read-only with the damaged rows quarantined, so one bad row costs
    /// one row rather than the whole database.
    Corrupt { what: &'static str },
    /// The database is open in SALVAGE mode with `quarantined` unreadable
    /// rows, and this operation cannot be answered honestly:
    ///
    /// - **writes** are refused outright (salvage never mutates);
    /// - a **`Get` miss** is refused, because "absent" is no longer
    ///   distinguishable from "was in a quarantined slot". A `Get` HIT is
    ///   still returned normally: it is checksum-verified and therefore
    ///   exactly right. Degradation costs certainty about what is missing,
    ///   never correctness about what is present.
    ///
    /// Rebuild with the inspector's `--repair-to` to return to a clean
    /// database.
    Degraded { quarantined: u64 },
    /// The batch names more operations, or needs more row slots, than one
    /// commit can carry. Distinct from `Full`, which is about the
    /// DATABASE being out of room: a batch can be too long for a commit
    /// while the database is nearly empty, and reporting that as "full at
    /// capacity 128" states the reverse of the truth.
    BatchTooLong { rows: u64, max: u64 },
    /// The value is longer than a single commit can carry. A value is
    /// stored as a run of row slots inside ONE commit, so its ceiling is
    /// the longest commit the format can describe (see `MAX_VALUE_LEN`).
    /// Anything larger belongs in object storage with a reference here.
    ValueTooLong { len: u32, max: u32 },
    /// The host reported an I/O error on this file. The engine fail-stops
    /// (TigerBeetle-style): the in-flight operation is failed, all further
    /// operations are rejected, and the host must restart and re-open. The
    /// partially-performed operation resolves to all-or-nothing at recovery,
    /// exactly like a crash.
    IoFailed { file: FileId },
}

/// The longest value this store will hold.
///
/// A value too long for one row slot is written as a run of slots inside
/// ONE commit — that is what makes a long value atomic — so its ceiling is
/// the longest commit the row format can describe. Larger payloads belong
/// in object storage with a reference stored here (docs/DESIGN.md §4.5).
pub const MAX_VALUE_LEN: usize = VALUE_LEN * MAX_COMMIT_ROWS;

/// A bounded window onto a value, which may be longer than one row.
///
/// Reads are windowed rather than whole because every buffer the core
/// touches is fixed-size (docs/DESIGN.md §4.5): a long value comes back as
/// a sequence of windows, never as one allocation the core had to make.
/// `total` is the value's full length, so a caller knows on the FIRST
/// window how much there is and can never mistake a prefix for the whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueWindow {
    /// The whole value's length in bytes.
    pub total: u32,
    /// Where this window starts within the value.
    pub offset: u32,
    /// Bytes valid in `bytes`.
    pub len: u8,
    pub bytes: [u8; VALUE_LEN],
}

impl ValueWindow {
    /// The bytes this window carries.
    pub fn payload(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
    /// Where the next window starts, or `None` when this one ends the
    /// value.
    pub fn next_offset(&self) -> Option<u32> {
        let end = self.offset + self.len as u32;
        (end < self.total).then_some(end)
    }
    /// The whole value, when it fits in one window. `None` means the
    /// value is longer than a row and the caller must read the rest —
    /// deliberately not a truncated `&[u8]`, so a prefix cannot be
    /// mistaken for the value.
    pub fn whole(&self) -> Option<&[u8]> {
        (self.total == self.len as u32).then(|| self.payload())
    }
}

/// One row of a scan result: the id, the value's full length, and as much
/// of the value as fits in a row.
///
/// Pages carry the LENGTH as well as the bytes so that a value too long
/// for one row is visibly partial rather than silently truncated — the
/// caller either gets the whole thing from [`RowRef::value`] or gets
/// `None` and reads it properly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRef {
    pub id: u64,
    /// The value's full length, which may exceed `VALUE_LEN`.
    pub len: u32,
    /// The value's first `min(len, VALUE_LEN)` bytes.
    pub head: [u8; VALUE_LEN],
}

impl RowRef {
    const EMPTY: RowRef = RowRef {
        id: 0,
        len: 0,
        head: [0; VALUE_LEN],
    };

    /// The whole value, when it fits in one row; `None` when it spans
    /// several and must be read with `Input::GetFrom`.
    pub fn value(&self) -> Option<&[u8]> {
        (self.len as usize <= VALUE_LEN).then(|| &self.head[..self.len as usize])
    }
}

/// One operation inside an atomic batch (`Input::Batch`).
///
/// The same three writes the engine offers singly. What a batch changes is
/// not what an operation means but when it becomes true: every op in a
/// batch becomes durable under ONE superblock flip, so the batch lands
/// whole or not at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOp<'a> {
    /// Add a row; refuses the batch if the id is already live.
    Insert { id: u64, value: &'a [u8] },
    /// Replace a live row's value; refuses the batch if the id is absent.
    Update { id: u64, value: &'a [u8] },
    /// Insert or replace, whichever applies. Resolved during validation
    /// against the state the batch's earlier ops would leave, so it is
    /// exact: no read-then-write race can open between the decision and
    /// the commit, because there is no gap to race in.
    Put { id: u64, value: &'a [u8] },
    /// Delete a live row; refuses the batch if the id is absent.
    Delete { id: u64 },
    /// Delete the row if it is there, and do nothing if it is not. Stages
    /// no row when there is nothing to remove, so a batch is not lost to
    /// one target that had already gone.
    Remove { id: u64 },
}

/// Why a batch was refused, and where.
///
/// A batch is validated in full BEFORE any byte is written, against the
/// state each op would see if its predecessors had already applied. If any
/// op cannot proceed, the whole batch is refused having performed no I/O
/// at all — so a rejected batch is not a partial write to clean up, it is
/// a no-op. `at` says which operation stopped it, and `error` is exactly
/// the error that operation would have returned on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchReject {
    pub at: u16,
    pub error: DbError,
}

/// What one staged batch row will do at the commit point. Resolved during
/// validation, when the projected state is known, so that the commit
/// itself is a straight-line application with nothing left to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchEffect {
    /// A value written into `rows` consecutive slots starting at `at`: a
    /// head row plus however many continuations the value needs.
    /// `supersedes` is the slot whose record this retires, which is what
    /// distinguishes a replacement from a fresh insert.
    Store {
        id: u64,
        /// The slot this effect's HEAD row occupies. Carried rather than
        /// derived from the effect's position, because effects no longer
        /// take one slot each: a value spanning three slots moves every
        /// effect after it along by three.
        at: u64,
        rows: u16,
        supersedes: Option<u64>,
    },
    /// A tombstone retiring `record_row`. One slot.
    Delete {
        id: u64,
        /// The slot this tombstone occupies.
        at: u64,
        /// The slot holding the head row this tombstone retires.
        record_row: u64,
    },
}

impl BatchEffect {
    fn id(self) -> u64 {
        match self {
            BatchEffect::Store { id, .. } | BatchEffect::Delete { id, .. } => id,
        }
    }
    /// The slot this effect's head row occupies.
    fn at(self) -> u64 {
        match self {
            BatchEffect::Store { at, .. } | BatchEffect::Delete { at, .. } => at,
        }
    }
    /// Does this effect leave `id` live in the slot it occupies?
    fn leaves_live(self) -> bool {
        matches!(self, BatchEffect::Store { .. })
    }
}

/// An owned, bounded write payload. Rows are 32 bytes and superblock copies
/// 64, so every write the core ever issues fits in one fixed buffer — no
/// allocation, no streaming (docs/DESIGN.md §4.5: bounded buffers
/// everywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteBuf {
    len: u8,
    buf: [u8; SB_COPY_SIZE],
}

impl WriteBuf {
    pub(crate) fn from_slice(src: &[u8]) -> Self {
        assert!(src.len() <= SB_COPY_SIZE, "write exceeds bounded buffer");
        assert!(!src.is_empty(), "empty write is a protocol bug");
        let mut buf = [0u8; SB_COPY_SIZE];
        buf[..src.len()].copy_from_slice(src);
        WriteBuf {
            len: src.len() as u8,
            buf,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

/// Everything that can happen to the engine. I/O completions are fed back by
/// the host; client operations come from the embedding application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input<'a> {
    /// Begin opening. The host reports the existing sizes of the declared
    /// file set (0 = fresh). File creation happens only at open
    /// (docs/DESIGN.md §4.4) and is the host's job.
    Open { superblock_len: u64, rows_len: u64 },
    /// Begin opening in SALVAGE mode: identical to `Open`, except that a
    /// committed row which fails verification is QUARANTINED instead of
    /// failing the open (docs/FAULTS.md, "corruption containment").
    ///
    /// The resulting database is read-only and honest about its damage:
    /// every row it serves is checksum-verified, and every question it
    /// cannot answer truthfully becomes `DbError::Degraded` rather than a
    /// confident wrong answer. Salvage writes NO data byte — it only
    /// fsyncs — so it can never make a damaged file worse.
    ///
    /// A salvage open of an undamaged database is exactly a normal open.
    OpenSalvage { superblock_len: u64, rows_len: u64 },
    /// A read the core requested has completed with these bytes.
    ReadDone { file: FileId, data: &'a [u8] },
    /// A write the core requested has completed.
    WriteDone { file: FileId },
    /// An fsync the core requested has completed.
    FsyncDone { file: FileId },
    /// A truncate the core requested has completed.
    TruncateDone { file: FileId },
    /// The read, write, or fsync the core requested FAILED (EIO and
    /// friends). The write may or may not have reached the disk or page
    /// cache — the engine assumes nothing. It fail-stops; restart to
    /// recover.
    IoFailed { file: FileId },
    /// Client: insert a row.
    Insert { id: u64, value: [u8; VALUE_LEN] },
    /// Client: replace the value of an existing row.
    ///
    /// ONE atomic commit: a new row is appended superseding the old one.
    /// Delete-then-insert would be two commits, and a crash between them
    /// would leave the row gone — which is why this is a first-class
    /// operation and not a convenience built on the other two.
    Update { id: u64, value: [u8; VALUE_LEN] },
    /// Client: delete a row by primary key.
    ///
    /// Recorded by APPENDING a tombstone, never by overwriting the record
    /// it removes: the rows file stays append-only, so a crash mid-delete
    /// resolves all-or-nothing exactly like a crash mid-insert. The cost
    /// is that a delete consumes a row slot like an insert does; a rebuild
    /// (`dabqlite-inspect --repair-to`) compacts both the tombstone and
    /// the record it retired.
    Delete { id: u64 },
    /// Client: apply several writes as ONE commit.
    ///
    /// The rows are appended like any other, then a single superblock flip
    /// makes all of them visible at once: the batch is atomic, and its
    /// fsync count does not grow with its length (two, as for one insert).
    /// A crash anywhere inside it resolves to all-or-nothing exactly like
    /// a crash inside a single write, because it IS a single commit.
    ///
    /// Each op is validated against the state its predecessors in the same
    /// batch would leave, so `insert 5; delete 5; insert 5` is legal and
    /// means what it reads as. If any op is refused, the whole batch is
    /// refused with no I/O performed (see [`BatchReject`]).
    ///
    /// An empty batch commits nothing and is `Ok`: there is no generation
    /// to flip and nothing to make durable.
    Batch { ops: &'a [BatchOp<'a>] },
    /// Client: fetch a row by primary key. Returns the FIRST window of
    /// the value, which carries the value's whole length, so a caller can
    /// tell in one call whether there is more to read.
    Get { id: u64 },
    /// Client: continue reading a value from `offset`. `offset` must be a
    /// multiple of the row width — the boundaries `Get` and earlier
    /// windows hand back — so a window never straddles two slots.
    GetFrom { id: u64, offset: u32 },
    /// Client: range scan by primary key, `lo..=hi`, one bounded page per
    /// call. Continue by re-issuing with `lo = page.next`.
    Range { lo: u64, hi: u64 },
    /// Client: substring search over `value` bytes (trigram-accelerated,
    /// verification-exact). One bounded page per call, in insertion
    /// (row) order; continue by re-issuing with `after = page.next`.
    /// `needle_len` bytes of `needle` are the pattern (<= VALUE_LEN).
    Find {
        needle: [u8; VALUE_LEN],
        needle_len: u8,
        after: Option<FindCursor>,
    },
}

/// Exactly one output per input. I/O requests must be completed before the
/// next client operation is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Host: read `len` bytes at `offset` and feed back `ReadDone`.
    Read { file: FileId, offset: u64, len: u64 },
    /// Host: write these bytes at `offset` and feed back `WriteDone`.
    Write {
        file: FileId,
        offset: u64,
        data: WriteBuf,
    },
    /// Host: fsync the file and feed back `FsyncDone`.
    Fsync { file: FileId },
    /// Host: shorten the file to `len` bytes and feed back `TruncateDone`.
    /// Only ever asked for bytes the manifest does not reference.
    Truncate { file: FileId, len: u64 },
    /// Open finished. `Ok(n)` = recovered `n` committed rows.
    OpenDone { result: Result<u64, DbError> },
    /// Insert finished (durably committed if `Ok`).
    InsertDone {
        id: u64,
        result: Result<(), DbError>,
    },
    /// Update finished (durably committed if `Ok`).
    UpdateDone {
        id: u64,
        result: Result<(), DbError>,
    },
    /// Delete finished (durably committed if `Ok`).
    DeleteDone {
        id: u64,
        result: Result<(), DbError>,
    },
    /// Batch finished. `rows` is the number of row slots it committed —
    /// equal to the number of ops on success, and 0 on refusal, because a
    /// refused batch performs no I/O and applies nothing.
    BatchDone {
        rows: u64,
        result: Result<(), BatchReject>,
    },
    /// Get finished (pure in-memory lookup, always immediate). `None`
    /// means the id is absent; a window carries the value's whole length
    /// alongside the bytes it holds.
    GetDone {
        id: u64,
        result: Result<Option<ValueWindow>, DbError>,
    },
    /// Range page finished (pure in-memory, always immediate).
    RangeDone { result: Result<RangePage, DbError> },
    /// Find page finished (pure in-memory, always immediate).
    FindDone { result: Result<FindPage, DbError> },
    /// Offline migration finished (docs/DESIGN.md §4.8). `Ok(n)` = the
    /// file now carries this binary's schema with `n` rows — either
    /// migrated just now or found already current (idempotent no-op).
    /// Emitted only by [`crate::migration::MigrationEngine`].
    MigrateDone { result: Result<u64, DbError> },
}

/// Rows per range page. Results are bounded buffers (docs/DESIGN.md §4.5):
/// large results are sequences of bounded pages, never unbounded blobs.
pub const RANGE_PAGE: usize = 8;

/// Rows per find page (bounded buffers, docs/DESIGN.md §4.5).
pub const FIND_PAGE: usize = 8;

/// One bounded page of a substring search, in ascending INSERTION (row)
/// order — substring results have no natural key order, and row order is
/// deterministic and stable. `next` is an opaque continuation cursor: a
/// full page sets it, and the final continuation may legitimately return
/// an empty page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub struct FindPage {
    pub items: [RowRef; FIND_PAGE],
    pub count: u8,
    /// Where to continue from; `None` ends the search. Opaque — pass it
    /// back unchanged.
    pub next: Option<FindCursor>,
    /// True when the database is open in salvage mode with quarantined
    /// rows: every row returned is verified and exact, but rows that
    /// would have matched may be missing. A scan that silently omitted
    /// them would be indistinguishable from data loss, so it says so.
    pub incomplete: bool,
}

/// One bounded page of a range scan, in strictly ascending key order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangePage {
    pub items: [RowRef; RANGE_PAGE],
    pub count: u8,
    /// `Some(k)`: more rows exist; continue with `lo = k`.
    pub next: Option<u64>,
    /// True when the database is open in salvage mode with quarantined
    /// rows — see [`FindPage::incomplete`].
    pub incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Constructed, no `Open` input yet.
    New,
    /// Fresh database: initial superblock write in flight (copy 0 or 1 of
    /// the generation's pair).
    InitWriteSb { copy: u8 },
    /// Fresh database: initial superblock fsync in flight.
    InitFsyncSb,
    /// Recovery: superblock zone read in flight.
    RecoverReadSb,
    /// Recovery: committed-rows read in flight.
    RecoverReadRows { generation: u64, row_count: u64 },
    /// Recovery: TRUNCATE of the rows file in flight, dropping everything
    /// past the manifest before anything else is made durable.
    ///
    /// What lies past the manifest is the residue of commits that were
    /// never acknowledged, and this open has just finished reading it: the
    /// rollback verdict is already decided. Leaving it there would let
    /// residue from SEVERAL incarnations pile up, and the pile is what
    /// makes a false alarm — two never-acknowledged commits of different
    /// widths look exactly like one acknowledged commit that was rolled
    /// back. Clearing it means the region always belongs to at most one
    /// commit: this incarnation's.
    ///
    /// Only ever shrinks, and only ever bytes the manifest does not
    /// reference, so a crash mid-truncate leaves committed data untouched
    /// and the next open repeats it.
    RecoverTruncateRows { generation: u64, row_count: u64 },
    /// Recovery: rows-file fsync in flight. Recovery fsyncs both files
    /// before OpenDone: after a fail-stop restart (process died, machine
    /// did not), the page cache can show state that was never made durable.
    /// Serving it without fsyncing would mean a later power loss erases
    /// rows that this incarnation already showed the application.
    RecoverFsyncRows { generation: u64, row_count: u64 },
    /// Recovery: REPAIR write in flight — recovery rewrites the chosen
    /// generation's INVALID twin slot before the final fsync, restoring
    /// the two-copy redundancy invariant no matter how the generation was
    /// found (a single surviving copy after a torn commit, a
    /// media-faulted twin, a cache-only copy after an EIO'd commit).
    /// Without repair, a generation recovered from one copy runs with NO
    /// redundancy and a single later in-budget fault silently drops it —
    /// found by the full-surface storm (seed 2: torn commit kept one
    /// copy, visible-implies-durable acked it, one transient read fault
    /// later rolled it back).
    ///
    /// ONLY the invalid twin is ever written — never the valid copy the
    /// generation was recovered FROM. Rewriting a valid copy in place
    /// would break the protocol's golden rule (never overwrite the only
    /// copy of the truth): a crash DURING recovery could tear the repair
    /// write over the sole good copy and lose the generation — the storm
    /// found that too (seed 16, against an earlier rewrite-both repair).
    /// Torn this way, a repair write can only damage a slot that was
    /// already dead: strictly monotone.
    RecoverRepairSb {
        generation: u64,
        row_count: u64,
        /// The pair-relative index (0 or 1) of the twin being repaired.
        copy: u8,
    },
    /// Recovery: superblock fsync in flight (see `RecoverFsyncRows`).
    RecoverFsyncSb { generation: u64, row_count: u64 },
    /// Open and idle.
    Ready,
    /// Insert: row-slot write in flight.
    InsertWriteRow,
    /// Insert: rows-file fsync in flight (durability point for the row).
    InsertFsyncRows,
    /// Insert: superblock-copy write in flight (copy 0 or 1 of the pair).
    InsertWriteSb { copy: u8 },
    /// Insert: superblock fsync in flight (the commit point).
    InsertFsyncSb,
    /// Update: superseding-row write in flight.
    UpdateWriteRow,
    /// Update: rows-file fsync in flight.
    UpdateFsyncRows,
    /// Update: superblock-copy write in flight.
    UpdateWriteSb { copy: u8 },
    /// Update: superblock fsync in flight (the commit point).
    UpdateFsyncSb,
    /// Delete: tombstone-slot write in flight.
    DeleteWriteRow,
    /// Delete: rows-file fsync in flight (durability point for the
    /// tombstone).
    DeleteFsyncRows,
    /// Delete: superblock-copy write in flight (copy 0 or 1 of the pair).
    DeleteWriteSb { copy: u8 },
    /// Delete: superblock fsync in flight (the commit point).
    DeleteFsyncSb,
    /// Batch: the write of staged row `next` (batch-relative) is in
    /// flight. Rows are written one at a time, in order, so a crash can
    /// only ever leave a PREFIX of the batch on disk — which is what
    /// makes the span bytes enough for recovery to recognize it.
    BatchWriteRow { next: u16 },
    /// Batch: rows-file fsync in flight — one fsync for the whole batch,
    /// covering every row it wrote.
    BatchFsyncRows,
    /// Batch: superblock-copy write in flight (copy 0 or 1 of the pair).
    BatchWriteSb { copy: u8 },
    /// Batch: superblock fsync in flight (the commit point for every op
    /// in the batch at once).
    BatchFsyncSb,
    /// Open in SALVAGE mode with quarantined rows: READ-ONLY, and honest.
    /// Verified rows are served exactly; anything the quarantine makes
    /// unanswerable returns `DbError::Degraded`.
    Degraded,
    /// Unrecoverable (corrupt or schema-mismatched file). All ops fail.
    Failed(DbError),
}

/// What recovery observed. See [`Engine::recovery_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Committed rows recovered.
    pub row_count: u64,
    /// Checksum-valid rows found beyond the manifest. Exactly one is the
    /// normal artifact of an insert that was in flight (never acknowledged)
    /// at a crash. Two or more cannot arise that way.
    pub orphan_valid_rows: u64,
    /// True when the orphan count proves at least one *acknowledged* commit
    /// was rolled back by an out-of-budget fault (lying fsync). The
    /// recovered prefix is still exactly correct; what follows it is gone,
    /// and this flag is the loud version of that fact.
    pub rollback_evidence: bool,
    /// The row capacity this database was created with, as recorded in
    /// its superblock — which may differ from the capacity this engine
    /// was opened with, if the caller asked for a different one.
    pub declared_capacity: u64,
    /// Committed rows QUARANTINED by a salvage open: they failed checksum
    /// or padding validation, or duplicated an id already seen, so they
    /// are not served. Always 0 after a strict `Open` (which refuses such
    /// a file instead). Nonzero means the database is running degraded and
    /// should be rebuilt.
    pub quarantined_rows: u64,
}

/// The engine. See module docs for the protocol.
pub struct Engine {
    state: State,
    caps: Capacities,
    /// Superblock generation currently committed. 0 = none yet. The slots
    /// holding a generation are derived from it: pair `g % 2`.
    generation: u64,
    /// Committed row count. Rows `0..row_count` in the arena are live.
    row_count: u64,
    /// The capacity recorded in the file's superblock. Equal to
    /// `caps.rows` for a database this engine created, and possibly
    /// different for one it was handed.
    file_capacity: u64,
    /// The insert currently in flight, if any.
    pending: Option<(u64, [u8; VALUE_LEN])>,
    /// Rows file length reported at open; used to cross-check recovery.
    opened_rows_len: u64,
    /// Checksum-valid rows found beyond the manifest during recovery.
    orphan_valid_rows: u64,
    /// True when those rows cannot all have come from one interrupted
    /// commit. See [`scan_orphans`].
    orphan_rollback: bool,
    /// Recovery scratch: the chosen generation's twin slot needs repair
    /// (pair-relative copy index). Set while reading the superblock,
    /// consumed when staging the recovery fsyncs.
    pending_repair: Option<u8>,
    /// Row-slot arena: one allocation at init, never grown (§4.2).
    arena: Vec<u8>,
    /// Open-addressing primary-key index: slot -> row index + 1 (0 = empty).
    /// Sized to 2x capacity rounded up to a power of two, so load factor is
    /// bounded by 0.5 and probes provably terminate.
    index: Vec<u64>,
    /// Ordered primary-key index (B+tree, fixed node pool) backing range
    /// scans. Derived state: rebuilt from committed rows at every recovery.
    ordered: BTreeIndex,
    /// Trigram index over `value` bytes (docs/DESIGN.md §4.6): derived
    /// state like the btree — rebuilt at every recovery, updated at the
    /// commit point.
    trigram: TrigramIndex,
    /// The delete in flight: the id, and the row slot holding the record
    /// it retires (cleared at the commit point).
    pending_delete: Option<(u64, u64)>,
    /// The update in flight: the id, its new value, and the row slot it
    /// supersedes.
    pending_update: Option<(u64, [u8; VALUE_LEN], u64)>,
    /// The batch in flight: what each staged row will do at the commit
    /// point, in slot order. Empty when no batch is in flight.
    ///
    /// Allocated ONCE at init with room for the largest batch the format
    /// can describe, and only ever cleared and refilled, so a batch — like
    /// everything else here — allocates nothing at run time.
    batch: Vec<BatchEffect>,
    /// Row slots the in-flight batch stages. Not derivable from
    /// `batch.len()` any more: one effect may hold a value spanning
    /// several slots.
    batch_rows: u16,
    /// One bit per row slot: set when that slot holds a LIVE record.
    /// Cleared when a tombstone retires it, and never set for a tombstone
    /// slot itself. Derived state, sized at init like every other arena —
    /// this is what lets deletes work without removing anything from the
    /// indices, which stay append-only and keep pointing at slots whose
    /// liveness is decided here.
    live_bits: Vec<u64>,
    /// Live records — `row_count` counts SLOTS (records + tombstones).
    live_count: u64,
    /// Record slots whose row has since been deleted or superseded: dead
    /// weight a rebuild would compact away.
    retired: u64,
    /// Deletion slots. Also dead weight, and also compacted by a rebuild.
    tombstones: u64,
    /// Continuation slots: the second and later rows of values too long
    /// for one slot. Not records and not deletions, but every slot has to
    /// be accounted for somewhere or the counting invariant is a lie.
    chunks: u64,
    /// Rows a substring search has verified since this engine opened.
    ///
    /// Diagnostic only, and exposed on purpose: "paging a search is
    /// linear in the number of matches" is a property worth testing
    /// directly rather than by wall clock, and this is the work that
    /// would grow if a page ever went back to re-walking the chain.
    find_verifications: core::cell::Cell<u64>,
    /// Head rows whose value spans more than one slot. While this is
    /// nonzero, substring search takes the exhaustive path: the trigram
    /// index only holds single-slot values, so a chain walk would be a
    /// SUPERSET of the answer no longer (see `find_page`).
    long_values: u64,
    /// Salvage mode was requested at open: verification failures quarantine
    /// a row instead of failing the whole database.
    salvage: bool,
    /// Committed rows quarantined by a salvage open (0 in every other mode).
    quarantined: u64,
    /// Negative-space invariant: the allocations must never move. If any
    /// pointer changes, something allocated after init.
    arena_addr: usize,
    index_addr: usize,
    batch_addr: usize,
}

/// What the slots past the manifest add up to.
struct OrphanScan {
    /// Checksum-valid rows anywhere beyond the manifest.
    valid: u64,
    /// True when those rows cannot all have come from ONE commit that was
    /// in flight at the crash.
    rollback_evidence: bool,
}

/// Read the slots past the manifest and work out whether they are the
/// ordinary trace of a commit that was in flight at the crash, or evidence
/// that an acknowledged commit was rolled back by an out-of-budget fault
/// such as a lying fsync.
///
/// The trick is that each row of a commit knows how big its commit was. A
/// commit of `n` rows writes spans `n-1, n-2, ..., 0` into consecutive
/// slots starting at the manifest, so a row found `j` slots past the
/// manifest carrying span `s` is claiming: *I am row `j` of a commit of
/// `j + s + 1` rows.* Every row of one interrupted commit makes the SAME
/// claim, whichever of them survived and whichever did not.
///
/// So the test is agreement. All valid rows past the manifest agreeing on
/// one commit size is exactly what an interrupted commit looks like, and
/// nothing else produces it:
///
/// - An interrupted commit of `n` leaves any subset of its `n` slots (the
///   writes were issued in order but none were fsynced, so the settle can
///   drop any of them, leaving holes). Each survivor still claims `n`.
/// - Two acknowledged single-row commits stranded past the manifest sit at
///   offsets 0 and 1 with span 0, claiming sizes 1 and 2. They disagree,
///   and that disagreement is the loss becoming loud — the same verdict
///   the pre-batch rule ("two or more orphans") reached, reached the same
///   way for the same reason.
/// - A valid row beyond the group's end claims a size larger than the
///   group's, so it cannot hide behind it.
/// - A misdirected write dropping a genuine row with a large span at the
///   head of the run raises the claimed size, but a claim is only believed
///   while every other survivor makes the same one.
///
/// The limit of the method, stated plainly: a rolled-back commit of
/// exactly `n` rows looks the same as an interrupted commit of `n` rows,
/// because it is the same bytes in the same places. Detection begins at
/// the first row that cannot be explained that way — which for single-row
/// commits is the second orphan, exactly as before.
///
/// Walked with `chunks_exact` rather than hand-rolled index arithmetic,
/// deliberately: a manual `off += ROW_SIZE` cursor can be made to stop
/// advancing (mutation testing found exactly that — `off *= ROW_SIZE` with
/// `off == 0` loops forever), and an unbounded loop INSIDE one `tick` is
/// the one stall the fuel watchdog cannot see, because the engine never
/// returns to be counted. An iterator over fixed-size chunks cannot fail
/// to terminate, so the failure mode is structurally absent instead of
/// merely untested.
fn scan_orphans(past_manifest: &[u8]) -> OrphanScan {
    let mut valid = 0u64;
    let mut claimed: Option<u64> = None;
    let mut disagreed = false;
    for (j, chunk) in past_manifest.chunks_exact(ROW_SIZE).enumerate() {
        let Some(slot) = decode_row(chunk) else {
            continue;
        };
        valid += 1;
        let claim = j as u64 + slot.span as u64 + 1;
        match claimed {
            None => claimed = Some(claim),
            Some(first) if first == claim => {}
            Some(_) => disagreed = true,
        }
    }
    OrphanScan {
        valid,
        rollback_evidence: disagreed,
    }
}

impl Engine {
    /// Allocate the arenas for the declared capacities. This is the one and
    /// only allocation point (docs/DESIGN.md §4.2).
    pub fn new(caps: Capacities) -> Self {
        assert!(caps.rows > 0, "capacity must be positive");
        let arena_bytes = (caps.rows as usize)
            .checked_mul(ROW_SIZE)
            .expect("rows capacity overflows arena size");
        let index_len = (caps.rows as usize)
            .checked_mul(2)
            .and_then(|n| n.checked_next_power_of_two())
            .expect("rows capacity overflows index size");
        let arena = vec![0u8; arena_bytes];
        let index = vec![0u64; index_len];
        let ordered = BTreeIndex::new(caps.rows);
        let trigram = TrigramIndex::new(caps.rows);
        // Room for the longest commit the row format can describe, taken
        // once so that pushing effects during validation can never
        // reallocate (asserted in `check_invariants`).
        let batch_staging: Vec<BatchEffect> = Vec::with_capacity(MAX_COMMIT_ROWS);
        let arena_addr = arena.as_ptr() as usize;
        let index_addr = index.as_ptr() as usize;
        let batch_addr = batch_staging.as_ptr() as usize;
        Engine {
            state: State::New,
            caps,
            generation: 0,
            row_count: 0,
            file_capacity: caps.rows,
            pending: None,
            opened_rows_len: 0,
            orphan_valid_rows: 0,
            orphan_rollback: false,
            pending_repair: None,
            pending_delete: None,
            pending_update: None,
            batch: batch_staging,
            batch_rows: 0,
            live_bits: vec![0u64; (caps.rows as usize).div_ceil(64)],
            live_count: 0,
            retired: 0,
            tombstones: 0,
            chunks: 0,
            long_values: 0,
            find_verifications: core::cell::Cell::new(0),
            salvage: false,
            quarantined: 0,
            arena,
            index,
            ordered,
            trigram,
            arena_addr,
            index_addr,
            batch_addr,
        }
    }

    /// Committed rows and configured capacity, so hosts can alarm at 80%
    /// instead of discovering the ceiling by hitting it (docs/DESIGN.md §6).
    pub fn usage(&self) -> (u64, u64) {
        (self.row_count, self.caps.rows)
    }

    /// The capacities this engine was opened with.
    pub fn caps(&self) -> Capacities {
        self.caps
    }

    /// Committed superblock generation (0 before open completes).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What recovery found, beyond the row count. Hosts SHOULD check
    /// `rollback_evidence` after every open and alarm on it: it means
    /// acknowledged commits were rolled back by a fault outside the declared
    /// budget (e.g. a lying fsync) and the on-disk evidence survived to
    /// prove it. The recovered data itself is still exactly correct — an
    /// in-order prefix — but newer commits existed and are gone.
    pub fn recovery_report(&self) -> RecoveryReport {
        RecoveryReport {
            row_count: self.row_count,
            declared_capacity: self.file_capacity,
            orphan_valid_rows: self.orphan_valid_rows,
            rollback_evidence: self.orphan_rollback,
            quarantined_rows: self.quarantined,
        }
    }

    /// True when this database is open in salvage mode WITH quarantined
    /// rows: read-only, serving verified rows only, refusing every question
    /// it cannot answer honestly. Rebuild to clear it.
    pub fn is_degraded(&self) -> bool {
        matches!(self.state, State::Degraded)
    }

    /// Committed rows quarantined by a salvage open — damaged slots that
    /// exist in the manifest but cannot be verified, so are never served.
    pub fn quarantined(&self) -> u64 {
        self.quarantined
    }

    /// Rows verified by substring search since open. See the field.
    pub fn find_verifications(&self) -> u64 {
        self.find_verifications.get()
    }

    /// The readable values, ascending by row. The basis of a rebuild:
    /// exactly the rows a clean database should contain, each with its
    /// WHOLE value — a rebuild that copied only a long value's head row
    /// would write the truncation it was meant to repair.
    pub fn live_rows(&self) -> impl Iterator<Item = (u64, Vec<u8>)> + '_ {
        (0..self.row_count).filter_map(move |row| {
            let off = (row as usize) * ROW_SIZE;
            if !self.is_live(row) {
                return None;
            }
            let (id, _) = decode_row(&self.arena[off..off + ROW_SIZE])?.record()?;
            // Quarantined slots were never copied into the arena, so they
            // hold zeros — which `decode_row` rejects, since the checksum
            // of 24 zero bytes is not zero. The index check is the belt to
            // that braces: a slot is live only if the index agrees THIS row
            // is where that id lives.
            if self.index_lookup(id) != Some(row) {
                return None;
            }
            let (rows, _) = self.value_extent(row);
            let mut buf = [0u8; MAX_VALUE_LEN];
            let len = self.assemble_from_arena(row, rows, &mut buf);
            Some((id, buf[..len].to_vec()))
        })
    }

    /// Advance the state machine by one input.
    pub fn tick(&mut self, input: Input<'_>) -> Output {
        self.assert_invariants();
        let out = self.tick_inner(input);
        self.assert_invariants();
        out
    }

    /// Live records — distinct from `usage()`, which counts SLOTS. A
    /// deletion consumes a slot (it is appended, never overwritten), so
    /// these diverge as soon as anything is deleted; a rebuild compacts.
    pub fn live_count(&self) -> u64 {
        self.live_count
    }

    /// Slots that hold neither a live record nor useful history: retired
    /// records plus the deletions that retired them. This is the dead
    /// weight a rebuild (`dabqlite-inspect --repair-to`) compacts away,
    /// and the number a host should watch to decide when to do it.
    pub fn dead_slots(&self) -> u64 {
        self.retired + self.tombstones
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(
            self.arena.as_ptr() as usize,
            self.arena_addr,
            "arena moved: allocation after init is forbidden"
        );
        debug_assert_eq!(
            self.index.as_ptr() as usize,
            self.index_addr,
            "index moved: allocation after init is forbidden"
        );
        debug_assert_eq!(
            self.batch.as_ptr() as usize,
            self.batch_addr,
            "batch staging moved: a batch longer than the format allows got past validation"
        );
        debug_assert!(self.batch.len() <= MAX_COMMIT_ROWS);
        debug_assert!(self.row_count <= self.caps.rows);
        // The ordered index is derived state over exactly the committed
        // rows — outside recovery, where it is rebuilt before `row_count`
        // is published at finish_open.
        if matches!(
            self.state,
            State::Ready
                | State::InsertWriteRow
                | State::InsertFsyncRows
                | State::InsertWriteSb { .. }
                | State::InsertFsyncSb
                | State::BatchWriteRow { .. }
                | State::BatchFsyncRows
                | State::BatchWriteSb { .. }
                | State::BatchFsyncSb
        ) {
            // Every SLOT is accounted for in the trigram cursor, records
            // indexed and tombstones skipped, so row numbers stay true.
            debug_assert_eq!(self.trigram.len(), self.row_count);
            // The ordered tree holds one entry per distinct id ever
            // inserted: at least the live ones, at most one per slot.
            debug_assert!(self.live_count <= self.ordered.len());
            debug_assert!(self.ordered.len() <= self.row_count);
            // Slots are records, deletions and continuations, exactly.
            debug_assert_eq!(
                self.live_count + self.retired + self.tombstones + self.chunks,
                self.row_count
            );
        }
        // In salvage mode the manifest still counts the damaged slots, so
        // the indices are short by exactly the quarantine.
        if matches!(self.state, State::Degraded) {
            // The ordered index holds only rows that verified...
            debug_assert!(self.ordered.len() + self.quarantined <= self.row_count);
            // ...while the trigram's cursor accounts for every slot,
            // indexed or skipped, so row numbers stay true.
            debug_assert_eq!(self.trigram.len(), self.row_count);
        }
        // Pending insert exists exactly in the insert-in-flight states.
        let inserting = matches!(
            self.state,
            State::InsertWriteRow
                | State::InsertFsyncRows
                | State::InsertWriteSb { .. }
                | State::InsertFsyncSb
        );
        debug_assert_eq!(self.pending.is_some(), inserting);
        let updating = matches!(
            self.state,
            State::UpdateWriteRow
                | State::UpdateFsyncRows
                | State::UpdateWriteSb { .. }
                | State::UpdateFsyncSb
        );
        debug_assert_eq!(self.pending_update.is_some(), updating);
        let deleting = matches!(
            self.state,
            State::DeleteWriteRow
                | State::DeleteFsyncRows
                | State::DeleteWriteSb { .. }
                | State::DeleteFsyncSb
        );
        debug_assert_eq!(self.pending_delete.is_some(), deleting);
        let batching = matches!(
            self.state,
            State::BatchWriteRow { .. }
                | State::BatchFsyncRows
                | State::BatchWriteSb { .. }
                | State::BatchFsyncSb
        );
        // Staged effects exist exactly while a batch is in flight, and the
        // engine forgets them the moment the batch commits or fails — a
        // leftover effect would be applied twice by the next batch.
        debug_assert_eq!(!self.batch.is_empty(), batching);
        // Never more than one write in flight: v1 serializes all access.
        debug_assert!(
            u8::from(inserting) + u8::from(deleting) + u8::from(updating) + u8::from(batching) <= 1
        );
        // Live records are a subset of the slots, and of the keys the
        // ordered index holds (one per distinct id ever inserted). Only
        // meaningful once open has published `row_count`: during recovery
        // the replay is still counting.
        if matches!(
            self.state,
            State::Ready
                | State::Degraded
                | State::InsertWriteRow
                | State::InsertFsyncRows
                | State::InsertWriteSb { .. }
                | State::InsertFsyncSb
                | State::DeleteWriteRow
                | State::DeleteFsyncRows
                | State::DeleteWriteSb { .. }
                | State::DeleteFsyncSb
                | State::UpdateWriteRow
                | State::UpdateFsyncRows
                | State::UpdateWriteSb { .. }
                | State::UpdateFsyncSb
                | State::BatchWriteRow { .. }
                | State::BatchFsyncRows
                | State::BatchWriteSb { .. }
                | State::BatchFsyncSb
        ) {
            debug_assert!(self.live_count <= self.row_count);
            debug_assert!(self.live_count <= self.ordered.len());
        }
    }

    fn tick_inner(&mut self, input: Input<'_>) -> Output {
        match input {
            Input::Open {
                superblock_len,
                rows_len,
            } => self.on_open(superblock_len, rows_len),
            Input::OpenSalvage {
                superblock_len,
                rows_len,
            } => {
                self.salvage = true;
                self.on_open(superblock_len, rows_len)
            }
            Input::ReadDone { file, data } => self.on_read_done(file, data),
            Input::WriteDone { file } => self.on_write_done(file),
            Input::FsyncDone { file } => self.on_fsync_done(file),
            Input::TruncateDone { file } => self.on_truncate_done(file),
            Input::IoFailed { file } => self.on_io_failed(file),
            Input::Insert { id, value } => self.on_insert(id, value),
            Input::Update { id, value } => self.on_update(id, value),
            Input::Delete { id } => self.on_delete(id),
            Input::Batch { ops } => self.on_batch(ops),
            Input::Get { id } => self.on_get(id),
            Input::GetFrom { id, offset } => self.read_window(id, offset),
            Input::Range { lo, hi } => self.on_range(lo, hi),
            Input::Find {
                needle,
                needle_len,
                after,
            } => self.on_find(needle, needle_len, after),
        }
    }

    // ---- open & recovery -------------------------------------------------

    fn on_open(&mut self, superblock_len: u64, rows_len: u64) -> Output {
        assert!(
            self.state == State::New,
            "protocol violation: Open on an already-opened engine"
        );
        self.opened_rows_len = rows_len;
        if superblock_len == 0 {
            if rows_len != 0 {
                // Negative space: row data can only exist after an initial
                // superblock was durably written. Refuse rather than wipe.
                return self.fail_open(DbError::Corrupt {
                    what: "rows file present without any superblock",
                });
            }
            self.stage_initial_superblock()
        } else {
            self.state = State::RecoverReadSb;
            Output::Read {
                file: FileId::Superblock,
                offset: 0,
                len: SB_ZONE_SIZE as u64,
            }
        }
    }

    /// The two slots holding generation `g`: pair `g % 2`.
    pub(crate) fn sb_slots_for(generation: u64) -> [u8; 2] {
        debug_assert!(generation > 0);
        debug_assert_eq!(SB_COPIES, 4, "pair rotation assumes 4 slots");
        let pair = (generation % 2) as u8;
        [pair * 2, pair * 2 + 1]
    }

    /// Build the write request for copy `copy` (0 or 1) of a generation.
    ///
    /// `capacity` rides along in every copy so that reopening a database
    /// does not have to be told how big it was declared.
    pub(crate) fn sb_copy_write(
        generation: u64,
        row_count: u64,
        capacity: u64,
        copy: u8,
    ) -> Output {
        debug_assert!(copy < 2);
        let mut bytes = [0u8; SB_COPY_SIZE];
        encode_sb(generation, row_count, capacity, &mut bytes);
        let slot = Self::sb_slots_for(generation)[copy as usize];
        Output::Write {
            file: FileId::Superblock,
            offset: slot as u64 * SB_COPY_SIZE as u64,
            data: WriteBuf::from_slice(&bytes),
        }
    }

    fn stage_initial_superblock(&mut self) -> Output {
        self.state = State::InitWriteSb { copy: 0 };
        Self::sb_copy_write(1, 0, self.caps.rows, 0)
    }

    fn on_read_done(&mut self, file: FileId, data: &[u8]) -> Output {
        match (self.state, file) {
            (State::RecoverReadSb, FileId::Superblock) => self.recover_from_sb(data),
            (
                State::RecoverReadRows {
                    generation,
                    row_count,
                },
                FileId::Rows,
            ) => self.recover_from_rows(generation, row_count, data),
            (state, file) => {
                panic!("protocol violation: ReadDone({file:?}) in state {state:?}")
            }
        }
    }

    fn recover_from_sb(&mut self, data: &[u8]) -> Output {
        // Read all copies, take the highest generation with a valid
        // checksum (docs/DESIGN.md §4.4).
        let mut best: Option<(u8, crate::layout::SbCopy)> = None;
        let mut schema_mismatch: Option<u64> = None;
        for slot in 0..SB_COPIES {
            let Some(chunk) = data.get(slot * SB_COPY_SIZE..(slot + 1) * SB_COPY_SIZE) else {
                break; // short file: remaining slots were never written
            };
            match decode_sb(chunk) {
                Ok(copy) => {
                    // A copy's slot position is part of its validity: the
                    // engine only ever writes generation g to pair g % 2, so
                    // a checksum-valid copy in a foreign slot is the product
                    // of a misdirected write. Distrust it. (Found by the
                    // misdirected-write sweep: this was an assert, which
                    // turned a survivable firmware fault into a panic.)
                    if !Self::sb_slots_for(copy.generation).contains(&(slot as u8)) {
                        continue;
                    }
                    if best.is_none_or(|(_, b)| copy.generation > b.generation) {
                        best = Some((slot as u8, copy));
                    }
                }
                Err(SbDecodeError::SchemaMismatch { file_schema }) => {
                    schema_mismatch = Some(file_schema);
                }
                Err(SbDecodeError::Invalid) => {}
            }
        }

        // Twin-repair decision, made from THIS read: for the chosen
        // generation's home pair, any slot that does not already hold a
        // canonical copy of exactly (generation, row_count) gets rewritten
        // before the recovery fsync — but the slot the generation was
        // recovered FROM is valid by construction and is NEVER touched.
        self.pending_repair = None;
        if let Some((_, chosen)) = best {
            let slots = Self::sb_slots_for(chosen.generation);
            for (pair_idx, &slot) in slots.iter().enumerate() {
                let healthy = data
                    .get(slot as usize * SB_COPY_SIZE..(slot as usize + 1) * SB_COPY_SIZE)
                    .and_then(|chunk| decode_sb(chunk).ok())
                    .is_some_and(|c| {
                        c.generation == chosen.generation && c.row_count == chosen.row_count
                    });
                if !healthy {
                    self.pending_repair = Some(pair_idx as u8);
                }
            }
        }

        let Some((_slot, copy)) = best else {
            if let Some(file_schema) = schema_mismatch {
                return self.fail_open(DbError::SchemaMismatch {
                    file_schema,
                    binary_schema: SCHEMA_HASH,
                });
            }
            if self.opened_rows_len > 0 {
                // Negative space: by protocol order, committed rows imply at
                // least one valid superblock copy. Refuse rather than wipe.
                return self.fail_open(DbError::Corrupt {
                    what: "no valid superblock copy but rows file is non-empty",
                });
            }
            // Only reachable when a crash tore the very first superblock
            // write before anything was committed. Re-run initialization.
            return self.stage_initial_superblock();
        };

        // What the file says it was created with. Kept whatever this
        // engine's own capacity is, so a caller can see that it opened a
        // database smaller (or larger) than the one that was written.
        self.file_capacity = copy.capacity;
        if copy.row_count > self.caps.rows {
            return self.fail_open(DbError::CapacityBelowData {
                required: copy.row_count,
                configured: self.caps.rows,
            });
        }
        // The commit protocol fsyncs row slots before the superblock that
        // references them, so committed rows must all be on disk.
        if copy
            .row_count
            .checked_mul(ROW_SIZE as u64)
            .expect("checked at open")
            > self.opened_rows_len
        {
            return self.fail_open(DbError::Corrupt {
                what: "superblock references rows beyond the rows file",
            });
        }

        // Read the committed rows AND everything beyond them, up to the
        // configured capacity: bytes past the manifest are scanned for
        // rollback evidence (valid rows the superblock no longer
        // references). Bounded by the arena capacity, so the read cannot
        // exceed what the engine could ever have written.
        let scan_len = self.opened_rows_len.min(self.caps.rows * ROW_SIZE as u64);
        if scan_len == 0 {
            debug_assert_eq!(copy.row_count, 0, "checked against rows_len above");
            self.stage_recovery_fsyncs(copy.generation, 0)
        } else {
            self.state = State::RecoverReadRows {
                generation: copy.generation,
                row_count: copy.row_count,
            };
            Output::Read {
                file: FileId::Rows,
                offset: 0,
                len: scan_len,
            }
        }
    }

    /// Everything recovery is about to make visible must be durable first
    /// (see `State::RecoverFsyncRows`). Fsync rows, then superblock, then
    /// report OpenDone.
    fn stage_recovery_fsyncs(&mut self, generation: u64, row_count: u64) -> Output {
        let live = row_count * ROW_SIZE as u64;
        if self.opened_rows_len > live {
            self.state = State::RecoverTruncateRows {
                generation,
                row_count,
            };
            return Output::Truncate {
                file: FileId::Rows,
                len: live,
            };
        }
        self.state = State::RecoverFsyncRows {
            generation,
            row_count,
        };
        Output::Fsync { file: FileId::Rows }
    }

    fn on_truncate_done(&mut self, file: FileId) -> Output {
        match (self.state, file) {
            (
                State::RecoverTruncateRows {
                    generation,
                    row_count,
                },
                FileId::Rows,
            ) => {
                // The residue is gone; make that, and the rows, durable.
                self.opened_rows_len = row_count * ROW_SIZE as u64;
                self.state = State::RecoverFsyncRows {
                    generation,
                    row_count,
                };
                Output::Fsync { file: FileId::Rows }
            }
            (state, file) => {
                panic!("protocol violation: TruncateDone({file:?}) in state {state:?}")
            }
        }
    }

    fn recover_from_rows(&mut self, generation: u64, row_count: u64, data: &[u8]) -> Output {
        let live = (row_count as usize) * ROW_SIZE;
        if data.len() < live {
            return self.fail_open(DbError::Corrupt {
                what: "short read of committed rows",
            });
        }
        // Verification is per-row, and so is the consequence. A strict open
        // refuses the whole file (detection over availability); a salvage
        // open quarantines just the damaged slot, so one bad row costs one
        // row. Either way a row is only ever SERVED after it verifies —
        // "never wrong" is not traded away for availability.
        self.quarantined = 0;
        self.live_count = 0;
        self.retired = 0;
        self.tombstones = 0;
        self.chunks = 0;
        self.long_values = 0;
        self.live_bits.fill(0);
        // Rows already consumed as part of a value's run. A value is read
        // whole, at its head, so its continuations are not visited again.
        let mut skip_until = 0u64;
        // The rows file IS the commit order — one run of slots appended
        // per commit, records, deletions and continuations alike — so
        // replaying it in order replays history exactly. An id may be
        // inserted, deleted, and inserted again; the last word wins
        // because it is last.
        for row in 0..row_count {
            if row < skip_until {
                continue;
            }
            let off = (row as usize) * ROW_SIZE;
            let chunk = &data[off..off + ROW_SIZE];
            // Pair assertion (docs/DESIGN.md §7.4): rows were verified when
            // encoded on the write path; verify again reading them back.
            let Some(slot) = decode_row(chunk) else {
                if self.salvage {
                    self.quarantined += 1;
                    // Account for the slot without indexing it, so every
                    // later row keeps its true row number.
                    self.trigram.skip_row(row);
                    continue;
                }
                return self.fail_open(DbError::Corrupt {
                    what: crate::defect::ROW_CHECKSUM,
                });
            };
            let id = slot.id;
            match slot.kind {
                RowKind::Record | RowKind::Update => {
                    // The value's whole run has to be read before any of it
                    // can be trusted: a value is only ever written whole,
                    // so a run that does not end where it promised is not
                    // one this engine wrote, and serving its head alone
                    // would be serving a value truncated.
                    let extent = self.verify_run(data, row, row_count, &slot);
                    let Some(rows) = extent else {
                        if self.salvage {
                            // The head and every continuation that was
                            // supposed to follow it are unreadable together.
                            let broken = self.broken_run_len(data, row, row_count, &slot);
                            for r in row..row + broken {
                                self.quarantined += 1;
                                self.trigram.skip_row(r);
                            }
                            skip_until = row + broken;
                            continue;
                        }
                        return self.fail_open(DbError::Corrupt {
                            what: crate::defect::TRUNCATED_VALUE,
                        });
                    };
                    let duplicate = slot.kind == RowKind::Record && self.live_row_of(id).is_some();
                    let orphan = slot.kind == RowKind::Update && self.live_row_of(id).is_none();
                    if duplicate || orphan {
                        if self.salvage {
                            // Keep the first occurrence; the later
                            // duplicate is the damaged one as far as
                            // anyone can tell. Either way the whole value
                            // goes, not just its head.
                            for r in row..row + rows {
                                self.quarantined += 1;
                                self.trigram.skip_row(r);
                            }
                            skip_until = row + rows;
                            continue;
                        }
                        return self.fail_open(DbError::Corrupt {
                            what: if duplicate {
                                crate::defect::DUPLICATE_ID
                            } else {
                                crate::defect::ORPHAN_UPDATE
                            },
                        });
                    }
                    if let Some(old_row) = self.live_row_of(id) {
                        self.set_live(old_row, false);
                        self.retired += 1;
                    } else {
                        self.live_count += 1;
                    }
                    for r in row..row + rows {
                        let o = (r as usize) * ROW_SIZE;
                        self.arena[o..o + ROW_SIZE].copy_from_slice(&data[o..o + ROW_SIZE]);
                    }
                    self.bind_indices(id, row);
                    self.set_live(row, true);
                    self.chunks += rows - 1;
                    if rows > 1 {
                        self.long_values += 1;
                    }
                    let mut value = [0u8; MAX_VALUE_LEN];
                    let len = self.assemble_from_arena(row, rows, &mut value);
                    self.trigram.insert_value(row, rows, &value[..len]);
                    skip_until = row + rows;
                }
                RowKind::Chunk => {
                    // Reached only when nothing in front of it claimed it:
                    // every well-formed continuation is consumed with its
                    // head, above.
                    if self.salvage {
                        self.quarantined += 1;
                        self.trigram.skip_row(row);
                        continue;
                    }
                    return self.fail_open(DbError::Corrupt {
                        what: crate::defect::ORPHAN_CHUNK,
                    });
                }
                RowKind::Tombstone => {
                    // A deletion of something not live cannot be produced by
                    // the engine (it refuses `NotFound` before any I/O), so
                    // it is evidence of damage or of a file we did not write.
                    // Neither can a deletion that claims to continue.
                    let record_row = self.live_row_of(id).filter(|_| !slot.more);
                    let Some(record_row) = record_row else {
                        if self.salvage {
                            self.quarantined += 1;
                            self.trigram.skip_row(row);
                            continue;
                        }
                        return self.fail_open(DbError::Corrupt {
                            what: crate::defect::ORPHAN_TOMBSTONE,
                        });
                    };
                    let off = (row as usize) * ROW_SIZE;
                    self.arena[off..off + ROW_SIZE].copy_from_slice(&data[off..off + ROW_SIZE]);
                    self.set_live(record_row, false);
                    self.live_count -= 1;
                    self.retired += 1;
                    self.tombstones += 1;
                    // Accounted for, never indexed: a deletion is not
                    // searchable content.
                    self.trigram.skip_row(row);
                }
            }
        }
        let scan = scan_orphans(&data[live..]);
        self.orphan_valid_rows = scan.valid;
        self.orphan_rollback = scan.rollback_evidence;

        // Salvage touches NOTHING. A damaged database must not be mutated
        // by the act of rescuing data from it, and a rescue must work on a
        // volume that is read-only or failing its writes — so a degraded
        // open performs no repair write AND no fsync. It reads, verifies,
        // and reports.
        //
        // The visible-implies-durable rule (see `State::RecoverFsyncRows`)
        // is not weakened: that rule exists so state shown to an
        // application cannot evaporate underneath later writes built on
        // it, and a degraded database accepts no writes at all. A salvage
        // open that finds NO damage is an ordinary open and keeps both the
        // twin repair and the fsyncs.
        if self.quarantined > 0 {
            self.pending_repair = None;
            return self.finish_open(generation, row_count);
        }
        self.stage_recovery_fsyncs(generation, row_count)
    }

    fn finish_open(&mut self, generation: u64, row_count: u64) -> Output {
        assert!(generation > 0, "committed generation must be positive");
        self.generation = generation;
        self.row_count = row_count;
        // A salvage open that found nothing wrong IS a normal open — the
        // mode is a fallback, not a downgrade.
        self.state = if self.quarantined > 0 {
            State::Degraded
        } else {
            State::Ready
        };
        Output::OpenDone {
            result: Ok(self.live_count),
        }
    }

    fn fail_open(&mut self, err: DbError) -> Output {
        self.state = State::Failed(err);
        Output::OpenDone { result: Err(err) }
    }

    // ---- insert ----------------------------------------------------------

    /// Does row slot `row` hold a live record?
    fn is_live(&self, row: u64) -> bool {
        let (word, bit) = ((row / 64) as usize, row % 64);
        self.live_bits
            .get(word)
            .is_some_and(|w| w & (1u64 << bit) != 0)
    }

    fn set_live(&mut self, row: u64, live: bool) {
        let (word, bit) = ((row / 64) as usize, row % 64);
        let mask = 1u64 << bit;
        if live {
            self.live_bits[word] |= mask;
        } else {
            self.live_bits[word] &= !mask;
        }
    }

    /// The slot holding `id`'s LIVE record, if it has one.
    ///
    /// The indices are append-only and keep pointing at a slot after its
    /// record is retired; liveness is decided here, in one place, so no
    /// read path can forget to ask.
    fn live_row_of(&self, id: u64) -> Option<u64> {
        let row = self.index_lookup(id)?;
        self.is_live(row).then_some(row)
    }

    fn on_update(&mut self, id: u64, value: [u8; VALUE_LEN]) -> Output {
        let err = self.write_gate();
        if let Some(e) = err {
            return Output::UpdateDone { id, result: Err(e) };
        }
        let Some(old_row) = self.live_row_of(id) else {
            return Output::UpdateDone {
                id,
                result: Err(DbError::NotFound { id }),
            };
        };
        if self.row_count == self.caps.rows {
            return Output::UpdateDone {
                id,
                result: Err(DbError::Full {
                    entity: "records",
                    capacity: self.caps.rows,
                    dead: self.dead_slots(),
                }),
            };
        }

        let off = (self.row_count as usize) * ROW_SIZE;
        let slot: &mut [u8; ROW_SIZE] = (&mut self.arena[off..off + ROW_SIZE])
            .try_into()
            .expect("fixed slice");
        // Span 0: a single-row commit, the only kind these three paths
        // make. `Input::Batch` is where a span above 0 comes from.
        encode_row(RowKind::Update, 0, VALUE_LEN as u8, false, id, &value, slot);
        self.pending_update = Some((id, value, old_row));
        self.state = State::UpdateWriteRow;
        Output::Write {
            file: FileId::Rows,
            offset: off as u64,
            data: WriteBuf::from_slice(&self.arena[off..off + ROW_SIZE]),
        }
    }

    fn on_delete(&mut self, id: u64) -> Output {
        let err = self.write_gate();
        if let Some(e) = err {
            return Output::DeleteDone { id, result: Err(e) };
        }
        let Some(record_row) = self.live_row_of(id) else {
            return Output::DeleteDone {
                id,
                result: Err(DbError::NotFound { id }),
            };
        };
        if self.row_count == self.caps.rows {
            // A tombstone needs a slot like any other row, so a database at
            // its ceiling cannot record a deletion. Refused before any I/O,
            // naming the ceiling; a rebuild compacts and frees the space.
            return Output::DeleteDone {
                id,
                result: Err(DbError::Full {
                    entity: "records",
                    capacity: self.caps.rows,
                    dead: self.dead_slots(),
                }),
            };
        }

        // Stage the tombstone in its own slot. Like an insert, it becomes
        // visible only when the superblock generation flips.
        let off = (self.row_count as usize) * ROW_SIZE;
        let slot: &mut [u8; ROW_SIZE] = (&mut self.arena[off..off + ROW_SIZE])
            .try_into()
            .expect("fixed slice");
        // A tombstone carries no payload at all: len 0, so every byte of
        // its value field is padding the decoder will never hand back.
        encode_row(RowKind::Tombstone, 0, 0, false, id, &[0u8; VALUE_LEN], slot);
        self.pending_delete = Some((id, record_row));
        self.state = State::DeleteWriteRow;
        Output::Write {
            file: FileId::Rows,
            offset: off as u64,
            data: WriteBuf::from_slice(&self.arena[off..off + ROW_SIZE]),
        }
    }

    fn on_insert(&mut self, id: u64, value: [u8; VALUE_LEN]) -> Output {
        let err = self.write_gate();
        if let Some(e) = err {
            return Output::InsertDone { id, result: Err(e) };
        }
        // A DELETED id may be inserted again: the index still points at the
        // retired slot, but that slot is no longer live, so the id is free.
        if self.live_row_of(id).is_some() {
            return Output::InsertDone {
                id,
                result: Err(DbError::DuplicateId { id }),
            };
        }
        if self.row_count == self.caps.rows {
            // First-class capacity exhaustion (docs/DESIGN.md §6). Rejected
            // before any I/O: nothing is partially applied.
            return Output::InsertDone {
                id,
                result: Err(DbError::Full {
                    entity: "records",
                    capacity: self.caps.rows,
                    dead: self.dead_slots(),
                }),
            };
        }

        // Stage the row in its arena slot. It becomes visible only when the
        // superblock generation flips.
        let off = (self.row_count as usize) * ROW_SIZE;
        let slot: &mut [u8; ROW_SIZE] = (&mut self.arena[off..off + ROW_SIZE])
            .try_into()
            .expect("fixed slice");
        encode_row(RowKind::Record, 0, VALUE_LEN as u8, false, id, &value, slot);
        self.pending = Some((id, value));
        self.state = State::InsertWriteRow;
        Output::Write {
            file: FileId::Rows,
            offset: off as u64,
            data: WriteBuf::from_slice(&self.arena[off..off + ROW_SIZE]),
        }
    }

    // ---- batch: several writes, one commit ---------------------------

    /// The state gate every client WRITE shares. Reads have their own (a
    /// degraded database still answers hits); writes are refused in every
    /// state but `Ready`, and this is the single place that says so, so a
    /// new write operation cannot accidentally be laxer than the others.
    fn write_gate(&self) -> Option<DbError> {
        match self.state {
            State::Ready => None,
            State::New
            | State::InitWriteSb { .. }
            | State::InitFsyncSb
            | State::RecoverReadSb
            | State::RecoverReadRows { .. }
            | State::RecoverTruncateRows { .. }
            | State::RecoverFsyncRows { .. }
            | State::RecoverRepairSb { .. }
            | State::RecoverFsyncSb { .. } => Some(DbError::NotOpen),
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb
            | State::UpdateWriteRow
            | State::UpdateFsyncRows
            | State::UpdateWriteSb { .. }
            | State::UpdateFsyncSb
            | State::DeleteWriteRow
            | State::DeleteFsyncRows
            | State::DeleteWriteSb { .. }
            | State::DeleteFsyncSb
            | State::BatchWriteRow { .. }
            | State::BatchFsyncRows
            | State::BatchWriteSb { .. }
            | State::BatchFsyncSb => Some(DbError::Busy),
            // Salvage is strictly read-only: appending to a file we know is
            // damaged, and flipping the manifest over it, could only make a
            // recoverable situation worse.
            State::Degraded => Some(DbError::Degraded {
                quarantined: self.quarantined,
            }),
            State::Failed(e) => Some(e),
        }
    }

    /// Where `id`'s live record sits partway through validating a batch:
    /// what the committed state says, amended by the batch's own earlier
    /// operations.
    ///
    /// This is what makes a batch mean what it reads as. `insert 5; delete
    /// 5; insert 5` has to be legal — the second insert sees an id the
    /// batch itself freed — while `insert 5; insert 5` has to be a
    /// duplicate. Validating each op against the committed state alone
    /// would get both backwards.
    fn projected_live_row(&self, staged: usize, id: u64) -> Option<u64> {
        for i in (0..staged).rev() {
            let effect = self.batch[i];
            if effect.id() == id {
                return effect.leaves_live().then_some(effect.at());
            }
        }
        self.live_row_of(id)
    }

    fn on_batch(&mut self, ops: &[BatchOp]) -> Output {
        if let Some(error) = self.write_gate() {
            return Output::BatchDone {
                rows: 0,
                result: Err(BatchReject { at: 0, error }),
            };
        }
        // An empty batch commits nothing. There is no generation to flip
        // and nothing to make durable, so it performs no I/O and succeeds
        // — refusing it would be inventing a failure out of a no-op.
        if ops.is_empty() {
            return Output::BatchDone {
                rows: 0,
                result: Ok(()),
            };
        }
        if ops.len() > MAX_COMMIT_ROWS {
            // The span byte is what lets recovery recognize a commit group;
            // a batch longer than it can describe would be a commit the
            // engine could write but not recognize afterwards. Refused
            // before any I/O, like every other capacity limit.
            return Output::BatchDone {
                rows: 0,
                result: Err(BatchReject {
                    at: MAX_COMMIT_ROWS as u16,
                    error: DbError::BatchTooLong {
                        rows: ops.len() as u64,
                        max: MAX_COMMIT_ROWS as u64,
                    },
                }),
            };
        }

        // Validate the WHOLE batch first, against the projected state, and
        // refuse it entire if any op cannot proceed. Nothing is written
        // until every op is known to be legal, so a rejected batch leaves
        // no partial work behind and needs no unwinding.
        let base = self.row_count;
        self.batch.clear();
        let mut staged_rows = 0u16;
        // One slice per STAGED effect, so the staging pass below can find
        // each value's bytes without re-deciding which ops staged
        // anything. A fixed-size stack array, not a `Vec`: the engine
        // allocates once, at init, and never again.
        let mut payloads: [&[u8]; MAX_COMMIT_ROWS] = [&[]; MAX_COMMIT_ROWS];
        for (i, op) in ops.iter().enumerate() {
            let reject = |at: usize, error: DbError| Output::BatchDone {
                rows: 0,
                result: Err(BatchReject {
                    at: at as u16,
                    error,
                }),
            };
            // Slots are consumed by STAGED rows, not by ops: a `Remove` of
            // an absent id stages nothing, and a long value stages several.
            let row = base + staged_rows as u64;
            let bytes = match *op {
                BatchOp::Insert { value, .. }
                | BatchOp::Update { value, .. }
                | BatchOp::Put { value, .. } => value,
                BatchOp::Delete { .. } | BatchOp::Remove { .. } => &[][..],
            };
            if bytes.len() > MAX_VALUE_LEN {
                self.batch.clear();
                return reject(
                    i,
                    DbError::ValueTooLong {
                        len: bytes.len() as u32,
                        max: MAX_VALUE_LEN as u32,
                    },
                );
            }
            // A value occupies one slot per row-width of bytes, and at
            // least one slot even when empty: a zero-length value is still
            // a value, and needs a row to say so.
            let need = match *op {
                BatchOp::Delete { .. } | BatchOp::Remove { .. } => 1u16,
                _ => bytes.len().div_ceil(VALUE_LEN).max(1) as u16,
            };
            if row + need as u64 > self.caps.rows {
                self.batch.clear();
                return reject(
                    i,
                    DbError::Full {
                        entity: "records",
                        capacity: self.caps.rows,
                        dead: self.dead_slots(),
                    },
                );
            }
            // The whole commit, not just one value, is bounded by what the
            // span byte can describe.
            if staged_rows as usize + need as usize > MAX_COMMIT_ROWS {
                self.batch.clear();
                return reject(
                    i,
                    DbError::BatchTooLong {
                        rows: staged_rows as u64 + need as u64,
                        max: MAX_COMMIT_ROWS as u64,
                    },
                );
            }
            let effect = match *op {
                BatchOp::Insert { id, .. } => {
                    if self.projected_live_row(self.batch.len(), id).is_some() {
                        self.batch.clear();
                        return reject(i, DbError::DuplicateId { id });
                    }
                    BatchEffect::Store {
                        id,
                        at: row,
                        rows: need,
                        supersedes: None,
                    }
                }
                BatchOp::Update { id, .. } => match self.projected_live_row(self.batch.len(), id) {
                    Some(old_row) => BatchEffect::Store {
                        id,
                        at: row,
                        rows: need,
                        supersedes: Some(old_row),
                    },
                    None => {
                        self.batch.clear();
                        return reject(i, DbError::NotFound { id });
                    }
                },
                BatchOp::Put { id, .. } => BatchEffect::Store {
                    id,
                    at: row,
                    rows: need,
                    supersedes: self.projected_live_row(self.batch.len(), id),
                },
                BatchOp::Delete { id } => match self.projected_live_row(self.batch.len(), id) {
                    Some(record_row) => BatchEffect::Delete {
                        id,
                        at: row,
                        record_row,
                    },
                    None => {
                        self.batch.clear();
                        return reject(i, DbError::NotFound { id });
                    }
                },
                BatchOp::Remove { id } => {
                    match self.projected_live_row(self.batch.len(), id) {
                        Some(record_row) => BatchEffect::Delete {
                            id,
                            at: row,
                            record_row,
                        },
                        // Nothing to remove: stage no row at all. The batch
                        // stays as long as it needs to be and no longer.
                        None => continue,
                    }
                }
            };
            payloads[self.batch.len()] = bytes;
            self.batch.push(effect);
            staged_rows += need;
        }

        // Every op turned out to be a no-op (a batch of `Remove`s for ids
        // that had already gone). There is nothing to make durable, so
        // this is the empty batch again: no I/O, no generation flip.
        if self.batch.is_empty() {
            return Output::BatchDone {
                rows: 0,
                result: Ok(()),
            };
        }

        // Stage every row in its arena slot, carrying the span that tells
        // recovery how many rows travel with it. They become visible only
        // when the superblock generation flips, all at once.
        let mut row = base;
        for (effect, bytes) in self.batch.clone().iter().zip(payloads.iter()) {
            match *effect {
                BatchEffect::Store {
                    id,
                    at,
                    rows,
                    supersedes,
                } => {
                    debug_assert_eq!(at, row, "staging disagrees with validation");
                    let head_kind = if supersedes.is_some() {
                        RowKind::Update
                    } else {
                        RowKind::Record
                    };
                    for k in 0..rows as usize {
                        let from = (k * VALUE_LEN).min(bytes.len());
                        let n = (bytes.len() - from).min(VALUE_LEN);
                        let mut value = [0u8; VALUE_LEN];
                        value[..n].copy_from_slice(&bytes[from..from + n]);
                        let kind = if k == 0 { head_kind } else { RowKind::Chunk };
                        // The last row of the value says so; every earlier
                        // one says the value continues.
                        let more = k + 1 < rows as usize;
                        self.stage_row(row, kind, staged_rows, base, n as u8, more, id, &value);
                        row += 1;
                    }
                }
                BatchEffect::Delete { id, at, .. } => {
                    debug_assert_eq!(at, row, "staging disagrees with validation");
                    // A tombstone carries no payload: len 0, so every byte
                    // of its value field is padding.
                    self.stage_row(
                        row,
                        RowKind::Tombstone,
                        staged_rows,
                        base,
                        0,
                        false,
                        id,
                        &[0; VALUE_LEN],
                    );
                    row += 1;
                }
            }
        }
        debug_assert_eq!(row - base, staged_rows as u64, "staging lost a row");
        self.batch_rows = staged_rows;
        self.state = State::BatchWriteRow { next: 0 };
        self.batch_row_write(0)
    }

    /// Encode one staged row into its arena slot. `total` is the whole
    /// commit's row count, from which the span counts down.
    #[allow(clippy::too_many_arguments)]
    fn stage_row(
        &mut self,
        row: u64,
        kind: RowKind,
        total: u16,
        base: u64,
        len: u8,
        more: bool,
        id: u64,
        value: &[u8; VALUE_LEN],
    ) {
        let span = (total as u64 - 1 - (row - base)) as u8;
        let off = (row as usize) * ROW_SIZE;
        let slot: &mut [u8; ROW_SIZE] = (&mut self.arena[off..off + ROW_SIZE])
            .try_into()
            .expect("fixed slice");
        encode_row(kind, span, len, more, id, value, slot);
    }

    /// The write request for batch-relative row `next`.
    fn batch_row_write(&self, next: u16) -> Output {
        let off = (self.row_count as usize + next as usize) * ROW_SIZE;
        Output::Write {
            file: FileId::Rows,
            offset: off as u64,
            data: WriteBuf::from_slice(&self.arena[off..off + ROW_SIZE]),
        }
    }

    /// Apply every staged effect, in slot order. Called once, at the
    /// commit point, when the superblock flip that makes all of them
    /// visible is already durable.
    ///
    /// Order matters and is the same order the ops were written in: an
    /// effect may retire a slot an earlier effect in the same batch
    /// created (`insert 5; delete 5`), which only comes out right if the
    /// creation is applied first.
    fn apply_batch(&mut self) {
        let base = self.row_count;
        let mut row = base;
        for i in 0..self.batch.len() {
            match self.batch[i] {
                BatchEffect::Store {
                    id,
                    at,
                    rows,
                    supersedes,
                } => {
                    debug_assert_eq!(at, row, "commit disagrees with validation");
                    if let Some(old_row) = supersedes {
                        self.set_live(old_row, false);
                        self.retired += 1;
                    } else {
                        self.live_count += 1;
                    }
                    self.bind_indices(id, row);
                    self.set_live(row, true);
                    let mut value = [0u8; MAX_VALUE_LEN];
                    let len = self.assemble_from_arena(row, rows as u64, &mut value);
                    self.trigram.insert_value(row, rows as u64, &value[..len]);
                    if rows > 1 {
                        self.long_values += 1;
                    }
                    // Continuations are slots like any other and have to
                    // be counted, or the accounting invariant is a lie.
                    self.chunks += rows as u64 - 1;
                    row += rows as u64;
                }
                BatchEffect::Delete { at, record_row, .. } => {
                    debug_assert_eq!(at, row, "commit disagrees with validation");
                    self.set_live(record_row, false);
                    // Accounted for, never indexed: a deletion is not
                    // searchable content.
                    self.trigram.skip_row(row);
                    self.live_count -= 1;
                    self.retired += 1;
                    self.tombstones += 1;
                    row += 1;
                }
            }
        }
        debug_assert_eq!(row - base, self.batch_rows as u64, "commit lost a row");
        self.row_count += self.batch_rows as u64;
        self.batch.clear();
        self.batch_rows = 0;
    }

    fn on_write_done(&mut self, file: FileId) -> Output {
        match (self.state, file) {
            (State::InitWriteSb { copy: 0 }, FileId::Superblock) => {
                self.state = State::InitWriteSb { copy: 1 };
                Self::sb_copy_write(1, 0, self.caps.rows, 1)
            }
            (State::InitWriteSb { copy: 1 }, FileId::Superblock) => {
                self.state = State::InitFsyncSb;
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (
                State::RecoverRepairSb {
                    generation,
                    row_count,
                    ..
                },
                FileId::Superblock,
            ) => {
                self.state = State::RecoverFsyncSb {
                    generation,
                    row_count,
                };
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (State::UpdateWriteRow, FileId::Rows) => {
                self.state = State::UpdateFsyncRows;
                Output::Fsync { file: FileId::Rows }
            }
            (State::UpdateWriteSb { copy: 0 }, FileId::Superblock) => {
                self.state = State::UpdateWriteSb { copy: 1 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, self.caps.rows, 1)
            }
            (State::UpdateWriteSb { copy: 1 }, FileId::Superblock) => {
                self.state = State::UpdateFsyncSb;
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (State::DeleteWriteRow, FileId::Rows) => {
                self.state = State::DeleteFsyncRows;
                Output::Fsync { file: FileId::Rows }
            }
            (State::DeleteWriteSb { copy: 0 }, FileId::Superblock) => {
                self.state = State::DeleteWriteSb { copy: 1 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, self.caps.rows, 1)
            }
            (State::DeleteWriteSb { copy: 1 }, FileId::Superblock) => {
                self.state = State::DeleteFsyncSb;
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (State::BatchWriteRow { next }, FileId::Rows) => {
                // One row durable-ish (not yet fsynced); write the next, or
                // move to the single fsync that covers all of them.
                let n = self.batch_rows;
                debug_assert!(next < n, "batch write past the staged rows");
                if next + 1 < n {
                    self.state = State::BatchWriteRow { next: next + 1 };
                    self.batch_row_write(next + 1)
                } else {
                    self.state = State::BatchFsyncRows;
                    Output::Fsync { file: FileId::Rows }
                }
            }
            (State::BatchWriteSb { copy: 0 }, FileId::Superblock) => {
                self.state = State::BatchWriteSb { copy: 1 };
                Self::sb_copy_write(
                    self.generation + 1,
                    self.row_count + self.batch_rows as u64,
                    self.caps.rows,
                    1,
                )
            }
            (State::BatchWriteSb { copy: 1 }, FileId::Superblock) => {
                self.state = State::BatchFsyncSb;
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (State::InsertWriteRow, FileId::Rows) => {
                self.state = State::InsertFsyncRows;
                Output::Fsync { file: FileId::Rows }
            }
            (State::InsertWriteSb { copy: 0 }, FileId::Superblock) => {
                self.state = State::InsertWriteSb { copy: 1 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, self.caps.rows, 1)
            }
            (State::InsertWriteSb { copy: 1 }, FileId::Superblock) => {
                self.state = State::InsertFsyncSb;
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (state, file) => {
                panic!("protocol violation: WriteDone({file:?}) in state {state:?}")
            }
        }
    }

    fn on_fsync_done(&mut self, file: FileId) -> Output {
        match (self.state, file) {
            (State::InitFsyncSb, FileId::Superblock) => self.finish_open(1, 0),
            (
                State::RecoverFsyncRows {
                    generation,
                    row_count,
                },
                FileId::Rows,
            ) => {
                // Rows durable. If the chosen generation's twin slot is
                // not healthy, repair it (see `State::RecoverRepairSb`)
                // before the final fsync; otherwise fsync directly.
                if let Some(copy) = self.pending_repair.take() {
                    self.state = State::RecoverRepairSb {
                        generation,
                        row_count,
                        copy,
                    };
                    Self::sb_copy_write(generation, row_count, self.caps.rows, copy)
                } else {
                    self.state = State::RecoverFsyncSb {
                        generation,
                        row_count,
                    };
                    Output::Fsync {
                        file: FileId::Superblock,
                    }
                }
            }
            (
                State::RecoverFsyncSb {
                    generation,
                    row_count,
                },
                FileId::Superblock,
            ) => self.finish_open(generation, row_count),
            (State::UpdateFsyncRows, FileId::Rows) => {
                self.state = State::UpdateWriteSb { copy: 0 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, self.caps.rows, 0)
            }
            (State::UpdateFsyncSb, FileId::Superblock) => {
                // Commit point: the new value is durable.
                let (id, value, old_row) = self
                    .pending_update
                    .take()
                    .expect("pending update at commit");
                self.generation += 1;
                // The superseded slot stops being live; the new one starts.
                self.set_live(old_row, false);
                self.bind_indices(id, self.row_count);
                self.trigram.insert(self.row_count, &value);
                self.set_live(self.row_count, true);
                self.row_count += 1;
                self.retired += 1;
                self.state = State::Ready;
                // Pair assertion: the new value must now be the one read.
                debug_assert_eq!(self.lookup_value(id), Some(value));
                Output::UpdateDone { id, result: Ok(()) }
            }
            (State::DeleteFsyncRows, FileId::Rows) => {
                // The tombstone is durable; now flip the manifest over it.
                self.state = State::DeleteWriteSb { copy: 0 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, self.caps.rows, 0)
            }
            (State::DeleteFsyncSb, FileId::Superblock) => {
                // Commit point: the deletion is durable.
                let (id, record_row) = self
                    .pending_delete
                    .take()
                    .expect("pending delete at commit");
                self.generation += 1;
                // The record's slot stops being live. Nothing is removed
                // from any index: they keep pointing at the retired slot,
                // and every read path asks `is_live` before trusting it.
                self.set_live(record_row, false);
                // The tombstone slot is accounted for but never indexed.
                self.trigram.skip_row(self.row_count);
                self.row_count += 1;
                self.live_count -= 1;
                self.retired += 1;
                self.tombstones += 1;
                self.state = State::Ready;
                // Pair assertion: the deleted row must now be unreadable.
                debug_assert_eq!(self.lookup_value(id), None);
                Output::DeleteDone { id, result: Ok(()) }
            }
            (State::InsertFsyncRows, FileId::Rows) => {
                // The row is durable; now flip the superblock. The new
                // generation goes to the *other* pair of slots, so the live
                // generation's copies are untouched no matter what tears.
                self.state = State::InsertWriteSb { copy: 0 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, self.caps.rows, 0)
            }
            (State::BatchFsyncRows, FileId::Rows) => {
                // Every row of the batch is durable; now flip the manifest
                // over all of them at once.
                self.state = State::BatchWriteSb { copy: 0 };
                Self::sb_copy_write(
                    self.generation + 1,
                    self.row_count + self.batch_rows as u64,
                    self.caps.rows,
                    0,
                )
            }
            (State::BatchFsyncSb, FileId::Superblock) => {
                // Commit point: every op in the batch is durable, together.
                let rows = self.batch_rows as u64;
                debug_assert!(rows > 0, "empty batch reached the commit point");
                self.generation += 1;
                self.apply_batch();
                self.state = State::Ready;
                Output::BatchDone {
                    rows,
                    result: Ok(()),
                }
            }
            (State::InsertFsyncSb, FileId::Superblock) => {
                // Commit point: the new generation is durable.
                let (id, value) = self.pending.take().expect("pending insert at commit");
                self.generation += 1;
                // `bind`, not blind insert: this id may have been deleted
                // earlier, in which case both indices still hold an entry
                // for it pointing at the retired slot.
                self.bind_indices(id, self.row_count);
                self.trigram.insert(self.row_count, &value);
                self.set_live(self.row_count, true);
                self.row_count += 1;
                self.live_count += 1;
                self.state = State::Ready;
                // Pair assertion: the committed row must now be readable.
                debug_assert_eq!(self.lookup_value(id), Some(value));
                Output::InsertDone { id, result: Ok(()) }
            }
            (state, file) => {
                panic!("protocol violation: FsyncDone({file:?}) in state {state:?}")
            }
        }
    }

    // ---- I/O failure: fail-stop ----------------------------------------

    /// The host reported an I/O error for the in-flight request. Fail-stop:
    /// resolve the in-flight operation with an error, reject everything
    /// afterwards. The host restarts and re-opens; the half-done operation
    /// resolves to all-or-nothing at recovery, exactly like a crash.
    fn on_io_failed(&mut self, file: FileId) -> Output {
        let err = DbError::IoFailed { file };
        // Negative space: the failure must name the file the in-flight
        // request actually targeted; anything else is a confused host.
        let expected = match self.state {
            State::InitWriteSb { .. } | State::InitFsyncSb | State::RecoverReadSb => {
                FileId::Superblock
            }
            State::RecoverReadRows { .. }
            | State::RecoverTruncateRows { .. }
            | State::RecoverFsyncRows { .. } => FileId::Rows,
            State::RecoverRepairSb { .. } | State::RecoverFsyncSb { .. } => FileId::Superblock,
            State::InsertWriteRow | State::InsertFsyncRows => FileId::Rows,
            State::InsertWriteSb { .. } | State::InsertFsyncSb => FileId::Superblock,
            State::UpdateWriteRow | State::UpdateFsyncRows => FileId::Rows,
            State::UpdateWriteSb { .. } | State::UpdateFsyncSb => FileId::Superblock,
            State::DeleteWriteRow | State::DeleteFsyncRows => FileId::Rows,
            State::DeleteWriteSb { .. } | State::DeleteFsyncSb => FileId::Superblock,
            State::BatchWriteRow { .. } | State::BatchFsyncRows => FileId::Rows,
            State::BatchWriteSb { .. } | State::BatchFsyncSb => FileId::Superblock,
            state => panic!("protocol violation: IoFailed({file:?}) in state {state:?}"),
        };
        assert!(
            file == expected,
            "protocol violation: IoFailed({file:?}) but in-flight request targets {expected:?}"
        );
        match self.state {
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb => {
                let (id, _) = self.pending.take().expect("pending insert on failure");
                self.state = State::Failed(err);
                Output::InsertDone {
                    id,
                    result: Err(err),
                }
            }
            State::UpdateWriteRow
            | State::UpdateFsyncRows
            | State::UpdateWriteSb { .. }
            | State::UpdateFsyncSb => {
                let (id, _, _) = self
                    .pending_update
                    .take()
                    .expect("pending update on failure");
                self.state = State::Failed(err);
                Output::UpdateDone {
                    id,
                    result: Err(err),
                }
            }
            State::DeleteWriteRow
            | State::DeleteFsyncRows
            | State::DeleteWriteSb { .. }
            | State::DeleteFsyncSb => {
                let (id, _) = self
                    .pending_delete
                    .take()
                    .expect("pending delete on failure");
                self.state = State::Failed(err);
                Output::DeleteDone {
                    id,
                    result: Err(err),
                }
            }
            State::BatchWriteRow { .. }
            | State::BatchFsyncRows
            | State::BatchWriteSb { .. }
            | State::BatchFsyncSb => {
                // Fail-stop, and the staged effects are dropped unapplied:
                // the superblock still names the old generation, so
                // whatever rows did reach the disk are past the manifest
                // and inert. `rows: 0` is the literal truth — nothing this
                // batch wrote is committed.
                debug_assert!(!self.batch.is_empty(), "batch effects lost before failure");
                self.batch.clear();
                self.batch_rows = 0;
                self.state = State::Failed(err);
                Output::BatchDone {
                    rows: 0,
                    result: Err(BatchReject { at: 0, error: err }),
                }
            }
            _ => self.fail_open(err),
        }
    }

    // ---- get ---------------------------------------------------------

    fn on_get(&mut self, id: u64) -> Output {
        self.read_window(id, 0)
    }

    /// A value's first window, or a later one. One code path, because the
    /// only difference between "read this row" and "read the rest of it"
    /// is where you start.
    fn read_window(&mut self, id: u64, offset: u32) -> Output {
        let result = match self.state {
            State::Ready => Ok(self.window_of(id, offset)),
            // A HIT is checksum-verified and therefore exactly right, in
            // salvage mode as in any other. A MISS is the honest problem:
            // the id may have lived in a quarantined slot, so `None` would
            // be a confident answer we cannot justify. Refuse instead.
            State::Degraded => match self.window_of(id, offset) {
                Some(window) => Ok(Some(window)),
                None => Err(DbError::Degraded {
                    quarantined: self.quarantined,
                }),
            },
            State::New
            | State::InitWriteSb { .. }
            | State::InitFsyncSb
            | State::RecoverReadSb
            | State::RecoverReadRows { .. }
            | State::RecoverTruncateRows { .. }
            | State::RecoverFsyncRows { .. }
            | State::RecoverRepairSb { .. }
            | State::RecoverFsyncSb { .. } => Err(DbError::NotOpen),
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb
            | State::UpdateWriteRow
            | State::UpdateFsyncRows
            | State::UpdateWriteSb { .. }
            | State::UpdateFsyncSb
            | State::DeleteWriteRow
            | State::DeleteFsyncRows
            | State::DeleteWriteSb { .. }
            | State::DeleteFsyncSb
            | State::BatchWriteRow { .. }
            | State::BatchFsyncRows
            | State::BatchWriteSb { .. }
            | State::BatchFsyncSb => Err(DbError::Busy),
            State::Failed(e) => Err(e),
        };
        Output::GetDone { id, result }
    }

    fn on_range(&mut self, lo: u64, hi: u64) -> Output {
        let result = match self.state {
            // Served, but every page carries `incomplete: true` — the rows
            // returned are exact; rows that would have matched may be
            // missing, and silence about that would be indistinguishable
            // from data loss.
            State::Ready | State::Degraded => Ok(self.scan_page(lo, hi)),
            State::New
            | State::InitWriteSb { .. }
            | State::InitFsyncSb
            | State::RecoverReadSb
            | State::RecoverReadRows { .. }
            | State::RecoverTruncateRows { .. }
            | State::RecoverFsyncRows { .. }
            | State::RecoverRepairSb { .. }
            | State::RecoverFsyncSb { .. } => Err(DbError::NotOpen),
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb
            | State::UpdateWriteRow
            | State::UpdateFsyncRows
            | State::UpdateWriteSb { .. }
            | State::UpdateFsyncSb
            | State::DeleteWriteRow
            | State::DeleteFsyncRows
            | State::DeleteWriteSb { .. }
            | State::DeleteFsyncSb
            | State::BatchWriteRow { .. }
            | State::BatchFsyncRows
            | State::BatchWriteSb { .. }
            | State::BatchFsyncSb => Err(DbError::Busy),
            State::Failed(e) => Err(e),
        };
        Output::RangeDone { result }
    }

    fn on_find(
        &mut self,
        needle: [u8; VALUE_LEN],
        needle_len: u8,
        after: Option<FindCursor>,
    ) -> Output {
        assert!(
            (needle_len as usize) <= VALUE_LEN,
            "needle exceeds the value width"
        );
        let result = match self.state {
            State::Ready | State::Degraded => {
                Ok(self.find_page(&needle[..needle_len as usize], after))
            }
            State::New
            | State::InitWriteSb { .. }
            | State::InitFsyncSb
            | State::RecoverReadSb
            | State::RecoverReadRows { .. }
            | State::RecoverTruncateRows { .. }
            | State::RecoverFsyncRows { .. }
            | State::RecoverRepairSb { .. }
            | State::RecoverFsyncSb { .. } => Err(DbError::NotOpen),
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb
            | State::UpdateWriteRow
            | State::UpdateFsyncRows
            | State::UpdateWriteSb { .. }
            | State::UpdateFsyncSb
            | State::DeleteWriteRow
            | State::DeleteFsyncRows
            | State::DeleteWriteSb { .. }
            | State::DeleteFsyncSb
            | State::BatchWriteRow { .. }
            | State::BatchFsyncRows
            | State::BatchWriteSb { .. }
            | State::BatchFsyncSb => Err(DbError::Busy),
            State::Failed(e) => Err(e),
        };
        Output::FindDone { result }
    }

    /// One bounded page of rows whose value contains `needle`, ascending
    /// by row (insertion order). The trigram index only narrows the
    /// candidates; every returned row is VERIFIED against the arena
    /// bytes, so results are exact regardless of index state — the index
    /// can only make this slower, never wrong. Committed state only:
    /// like the btree, the trigram index is updated at the commit point.
    fn find_page(&self, needle: &[u8], after: Option<FindCursor>) -> FindPage {
        let mut rows = [0u64; FIND_PAGE];
        // Once any value spans more than one slot, the chain walk stops
        // being the cheapest way to reach every candidate, so take the
        // exhaustive path. It is bounded by the row count and exactly as
        // correct — a cost, never a compromise (see `find_page` in the
        // trigram index).
        let exhaustive = self.long_values > 0;
        let (n, next) = self
            .trigram
            .find_page(needle, after, &mut rows, exhaustive, |row| {
                self.find_verifications
                    .set(self.find_verifications.get() + 1);
                // Postings survive their record's retirement (the trigram
                // index is append-only); liveness filters them out here.
                if !self.is_live(row) {
                    return false;
                }
                let off = (row as usize) * ROW_SIZE;
                match decode_row(&self.arena[off..off + ROW_SIZE]) {
                    Some(slot) if slot.record().is_some() => {
                        if needle.is_empty() {
                            return true;
                        }
                        // Match against the WHOLE value, not the head slot: a
                        // substring may straddle a slot boundary, and a scan
                        // that missed those would be quietly incomplete.
                        let (rows, _) = self.value_extent(row);
                        let mut value = [0u8; MAX_VALUE_LEN];
                        let len = self.assemble_from_arena(row, rows, &mut value);
                        value[..len].windows(needle.len()).any(|w| w == needle)
                    }
                    // A tombstone or a continuation indexes nothing and
                    // matches nothing.
                    Some(_) => false,
                    // A quarantined slot holds no verified row, so it matches
                    // nothing. Short needles scan every row number, so this is
                    // reachable in salvage mode — and ONLY there: in every
                    // other mode an undecodable live row is a bug, and the
                    // assertion still says so.
                    None => {
                        debug_assert!(
                            self.quarantined > 0,
                            "live arena row must decode outside salvage mode"
                        );
                        false
                    }
                }
            });
        let mut page = FindPage {
            items: [RowRef::EMPTY; FIND_PAGE],
            count: n as u8,
            next: None,
            incomplete: self.quarantined > 0,
        };
        for (slot, &row) in page.items.iter_mut().zip(rows.iter().take(n)) {
            let off = (row as usize) * ROW_SIZE;
            let (id, _) = decode_row(&self.arena[off..off + ROW_SIZE])
                .and_then(|r| r.record())
                .expect("live arena row must decode");
            *slot = self.row_ref(id, row);
        }
        page.next = next;
        page
    }

    /// One bounded page of `lo..=hi`, ascending. Reads only committed
    /// state: the ordered index is updated at the commit point, so an
    /// in-flight insert is never visible here (serializable, §5).
    fn scan_page(&self, lo: u64, hi: u64) -> RangePage {
        let mut page = RangePage {
            items: [RowRef::EMPTY; RANGE_PAGE],
            count: 0,
            next: None,
            incomplete: self.quarantined > 0,
        };
        if lo > hi {
            return page; // inverted bounds: honestly empty, not an error
        }
        let mut hits = [(0u64, 0u64); RANGE_PAGE];
        let mut n = 0usize;
        let mut next = None;
        self.ordered.for_each_from(lo, |key, row| {
            if key > hi {
                return false;
            }
            // The tree keeps an entry for every id ever inserted, including
            // ones whose record has since been retired. Liveness decides.
            if !self.is_live(row) {
                return true;
            }
            if n == RANGE_PAGE {
                next = Some(key);
                return false;
            }
            hits[n] = (key, row);
            n += 1;
            true
        });
        for (slot, &(key, row)) in page.items.iter_mut().zip(hits.iter().take(n)) {
            *slot = self.row_ref(key, row);
        }
        for pair in hits[..n].windows(2) {
            debug_assert!(pair[0].0 < pair[1].0, "range page out of order");
        }
        page.count = n as u8;
        page.next = next;
        page
    }

    /// Read a value's whole run out of `data`, returning how many slots it
    /// occupies — or `None` when the run does not end where its rows say
    /// it should.
    ///
    /// A value is only ever written whole, in one commit, so a run that
    /// stops short is not something this engine produced. Checking the
    /// WHOLE run before trusting any of it is what stops a damaged
    /// continuation from turning into a silently truncated value: the head
    /// is never served on its own.
    fn verify_run(
        &self,
        data: &[u8],
        head_row: u64,
        row_count: u64,
        head: &RowSlot,
    ) -> Option<u64> {
        let mut rows = 1u64;
        let mut more = head.more;
        while more {
            let r = head_row + rows;
            if r >= row_count {
                // The value promised a continuation the file does not
                // have. Truncation, or a manifest that names fewer rows
                // than the value needs.
                return None;
            }
            let o = (r as usize) * ROW_SIZE;
            let slot = decode_row(&data[o..o + ROW_SIZE])?;
            if slot.kind != RowKind::Chunk || slot.id != head.id {
                return None;
            }
            if rows as usize >= MAX_COMMIT_ROWS {
                // Longer than any commit can be, so longer than anything
                // this engine could have written.
                return None;
            }
            more = slot.more;
            rows += 1;
        }
        Some(rows)
    }

    /// How many slots a BROKEN run occupies, for quarantine purposes: the
    /// head plus every readable continuation of it that did arrive. They
    /// are unreadable together — the value they belong to cannot be
    /// served — so they are quarantined together rather than left behind
    /// as stranded chunks.
    fn broken_run_len(&self, data: &[u8], head_row: u64, row_count: u64, head: &RowSlot) -> u64 {
        let mut rows = 1u64;
        let mut more = head.more;
        while more && rows as usize <= MAX_COMMIT_ROWS {
            let r = head_row + rows;
            if r >= row_count {
                break;
            }
            let o = (r as usize) * ROW_SIZE;
            match decode_row(&data[o..o + ROW_SIZE]) {
                Some(slot) if slot.kind == RowKind::Chunk && slot.id == head.id => {
                    more = slot.more;
                    rows += 1;
                }
                _ => break,
            }
        }
        rows
    }

    /// Copy a value out of the arena into `out`, returning its length.
    /// `rows` is the run's length, already known to the caller.
    fn assemble_from_arena(
        &self,
        head_row: u64,
        rows: u64,
        out: &mut [u8; MAX_VALUE_LEN],
    ) -> usize {
        let mut at = 0usize;
        for r in head_row..head_row + rows {
            let off = (r as usize) * ROW_SIZE;
            let slot = decode_row(&self.arena[off..off + ROW_SIZE])
                .expect("a row already accepted into the arena must decode");
            let n = slot.len as usize;
            out[at..at + n].copy_from_slice(&slot.value[..n]);
            at += n;
        }
        at
    }

    /// How many slots the value starting at `head_row` occupies, and how
    /// long it is. The run ends at the first row that is not a
    /// continuation — which is exactly how the file describes it, since a
    /// value's chunks are written immediately after their head in the same
    /// commit and nothing can be interleaved between them.
    fn value_extent(&self, head_row: u64) -> (u64, u32) {
        let off = (head_row as usize) * ROW_SIZE;
        let head =
            decode_row(&self.arena[off..off + ROW_SIZE]).expect("live arena row must decode");
        // Fast path: with no multi-slot value anywhere, there is nothing
        // to look ahead for, and a point read touches one row.
        if self.long_values == 0 {
            return (1, head.len as u32);
        }
        let mut rows = 1u64;
        let mut total = head.len as u32;
        let mut r = head_row + 1;
        while r < self.row_count {
            let o = (r as usize) * ROW_SIZE;
            match decode_row(&self.arena[o..o + ROW_SIZE]) {
                Some(slot) if slot.kind == RowKind::Chunk && slot.id == head.id => {
                    rows += 1;
                    total += slot.len as u32;
                    r += 1;
                }
                _ => break,
            }
        }
        (rows, total)
    }

    /// One bounded window of `id`'s value, starting at `offset`.
    ///
    /// `offset` must be a slot boundary — the offsets `Get` and earlier
    /// windows hand back — so a window is always exactly one row's
    /// payload and never has to be stitched from two.
    fn window_of(&self, id: u64, offset: u32) -> Option<ValueWindow> {
        let head_row = self.live_row_of(id)?;
        let (rows, total) = self.value_extent(head_row);
        debug_assert!(
            (offset as usize).is_multiple_of(VALUE_LEN),
            "a window must start on a slot boundary"
        );
        let skip = offset as usize / VALUE_LEN;
        if skip as u64 >= rows {
            // Reading exactly at the end is an empty final window rather
            // than an error; reading past it is the caller's mistake, and
            // an empty window is still the honest answer.
            return Some(ValueWindow {
                total,
                offset,
                len: 0,
                bytes: [0; VALUE_LEN],
            });
        }
        let off = ((head_row as usize) + skip) * ROW_SIZE;
        let slot =
            decode_row(&self.arena[off..off + ROW_SIZE]).expect("live arena row must decode");
        debug_assert_eq!(slot.id, id, "the index must point at the row it claims");
        Some(ValueWindow {
            total,
            offset,
            len: slot.len,
            bytes: slot.value,
        })
    }

    /// A scan-result reference for the value living at `head_row`.
    fn row_ref(&self, id: u64, head_row: u64) -> RowRef {
        let (_, total) = self.value_extent(head_row);
        let off = (head_row as usize) * ROW_SIZE;
        let slot =
            decode_row(&self.arena[off..off + ROW_SIZE]).expect("live arena row must decode");
        debug_assert_eq!(slot.id, id);
        RowRef {
            id,
            len: total,
            head: slot.value,
        }
    }

    /// `id`'s whole value, when it fits in one slot.
    fn lookup_value(&self, id: u64) -> Option<[u8; VALUE_LEN]> {
        let row = self.live_row_of(id)?;
        let off = (row as usize) * ROW_SIZE;
        let (row_id, value) = decode_row(&self.arena[off..off + ROW_SIZE])
            .and_then(|r| r.record())
            .expect("live arena row must decode");
        // Pair assertion: the index must point at the row it claims.
        debug_assert_eq!(row_id, id);
        Some(value)
    }

    // ---- primary-key index ---------------------------------------------
    //
    // Open addressing with linear probing over a power-of-two table sized to
    // at least 2x row capacity. Entries store row_index + 1; 0 means empty.
    // No deletes in the vertical slice, so no tombstones.

    fn hash_slot(&self, id: u64) -> usize {
        let h = (id ^ (id >> 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (h as usize) & (self.index.len() - 1)
    }

    /// Advance to the next probe slot. The termination guard is exact
    /// (`!=`, not an inequality with slack): `probes` counts advances one
    /// by one, so equality is the first and only moment the table has been
    /// fully probed — unreachable while the load factor stays <= 0.5.
    /// Shared by lookup and insert so the guard exists exactly once;
    /// insert's loop inspects a subset of the slots lookup would, so a
    /// second private guard there could never fire and could never be
    /// tested.
    fn probe_next(&self, slot: usize, probes: &mut usize) -> usize {
        *probes += 1;
        assert!(
            *probes != self.index.len(),
            "index probe loop must terminate"
        );
        (slot + 1) & (self.index.len() - 1)
    }

    fn index_lookup(&self, id: u64) -> Option<u64> {
        let mut slot = self.hash_slot(id);
        let mut probes = 0usize;
        loop {
            match self.index[slot] {
                0 => return None,
                entry => {
                    let row = entry - 1;
                    if self.row_id_at(row) == id {
                        return Some(row);
                    }
                }
            }
            slot = self.probe_next(slot, &mut probes);
        }
    }

    /// Point `id` at `row`, whether or not the id is already in the table.
    ///
    /// An id whose record was retired keeps its index entry (the indices
    /// are append-only; liveness is tracked separately), so inserting that
    /// id again must REPOINT the existing entry rather than add a second
    /// one — two entries for one id would make lookups depend on probe
    /// order, which is exactly the kind of quiet wrongness this codebase
    /// does not tolerate.
    fn index_bind(&mut self, id: u64, row: u64) {
        let mut slot = self.hash_slot(id);
        let mut probes = 0usize;
        loop {
            match self.index[slot] {
                0 => {
                    self.index[slot] = row + 1;
                    break;
                }
                entry if self.row_id_at(entry - 1) == id => {
                    self.index[slot] = row + 1;
                    break;
                }
                _ => slot = self.probe_next(slot, &mut probes),
            }
        }
        debug_assert_eq!(self.index_lookup(id), Some(row));
    }

    /// Bind `id` in both keyed indices — the hash table and the ordered
    /// tree — to the slot that now holds its record.
    fn bind_indices(&mut self, id: u64, row: u64) {
        self.index_bind(id, row);
        if !self.ordered.repoint(id, row) {
            self.ordered.insert(id, row);
        }
    }

    fn row_id_at(&self, row: u64) -> u64 {
        let off = (row as usize) * ROW_SIZE;
        u64::from_le_bytes(self.arena[off..off + 8].try_into().expect("fixed slice"))
    }
}

#[cfg(test)]
mod tests {
    //! A minimal in-memory host: no fault injection, no crash model. The
    //! real harness lives in `dabqlite-sim`; these tests pin the protocol
    //! shape itself.

    use super::*;
    use std::vec::Vec as StdVec;

    struct MiniHost {
        engine: Engine,
        superblock: StdVec<u8>,
        rows: StdVec<u8>,
    }

    impl MiniHost {
        fn new(caps: Capacities) -> Self {
            MiniHost {
                engine: Engine::new(caps),
                superblock: StdVec::new(),
                rows: StdVec::new(),
            }
        }

        fn file(&mut self, id: FileId) -> &mut StdVec<u8> {
            match id {
                FileId::Superblock => &mut self.superblock,
                FileId::Rows => &mut self.rows,
                FileId::RowsOld => unreachable!("the row engine never touches the legacy file"),
            }
        }

        fn drive(&mut self, first: Input<'_>) -> Output {
            let mut out = self.engine.tick(first);
            loop {
                match out {
                    Output::Read { file, offset, len } => {
                        let f = self.file(file);
                        let end = ((offset + len) as usize).min(f.len());
                        let data = f[(offset as usize).min(f.len())..end].to_vec();
                        out = self.engine.tick(Input::ReadDone { file, data: &data });
                    }
                    Output::Write { file, offset, data } => {
                        let f = self.file(file);
                        let end = offset as usize + data.as_slice().len();
                        if f.len() < end {
                            f.resize(end, 0);
                        }
                        f[offset as usize..end].copy_from_slice(data.as_slice());
                        out = self.engine.tick(Input::WriteDone { file });
                    }
                    Output::Fsync { file } => {
                        out = self.engine.tick(Input::FsyncDone { file });
                    }
                    terminal => return terminal,
                }
            }
        }

        fn open(&mut self) -> Output {
            let input = Input::Open {
                superblock_len: self.superblock.len() as u64,
                rows_len: self.rows.len() as u64,
            };
            self.drive(input)
        }
    }

    fn val(b: u8) -> [u8; VALUE_LEN] {
        [b; VALUE_LEN]
    }

    /// The window a full-width value comes back in.
    fn win(b: u8) -> ValueWindow {
        ValueWindow {
            total: VALUE_LEN as u32,
            offset: 0,
            len: VALUE_LEN as u8,
            bytes: val(b),
        }
    }

    #[test]
    fn fresh_open_insert_get() {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        assert_eq!(h.open(), Output::OpenDone { result: Ok(0) });
        assert_eq!(
            h.drive(Input::Insert {
                id: 1,
                value: val(7)
            }),
            Output::InsertDone {
                id: 1,
                result: Ok(())
            }
        );
        assert_eq!(
            h.drive(Input::Get { id: 1 }),
            Output::GetDone {
                id: 1,
                result: Ok(Some(win(7)))
            }
        );
        // Negative space: an id never inserted must be absent.
        assert_eq!(
            h.drive(Input::Get { id: 2 }),
            Output::GetDone {
                id: 2,
                result: Ok(None)
            }
        );
        assert_eq!(h.engine.usage(), (1, 8));
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        h.open();
        h.drive(Input::Insert {
            id: 5,
            value: val(1),
        });
        assert_eq!(
            h.drive(Input::Insert {
                id: 5,
                value: val(2)
            }),
            Output::InsertDone {
                id: 5,
                result: Err(DbError::DuplicateId { id: 5 })
            }
        );
        // The original value must be untouched.
        assert_eq!(
            h.drive(Input::Get { id: 5 }),
            Output::GetDone {
                id: 5,
                result: Ok(Some(win(1)))
            }
        );
    }

    #[test]
    fn capacity_exhaustion_is_first_class() {
        let mut h = MiniHost::new(Capacities { rows: 2 });
        h.open();
        h.drive(Input::Insert {
            id: 1,
            value: val(1),
        });
        h.drive(Input::Insert {
            id: 2,
            value: val(2),
        });
        assert_eq!(
            h.drive(Input::Insert {
                id: 3,
                value: val(3)
            }),
            Output::InsertDone {
                id: 3,
                result: Err(DbError::Full {
                    entity: "records",
                    capacity: 2,
                    dead: 0
                })
            }
        );
        assert_eq!(h.engine.usage(), (2, 2));
    }

    #[test]
    fn reopen_recovers_committed_rows() {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        h.open();
        for i in 0..5u64 {
            h.drive(Input::Insert {
                id: i * 10,
                value: val(i as u8),
            });
        }
        let gen_before = h.engine.generation();
        // "Restart the process": new engine, same files.
        let (sb, rows) = (h.superblock.clone(), h.rows.clone());
        let mut h2 = MiniHost::new(Capacities { rows: 8 });
        h2.superblock = sb;
        h2.rows = rows;
        assert_eq!(h2.open(), Output::OpenDone { result: Ok(5) });
        assert_eq!(h2.engine.generation(), gen_before);
        for i in 0..5u64 {
            assert_eq!(
                h2.drive(Input::Get { id: i * 10 }),
                Output::GetDone {
                    id: i * 10,
                    result: Ok(Some(win(i as u8)))
                }
            );
        }
    }

    #[test]
    fn reopen_with_smaller_capacity_fails_loudly() {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        h.open();
        for i in 0..5u64 {
            h.drive(Input::Insert {
                id: i,
                value: val(0),
            });
        }
        let mut h2 = MiniHost::new(Capacities { rows: 3 });
        h2.superblock = h.superblock.clone();
        h2.rows = h.rows.clone();
        assert_eq!(
            h2.open(),
            Output::OpenDone {
                result: Err(DbError::CapacityBelowData {
                    required: 5,
                    configured: 3
                })
            }
        );
    }

    #[test]
    fn ops_before_open_fail() {
        let mut e = Engine::new(Capacities { rows: 2 });
        assert_eq!(
            e.tick(Input::Get { id: 1 }),
            Output::GetDone {
                id: 1,
                result: Err(DbError::NotOpen)
            }
        );
        assert_eq!(
            e.tick(Input::Insert {
                id: 1,
                value: val(0)
            }),
            Output::InsertDone {
                id: 1,
                result: Err(DbError::NotOpen)
            }
        );
    }

    /// Get an engine into the middle of an insert (row write in flight).
    fn engine_mid_insert() -> Engine {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        h.open();
        let out = h.engine.tick(Input::Insert {
            id: 1,
            value: val(1),
        });
        assert!(matches!(
            out,
            Output::Write {
                file: FileId::Rows,
                ..
            }
        ));
        h.engine
    }

    #[test]
    fn v1_serializes_everything_mid_insert() {
        // Isolation (docs/DESIGN.md §5): single writer, serialized access.
        // While an insert's I/O is in flight, everything else is Busy.
        let mut e = engine_mid_insert();
        assert_eq!(
            e.tick(Input::Insert {
                id: 2,
                value: val(2)
            }),
            Output::InsertDone {
                id: 2,
                result: Err(DbError::Busy)
            }
        );
        assert_eq!(
            e.tick(Input::Get { id: 1 }),
            Output::GetDone {
                id: 1,
                result: Err(DbError::Busy)
            }
        );
    }

    #[test]
    fn io_failure_is_fail_stop() {
        let mut e = engine_mid_insert();
        // The row write fails: the insert errors, and the engine refuses
        // everything from then on. Restart-and-recover is the only exit.
        let err = DbError::IoFailed { file: FileId::Rows };
        assert_eq!(
            e.tick(Input::IoFailed { file: FileId::Rows }),
            Output::InsertDone {
                id: 1,
                result: Err(err)
            }
        );
        assert_eq!(
            e.tick(Input::Insert {
                id: 2,
                value: val(2)
            }),
            Output::InsertDone {
                id: 2,
                result: Err(err)
            }
        );
        assert_eq!(
            e.tick(Input::Get { id: 1 }),
            Output::GetDone {
                id: 1,
                result: Err(err)
            }
        );
    }

    // ---- the host-protocol seam: violations must be loud, not lenient ----

    #[test]
    #[should_panic(expected = "protocol violation")]
    fn write_done_in_ready_panics() {
        let mut h = MiniHost::new(Capacities { rows: 2 });
        h.open();
        h.engine.tick(Input::WriteDone { file: FileId::Rows });
    }

    #[test]
    #[should_panic(expected = "protocol violation")]
    fn fsync_done_before_open_panics() {
        let mut e = Engine::new(Capacities { rows: 2 });
        e.tick(Input::FsyncDone {
            file: FileId::Superblock,
        });
    }

    #[test]
    #[should_panic(expected = "protocol violation")]
    fn double_open_panics() {
        let mut h = MiniHost::new(Capacities { rows: 2 });
        h.open();
        h.engine.tick(Input::Open {
            superblock_len: 0,
            rows_len: 0,
        });
    }

    #[test]
    #[should_panic(expected = "protocol violation")]
    fn completion_for_wrong_file_panics() {
        // Mid-insert the in-flight write targets Rows; a completion for the
        // superblock is a sequencing bug in the host.
        let mut e = engine_mid_insert();
        e.tick(Input::WriteDone {
            file: FileId::Superblock,
        });
    }

    #[test]
    #[should_panic(expected = "protocol violation")]
    fn io_failed_for_wrong_file_panics() {
        let mut e = engine_mid_insert();
        e.tick(Input::IoFailed {
            file: FileId::Superblock,
        });
    }

    // ---- mutation-gap closures ------------------------------------------

    /// The engine's own invariant tripwire must fire: an engine whose
    /// arena pointer changed (allocation after init — forbidden) must
    /// refuse to tick.
    #[test]
    #[should_panic(expected = "arena moved")]
    fn invariant_tripwire_has_teeth() {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        h.open();
        h.engine.arena_addr ^= 1;
        h.engine.tick(Input::Get { id: 1 });
    }

    /// hash_slot pinned against an independent restatement of the mixing
    /// function. Every mutation of the mix (xor→or, xor→and, shift flip,
    /// constant-zero) degrades to "still correct, just clustered" — no
    /// behavioral test can see it, so the values themselves are the spec.
    #[test]
    fn hash_slot_matches_reference_mix() {
        let e = Engine::new(Capacities { rows: 8 });
        assert_eq!(e.index.len(), 16);
        for id in [
            0u64,
            1,
            7,
            0xDEAD_BEEF_1234_5678,
            0x0123_4567_89AB_CDEF,
            u64::MAX,
        ] {
            let mixed = (id ^ (id >> 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let expected = (mixed as usize) & (e.index.len() - 1);
            assert_eq!(e.hash_slot(id), expected, "id={id:#x}");
        }
    }

    /// The probe-termination guard must fire on a full table. Unreachable
    /// through the public API (load factor is capped at 0.5), so the state
    /// is forged directly. Under the mutant that disables the counter this
    /// hangs — a timeout kill.
    #[test]
    #[should_panic(expected = "probe loop must terminate")]
    fn probe_guard_has_teeth() {
        let mut e = Engine::new(Capacities { rows: 8 });
        for slot in e.index.iter_mut() {
            *slot = 1; // every slot claims row 0; arena is zeroed, id 0
        }
        e.index_lookup(7); // never matches, never finds an empty slot
    }

    /// Colliding keys must probe through occupied slots and still resolve
    /// exactly — exercises multi-step probing on the REAL insert/get path,
    /// which no uniform workload guarantees.
    #[test]
    fn colliding_keys_probe_correctly() {
        let mut h = MiniHost::new(Capacities { rows: 8 });
        h.open();
        // Find three ids sharing one hash slot in the 16-slot table.
        let target = h.engine.hash_slot(0);
        let ids: StdVec<u64> = (0..2000u64)
            .filter(|&id| h.engine.hash_slot(id) == target)
            .take(3)
            .collect();
        assert_eq!(ids.len(), 3, "collision search must find a full chain");
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(
                h.drive(Input::Insert {
                    id,
                    value: val(i as u8 + 1)
                }),
                Output::InsertDone { id, result: Ok(()) }
            );
        }
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(
                h.drive(Input::Get { id }),
                Output::GetDone {
                    id,
                    result: Ok(Some(win(i as u8 + 1)))
                }
            );
        }
        // A missing id hashing to the same slot walks the chain to a
        // genuine empty slot: None, not a false positive.
        let absent = (0..5000u64)
            .filter(|&id| h.engine.hash_slot(id) == target)
            .find(|id| !ids.contains(id))
            .expect("a fourth colliding id exists");
        assert_eq!(
            h.drive(Input::Get { id: absent }),
            Output::GetDone {
                id: absent,
                result: Ok(None)
            }
        );
    }
}
