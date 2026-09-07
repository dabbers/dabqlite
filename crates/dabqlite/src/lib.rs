//! # dabqlite
//!
//! An embeddable record store that is hard to lose data with.
//!
//! ```
//! use dabqlite::{Db, Value};
//!
//! let mut db = Db::in_memory()?;
//! db.insert(1, Value::from_text("hello")?)?;
//! db.update(1, Value::from_text("hello again")?)?;
//! assert_eq!(db.get(1)?.map(|v| v.text().to_string()), Some("hello again".into()));
//! assert!(db.remove(1)?);
//! assert!(db.is_empty());
//! # Ok::<(), dabqlite::Error>(())
//! ```
//!
//! ## What you get
//!
//! - **Crash safety as the default, not a mode.** Every write — insert,
//!   update, delete — is one appended row plus a manifest flip, so a
//!   crash at any point leaves the operation entirely applied or entirely
//!   not. There is no journal to replay and no configuration that makes
//!   it unsafe.
//! - **Corruption is contained, not fatal.** A damaged row costs that row,
//!   not the database ([`Db::salvage`]), and a rebuild recovers the rest.
//! - **The same database everywhere.** In memory, on POSIX files, or in a
//!   browser on OPFS — byte-identical files, verified against each other.
//!
//! ## Choosing a backend
//!
//! [`Db::in_memory`] needs nothing and runs anywhere, including browsers
//! without OPFS; it has no durability, and [`Db::snapshot`] /
//! [`Db::load`] move the bytes wherever you can keep them.
//! [`Db::open`] uses real files with real fsyncs and full durability.
//! Both speak the same API, and a snapshot taken from one opens in the
//! other.
//!
//! ## The v1 shape, stated plainly
//!
//! One table of `(id: u64, value: [u8; 16])`, a declared row capacity
//! fixed at open, and one writer at a time. Values are 16 bytes because
//! the schema says so ([`VALUE_LEN`]); [`Value`] helps you pack text into
//! them and tells you when it does not fit rather than truncating.

use dabqlite_core::{Capacities, DbError, Output, VALUE_LEN as CORE_VALUE_LEN};
use dabqlite_host::{Host, MemoryStorage, Storage};

#[cfg(unix)]
use dabqlite_host::{PosixStorage, ReadOnlyDir};

pub use dabqlite_core::{DbError as EngineError, RecoveryReport, VALUE_LEN};

/// Rows a database can hold when you do not say otherwise.
///
/// Capacity is declared at open and the arena is allocated once, so this
/// is a memory decision as much as a size one: 64 Ki rows is about 2 MiB
/// of row arena. Use [`Db::open_with`] or [`Db::in_memory_with`] to pick.
pub const DEFAULT_ROWS: u64 = 65_536;

/// A fixed-width value. Sixteen bytes, because that is what the compiled
/// schema declares; [`Value::from_text`] and [`Value::from_bytes`] refuse
/// anything longer rather than silently truncating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Value(pub [u8; VALUE_LEN]);

impl Value {
    /// Pack text into a value, zero-padded. Fails if it does not fit.
    pub fn from_text(text: &str) -> Result<Self, Error> {
        Self::from_bytes(text.as_bytes())
    }

    /// Pack bytes into a value, zero-padded. Fails if they do not fit.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > VALUE_LEN {
            return Err(Error::ValueTooLong {
                len: bytes.len(),
                max: VALUE_LEN,
            });
        }
        let mut v = [0u8; VALUE_LEN];
        v[..bytes.len()].copy_from_slice(bytes);
        Ok(Value(v))
    }

    /// The bytes up to the first zero pad — what [`Value::from_bytes`] was
    /// given, assuming it did not itself end in zeros.
    pub fn as_bytes(&self) -> &[u8] {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(VALUE_LEN);
        &self.0[..end]
    }

    /// The value as text, lossily — invalid UTF-8 becomes replacement
    /// characters rather than an error, because a display path should not
    /// be able to fail.
    pub fn text(&self) -> alloc_string::String {
        alloc_string::String::from_utf8_lossy(self.as_bytes()).into_owned()
    }

    /// The raw, padded bytes.
    pub fn raw(&self) -> [u8; VALUE_LEN] {
        self.0
    }
}

