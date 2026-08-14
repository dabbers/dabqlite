//! The simulated host: drives the engine's I/O requests against a
//! [`SimDisk`], with an optional crash boundary.
//!
//! `crash_after: Some(b)` means the process dies immediately before
//! performing I/O operation number `b` (0-indexed). Sweeping `b` over every
//! operation in a run crashes the process at every I/O boundary
//! (docs/DESIGN.md §7.3).

use dabqlite_core::migration::MigrationEngine;
use dabqlite_core::{Capacities, Engine, FileId, Input, Output, VALUE_LEN};

use crate::disk::SimDisk;

/// A client operation, owned so runs can be replayed verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOp {
    Insert { id: u64, value: [u8; VALUE_LEN] },
    Get { id: u64 },
}

/// Result of driving one client operation to completion.
///
/// `Output` is a deliberately flat value type (~a range page wide): the
/// core's protocol uses bounded buffers, never allocation, so the size
/// asymmetry with `Crashed` is by design.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driven {
    /// The operation finished with this terminal output.
    Done(Output),
    /// The process crashed at the configured I/O boundary. The disk retains
    /// its pre-crash state; call `SimDisk::crash` to settle unsynced writes.
    Crashed,
}

/// A firmware-style misdirected write: the device reports success but the
/// bytes landed in the wrong place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misdirect {
    /// Landed at `offset + shift` in the same file.
    Shift(i64),
    /// Landed at the same offset in the *other* file.
    CrossFile,
}

pub struct SimHost {
    pub engine: Engine,
    /// When set, I/O completions drive the migration engine instead of the
    /// row engine — same disk, same fault model, same crash boundaries.
    pub migrating: Option<MigrationEngine>,
    pub disk: SimDisk,
    /// I/O operations performed so far (reads, writes, and fsyncs all count).
    pub io_count: u64,
    /// Die immediately before performing I/O op with this index.
    pub crash_after: Option<u64>,
    /// Report I/O op with this index as failed (EIO). A failed write still
    /// dirties the page cache first — a failed syscall may have partially
    /// succeeded; a failed fsync syncs nothing.
    pub fail_after: Option<u64>,
    /// Misdirect the write at this I/O index (non-write ops are unaffected;
    /// see `misdirected` to check whether it actually fired).
    pub misdirect_at: Option<(u64, Misdirect)>,
    /// How many writes were actually misdirected.
    pub misdirected: u64,
    /// Corrupt the read at this I/O index *in flight*: flip `mask` into the
    /// returned buffer at `byte`, leaving the disk untouched (bus/DMA/cache
    /// corruption — a retry would see clean data, but the engine never gets
    /// one). `(io_index, byte, mask)`.
    pub read_corrupt_at: Option<(u64, usize, u8)>,
    /// How many reads were corrupted in flight.
    pub reads_corrupted: u64,
    /// Misdirect the read at this I/O index: return bytes from
    /// `offset + shift` instead (firmware read from the wrong sector). The
    /// data is *valid data from the wrong place* — the hardest case.
    pub read_misdirect_at: Option<(u64, i64)>,
    /// How many reads were misdirected.
    pub reads_misdirected: u64,
    /// The fsync at this I/O index LIES: reports success, persists nothing
    /// (fsyncgate). Unsynced writes silently remain unsynced.
    pub lie_fsync_at: Option<u64>,
    /// EVERY fsync from this I/O index on lies — the full fsyncgate
    /// scenario, where one swallowed error turns all subsequent fsyncs
    /// into silent no-ops. Enables arbitrarily deep rollback.
    pub lie_fsync_from: Option<u64>,
    /// How many fsyncs lied.
    pub fsyncs_lied: u64,
    /// Reads performed (for simulated-time accounting).
    pub n_reads: u64,
    /// Writes performed.
    pub n_writes: u64,
    /// Fsyncs performed (honest or lying — the caller waited either way).
    pub n_fsyncs: u64,
}

