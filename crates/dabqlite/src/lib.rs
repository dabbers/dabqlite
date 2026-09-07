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

use dabqlite_core::{BatchOp, Capacities, DbError, Output, VALUE_LEN as CORE_VALUE_LEN};
use dabqlite_host::Host;

pub use dabqlite_core::{DbError as EngineError, FindCursor, RecoveryReport, VALUE_LEN};

// The backends, re-exported so that `Db<S>` can actually be WRITTEN DOWN by
// a caller. Without these a database could only ever be a local binding
// whose type was inferred — never a struct field, a function parameter, a
// return type, or a trait impl. Three independent sample projects each hit
// this within minutes and each had to invent its own type erasure to work
// around it.
pub use dabqlite_host::{MemoryStorage, Storage};
#[cfg(unix)]
pub use dabqlite_host::{PosixStorage, ReadOnlyDir};

/// A database backed by real files. The type you put in a struct.
#[cfg(unix)]
pub type FileDb = Db<PosixStorage>;
/// A database held entirely in memory.
pub type MemDb = Db<MemoryStorage>;
/// A damaged database opened read-only for rescue (see [`Db::salvage`]).
#[cfg(unix)]
pub type SalvageDb = Db<ReadOnlyDir>;

/// The capacity recorded in a superblock image, if it holds one.
///
/// Read directly rather than through the engine because the arena has to
/// be sized before the engine exists — the capacity is the one thing that
/// must be known before anything else can be.
fn recorded_capacity(superblock: &[u8]) -> Option<u64> {
    use dabqlite_core::layout::{decode_sb, SB_COPIES, SB_COPY_SIZE};
    // The highest generation among valid copies is the live one, exactly
    // as recovery decides it.
    (0..SB_COPIES)
        .filter_map(|slot| {
            let at = slot * SB_COPY_SIZE;
            decode_sb(superblock.get(at..at + SB_COPY_SIZE)?).ok()
        })
        .max_by_key(|c| c.generation)
        .map(|c| c.capacity)
        .filter(|&c| c > 0)
}

/// Rows a database can hold when you do not say otherwise.
///
/// Capacity is declared at open and the arena is allocated once, so this
/// is a memory decision as much as a size one: 64 Ki rows is about 2 MiB
/// of row arena. Use [`Db::open_with`] or [`Db::in_memory_with`] to pick.
pub const DEFAULT_ROWS: u64 = 65_536;

/// A stored value: any byte string up to [`MAX_VALUE_LEN`].
///
/// Values are exact. What you put in is what comes out, byte for byte,
/// including trailing zeros — a value is stored with its length, not
/// padded to a slot and guessed at on the way back.
///
/// A value longer than one row slot is stored across several, all written
/// in a single commit, so a long value is as atomic and as crash-safe as a
/// short one. [`Value::from_bytes`] refuses anything above the ceiling
/// rather than truncating it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Value(Vec<u8>);

impl Value {
    /// An empty value. Distinct from an absent row: a key can hold zero
    /// bytes and still be present.
    pub fn empty() -> Self {
        Value(Vec::new())
    }

    /// Store text. Fails only if it is longer than [`MAX_VALUE_LEN`].
    pub fn from_text(text: &str) -> Result<Self, Error> {
        Self::from_bytes(text.as_bytes())
    }