mod alloc_string {
    pub use std::string::String;
}

impl From<[u8; VALUE_LEN]> for Value {
    fn from(v: [u8; VALUE_LEN]) -> Self {
        Value(v)
    }
}

/// Everything that can go wrong, in the caller's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The row is not there.
    NotFound { id: u64 },
    /// A row with this id already exists. Use [`Db::put`] to overwrite.
    AlreadyExists { id: u64 },
    /// The database is at its declared row capacity. Reopen with a larger
    /// one, or rebuild to reclaim the slots deletes and updates consumed.
    Full { capacity: u64 },
    /// The value is longer than a row can hold.
    ValueTooLong { len: usize, max: usize },
    /// The database is open in salvage mode with unreadable rows, and this
    /// question cannot be answered honestly. Rebuild to clear it.
    Degraded { quarantined: u64 },
    /// The on-disk state is damaged. Try [`Db::salvage`] to read what
    /// survives, then rebuild.
    Corrupt { what: &'static str },
    /// The files were written by a different schema version.
    SchemaMismatch { file_schema: u64, binary: u64 },
    /// Storage failed. The database has fail-stopped; reopen it.
    Io { detail: String },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotFound { id } => write!(f, "no row with id {id}"),
            Error::AlreadyExists { id } => write!(f, "row {id} already exists"),
            Error::Full { capacity } => write!(
                f,
                "database is full at its declared capacity of {capacity} rows"
            ),
            Error::ValueTooLong { len, max } => {
                write!(f, "value is {len} bytes; the row holds {max}")
            }
            Error::Degraded { quarantined } => write!(
                f,
                "database is degraded: {quarantined} unreadable row(s); \
                 rebuild to clear"
            ),
            Error::Corrupt { what } => write!(f, "database is damaged: {what}"),
            Error::SchemaMismatch {
                file_schema,
                binary,
            } => write!(
                f,
                "files were written by schema 0x{file_schema:016X}, this build is \
                 0x{binary:016X}"
            ),
            Error::Io { detail } => write!(f, "storage failed: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound { id } => Error::NotFound { id },
            DbError::DuplicateId { id } => Error::AlreadyExists { id },
            DbError::Full { capacity, .. } => Error::Full { capacity },
            DbError::Degraded { quarantined } => Error::Degraded { quarantined },
            DbError::Corrupt { what } => Error::Corrupt { what },
            DbError::SchemaMismatch {
                file_schema,
                binary_schema,
            } => Error::SchemaMismatch {
                file_schema,
                binary: binary_schema,
            },
            DbError::CapacityBelowData { required, .. } => Error::Full { capacity: required },
            DbError::IoFailed { file } => Error::Io {
                detail: format!("{file:?}"),
            },
            DbError::Busy => Error::Io {
                detail: "an operation is already in flight".into(),
            },
            DbError::NotOpen => Error::Io {
                detail: "database is not open".into(),
            },
        }
    }
}

/// How full the database is, and how much of it is dead weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Rows you can read.
    pub live: u64,
    /// Row slots consumed — every insert, update and delete takes one.
    pub slots: u64,
    /// Slots holding superseded or deleted rows. Rebuild to reclaim.
    pub dead: u64,
    /// The declared ceiling.
    pub capacity: u64,
}

impl Stats {
    /// Fraction of capacity consumed, 0.0..=1.0. Watch this rather than
    /// waiting for [`Error::Full`].
    pub fn fill(&self) -> f64 {
        if self.capacity == 0 {
            return 1.0;
        }
        self.slots as f64 / self.capacity as f64
    }
}

/// A database's bytes, for moving it somewhere that is not a filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    superblock: Vec<u8>,
    rows: Vec<u8>,
}

const SNAP_MAGIC: &[u8; 8] = b"DABQSNP1";

