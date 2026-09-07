//! `kv` — a durable command-line key/value and session store built on
//! dabqlite.
//!
//! dabqlite offers one table of `(u64, [u8; 16])` rows with a capacity
//! fixed at open. This crate turns that into string keys, arbitrary-length
//! values, TTLs, listing, substring search and compaction, using only the
//! public `dabqlite` API.
//!
//! The layering is:
//!
//! * [`codec`] — how a record is laid out over row ids and 16-byte rows.
//! * [`plan`]  — pure functions from a snapshot of rows to row writes.
//! * [`exec`]  — the only module that touches a `Db`, because an open
//!   database cannot be named or passed around (see below).
//! * [`cli`]   — argument parsing and rendering.
//!
//! ## The one thing that shaped this design
//!
//! `dabqlite::Db<S: Storage>` is public, but `Storage`, `MemoryStorage`
//! and `PosixStorage` are not re-exported, so no downstream crate can
//! write the type of an open database:
//!
//! ```compile_fail
//! # use dabqlite::Db;
//! struct Store { db: Db<???> }          // no nameable type argument
//! fn get<S>(db: &mut Db<S>) {}          // error: `S: Storage` unsatisfied
//! ```
//!
//! An open database therefore cannot be a struct field, a function
//! parameter, or a return type. Everything below is arranged around that.

pub mod cli;
pub mod codec;
pub mod exec;
pub mod plan;

use std::fmt;
use std::path::{Path, PathBuf};

/// The sidecar file that remembers the row capacity, because dabqlite
/// does not: `Db::open` always applies `DEFAULT_ROWS`, so a database
/// created with a different capacity silently changes size on reopen.
pub const CAPACITY_FILE: &str = "kv-capacity";

/// Slots held back so that `del`, `purge` and `compact` still work when
/// the database is otherwise full. A full dabqlite refuses deletes too —
/// a delete appends a tombstone row and so needs a free slot.
pub const RESERVED_SLOTS: u64 = 8;

/// Everything `kv` can fail with.
#[derive(Debug)]
pub enum KvError {
    /// Straight through from the library.
    Db(dabqlite::Error),
    /// Another process holds the single-writer lock.
    Locked { dir: PathBuf, detail: String },
    /// The database is damaged; `kv rescue` may still get the data out.
    Damaged { dir: PathBuf, what: String },
    /// Rows exist that this crate's record layout cannot explain.
    Layout(String),
    KeyTooLong { len: usize, max: usize },
    ValueTooLong { len: usize, max: usize },
    BadKey(String),
    ProbeExhausted { probes: u64 },
    /// Not enough row slots left for the write, with the reserve honoured.
    NoRoom { needed: u64, free: u64, capacity: u64 },
    /// The requested capacity is below what the data already needs.
    CapacityTooSmall { asked: u64, required: u64 },
    Io { what: String, err: std::io::Error },
    Usage(String),
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KvError::Db(e) => write!(f, "{e}"),
            KvError::Locked { dir, detail } => write!(
                f,
                "{} is already open in another process (single writer): {detail}",
                dir.display()
            ),
            KvError::Damaged { dir, what } => write!(
                f,
                "{} is damaged: {what}\n  try: kv --db {} rescue <newdir>",
                dir.display(),
                dir.display()
            ),
            KvError::Layout(m) => write!(f, "unreadable record layout: {m}"),
            KvError::KeyTooLong { len, max } => {
                write!(f, "key is {len} bytes; the limit is {max}")
            }
            KvError::ValueTooLong { len, max } => {
                write!(f, "value is {len} bytes; this key leaves room for {max}")
            }
            KvError::BadKey(m) => write!(f, "{m}"),
            KvError::ProbeExhausted { probes } => write!(
                f,
                "gave up after {probes} colliding records; run `kv compact`"
            ),
            KvError::NoRoom {
                needed,
                free,
                capacity,
            } => write!(
                f,
                "not enough room: the write needs {needed} row slot(s), {free} of \
                 {capacity} are free (keeping {RESERVED_SLOTS} in reserve so \
                 deletes still work)\n  try: kv compact, or kv --rows <bigger> ...",
            ),
            KvError::CapacityTooSmall { asked, required } => write!(
                f,
                "--rows {asked} is below the {required} row slot(s) already in use"
            ),
            KvError::Io { what, err } => write!(f, "{what}: {err}"),
            KvError::Usage(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for KvError {}

impl From<dabqlite::Error> for KvError {
    fn from(e: dabqlite::Error) -> Self {
        KvError::Db(e)
    }
}

/// Where the database is and how big it may get.
#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    /// `--rows`; when absent the sidecar file or `DEFAULT_ROWS` decides.
    pub rows: Option<u64>,
}

impl Config {
    pub fn new(dir: impl Into<PathBuf>) -> Config {
        Config {
            dir: dir.into(),
            rows: None,
        }
    }

    pub fn with_rows(mut self, rows: u64) -> Config {
        self.rows = Some(rows);
        self
    }

    /// The capacity to open with: the flag, else what we recorded last
    /// time, else the library default.
    pub fn capacity(&self) -> u64 {
        if let Some(r) = self.rows {
            return r.max(1);
        }
        read_capacity(&self.dir).unwrap_or(dabqlite::DEFAULT_ROWS)
    }
}

fn read_capacity(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join(CAPACITY_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn write_capacity(dir: &Path, rows: u64) -> Result<(), KvError> {
    std::fs::write(dir.join(CAPACITY_FILE), format!("{rows}\n")).map_err(|err| KvError::Io {
        what: format!("writing {}", dir.join(CAPACITY_FILE).display()),
        err,
    })
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turn a library error from `open` into something a user can act on.
pub fn open_error(dir: &Path, e: dabqlite::Error) -> KvError {
    match e {
        dabqlite::Error::Io { ref detail } if detail.contains("WouldBlock") => KvError::Locked {
            dir: dir.to_path_buf(),
            detail: detail.clone(),
        },
        dabqlite::Error::Corrupt { what } => KvError::Damaged {
            dir: dir.to_path_buf(),
            what: what.to_string(),
        },
        other => KvError::Db(other),
    }
}