    /// Store bytes. Fails only if they are longer than [`MAX_VALUE_LEN`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_VALUE_LEN {
            return Err(Error::ValueTooLong {
                len: bytes.len(),
                max: MAX_VALUE_LEN,
            });
        }
        Ok(Value(bytes.to_vec()))
    }

    /// Take ownership of a byte vector as a value.
    pub fn from_vec(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() > MAX_VALUE_LEN {
            return Err(Error::ValueTooLong {
                len: bytes.len(),
                max: MAX_VALUE_LEN,
            });
        }
        Ok(Value(bytes))
    }

    /// The bytes, exactly as stored. No trimming, no padding, no guessing.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The bytes, taken.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value as text, lossily — invalid UTF-8 becomes replacement
    /// characters rather than an error, because a display path should not
    /// be able to fail.
    pub fn text(&self) -> alloc_string::String {
        alloc_string::String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl AsRef<[u8]> for Value {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

mod alloc_string {
    pub use std::string::String;
}

impl TryFrom<&[u8]> for Value {
    type Error = Error;
    fn try_from(v: &[u8]) -> Result<Self, Error> {
        Value::from_bytes(v)
    }
}

impl TryFrom<Vec<u8>> for Value {
    type Error = Error;
    fn try_from(v: Vec<u8>) -> Result<Self, Error> {
        Value::from_vec(v)
    }
}

impl TryFrom<&str> for Value {
    type Error = Error;
    fn try_from(v: &str) -> Result<Self, Error> {
        Value::from_text(v)
    }
}

/// One write inside a [`Db::batch`].
///
/// The constructors read better than the struct literals at a call site —
/// `Op::put(id, v)` beside `Op::remove(id)` — so prefer them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Add a row. Refuses the batch if the id is already there.
    Insert { id: u64, value: Value },
    /// Replace a row's value. Refuses the batch if the id is absent.
    Update { id: u64, value: Value },
    /// Insert or replace, whichever applies.
    Put { id: u64, value: Value },
    /// Delete a row. Refuses the batch if the id is absent.
    Delete { id: u64 },
    /// Delete a row if it is there; do nothing if it is not.
    Remove { id: u64 },
}

impl Op {
    pub fn insert(id: u64, value: Value) -> Self {
        Op::Insert { id, value }
    }
    pub fn update(id: u64, value: Value) -> Self {
        Op::Update { id, value }
    }
    pub fn put(id: u64, value: Value) -> Self {
        Op::Put { id, value }
    }
    pub fn delete(id: u64) -> Self {
        Op::Delete { id }
    }
    pub fn remove(id: u64) -> Self {
        Op::Remove { id }
    }

    fn to_core(&self) -> BatchOp<'_> {
        match self {
            Op::Insert { id, value } => BatchOp::Insert {
                id: *id,
                value: value.as_bytes(),
            },
            Op::Update { id, value } => BatchOp::Update {
                id: *id,
                value: value.as_bytes(),
            },
            Op::Put { id, value } => BatchOp::Put {
                id: *id,
                value: value.as_bytes(),
            },
            Op::Delete { id } => BatchOp::Delete { id: *id },
            Op::Remove { id } => BatchOp::Remove { id: *id },
        }
    }
}

/// The most operations one [`Db::batch`] may carry.
///
/// Fixed by the on-disk format: each row of a commit records how many
/// rows follow it in the same commit, and that field has a range
/// (docs/FORMAT.md). Larger workloads split into several batches — each
/// one still atomic in itself.
pub const MAX_BATCH: usize = dabqlite_core::MAX_COMMIT_ROWS;

/// The longest value this store will hold.
///
/// A value is written as a run of row slots inside ONE commit — that is
/// what makes a long value atomic — so its ceiling is the longest commit
/// the on-disk format can describe. Larger payloads belong in object
/// storage, with a key or URL stored here.
pub const MAX_VALUE_LEN: usize = dabqlite_core::MAX_VALUE_LEN;

