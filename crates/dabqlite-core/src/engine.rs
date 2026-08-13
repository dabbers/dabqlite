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

use crate::layout::{
    decode_row, decode_sb, encode_row, encode_sb, SbDecodeError, ROW_SIZE, SB_COPIES, SB_COPY_SIZE,
    SB_ZONE_SIZE, SCHEMA_HASH, VALUE_LEN,
};

/// The declared file set (docs/DESIGN.md §4.4): derived from the schema,
/// knowable before the program runs. One file per zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileId {
    /// The superblock copy set: the sole atomicity point.
    Superblock,
    /// Fixed-width row slots for the `records` table.
    Rows,
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
    Full { entity: &'static str, capacity: u64 },
    /// A row with this id already exists.
    DuplicateId { id: u64 },
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
    Corrupt { what: &'static str },
    /// The host reported an I/O error on this file. The engine fail-stops
    /// (TigerBeetle-style): the in-flight operation is failed, all further
    /// operations are rejected, and the host must restart and re-open. The
    /// partially-performed operation resolves to all-or-nothing at recovery,
    /// exactly like a crash.
    IoFailed { file: FileId },
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
    fn from_slice(src: &[u8]) -> Self {
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
    /// A read the core requested has completed with these bytes.
    ReadDone { file: FileId, data: &'a [u8] },
    /// A write the core requested has completed.
    WriteDone { file: FileId },
    /// An fsync the core requested has completed.
    FsyncDone { file: FileId },
    /// The read, write, or fsync the core requested FAILED (EIO and
    /// friends). The write may or may not have reached the disk or page
    /// cache — the engine assumes nothing. It fail-stops; restart to
    /// recover.
    IoFailed { file: FileId },
    /// Client: insert a row.
    Insert { id: u64, value: [u8; VALUE_LEN] },
    /// Client: fetch a row by primary key.
    Get { id: u64 },
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
    /// Open finished. `Ok(n)` = recovered `n` committed rows.
    OpenDone { result: Result<u64, DbError> },
    /// Insert finished (durably committed if `Ok`).
    InsertDone {
        id: u64,
        result: Result<(), DbError>,
    },
    /// Get finished (pure in-memory lookup, always immediate).
    GetDone {
        id: u64,
        result: Result<Option<[u8; VALUE_LEN]>, DbError>,
    },
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
    /// Recovery: rows-file fsync in flight. Recovery fsyncs both files
    /// before OpenDone: after a fail-stop restart (process died, machine
    /// did not), the page cache can show state that was never made durable.
    /// Serving it without fsyncing would mean a later power loss erases
    /// rows that this incarnation already showed the application.
    RecoverFsyncRows { generation: u64, row_count: u64 },
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
    /// The insert currently in flight, if any.
    pending: Option<(u64, [u8; VALUE_LEN])>,
    /// Rows file length reported at open; used to cross-check recovery.
    opened_rows_len: u64,
    /// Checksum-valid rows found beyond the manifest during recovery.
    orphan_valid_rows: u64,
    /// Row-slot arena: one allocation at init, never grown (§4.2).
    arena: Vec<u8>,
    /// Open-addressing primary-key index: slot -> row index + 1 (0 = empty).
    /// Sized to 2x capacity rounded up to a power of two, so load factor is
    /// bounded by 0.5 and probes provably terminate.
    index: Vec<u64>,
    /// Negative-space invariant: the allocations must never move. If either
    /// pointer changes, something allocated after init.
    arena_addr: usize,
    index_addr: usize,
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
        let arena_addr = arena.as_ptr() as usize;
        let index_addr = index.as_ptr() as usize;
        Engine {
            state: State::New,
            caps,
            generation: 0,
            row_count: 0,
            pending: None,
            opened_rows_len: 0,
            orphan_valid_rows: 0,
            arena,
            index,
            arena_addr,
            index_addr,
        }
    }

    /// Committed rows and configured capacity, so hosts can alarm at 80%
    /// instead of discovering the ceiling by hitting it (docs/DESIGN.md §6).
    pub fn usage(&self) -> (u64, u64) {
        (self.row_count, self.caps.rows)
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
            orphan_valid_rows: self.orphan_valid_rows,
            rollback_evidence: self.orphan_valid_rows >= 2,
        }
    }

    /// Advance the state machine by one input.
    pub fn tick(&mut self, input: Input<'_>) -> Output {
        self.assert_invariants();
        let out = self.tick_inner(input);
        self.assert_invariants();
        out
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
        debug_assert!(self.row_count <= self.caps.rows);
        // Pending insert exists exactly in the insert-in-flight states.
        let inserting = matches!(
            self.state,
            State::InsertWriteRow
                | State::InsertFsyncRows
                | State::InsertWriteSb { .. }
                | State::InsertFsyncSb
        );
        debug_assert_eq!(self.pending.is_some(), inserting);
    }

    fn tick_inner(&mut self, input: Input<'_>) -> Output {
        match input {
            Input::Open {
                superblock_len,
                rows_len,
            } => self.on_open(superblock_len, rows_len),
            Input::ReadDone { file, data } => self.on_read_done(file, data),
            Input::WriteDone { file } => self.on_write_done(file),
            Input::FsyncDone { file } => self.on_fsync_done(file),
            Input::IoFailed { file } => self.on_io_failed(file),
            Input::Insert { id, value } => self.on_insert(id, value),
            Input::Get { id } => self.on_get(id),
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
    fn sb_slots_for(generation: u64) -> [u8; 2] {
        debug_assert!(generation > 0);
        debug_assert_eq!(SB_COPIES, 4, "pair rotation assumes 4 slots");
        let pair = (generation % 2) as u8;
        [pair * 2, pair * 2 + 1]
    }

    /// Build the write request for copy `copy` (0 or 1) of a generation.
    fn sb_copy_write(generation: u64, row_count: u64, copy: u8) -> Output {
        debug_assert!(copy < 2);
        let mut bytes = [0u8; SB_COPY_SIZE];
        encode_sb(generation, row_count, &mut bytes);
        let slot = Self::sb_slots_for(generation)[copy as usize];
        Output::Write {
            file: FileId::Superblock,
            offset: slot as u64 * SB_COPY_SIZE as u64,
            data: WriteBuf::from_slice(&bytes),
        }
    }

    fn stage_initial_superblock(&mut self) -> Output {
        self.state = State::InitWriteSb { copy: 0 };
        Self::sb_copy_write(1, 0, 0)
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
        self.state = State::RecoverFsyncRows {
            generation,
            row_count,
        };
        Output::Fsync { file: FileId::Rows }
    }

    fn recover_from_rows(&mut self, generation: u64, row_count: u64, data: &[u8]) -> Output {
        let live = (row_count as usize) * ROW_SIZE;
        if data.len() < live {
            return self.fail_open(DbError::Corrupt {
                what: "short read of committed rows",
            });
        }
        for row in 0..row_count {
            let off = (row as usize) * ROW_SIZE;
            let chunk = &data[off..off + ROW_SIZE];
            // Pair assertion (docs/DESIGN.md §7.4): rows were verified when
            // encoded on the write path; verify again reading them back.
            let Some((id, _value)) = decode_row(chunk) else {
                return self.fail_open(DbError::Corrupt {
                    what: "committed row failed checksum",
                });
            };
            if self.index_lookup(id).is_some() {
                return self.fail_open(DbError::Corrupt {
                    what: "duplicate id among committed rows",
                });
            }
            self.arena[off..off + ROW_SIZE].copy_from_slice(chunk);
            self.index_insert(id, row);
        }
        // Rollback-evidence scan: checksum-valid rows beyond the manifest.
        // ONE is the normal artifact of an in-flight, never-acknowledged
        // insert. TWO OR MORE cannot arise that way (writes are serialized;
        // slot N+1 is only written after commit N+1 was acknowledged as
        // durable) — they are surviving evidence that acknowledged commits
        // were rolled back by an out-of-budget fault such as a lying fsync.
        // Silent loss becomes loud whenever the evidence physically exists.
        let mut orphans = 0u64;
        let mut off = live;
        while off + ROW_SIZE <= data.len() {
            if decode_row(&data[off..off + ROW_SIZE]).is_some() {
                orphans += 1;
            }
            off += ROW_SIZE;
        }
        self.orphan_valid_rows = orphans;
        self.stage_recovery_fsyncs(generation, row_count)
    }

    fn finish_open(&mut self, generation: u64, row_count: u64) -> Output {
        assert!(generation > 0, "committed generation must be positive");
        self.generation = generation;
        self.row_count = row_count;
        self.state = State::Ready;
        Output::OpenDone {
            result: Ok(row_count),
        }
    }

    fn fail_open(&mut self, err: DbError) -> Output {
        self.state = State::Failed(err);
        Output::OpenDone { result: Err(err) }
    }

    // ---- insert ----------------------------------------------------------

    fn on_insert(&mut self, id: u64, value: [u8; VALUE_LEN]) -> Output {
        let err = match self.state {
            State::Ready => None,
            State::New
            | State::InitWriteSb { .. }
            | State::InitFsyncSb
            | State::RecoverReadSb
            | State::RecoverReadRows { .. }
            | State::RecoverFsyncRows { .. }
            | State::RecoverFsyncSb { .. } => Some(DbError::NotOpen),
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb => Some(DbError::Busy),
            State::Failed(e) => Some(e),
        };
        if let Some(e) = err {
            return Output::InsertDone { id, result: Err(e) };
        }
        if self.index_lookup(id).is_some() {
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
                }),
            };
        }

        // Stage the row in its arena slot. It becomes visible only when the
        // superblock generation flips.
        let off = (self.row_count as usize) * ROW_SIZE;
        let slot: &mut [u8; ROW_SIZE] = (&mut self.arena[off..off + ROW_SIZE])
            .try_into()
            .expect("fixed slice");
        encode_row(id, &value, slot);
        self.pending = Some((id, value));
        self.state = State::InsertWriteRow;
        Output::Write {
            file: FileId::Rows,
            offset: off as u64,
            data: WriteBuf::from_slice(&self.arena[off..off + ROW_SIZE]),
        }
    }

    fn on_write_done(&mut self, file: FileId) -> Output {
        match (self.state, file) {
            (State::InitWriteSb { copy: 0 }, FileId::Superblock) => {
                self.state = State::InitWriteSb { copy: 1 };
                Self::sb_copy_write(1, 0, 1)
            }
            (State::InitWriteSb { copy: 1 }, FileId::Superblock) => {
                self.state = State::InitFsyncSb;
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
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, 1)
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
                self.state = State::RecoverFsyncSb {
                    generation,
                    row_count,
                };
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (
                State::RecoverFsyncSb {
                    generation,
                    row_count,
                },
                FileId::Superblock,
            ) => self.finish_open(generation, row_count),
            (State::InsertFsyncRows, FileId::Rows) => {
                // The row is durable; now flip the superblock. The new
                // generation goes to the *other* pair of slots, so the live
                // generation's copies are untouched no matter what tears.
                self.state = State::InsertWriteSb { copy: 0 };
                Self::sb_copy_write(self.generation + 1, self.row_count + 1, 0)
            }
            (State::InsertFsyncSb, FileId::Superblock) => {
                // Commit point: the new generation is durable.
                let (id, value) = self.pending.take().expect("pending insert at commit");
                self.generation += 1;
                self.index_insert(id, self.row_count);
                self.row_count += 1;
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
            State::RecoverReadRows { .. } | State::RecoverFsyncRows { .. } => FileId::Rows,
            State::RecoverFsyncSb { .. } => FileId::Superblock,
            State::InsertWriteRow | State::InsertFsyncRows => FileId::Rows,
            State::InsertWriteSb { .. } | State::InsertFsyncSb => FileId::Superblock,
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
            _ => self.fail_open(err),
        }
    }

    // ---- get ---------------------------------------------------------

    fn on_get(&mut self, id: u64) -> Output {
        let result = match self.state {
            State::Ready => Ok(self.lookup_value(id)),
            State::New
            | State::InitWriteSb { .. }
            | State::InitFsyncSb
            | State::RecoverReadSb
            | State::RecoverReadRows { .. }
            | State::RecoverFsyncRows { .. }
            | State::RecoverFsyncSb { .. } => Err(DbError::NotOpen),
            State::InsertWriteRow
            | State::InsertFsyncRows
            | State::InsertWriteSb { .. }
            | State::InsertFsyncSb => Err(DbError::Busy),
            State::Failed(e) => Err(e),
        };
        Output::GetDone { id, result }
    }

    fn lookup_value(&self, id: u64) -> Option<[u8; VALUE_LEN]> {
        let row = self.index_lookup(id)?;
        let off = (row as usize) * ROW_SIZE;
        let (row_id, value) =
            decode_row(&self.arena[off..off + ROW_SIZE]).expect("live arena row must decode");
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

    fn index_lookup(&self, id: u64) -> Option<u64> {
        let mut slot = self.hash_slot(id);
        let mut probes = 0usize;
        loop {
            // Termination: load factor <= 0.5 guarantees an empty slot.
            assert!(probes < self.index.len(), "index probe loop must terminate");
            match self.index[slot] {
                0 => return None,
                entry => {
                    let row = entry - 1;
                    if self.row_id_at(row) == id {
                        return Some(row);
                    }
                }
            }
            slot = (slot + 1) & (self.index.len() - 1);
            probes += 1;
        }
    }

    fn index_insert(&mut self, id: u64, row: u64) {
        debug_assert!(
            self.index_lookup(id).is_none(),
            "index insert must be fresh"
        );
        let mut slot = self.hash_slot(id);
        let mut probes = 0usize;
        while self.index[slot] != 0 {
            assert!(probes < self.index.len(), "index probe loop must terminate");
            slot = (slot + 1) & (self.index.len() - 1);
            probes += 1;
        }
        self.index[slot] = row + 1;
        // Pair assertion: inserted entry must be findable.
        debug_assert_eq!(self.index_lookup(id), Some(row));
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
                result: Ok(Some(val(7)))
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
                result: Ok(Some(val(1)))
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
                    capacity: 2
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
                    result: Ok(Some(val(i as u8)))
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
}
