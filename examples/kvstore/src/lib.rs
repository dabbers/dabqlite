//! `kv` — a durable command-line key/value and session store built on
//! dabqlite.
//!
//! **One key/value record is one dabqlite value.** That sentence is the
//! whole port. A value used to be exactly sixteen bytes, so this crate
//! carried a storage layer of its own: the u64 id was split into a record
//! number and a chunk ordinal, the payload was cut into 16-byte rows, two
//! banks were kept so an update could be undone, a header row was written
//! last as a commit point, and a sidecar file remembered the capacity the
//! library forgot. All of that is gone.
//!
//! What is left is what a key/value store on a `u64`-keyed table still has
//! to do for itself:
//!
//! * [`record`] — the bytes of one record, and the hash that turns a
//!   string key into a row id.
//! * [`store`]  — [`store::Store`], a `Db<S>` plus linear probing, TTL,
//!   listing and search.
//! * [`exec`]   — command dispatch.
//! * [`cli`]    — argument parsing and rendering.
//!
//! ## What the library still makes us do
//!
//! - **String keys are ours.** The table is keyed by `u64`, so a key is
//!   hashed and collisions are probed past. That means our own tombstones
//!   (a removed row is indistinguishable from one that never existed, so
//!   `Db::remove` would cut a probe chain), and it means `list` is a full
//!   scan and a sort, because id order is hash order.
//! - **Expiry is ours.** There is no TTL and no expression that can be
//!   evaluated at read time, so every record carries a timestamp and every
//!   read compares it.
//! - **"Is there a database in this directory?" is ours.** The superblock
//!   filename is not part of the public API and there is no call that
//!   answers the question, so `exec.rs` hardcodes the name.

pub mod cli;
pub mod exec;
pub mod record;
pub mod store;

use std::fmt;
use std::path::{Path, PathBuf};

/// Slots held back so that `del` and `purge` still work when the database
/// is otherwise full. A full dabqlite refuses deletes too — a delete
/// appends a tombstone row and so needs a free slot.
pub const RESERVED_SLOTS: u64 = 8;

/// Everything `kv` can fail with.
#[derive(Debug)]
pub enum KvError {
    /// Straight through from the library.
    Db(dabqlite::Error),
    /// Another process holds the single-writer lock.
    Locked {
        dir: PathBuf,
        detail: String,
    },
    /// The database is damaged; `kv rescue` may still get the data out.
    Damaged {
        dir: PathBuf,
        what: String,
    },
    /// A row exists that this crate's record layout cannot explain.
    Layout(String),
    KeyTooLong {
        len: usize,
        max: usize,
    },
    ValueTooLong {
        len: usize,
        max: usize,
    },
    BadKey(String),
    ProbeExhausted {
        probes: u64,
    },
    /// Not enough row slots left for the write, with the reserve honoured.
    NoRoom {
        needed: u64,
        free: u64,
        capacity: u64,
    },
    /// The requested capacity is below what the data already needs.
    CapacityTooSmall {
        asked: u64,
        required: u64,
    },
    Io {
        what: String,
        err: std::io::Error,
    },
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
    /// `--rows`. When absent the database's own recorded capacity decides:
    /// it remembers what it was created with, so there is nothing for this
    /// crate to remember. There used to be a `kv-capacity` sidecar file
    /// here, because `Db::open` always reopened at `DEFAULT_ROWS`.
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
}

/// Seconds since the Unix epoch.
///
/// The repository's clippy config disallows `SystemTime::now` because the
/// database core must stay deterministic; an application on top of it is
/// exactly where a clock belongs.
#[allow(clippy::disallowed_methods)]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turn a library error from `open` into something a user can act on.
pub fn open_error(dir: &Path, e: dabqlite::Error) -> KvError {
    match e {
        dabqlite::Error::Locked { detail } => KvError::Locked {
            dir: dir.to_path_buf(),
            detail,
        },
        dabqlite::Error::CapacityTooSmall { required, asked } => {
            KvError::CapacityTooSmall { asked, required }
        }
        dabqlite::Error::Corrupt { what } => KvError::Damaged {
            dir: dir.to_path_buf(),
            what: what.to_string(),
        },
        other => KvError::Db(other),
    }
}