/// Everything that can go wrong, in the caller's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The row is not there.
    NotFound { id: u64 },
    /// A row with this id already exists. Use [`Db::put`] to overwrite.
    AlreadyExists { id: u64 },
    /// The database is at its declared row capacity.
    ///
    /// `dead` is how many of those slots hold nothing useful — records
    /// that were superseded or deleted, and the tombstones that retired
    /// them. If it is nonzero, [`Db::compact`] gets them back, and it
    /// needs no free slot to do it. If it is zero the database is
    /// genuinely full of live rows, and the only way on is to reopen with
    /// a larger capacity.
    Full { capacity: u64, dead: u64 },
    /// The capacity asked for at open is smaller than the data already on
    /// disk. Reopen with at least `required`.
    ///
    /// Distinct from [`Error::Full`] on purpose: these are opposite
    /// situations, and conflating them produced a message that stated the
    /// reverse of the truth ("full at its declared capacity of 50" when
    /// you asked for 10 and there were 50).
    CapacityTooSmall { required: u64, asked: u64 },
    /// Another process holds the single-writer lock. Retryable, and
    /// deliberately NOT an [`Error::Io`]: contention is a normal
    /// condition, a failing disk is not.
    Locked { detail: String },
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
    ///
    /// `kind` is the classification a caller can branch on without
    /// reading English: `StorageFull` means run a compaction or make
    /// room, `PermissionDenied` means fix the mount, and the rest mean
    /// the volume is in trouble. Before it existed the only way to tell
    /// ENOSPC from EACCES from EIO was to substring-match a
    /// `Debug`-rendered `io::Error`, which is the antipattern
    /// [`Error::Locked`] was introduced to remove.
    Io {
        kind: std::io::ErrorKind,
        detail: String,
    },
    /// The batch needs more row slots than one commit can carry. NOT the
    /// same as [`Error::Full`], which is the database being out of room:
    /// a batch can be too long while the database is nearly empty.
    /// Split the work into several batches — each one still atomic in
    /// itself — or shorten the values.
    BatchTooLong { rows: usize, max: usize },
    /// A [`Db::batch`] was refused, and nothing in it was applied. `at` is
    /// the index of the operation that stopped it and `cause` is the error
    /// that operation would have returned on its own.
    ///
    /// The batch is validated in full before any byte is written, so this
    /// is never a partial write to clean up — the database is exactly as
    /// it was before the call.
    BatchRejected { at: usize, cause: Box<Error> },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotFound { id } => write!(f, "no row with id {id}"),
            Error::AlreadyExists { id } => write!(f, "row {id} already exists"),
            Error::Full { capacity, dead: 0 } => write!(
                f,
                "database is full at its declared capacity of {capacity} rows, and \
                 every slot holds live data; reopen it with a larger capacity"
            ),
            Error::Full { capacity, dead } => write!(
                f,
                "database is full at its declared capacity of {capacity} rows, but \
                 {dead} of them are dead weight; Db::compact() reclaims them and \
                 needs no free slot to do it"
            ),
            Error::CapacityTooSmall { required, asked } => write!(
                f,
                "opened with room for {asked} rows, but {required} are already \
                 stored; reopen with at least {required}"
            ),
            Error::Locked { detail } => write!(f, "database is open by another writer: {detail}"),
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
            Error::Io { kind, detail } => write!(f, "storage failed ({kind:?}): {detail}"),
            Error::BatchTooLong { rows, max } => write!(
                f,
                "this batch needs {rows} row slots and one commit holds {max}; \
                 split it into several batches, each still atomic in itself"
            ),
            Error::BatchRejected { at, cause } => write!(
                f,
                "batch refused at operation {at} ({cause}); nothing in it was applied"
            ),
        }
    }
}

impl std::error::Error for Error {
    /// The underlying cause, where there is one. Only a rejected batch has
    /// one today: it wraps the error of the operation that stopped it, so
    /// `source()` reaches the real reason without the caller having to
    /// destructure.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::BatchRejected { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound { id } => Error::NotFound { id },
            DbError::DuplicateId { id } => Error::AlreadyExists { id },
            DbError::Full { capacity, dead, .. } => Error::Full { capacity, dead },
            DbError::Degraded { quarantined } => Error::Degraded { quarantined },
            DbError::Corrupt { what } => Error::Corrupt { what },
            DbError::SchemaMismatch {
                file_schema,
                binary_schema,
            } => Error::SchemaMismatch {
                file_schema,
                binary: binary_schema,
            },
            DbError::CapacityBelowData {
                required,
                configured,
            } => Error::CapacityTooSmall {
                required,
                asked: configured,
            },
            DbError::BatchTooLong { rows, max } => Error::BatchTooLong {
                rows: rows as usize,
                max: max as usize,
            },
            DbError::ValueTooLong { len, max } => Error::ValueTooLong {
                len: len as usize,
                max: max as usize,
            },
            // The engine reports WHICH file failed, not why — the host
            // knows why, and keeps it in `Host::last_error`. Callers who
            // need the reason read it there; this is the shape the engine
            // can honestly produce.
            DbError::IoFailed { file } => Error::Io {
                kind: std::io::ErrorKind::Other,
                detail: format!("the {file:?} file failed"),
            },
            DbError::Busy => Error::Io {
                kind: std::io::ErrorKind::WouldBlock,
                detail: "an operation is already in flight".into(),
            },
            DbError::NotOpen => Error::Io {
                kind: std::io::ErrorKind::Other,
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
    /// `None` only while [`Db::compact`] has released the old handle and
    /// not yet attached the new one. That window cannot yield, so no
    /// caller can observe it — unless the reattachment itself fails, in
    /// which case the accessors below say so and say what to do.
    host: Option<Host<S>>,
    /// Where this database lives, when it lives somewhere. Kept so it can
    /// compact itself in place without the caller having to hand the path
    /// back.
    #[cfg(unix)]
    origin: Option<(std::path::PathBuf, u64)>,
}

/// What a detached handle says when used. Reachable only after a
/// `compact` whose final reopen failed — the data is safe either way,
/// because the swap is crash-safe and the next open resolves it.
const DETACHED: &str = "this database handle was detached by a failed compaction; \
                        the directory is intact — reopen it with Db::open";

impl<S: Storage> Db<S> {
    fn h(&self) -> &Host<S> {
        self.host.as_ref().expect(DETACHED)
    }
    fn hm(&mut self) -> &mut Host<S> {
        self.host.as_mut().expect(DETACHED)
    }
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
        let rows = recorded_capacity(&snapshot.superblock).unwrap_or(DEFAULT_ROWS);
        Self::load_with(snapshot, rows)
    }

