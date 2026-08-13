//! # dabqlite-host
//!
//! The storage seam and the generic host driver. The core is a pure state
//! machine (no I/O); a *host* performs the I/O the core requests. This crate
//! defines the [`Storage`] trait every backend implements and the
//! [`Host`] driver that runs the engine against any of them.
//!
//! ## The trait is shaped by the most awkward platform
//!
//! Per docs/DESIGN.md §8.1, the seam is modeled on OPFS sync access handles,
//! not POSIX: a fixed set of named files whose handles are acquired once at
//! open and held for the session; per-handle positional read/write, flush,
//! and size — and nothing else. No directory operations after open, no
//! renames, no atomic-replace tricks: platforms don't agree on those, and
//! the crash-consistency story (docs/DESIGN.md §4.4) never needs them.
//!
//! ## Failure semantics
//!
//! Any storage error is delivered to the engine as `Input::IoFailed`, which
//! fail-stops it (see `dabqlite-core`). The driver never retries and never
//! drops an error on the floor: the error value itself is kept in
//! [`Host::last_error`] for diagnostics, and the engine's terminal output
//! carries `DbError::IoFailed`.

use dabqlite_core::{Capacities, Engine, FileId, Input, Output};

#[cfg(unix)]
pub mod posix;

#[cfg(unix)]
pub use posix::PosixStorage;

/// A storage backend: the declared file set (docs/DESIGN.md §4.4), opened
/// once, addressed by [`FileId`].
///
/// Contract, matched exactly by the simulator so tests transfer:
///
/// - `read` returns the bytes in `[offset, offset+len)` clamped to the
///   current file size — a short or empty result is not an error.
/// - `write` extends the file as needed (zero-filling any gap) and is not
///   durable until `sync`.
/// - `sync` makes all previous writes to that file durable (fsync).
/// - Errors are terminal for the session: the driver fail-stops the engine.
pub trait Storage {
    type Error: core::fmt::Debug;

    fn len(&mut self, file: FileId) -> Result<u64, Self::Error>;
    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Self::Error>;
    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Self::Error>;
    fn sync(&mut self, file: FileId) -> Result<(), Self::Error>;
}

/// Drives an [`Engine`] against any [`Storage`]. One request in flight at a
/// time, lockstep, exactly as the core's protocol demands.
pub struct Host<S: Storage> {
    pub engine: Engine,
    pub storage: S,
    /// The storage error that fail-stopped the engine, if any.
    pub last_error: Option<S::Error>,
}

impl<S: Storage> Host<S> {
    pub fn new(caps: Capacities, storage: S) -> Self {
        Host {
            engine: Engine::new(caps),
            storage,
            last_error: None,
        }
    }

    /// Open the database (fresh init or recovery). `Err` is only possible
    /// for the two size probes needed to build the `Open` input; once the
    /// engine is running, storage failures surface as
    /// `OpenDone { result: Err(IoFailed) }` instead.
    pub fn open(&mut self) -> Result<Output, S::Error> {
        let superblock_len = self.storage.len(FileId::Superblock)?;
        let rows_len = self.storage.len(FileId::Rows)?;
        Ok(self.drive(Input::Open {
            superblock_len,
            rows_len,
        }))
    }

    pub fn insert(&mut self, id: u64, value: [u8; dabqlite_core::VALUE_LEN]) -> Output {
        self.drive(Input::Insert { id, value })
    }

    pub fn get(&mut self, id: u64) -> Output {
        self.drive(Input::Get { id })
    }

    fn drive(&mut self, first: Input<'_>) -> Output {
        let mut out = self.engine.tick(first);
        loop {
            match out {
                Output::Read { file, offset, len } => {
                    out = match self.storage.read(file, offset, len) {
                        Ok(data) => self.engine.tick(Input::ReadDone { file, data: &data }),
                        Err(e) => {
                            self.last_error = Some(e);
                            self.engine.tick(Input::IoFailed { file })
                        }
                    };
                }
                Output::Write { file, offset, data } => {
                    out = match self.storage.write(file, offset, data.as_slice()) {
                        Ok(()) => self.engine.tick(Input::WriteDone { file }),
                        Err(e) => {
                            self.last_error = Some(e);
                            self.engine.tick(Input::IoFailed { file })
                        }
                    };
                }
                Output::Fsync { file } => {
                    out = match self.storage.sync(file) {
                        Ok(()) => self.engine.tick(Input::FsyncDone { file }),
                        Err(e) => {
                            self.last_error = Some(e);
                            self.engine.tick(Input::IoFailed { file })
                        }
                    };
                }
                terminal => return terminal,
            }
        }
    }
}
