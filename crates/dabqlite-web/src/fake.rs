//! A faithful in-memory stand-in for an OPFS sync access handle.
//!
//! This is the piece that lets the OPFS backend be validated without a
//! browser: it reproduces the semantics the real API is specified to
//! have — EOF-truncated reads, gap zero-filling on writes past the end,
//! byte-exact positional I/O — and adds the knobs needed to prove the
//! backend handles the awkward cases:
//!
//! - `set_chunk` forces every read/write to move at most N bytes, which
//!   exercises the resumption loops (a real handle may legally return a
//!   short count);
//! - `stall` makes the next operation report zero progress, which must
//!   surface as a loud `ShortRead`/`ShortWrite`, never as a half-filled
//!   buffer handed to the engine;
//! - `fail_after` injects a handle failure, standing in for a
//!   `DOMException` and driving the engine's fail-stop path.
//!
//! The assumptions encoded here are not taken on faith: the browser
//! suite (`tests/opfs_browser.rs`) asserts the same behaviors against
//! real OPFS in real Chromium, so a divergence between this model and
//! the platform is itself a test failure.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::{OpfsStorage, SyncHandle};

/// Stands in for a `DOMException` from the real API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeError(pub &'static str);

/// One file's bytes plus its fault knobs, shared by every handle clone.
#[derive(Default)]
pub struct FakeFile {
    bytes: RefCell<Vec<u8>>,
    /// Maximum bytes moved per read/write call (0 = unlimited).
    chunk: Cell<usize>,
    /// When set, the next operation reports zero progress.
    stall: Cell<bool>,
    /// Fail once this many operations have completed.
    fail_after: Cell<Option<u64>>,
    ops: Cell<u64>,
    flushes: Cell<u64>,
    /// Bytes durable as of the last flush — the model of what a crash
    /// would leave behind.
    flushed: RefCell<Vec<u8>>,
}

impl FakeFile {
    /// The file's current bytes (including unflushed writes).
    pub fn contents(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }
    /// The bytes as of the last `flush()`.
    pub fn flushed_contents(&self) -> Vec<u8> {
        self.flushed.borrow().clone()
    }
    /// Replace the contents, for at-rest damage in fault tests.
    pub fn set_contents(&self, bytes: Vec<u8>) {
        *self.bytes.borrow_mut() = bytes.clone();
        *self.flushed.borrow_mut() = bytes;
    }
    pub fn flushes(&self) -> u64 {
        self.flushes.get()
    }
    /// Move at most `n` bytes per call (0 = unlimited).
    pub fn set_chunk(&self, n: usize) {
        self.chunk.set(n);
    }
    /// Make the next operation report zero progress.
    pub fn stall_once(&self) {
        self.stall.set(true);
    }
    /// Fail every operation from the `n`th onward.
    pub fn fail_after(&self, n: u64) {
        self.fail_after.set(Some(n));
    }

    fn step(&self) -> Result<(), FakeError> {
        let n = self.ops.get();
        self.ops.set(n + 1);
        match self.fail_after.get() {
            Some(limit) if n >= limit => Err(FakeError("injected handle failure")),
            _ => Ok(()),
        }
    }

    fn cap(&self, want: usize) -> usize {
        match self.chunk.get() {
            0 => want,
            c => want.min(c),
        }
    }
}

/// A handle onto a [`FakeFile`]. Cloning yields another handle onto the
/// same bytes, exactly as two references to one open file would.
#[derive(Clone, Default)]
pub struct FakeHandle(Rc<FakeFile>);

impl FakeHandle {
    pub fn new() -> Self {
        FakeHandle(Rc::new(FakeFile::default()))
    }
    /// The shared file behind this handle, for inspection and fault
    /// injection.
    pub fn file(&self) -> Rc<FakeFile> {
        Rc::clone(&self.0)
    }
    pub fn contents(&self) -> Vec<u8> {
        self.0.contents()
    }
}

impl SyncHandle for FakeHandle {
    type Error = FakeError;

    fn size(&self) -> Result<u64, FakeError> {
        self.0.step()?;
        Ok(self.0.bytes.borrow().len() as u64)
    }

    fn read_at(&self, buf: &mut [u8], at: u64) -> Result<usize, FakeError> {
        self.0.step()?;
        if self.0.stall.replace(false) {
            return Ok(0);
        }
        let bytes = self.0.bytes.borrow();
        let at = at as usize;
        if at >= bytes.len() || buf.is_empty() {
            // Reading at or past EOF is not an error; it moves 0 bytes.
            return Ok(0);
        }
        let n = self.0.cap(buf.len().min(bytes.len() - at));
        buf[..n].copy_from_slice(&bytes[at..at + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], at: u64) -> Result<usize, FakeError> {
        self.0.step()?;
        if self.0.stall.replace(false) {
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let n = self.0.cap(buf.len());
        let mut bytes = self.0.bytes.borrow_mut();
        let at = at as usize;
        // Writing past the end extends the file, zero-filling the gap.
        if at + n > bytes.len() {
            bytes.resize(at + n, 0);
        }
        bytes[at..at + n].copy_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&self) -> Result<(), FakeError> {
        self.0.step()?;
        self.0.flushes.set(self.0.flushes.get() + 1);
        *self.0.flushed.borrow_mut() = self.0.bytes.borrow().clone();
        Ok(())
    }
}

/// The declared file set as fakes: a ready-to-drive [`OpfsStorage`] plus
/// the three files, kept for inspection.
pub struct FakeSet {
    pub superblock: Rc<FakeFile>,
    pub rows: Rc<FakeFile>,
    pub rows_old: Rc<FakeFile>,
}

impl FakeSet {
    /// Build a backend and the handles onto its files.
    pub fn new() -> (OpfsStorage<FakeHandle>, FakeSet) {
        let superblock = FakeHandle::new();
        let rows = FakeHandle::new();
        let rows_old = FakeHandle::new();
        let set = FakeSet {
            superblock: superblock.file(),
            rows: rows.file(),
            rows_old: rows_old.file(),
        };
        (OpfsStorage::from_handles(superblock, rows, rows_old), set)
    }

    /// Reopen: a new backend over the SAME files, as a fresh worker
    /// acquiring handles to an existing database would see them.
    pub fn reopen(&self) -> OpfsStorage<FakeHandle> {
        OpfsStorage::from_handles(
            FakeHandle(Rc::clone(&self.superblock)),
            FakeHandle(Rc::clone(&self.rows)),
            FakeHandle(Rc::clone(&self.rows_old)),
        )
    }
}