    /// Write a snapshot into a directory as a real database, atomically.
    ///
    /// The inverse of [`Db::snapshot`], which had none: a snapshot IS the
    /// two files, but there was no way to put them back on disk safely,
    /// so every application that saved one wrote its own
    /// temp-file-and-rename. This builds the database in a sibling
    /// directory, fsyncs it, and swaps it in with the same crash-safe
    /// rename sequence [`Db::compact`] uses — so a crash leaves either the
    /// old contents or the new ones, never a mixture.
    ///
    /// Refuses to overwrite a database another writer holds open.
    #[cfg(unix)]
    pub fn restore(
        path: impl AsRef<std::path::Path>,
        snapshot: &Snapshot,
    ) -> Result<Db<PosixStorage>, Error> {
        let path = path.as_ref();
        let staging = sibling(path, COMPACT_STAGING);
        let retired = sibling(path, COMPACT_RETIRED);
        let rows = recorded_capacity(&snapshot.superblock).unwrap_or(DEFAULT_ROWS);

        // Refuse before touching anything if someone is using the target.
        if path.exists() {
            let probe = Db::<PosixStorage>::open_with(path, rows)?;
            drop(probe);
        }

        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&retired);
        {
            let mut source = Db::load_with(snapshot, rows)?;
            let live = source.all()?;
            let mut fresh = Db::<PosixStorage>::open_with(&staging, rows)?;
            fresh.refill(live)?;
        }
        sync_dir(&staging)?;

