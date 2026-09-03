//! # dabqlite-web
//!
//! The OPFS storage backend (docs/DESIGN.md §8.1) — the browser half of
//! the promise that wasm and OPFS are "first-class targets, not a port".
//!
//! ## Why this crate is mostly platform-independent
//!
//! An OPFS `FileSystemSyncAccessHandle` offers exactly four operations:
//! `getSize`, `read(buffer, {at})`, `write(buffer, {at})`, and `flush`.
//! That is the whole API — and it is precisely the shape the [`Storage`]
//! trait was designed around (§8.1: "the trait should be shaped by the
//! most awkward platform, not by POSIX").
//!
//! So this crate splits in two:
//!
//! - [`SyncHandle`] models those four operations and nothing else, and
//!   [`OpfsStorage`] implements [`Storage`] on top of *any* implementor.
//!   All the contract logic that could actually be wrong — EOF clamping,
//!   short-read/short-write resumption, gap zero-filling, size tracking —
//!   lives here, in ordinary portable Rust.
//! - [`opfs`] (wasm32 only) is the thin binding from those four methods
//!   to the real browser API, plus the async handle acquisition.
//!
//! The payoff is that the interesting half is testable *natively and
//! deterministically*: [`fake::FakeHandle`] reproduces OPFS semantics
//! (including short I/O and zero-filled gaps), and the backend is driven
//! through the same three-way equivalence and fault-outcome suites that
//! validate the POSIX backend. The browser then proves the remaining
//! assumption — that real OPFS behaves as modeled — against actual
//! Chromium, on actual OPFS, in a dedicated worker.
//!
//! ## Single-writer, for free
//!
//! §8.1 requires deciding the multi-tab story early. OPFS decides it for
//! us: a sync access handle takes an **exclusive per-file lock** unless
//! opened `readwrite-unsafe`, so a second worker's `createSyncAccessHandle`
//! rejects with `NoModificationAllowedError`. That is the same guarantee
//! `flock` gives the POSIX backend (design §2: "one writer, always"),
//! enforced by the platform rather than by convention — and, like
//! `flock`, released when the holder goes away.

#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use dabqlite_core::FileId;
use dabqlite_host::Storage;

pub mod fake;

#[cfg(target_arch = "wasm32")]
pub mod opfs;

/// The four operations an OPFS `FileSystemSyncAccessHandle` provides.
///
/// Implementors must reproduce OPFS (and POSIX `pwrite`) semantics:
///
/// - `read_at` fills as much of `buf` as the file has from `at` onward
///   and returns how many bytes that was; reading at or past EOF yields
///   `Ok(0)` rather than an error.
/// - `write_at` writes `buf` at `at`, extending the file if needed, and
///   **zero-filling any gap** between the old end and `at`. It returns
///   how many bytes were written.
/// - `flush` makes prior writes durable to the extent the platform
///   allows (see the durability note on [`OpfsStorage`]).
pub trait SyncHandle {
    type Error: core::fmt::Debug;

    fn size(&self) -> Result<u64, Self::Error>;
    fn read_at(&self, buf: &mut [u8], at: u64) -> Result<usize, Self::Error>;
    fn write_at(&self, buf: &[u8], at: u64) -> Result<usize, Self::Error>;
    fn flush(&self) -> Result<(), Self::Error>;
}

/// What can go wrong in the backend: the handle's own failures, plus the
/// two contract violations that would silently corrupt data if ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpfsError<E> {
    /// The underlying handle failed (a `DOMException` in the browser).
    Handle(E),
    /// A read made no progress before satisfying the request. Never
    /// observed from a conforming handle — but silently returning a
    /// half-filled buffer would hand the engine garbage, so it is an
    /// error, loudly.
    ShortRead {
        file: FileId,
        want: usize,
        got: usize,
    },
    /// A write made no progress. Same reasoning: a partial write that
    /// reported success would be exactly the "silent data loss" this
    /// project exists to make impossible.
    ShortWrite {
        file: FileId,
        want: usize,
        got: usize,
    },
}

impl<E> From<E> for OpfsError<E> {
    fn from(e: E) -> Self {
        OpfsError::Handle(e)
    }
}

/// The declared file set (docs/DESIGN.md §4.4) as three held sync access
/// handles, addressed by [`FileId`].
///
/// ## Durability honesty
///
/// `flush()` is OPFS's fsync, and the design is explicit that browser
/// durability is best-effort (§5: "OPFS `flush()` does not carry
/// POSIX-level crash guarantees"). Everything the engine builds on
/// *ordering* still holds — the commit protocol's write/flush sequence is
/// preserved — but a browser tab killed by the OS may lose a flush the
/// engine believed durable. That is the same fault class as a lying
/// fsync, which the simulator sweeps exhaustively (`fsync_lies.rs`):
/// the guarantee that survives is prefix consistency, never silent
/// divergence. See docs/FAULTS.md.
pub struct OpfsStorage<H> {
    superblock: H,
    rows: H,
    rows_old: H,
}

impl<H: SyncHandle> OpfsStorage<H> {
    /// Build a backend from three already-acquired handles. On wasm the
    /// handles come from [`opfs::open_dir`]; natively they come from the
    /// fake, which is how the contract suites run without a browser.
    pub fn from_handles(superblock: H, rows: H, rows_old: H) -> Self {
        OpfsStorage {
            superblock,
            rows,
            rows_old,
        }
    }

    fn handle(&self, id: FileId) -> &H {
        match id {
            FileId::Superblock => &self.superblock,
            FileId::Rows => &self.rows,
            FileId::RowsOld => &self.rows_old,
        }
    }
}

impl<H: SyncHandle> Storage for OpfsStorage<H> {
    type Error = OpfsError<H::Error>;

    fn len(&mut self, file: FileId) -> Result<u64, Self::Error> {
        Ok(self.handle(file).size()?)
    }

    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Self::Error> {
        // Clamp to EOF, matching the POSIX backend and the simulator
        // exactly: a short or empty result is not an error (see the
        // `Storage` contract). Clamping here also means we never hand
        // OPFS a request that straddles EOF, so a short read from the
        // handle would signal a real anomaly rather than normal EOF.
        let handle = self.handle(file);
        let file_len = handle.size()?;
        let start = offset.min(file_len);
        let end = offset.saturating_add(len).min(file_len);
        let mut buf = vec![0u8; (end - start) as usize];

        let mut done = 0usize;
        while done < buf.len() {
            let n = handle.read_at(&mut buf[done..], start + done as u64)?;
            if n == 0 {
                // No progress: refuse rather than serve a partly-filled
                // buffer the engine would decode as data.
                return Err(OpfsError::ShortRead {
                    file,
                    want: buf.len(),
                    got: done,
                });
            }
            done += n;
        }
        Ok(buf)
    }

    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Self::Error> {
        // Writing past EOF extends the file, zero-filling the gap — the
        // same behavior as POSIX `pwrite` and the simulator.
        let handle = self.handle(file);
        let mut done = 0usize;
        while done < data.len() {
            let n = handle.write_at(&data[done..], offset + done as u64)?;
            if n == 0 {
                return Err(OpfsError::ShortWrite {
                    file,
                    want: data.len(),
                    got: done,
                });
            }
            done += n;
        }
        Ok(())
    }

    fn sync(&mut self, file: FileId) -> Result<(), Self::Error> {
        Ok(self.handle(file).flush()?)
    }
}