impl SimHost {
    pub fn new(caps: Capacities, disk: SimDisk, crash_after: Option<u64>) -> Self {
        SimHost {
            engine: Engine::new(caps),
            migrating: None,
            disk,
            io_count: 0,
            crash_after,
            fail_after: None,
            misdirect_at: None,
            misdirected: 0,
            read_corrupt_at: None,
            reads_corrupted: 0,
            read_misdirect_at: None,
            reads_misdirected: 0,
            lie_fsync_at: None,
            lie_fsync_from: None,
            fsyncs_lied: 0,
            n_reads: 0,
            n_writes: 0,
            n_fsyncs: 0,
        }
    }

    fn at_crash_boundary(&mut self) -> bool {
        if Some(self.io_count) == self.crash_after {
            return true;
        }
        self.io_count += 1;
        false
    }

    /// True if the op just counted by `at_crash_boundary` must fail.
    /// (`io_count` was already advanced past it.)
    fn this_op_fails(&self) -> bool {
        Some(self.io_count - 1) == self.fail_after
    }

    fn this_op_misdirects(&self) -> Option<Misdirect> {
        match self.misdirect_at {
            Some((idx, kind)) if idx == self.io_count - 1 => Some(kind),
            _ => None,
        }
    }

    /// Tick whichever machine is active: the migration engine while a
    /// migration is in flight, the row engine otherwise. One dispatch
    /// point, so the fault-injection loop below is shared verbatim.
    fn tick_machine(&mut self, input: Input<'_>) -> Output {
        match &mut self.migrating {
            Some(m) => m.tick(input),
            None => self.engine.tick(input),
        }
    }

    fn drive(&mut self, first: Input<'_>) -> Driven {
        let out = self.tick_machine(first);
        self.drive_from(out)
    }

    fn drive_from(&mut self, first_out: Output) -> Driven {
        let mut out = first_out;
        loop {
            match out {
                Output::Read { file, offset, len } => {
                    if self.at_crash_boundary() {
                        return Driven::Crashed;
                    }
                    self.n_reads += 1;
                    if self.this_op_fails() {
                        out = self.tick_machine(Input::IoFailed { file });
                        continue;
                    }
                    let idx = self.io_count - 1;
                    let eff_offset = match self.read_misdirect_at {
                        Some((at, shift)) if at == idx && offset as i64 + shift >= 0 => {
                            self.reads_misdirected += 1;
                            (offset as i64 + shift) as u64
                        }
                        _ => offset,
                    };
                    let mut data = self.disk.read(file, eff_offset, len);
                    if let Some((at, byte, mask)) = self.read_corrupt_at {
                        if at == idx && byte < data.len() {
                            data[byte] ^= mask;
                            self.reads_corrupted += 1;
                        }
                    }
                    out = self.tick_machine(Input::ReadDone { file, data: &data });
                }
                Output::Write { file, offset, data } => {
                    if self.at_crash_boundary() {
                        return Driven::Crashed;
                    }
                    self.n_writes += 1;
                    if self.this_op_fails() {
                        // The syscall failed, but the pages may already be
                        // dirty: model the worst case (write applied to the
                        // cache, never acknowledged).
                        self.disk.write(file, offset, data.as_slice());
                        out = self.tick_machine(Input::IoFailed { file });
                        continue;
                    }
                    match self.this_op_misdirects() {
                        Some(Misdirect::Shift(shift)) if offset as i64 + shift >= 0 => {
                            self.disk
                                .write(file, (offset as i64 + shift) as u64, data.as_slice());
                            self.misdirected += 1;
                        }
                        Some(Misdirect::CrossFile) => {
                            let other = match file {
                                FileId::Superblock => FileId::Rows,
                                FileId::Rows => FileId::Superblock,
                                // A misdirect near the legacy file lands in
                                // the live rows file: the nastiest target.
                                FileId::RowsOld => FileId::Rows,
                            };
                            self.disk.write(other, offset, data.as_slice());
                            self.misdirected += 1;
                        }
                        // A shift below offset zero cannot land anywhere:
                        // write normally (the sweep counts real hits).
                        Some(Misdirect::Shift(_)) | None => {
                            self.disk.write(file, offset, data.as_slice());
                        }
                    }
                    out = self.tick_machine(Input::WriteDone { file });
                }
                Output::Fsync { file } => {
                    if self.at_crash_boundary() {
                        return Driven::Crashed;
                    }
                    self.n_fsyncs += 1;
                    if self.this_op_fails() {
                        out = self.tick_machine(Input::IoFailed { file });
                        continue;
                    }
                    let idx = self.io_count - 1;
                    let lying = Some(idx) == self.lie_fsync_at
                        || self.lie_fsync_from.is_some_and(|from| idx >= from);
                    if lying {
                        // fsyncgate: success reported, nothing persisted.
                        self.fsyncs_lied += 1;
                    } else {
                        self.disk.fsync(file);
                    }
                    out = self.tick_machine(Input::FsyncDone { file });
                }
                terminal => return Driven::Done(terminal),
            }
        }
    }