        if path.exists() {
            std::fs::rename(path, &retired).map_err(fs_err)?;
        }
        std::fs::rename(&staging, path).map_err(fs_err)?;
        if let Some(parent) = path.parent() {
            let _ = sync_dir(parent);
        }
        let _ = std::fs::remove_dir_all(&retired);
        Db::open_with(path, rows)
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
        let path = path.as_ref();
        // A database remembers the capacity it was created with, so
        // reopening it does not have to be told again — and a caller who
        // forgets does not silently get a different ceiling (and a
        // different memory footprint) than the one they chose.
        let rows = recorded_capacity(
            &std::fs::read(path.join(dabqlite_host::SUPERBLOCK_FILE)).unwrap_or_default(),
        )
        .unwrap_or(DEFAULT_ROWS);
        Self::open_with(path, rows)
    }

    /// As [`Db::open`], with a chosen row capacity, overriding whatever
    /// the database recorded.
    ///
    /// The new number takes effect immediately for this handle and is
    /// recorded in the file at the next commit — a capacity is written by
    /// writing, never by opening, so an open cannot modify a database it
    /// was only asked to read. The capacity must be
    /// at least as large as the data already there.
    pub fn open_with(path: impl AsRef<std::path::Path>, rows: u64) -> Result<Self, Error> {
        let path = path.as_ref();
        // Finish any compaction that a crash interrupted, before anything
        // looks at the directory.
        finish_interrupted_compaction(path)?;
        let storage = PosixStorage::open_dir(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Error::Locked {
                    detail: e.to_string(),
                }
            } else {
                fs_err(e)
            }
        })?;
        let mut db = Self::start(Host::new(caps(rows), storage))?;
        db.origin = Some((path.to_path_buf(), rows.max(1)));
        Ok(db)
    }

    /// Rebuild this database in place, dropping the slots that deletes and
    /// updates retired. The answer to [`Error::Full`] when
    /// [`Stats::dead`] is nonzero.
    ///
    /// Crash-safe by construction, and it is the library's job rather than
    /// the caller's: the compacted copy is built in a sibling directory
    /// and only then swapped in, so an interruption at any point leaves
    /// either the old database or the new one — never a mixture. An
    /// interrupted swap is finished automatically by the next
    /// [`Db::open`].
    ///
    /// Note that compaction reclaims DEAD slots only. A database whose
    /// capacity is genuinely full of live rows needs a larger capacity,
    /// not a rebuild, and says so.
    pub fn compact(&mut self) -> Result<(), Error> {
        let (path, rows) = self.origin.clone().ok_or_else(|| Error::Io {
            kind: std::io::ErrorKind::Unsupported,
            detail: "this database was not opened from a path".into(),
        })?;
        let staging = sibling(&path, COMPACT_STAGING);
        let retired = sibling(&path, COMPACT_RETIRED);

        let live = self.all()?;
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&retired);
        {
            let mut fresh = Db::open_with(&staging, rows)?;
            fresh.refill(live)?;
            // Drop to release the staging lock before the swap.
        }
        sync_dir(&staging)?;

        // Release the single-writer lock before touching the directory.
        // From here to the reopen at the bottom this handle is detached,
        // and nothing between the two can yield to a caller.
        drop(self.host.take());

        // The swap. Each step is a rename, and the reopen below resolves
        // every point a crash can land between them.
        let swap = (|| -> Result<(), Error> {
            std::fs::rename(&path, &retired).map_err(fs_err)?;
            std::fs::rename(&staging, &path).map_err(fs_err)?;
            if let Some(parent) = path.parent() {
                let _ = sync_dir(parent);
            }
            let _ = std::fs::remove_dir_all(&retired);
            Ok(())
        })();

        // Reattach whatever the directory now holds, whether or not the
        // swap got all the way through: `open_with` finishes an
        // interrupted compaction, so this lands on a correct database
        // either way.
        let reopened = Db::open_with(&path, rows)?;
        self.host = reopened.host;
        self.origin = reopened.origin;
        swap
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
        let path = path.as_ref();
        Self::salvage_with(path, Self::recorded_rows(path))
    }

    /// Open a HEALTHY database read-only, taking no writer lock.
    ///
    /// The reader every application built on this store asked for. It sees
    /// the database as of the moment it opened — a committed generation,
    /// never a half-written one, because a commit only becomes visible
    /// when the superblock flips and the previous generation always
    /// survives in its own pair of slots. Later writes by the writer are
    /// not visible to it; reopen for a newer view.
    ///
    /// Any number of these can run at once, alongside the single writer.
    /// They write nothing at all — not one byte, not one fsync — so a
    /// reader cannot slow a writer down or damage anything.
    ///
    /// It is the same machinery as [`Db::salvage`], which is the point:
    /// the honest read-only mode already existed, and a healthy database
    /// opened this way behaves exactly like a normal one. On a DAMAGED
    /// database this will report [`Error::Degraded`] for questions the
    /// quarantine makes unanswerable; call `salvage` when that is what you
    /// are expecting, so the intent is in the code.
    pub fn read_only(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::salvage_with(path, Self::recorded_rows(path))
    }

    /// The capacity this database recorded, or the default if it has not
    /// recorded one. A reader has to size its arena before it can read the
    /// superblock, same as a writer.
    fn recorded_rows(path: &std::path::Path) -> u64 {
        recorded_capacity(
            &std::fs::read(path.join(dabqlite_host::SUPERBLOCK_FILE)).unwrap_or_default(),
        )
        .unwrap_or(DEFAULT_ROWS)
    }

    /// As [`Db::salvage`] or [`Db::read_only`], with a chosen row capacity.
    pub fn salvage_with(path: impl AsRef<std::path::Path>, rows: u64) -> Result<Self, Error> {
        let storage = ReadOnlyDir::open_dir(path.as_ref()).map_err(fs_err)?;
        let mut host = Host::new(caps(rows), storage);
        match host.open_salvage().map_err(io_err::<ReadOnlyDir>)? {
            Output::OpenDone { result: Ok(_) } => Ok(Db {
                host: Some(host),
                origin: None,
            }),
            Output::OpenDone { result: Err(e) } => Err(e.into()),
            other => unreachable!("open returned {other:?}"),
        }
    }
}

#[cfg(unix)]
const COMPACT_STAGING: &str = ".compacting";
#[cfg(unix)]
const COMPACT_RETIRED: &str = ".retired";

#[cfg(unix)]
fn sibling(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(unix)]
fn sync_dir(path: &std::path::Path) -> Result<(), Error> {
    std::fs::File::open(path)
        .and_then(|d| d.sync_all())
        .map_err(fs_err)
}