impl Snapshot {
    /// One self-describing blob: hand it to IndexedDB, a download, a
    /// server, anywhere that takes bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.superblock.len() + self.rows.len());
        out.extend_from_slice(SNAP_MAGIC);
        out.extend_from_slice(&(self.superblock.len() as u64).to_le_bytes());
        out.extend_from_slice(&(self.rows.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.superblock);
        out.extend_from_slice(&self.rows);
        out
    }

    /// Parse a blob from [`Snapshot::to_bytes`]. Refuses anything it does
    /// not recognise rather than guessing at a layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let bad = |what: &'static str| Error::Corrupt { what };
        if bytes.len() < 24 || &bytes[..8] != SNAP_MAGIC {
            return Err(bad("not a dabqlite snapshot"));
        }
        let sb_len = u64::from_le_bytes(bytes[8..16].try_into().expect("fixed")) as usize;
        let rows_len = u64::from_le_bytes(bytes[16..24].try_into().expect("fixed")) as usize;
        if bytes.len() != 24 + sb_len + rows_len {
            return Err(bad("snapshot length does not match its header"));
        }
        Ok(Snapshot {
            superblock: bytes[24..24 + sb_len].to_vec(),
            rows: bytes[24 + sb_len..].to_vec(),
        })
    }
}

/// A row as the API hands it back.
pub type Row = (u64, Value);

/// One page of results, plus where to continue from (`None` = the end).
pub type Page = (Vec<Row>, Option<u64>);

/// An open database.
pub struct Db<S: Storage> {
    host: Host<S>,
}

impl<S: Storage> core::fmt::Debug for Db<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = self.stats();
        f.debug_struct("Db")
            .field("live", &s.live)
            .field("slots", &s.slots)
            .field("dead", &s.dead)
            .field("capacity", &s.capacity)
            .field("degraded", &self.is_degraded())
            .finish()
    }
}

impl Db<MemoryStorage> {
    /// A database in memory. Runs anywhere; no durability — see
    /// [`Db::snapshot`].
    pub fn in_memory() -> Result<Self, Error> {
        Self::in_memory_with(DEFAULT_ROWS)
    }

    /// As [`Db::in_memory`], with a chosen row capacity.
    pub fn in_memory_with(rows: u64) -> Result<Self, Error> {
        Self::start(Host::new(caps(rows), MemoryStorage::new()))
    }

    /// Reopen a snapshot taken by [`Db::snapshot`] — or produced by a
    /// file-backed database, since the bytes are the same.
    pub fn load(snapshot: &Snapshot) -> Result<Self, Error> {
        Self::load_with(snapshot, DEFAULT_ROWS)
    }

    /// As [`Db::load`], with a chosen row capacity.
    pub fn load_with(snapshot: &Snapshot, rows: u64) -> Result<Self, Error> {
        let storage = MemoryStorage::from_images(
            snapshot.superblock.clone(),
            snapshot.rows.clone(),
            Vec::new(),
        );
        Self::start(Host::new(caps(rows), storage))
    }
}

#[cfg(unix)]
impl Db<PosixStorage> {
    /// Open (or create) a database in a directory, with real files and
    /// real fsyncs. Takes the single-writer lock for as long as it lives.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        Self::open_with(path, DEFAULT_ROWS)
    }

    /// As [`Db::open`], with a chosen row capacity. The capacity must be
    /// at least as large as the data already there.
    pub fn open_with(path: impl AsRef<std::path::Path>, rows: u64) -> Result<Self, Error> {
        let storage = PosixStorage::open_dir(path.as_ref()).map_err(io_err)?;
        Self::start(Host::new(caps(rows), storage))
    }
}

#[cfg(unix)]
impl Db<ReadOnlyDir> {
    /// Open a DAMAGED database read-only, quarantining rows that cannot be
    /// verified so the rest stays readable.
    ///
    /// Rows it serves are checksum-verified and exactly right. Questions
    /// the quarantine makes unanswerable — a `get` that misses, any write —
    /// return [`Error::Degraded`] rather than a confident wrong answer.
    /// Takes no lock and writes nothing, so it is safe on a failing volume.
    pub fn salvage(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        Self::salvage_with(path, DEFAULT_ROWS)
    }