    /// Open the database (fresh init or recovery, depending on disk state).
    pub fn open(&mut self) -> Driven {
        let input = Input::Open {
            superblock_len: self.disk.len(FileId::Superblock),
            rows_len: self.disk.len(FileId::Rows),
        };
        self.drive(input)
    }

    /// Run the offline migration (docs/DESIGN.md §4.8) under the same
    /// fault model as everything else: every crash/EIO/misdirection knob
    /// applies to migration I/O exactly as it does to commits. The row
    /// engine is untouched — after a successful migration, `open()` on a
    /// fresh host recovers the migrated file.
    pub fn run_migration(&mut self) -> Driven {
        assert!(self.migrating.is_none(), "migration already in flight");
        let mut m = MigrationEngine::new(self.engine.caps());
        let first = m.start(
            self.disk.len(FileId::Superblock),
            self.disk.len(FileId::RowsOld),
        );
        self.migrating = Some(m);
        let driven = self.drive_from(first);
        self.migrating = None;
        driven
    }

    /// Drive a client input produced by the generated query surface
    /// (`dabqlite_core::generated::queries`) to completion.
    pub fn run_input(&mut self, input: Input<'static>) -> Driven {
        assert!(
            matches!(
                input,
                Input::Insert { .. } | Input::Get { .. } | Input::Range { .. }
            ),
            "run_input takes client operations, not I/O completions"
        );
        self.drive(input)
    }

    pub fn run(&mut self, op: ClientOp) -> Driven {
        match op {
            ClientOp::Insert { id, value } => self.drive(Input::Insert { id, value }),
            ClientOp::Get { id } => self.drive(Input::Get { id }),
        }
    }

    /// Convenience: get that must complete (pure in-memory, no I/O).
    pub fn get(&mut self, id: u64) -> Option<[u8; VALUE_LEN]> {
        match self.run(ClientOp::Get { id }) {
            Driven::Done(Output::GetDone { result: Ok(v), .. }) => v,
            other => panic!("get({id}) did not complete cleanly: {other:?}"),
        }
    }

    /// Full paged range scan `lo..=hi`, concatenated: a test convenience
    /// over the bounded-page protocol (each page is one `Input::Range`).
    pub fn range_all(&mut self, lo: u64, hi: u64) -> Vec<(u64, [u8; VALUE_LEN])> {
        let mut out = Vec::new();
        let mut cursor = lo;
        loop {
            let page = match self.run_input(Input::Range { lo: cursor, hi }) {
                Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
                other => panic!("range_all: {other:?}"),
            };
            out.extend_from_slice(&page.items[..page.count as usize]);
            match page.next {
                Some(n) => cursor = n,
                None => return out,
            }
        }
    }
}