/// Resolve a compaction that a crash interrupted.
///
/// The swap is `rename(live -> retired)` then `rename(staging -> live)`,
/// so exactly three states are possible afterwards, and each has one
/// correct resolution:
///
/// - live present, retired present → the swap completed; drop the retired
///   copy (a crash before cleanup);
/// - live MISSING, retired present → the crash landed between the two
///   renames; put the old database back, since the new one was not yet
///   in place;
/// - anything else → nothing to do.
///
/// Staging is always discarded: a half-built copy is worth nothing, and
/// the original is untouched either way.
#[cfg(unix)]
fn finish_interrupted_compaction(path: &std::path::Path) -> Result<(), Error> {
    let retired = sibling(path, COMPACT_RETIRED);
    let staging = sibling(path, COMPACT_STAGING);
    if retired.exists() {
        if path.exists() {
            std::fs::remove_dir_all(&retired).map_err(fs_err)?;
        } else {
            std::fs::rename(&retired, path).map_err(fs_err)?;
        }
    }
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(fs_err)?;
    }
    Ok(())
}

fn caps(rows: u64) -> Capacities {
    Capacities { rows: rows.max(1) }
}

/// Turn a backend error into the caller's, asking the backend how to
/// classify it (see `Storage::classify`).
fn io_err<S: Storage>(e: S::Error) -> Error {
    Error::Io {
        kind: S::classify(&e),
        detail: format!("{e:?}"),
    }
}

/// The same, for the facade's own filesystem work, where the error is a
/// real `io::Error` that already carries a kind and a readable message.
fn fs_err(e: std::io::Error) -> Error {
    Error::Io {
        kind: e.kind(),
        detail: e.to_string(),
    }
}

const _: () = assert!(VALUE_LEN == CORE_VALUE_LEN);

impl<S: Storage> Db<S> {
    fn start(mut host: Host<S>) -> Result<Self, Error> {
        match host.open().map_err(io_err::<S>)? {
            Output::OpenDone { result: Ok(_) } => Ok(Db {
                host: Some(host),
                #[cfg(unix)]
                origin: None,
            }),
            Output::OpenDone { result: Err(e) } => Err(e.into()),
            other => unreachable!("open returned {other:?}"),
        }
    }

    /// Add a row. Fails if the id is taken — use [`Db::put`] to overwrite.
    pub fn insert(&mut self, id: u64, value: Value) -> Result<(), Error> {
        self.one(Op::Insert { id, value })
    }

    /// Replace an existing row's value, atomically. Fails if it is absent.
    pub fn update(&mut self, id: u64, value: Value) -> Result<(), Error> {
        self.one(Op::Update { id, value })
    }

    /// Insert or replace, whichever applies — one atomic commit either way.
    ///
    /// Not insert-then-update-on-failure: the engine decides which it is
    /// while the batch is being validated, so there is no window between
    /// the decision and the write for anything to change underneath it.
    pub fn put(&mut self, id: u64, value: Value) -> Result<(), Error> {
        self.one(Op::Put { id, value })
    }

    /// Delete a row. Fails if it is absent; see [`Db::remove`] for the
    /// forgiving version.
    pub fn delete(&mut self, id: u64) -> Result<(), Error> {
        self.one(Op::Delete { id })
    }

    /// One write, as a one-operation batch.
    ///
    /// Every write goes through the same path, so a value spanning
    /// several row slots is handled identically whether it arrives alone
    /// or in company — and a single write costs exactly what it always
    /// did, two fsyncs.
    fn one(&mut self, op: Op) -> Result<(), Error> {
        match self.batch(core::slice::from_ref(&op)) {
            // A one-op batch can only be refused at operation 0, and the
            // caller asked for one operation: give them its error, not a
            // wrapper around it.
            Err(Error::BatchRejected { cause, .. }) => Err(*cause),
            other => other,
        }
    }

