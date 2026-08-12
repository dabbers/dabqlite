//! The simulated host: drives the engine's I/O requests against a
//! [`SimDisk`], with an optional crash boundary.
//!
//! `crash_after: Some(b)` means the process dies immediately before
//! performing I/O operation number `b` (0-indexed). Sweeping `b` over every
//! operation in a run crashes the process at every I/O boundary
//! (docs/DESIGN.md §7.3).

use dabqlite_core::{Capacities, Engine, FileId, Input, Output, VALUE_LEN};

use crate::disk::SimDisk;

/// A client operation, owned so runs can be replayed verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOp {
    Insert { id: u64, value: [u8; VALUE_LEN] },
    Get { id: u64 },
}

/// Result of driving one client operation to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driven {
    /// The operation finished with this terminal output.
    Done(Output),
    /// The process crashed at the configured I/O boundary. The disk retains
    /// its pre-crash state; call `SimDisk::crash` to settle unsynced writes.
    Crashed,
}

pub struct SimHost {
    pub engine: Engine,
    pub disk: SimDisk,
    /// I/O operations performed so far (reads, writes, and fsyncs all count).
    pub io_count: u64,
    /// Die immediately before performing I/O op with this index.
    pub crash_after: Option<u64>,
}

impl SimHost {
    pub fn new(caps: Capacities, disk: SimDisk, crash_after: Option<u64>) -> Self {
        SimHost {
            engine: Engine::new(caps),
            disk,
            io_count: 0,
            crash_after,
        }
    }

    fn at_crash_boundary(&mut self) -> bool {
        if Some(self.io_count) == self.crash_after {
            return true;
        }
        self.io_count += 1;
        false
    }

    fn drive(&mut self, first: Input<'_>) -> Driven {
        let mut out = self.engine.tick(first);
        loop {
            match out {
                Output::Read { file, offset, len } => {
                    if self.at_crash_boundary() {
                        return Driven::Crashed;
                    }
                    let data = self.disk.read(file, offset, len);
                    out = self.engine.tick(Input::ReadDone { file, data: &data });
                }
                Output::Write { file, offset, data } => {
                    if self.at_crash_boundary() {
                        return Driven::Crashed;
                    }
                    self.disk.write(file, offset, data.as_slice());
                    out = self.engine.tick(Input::WriteDone { file });
                }
                Output::Fsync { file } => {
                    if self.at_crash_boundary() {
                        return Driven::Crashed;
                    }
                    self.disk.fsync(file);
                    out = self.engine.tick(Input::FsyncDone { file });
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
}