    /// As [`Db::salvage`], with a chosen row capacity.
    pub fn salvage_with(path: impl AsRef<std::path::Path>, rows: u64) -> Result<Self, Error> {
        let storage = ReadOnlyDir::open_dir(path.as_ref()).map_err(io_err)?;
        let mut host = Host::new(caps(rows), storage);
        match host.open_salvage().map_err(io_err)? {
            Output::OpenDone { result: Ok(_) } => Ok(Db { host }),
            Output::OpenDone { result: Err(e) } => Err(e.into()),
            other => unreachable!("open returned {other:?}"),
        }
    }
}

fn caps(rows: u64) -> Capacities {
    Capacities { rows: rows.max(1) }
}

fn io_err<E: core::fmt::Debug>(e: E) -> Error {
    Error::Io {
        detail: format!("{e:?}"),
    }
}

const _: () = assert!(VALUE_LEN == CORE_VALUE_LEN);

impl<S: Storage> Db<S> {
    fn start(mut host: Host<S>) -> Result<Self, Error> {
        match host.open().map_err(io_err)? {
            Output::OpenDone { result: Ok(_) } => Ok(Db { host }),
            Output::OpenDone { result: Err(e) } => Err(e.into()),
            other => unreachable!("open returned {other:?}"),
        }
    }

    /// Add a row. Fails if the id is taken — use [`Db::put`] to overwrite.
    pub fn insert(&mut self, id: u64, value: Value) -> Result<(), Error> {
        match self.host.insert(id, value.0) {
            Output::InsertDone { result: Ok(()), .. } => Ok(()),
            Output::InsertDone { result: Err(e), .. } => Err(e.into()),
            other => unreachable!("insert returned {other:?}"),
        }
    }

    /// Replace an existing row's value, atomically. Fails if it is absent.
    pub fn update(&mut self, id: u64, value: Value) -> Result<(), Error> {
        match self.host.update(id, value.0) {
            Output::UpdateDone { result: Ok(()), .. } => Ok(()),
            Output::UpdateDone { result: Err(e), .. } => Err(e.into()),
            other => unreachable!("update returned {other:?}"),
        }
    }

    /// Insert or replace, whichever applies — one atomic commit either way.
    pub fn put(&mut self, id: u64, value: Value) -> Result<(), Error> {
        match self.insert(id, value) {
            Err(Error::AlreadyExists { .. }) => self.update(id, value),
            other => other,
        }
    }

    /// Delete a row. Fails if it is absent; see [`Db::remove`] for the
    /// forgiving version.
    pub fn delete(&mut self, id: u64) -> Result<(), Error> {
        match self.host.delete(id) {
            Output::DeleteDone { result: Ok(()), .. } => Ok(()),
            Output::DeleteDone { result: Err(e), .. } => Err(e.into()),
            other => unreachable!("delete returned {other:?}"),
        }
    }