    /// Apply several writes as ONE atomic commit.
    ///
    /// Either every operation lands or none does — including across a
    /// crash, a kill, or a storage failure in the middle. There is no
    /// window in which half a batch is visible, so an invariant that spans
    /// rows ("this row moves to done exactly when that one is deleted")
    /// can be maintained without a journal of your own.
    ///
    /// It is also the throughput lever. A single write costs two fsyncs;
    /// a batch of `n` writes costs the same two, not `2n`. Nothing about
    /// durability is traded away for that — every row is still made
    /// durable before the superblock that makes it visible.
    ///
    /// Each operation sees the state the ones before it in the same batch
    /// would leave, so `[insert(5, a), delete(5), insert(5, b)]` is legal
    /// and means what it reads as, and [`Op::Put`] resolves to an insert
    /// or a replace with no read-then-write gap to race in.
    ///
    /// If any operation is refused, the batch is refused whole with
    /// [`Error::BatchRejected`] naming which one and why, and NOTHING is
    /// written — not even the operations before it. Batches are limited to
    /// [`MAX_BATCH`] operations.
    ///
    /// ```no_run
    /// # use dabqlite::{MemDb, Op, Value};
    /// # fn main() -> Result<(), dabqlite::Error> {
    /// # let mut db = MemDb::in_memory_with(64)?;
    /// db.batch(&[
    ///     Op::put(1, Value::from_bytes(b"ready")?),
    ///     Op::put(2, Value::from_bytes(b"ready")?),
    ///     Op::remove(3),
    /// ])?;
    /// # Ok(()) }
    /// ```
    pub fn batch(&mut self, ops: &[Op]) -> Result<(), Error> {
        // The core takes its own op type; translating here keeps the
        // public surface free of `[u8; 16]`.
        let mut core_ops = Vec::with_capacity(ops.len());
        core_ops.extend(ops.iter().map(|op| op.to_core()));
        match self.hm().batch(&core_ops) {
            Output::BatchDone { result: Ok(()), .. } => Ok(()),
            Output::BatchDone {
                result: Err(reject),
                ..
            } => Err(Error::BatchRejected {
                at: reject.at as usize,
                cause: Box::new(reject.error.into()),
            }),
            other => unreachable!("batch returned {other:?}"),
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
    ///
    /// A value longer than one row slot is reassembled here from the
    /// bounded windows the engine hands back — the core never allocates,
    /// and the caller never sees a partial value.
    pub fn get(&mut self, id: u64) -> Result<Option<Value>, Error> {
        use dabqlite_core::Input;
        let first = match self.hm().get(id) {
            Output::GetDone { result: Ok(v), .. } => v,
            Output::GetDone { result: Err(e), .. } => return Err(e.into()),
            other => unreachable!("get returned {other:?}"),
        };
        let Some(first) = first else { return Ok(None) };
        let mut bytes = Vec::with_capacity(first.total as usize);
        bytes.extend_from_slice(first.payload());
        let mut next = first.next_offset();
        while let Some(offset) = next {
            let window = match self.hm().run(Input::GetFrom { id, offset }) {
                Output::GetDone {
                    result: Ok(Some(w)),
                    ..
                } => w,
                Output::GetDone {
                    result: Ok(None), ..
                } => {
                    unreachable!("a value vanished between windows of one read")
                }
                Output::GetDone { result: Err(e), .. } => return Err(e.into()),
                other => unreachable!("get returned {other:?}"),
            };
            bytes.extend_from_slice(window.payload());
            next = window.next_offset();
        }
        debug_assert_eq!(bytes.len(), first.total as usize);
        Ok(Some(Value(bytes)))
    }

    /// The largest id this database has ever held, live or since
    /// deleted — the number to hand out next if you are allocating ids.
    /// `None` when nothing has ever been inserted.
    ///
    /// Constant-ish time, not a scan. Without it the only way to ask was
    /// to read every row and take the maximum, and then to cache the
    /// answer in a row of its own — which one sample application did, and
    /// measured at 49,999 dead slots after 50,000 inserts.
    pub fn max_id(&self) -> Option<u64> {
        self.h().engine.max_id()
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
        match self.hm().run(Input::Range { lo, hi }) {
            Output::RangeDone { result: Ok(page) } => {
                let items: Vec<dabqlite_core::RowRef> = page.items[..page.count as usize].to_vec();
                let next = page.next;
                Ok((self.rows_from(&items)?, next))
            }
            Output::RangeDone { result: Err(e) } => Err(e.into()),
            other => unreachable!("range returned {other:?}"),
        }
    }

    /// Turn a page of scan references into rows, reading back any value
    /// too long to travel in the page itself. A page carries the whole
    /// value when it fits and its LENGTH when it does not, so a long
    /// value costs an extra read rather than arriving silently truncated.
    fn rows_from(&mut self, items: &[dabqlite_core::RowRef]) -> Result<Vec<Row>, Error> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item.value() {
                Some(bytes) => out.push((item.id, Value(bytes.to_vec()))),
                None => {
                    let value = self.get(item.id)?.ok_or(Error::NotFound { id: item.id })?;
                    out.push((item.id, value));
                }
            }
        }
        Ok(out)
    }

    /// Every row, ascending by id.
    pub fn all(&mut self) -> Result<Vec<Row>, Error> {
        self.range(0, u64::MAX)
    }

    /// Every row whose value contains `needle`, NEWEST FIRST.
    ///
    /// Exact: the index only narrows candidates, and each one is verified
    /// against the actual bytes — including bytes that straddle the slot
    /// boundary of a value too long for one row.
    ///
    /// Newest-first because that is the order the index can page cheaply
    /// and the order a search box wants; see [`Db::find_page`] to stop
    /// early rather than collecting every match.
    pub fn find(&mut self, needle: &[u8]) -> Result<Vec<Row>, Error> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let (page, next) = self.find_page(needle, cursor)?;
            out.extend(page);
            match next {
                Some(c) => cursor = Some(c),
                None => return Ok(out),
            }
        }
    }

    /// One bounded page of a substring search, plus where to continue
    /// from. `None` for `after` starts at the newest match.
    ///
    /// Paging costs the same per page however many matches there are, so
    /// a search box can show its first results without paying for the
    /// long tail — which is the point of stopping early.
    pub fn find_page(
        &mut self,
        needle: &[u8],
        after: Option<FindCursor>,
    ) -> Result<(Vec<Row>, Option<FindCursor>), Error> {
        use dabqlite_core::Input;
        if needle.len() > VALUE_LEN {
            return Err(Error::ValueTooLong {
                len: needle.len(),
                max: VALUE_LEN,
            });
        }
        let mut padded = [0u8; VALUE_LEN];
        padded[..needle.len()].copy_from_slice(needle);
        let page = match self.hm().run(Input::Find {
            needle: padded,
            needle_len: needle.len() as u8,
            after,
        }) {
            Output::FindDone { result: Ok(p) } => p,
            Output::FindDone { result: Err(e) } => return Err(e.into()),
            other => unreachable!("find returned {other:?}"),
        };
        let items: Vec<dabqlite_core::RowRef> = page.items[..page.count as usize].to_vec();
        let next = page.next;
        Ok((self.rows_from(&items)?, next))
    }

    /// Text convenience over [`Db::find`].
    pub fn find_text(&mut self, needle: &str) -> Result<Vec<Row>, Error> {
        self.find(needle.as_bytes())
    }

    /// How many rows you can read.
    pub fn len(&self) -> u64 {
        self.h().engine.live_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Capacity and dead-weight accounting.
    pub fn stats(&self) -> Stats {
        let (slots, capacity) = self.h().engine.usage();
        Stats {
            live: self.h().engine.live_count(),
            slots,
            dead: self.h().engine.dead_slots(),
            capacity,
        }
    }

    /// True when this database was opened in salvage mode and some rows
    /// could not be verified.
    pub fn is_degraded(&self) -> bool {
        self.h().engine.is_degraded()
    }

    /// What recovery found when this database was opened. Check
    /// `rollback_evidence` after opening and alarm on it: it means
    /// acknowledged writes were lost to a fault outside the design's
    /// budget, and the on-disk evidence survived to prove it.
    pub fn recovery_report(&self) -> RecoveryReport {
        self.h().engine.recovery_report()
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
            let len = self.hm().storage.len(file).map_err(io_err::<S>)?;
            self.hm().storage.read(file, 0, len).map_err(io_err::<S>)
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
        out.refill(rows)?;
        Ok(out)
    }

    /// Write `rows` into an empty database, batched.
    ///
    /// Deliberately not a loop of `insert`: rebuilding a 60,000-row
    /// database one commit at a time is 120,000 fsyncs where a few hundred
    /// will do, and a library that does not use its own batch API to move
    /// its own data is not making a serious offer.
    ///
    /// Batches are packed by ROW cost, not by operation count, because a
    /// value spanning several slots takes several of the commit's rows.
    fn refill(&mut self, rows: Vec<Row>) -> Result<(), Error> {
        let mut ops: Vec<Op> = Vec::with_capacity(MAX_BATCH);
        let mut staged = 0usize;
        for (id, value) in rows {
            let cost = value.len().div_ceil(VALUE_LEN).max(1);
            if cost > MAX_BATCH {
                // Unreachable while MAX_VALUE_LEN is bounded by the commit
                // length, but stated rather than assumed.
                return Err(Error::ValueTooLong {
                    len: value.len(),
                    max: MAX_VALUE_LEN,
                });
            }
            if staged + cost > MAX_BATCH {
                self.batch(&ops)?;
                ops.clear();
                staged = 0;
            }
            staged += cost;
            ops.push(Op::Insert { id, value });
        }
        if !ops.is_empty() {
            self.batch(&ops)?;
        }
        Ok(())
    }
}