    /// Delete a row if it is there. `Ok(false)` means it was not.
    pub fn remove(&mut self, id: u64) -> Result<bool, Error> {
        match self.delete(id) {
            Ok(()) => Ok(true),
            Err(Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Read one row.
    pub fn get(&mut self, id: u64) -> Result<Option<Value>, Error> {
        match self.host.get(id) {
            Output::GetDone { result: Ok(v), .. } => Ok(v.map(Value)),
            Output::GetDone { result: Err(e), .. } => Err(e.into()),
            other => unreachable!("get returned {other:?}"),
        }
    }

    /// Is this id present?
    pub fn contains(&mut self, id: u64) -> Result<bool, Error> {
        Ok(self.get(id)?.is_some())
    }

    /// Every row with `lo <= id <= hi`, in ascending id order.
    ///
    /// Collects the whole range; for a very large one, page with
    /// [`Db::range_page`] instead.
    pub fn range(&mut self, lo: u64, hi: u64) -> Result<Vec<Row>, Error> {
        let mut out = Vec::new();
        let mut cursor = lo;
        loop {
            let (page, next) = self.range_page(cursor, hi)?;
            out.extend(page);
            match next {
                Some(n) => cursor = n,
                None => return Ok(out),
            }
        }
    }

    /// One bounded page of a range, plus where to continue from.
    pub fn range_page(&mut self, lo: u64, hi: u64) -> Result<Page, Error> {
        use dabqlite_core::Input;
        match self.host.run(Input::Range { lo, hi }) {
            Output::RangeDone { result: Ok(page) } => Ok((
                page.items[..page.count as usize]
                    .iter()
                    .map(|&(k, v)| (k, Value(v)))
                    .collect(),
                page.next,
            )),
            Output::RangeDone { result: Err(e) } => Err(e.into()),
            other => unreachable!("range returned {other:?}"),
        }
    }

    /// Every row, ascending by id.
    pub fn all(&mut self) -> Result<Vec<Row>, Error> {
        self.range(0, u64::MAX)
    }

    /// Every row whose value contains `needle`, in insertion order.
    /// Exact: the index only narrows candidates, and each is verified.
    pub fn find(&mut self, needle: &[u8]) -> Result<Vec<Row>, Error> {
        use dabqlite_core::Input;
        if needle.len() > VALUE_LEN {
            return Err(Error::ValueTooLong {
                len: needle.len(),
                max: VALUE_LEN,
            });
        }
        let mut padded = [0u8; VALUE_LEN];
        padded[..needle.len()].copy_from_slice(needle);
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = match self.host.run(Input::Find {
                needle: padded,
                needle_len: needle.len() as u8,
                after,
            }) {
                Output::FindDone { result: Ok(p) } => p,
                Output::FindDone { result: Err(e) } => return Err(e.into()),
                other => unreachable!("find returned {other:?}"),
            };
            out.extend(
                page.items[..page.count as usize]
                    .iter()
                    .map(|&(k, v)| (k, Value(v))),
            );
            match page.next {
                Some(n) => after = Some(n),
                None => return Ok(out),
            }
        }
    }

    /// Text convenience over [`Db::find`].
    pub fn find_text(&mut self, needle: &str) -> Result<Vec<Row>, Error> {
        self.find(needle.as_bytes())
    }

    /// How many rows you can read.
    pub fn len(&self) -> u64 {
        self.host.engine.live_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Capacity and dead-weight accounting.
    pub fn stats(&self) -> Stats {
        let (slots, capacity) = self.host.engine.usage();
        Stats {
            live: self.host.engine.live_count(),
            slots,
            dead: self.host.engine.dead_slots(),
            capacity,
        }
    }

    /// True when this database was opened in salvage mode and some rows
    /// could not be verified.
    pub fn is_degraded(&self) -> bool {
        self.host.engine.is_degraded()
    }

    /// What recovery found when this database was opened. Check
    /// `rollback_evidence` after opening and alarm on it: it means
    /// acknowledged writes were lost to a fault outside the design's
    /// budget, and the on-disk evidence survived to prove it.
    pub fn recovery_report(&self) -> RecoveryReport {
        self.host.engine.recovery_report()
    }

    /// The database's bytes, right now — whatever backend it lives on.
    ///
    /// A snapshot of a file-backed database opens in memory and vice
    /// versa: the bytes are the same, so this is how a database moves
    /// between a server, a browser tab, and a backup, without the caller
    /// knowing anything about the file layout.
    ///
    /// Taken through the storage seam, so it reflects committed state.
    pub fn snapshot(&mut self) -> Result<Snapshot, Error> {
        use dabqlite_core::FileId;
        let mut read_all = |file| -> Result<Vec<u8>, Error> {
            let len = self.host.storage.len(file).map_err(io_err)?;
            self.host.storage.read(file, 0, len).map_err(io_err)
        };
        Ok(Snapshot {
            superblock: read_all(FileId::Superblock)?,
            rows: read_all(FileId::Rows)?,
        })
    }

    /// Copy every readable row into a fresh in-memory database — the
    /// compaction path, and the way to get data out of a degraded one.
    pub fn compact_to_memory(&mut self) -> Result<Db<MemoryStorage>, Error> {
        let rows = self.all()?;
        let mut out = Db::in_memory_with(self.stats().capacity)?;
        for (id, value) in rows {
            out.insert(id, value)?;
        }
        Ok(out)
    }
}
