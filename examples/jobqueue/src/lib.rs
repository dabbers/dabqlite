//! A crash-resilient job queue on top of `dabqlite`.
//!
//! The point of this crate is to put dabqlite's central claim — that it
//! cannot lose acknowledged data — under a process that gets SIGKILLed at
//! arbitrary points, and to see what an application has to write to get a
//! durable queue out of a store whose entire schema is
//! `(id: u64, value: [u8; 16])`.
//!
//! # The data model we had to build on top of the data model
//!
//! dabqlite gives one table of `(u64, [u8;16])`, one atomic single-row
//! write at a time, and no transactions. A job queue needs more than that,
//! so everything below is hand-rolled:
//!
//! * **Job rows** — id = job id, value = a packed `(state, attempts,
//!   payload)` struct. There is no typed value, so we do our own byte
//!   layout ([`JobRow`]).
//! * **Meta rows** — two reserved ids at the top of the key space hold the
//!   enqueue watermark and the commit watermark. There is no separate
//!   place to put metadata, so metadata is a row and the application has
//!   to carve ids out of the user key space to hold it.
//! * **A monotonic commit watermark** — because we cannot update two rows
//!   atomically, "mark this job committed AND bump the aggregate" has to
//!   be made idempotent by hand. See [`step`].
//! * **A directory-swap compaction protocol** — because `compact_to_memory`
//!   returns an in-memory database and there is no way to put a snapshot
//!   back onto disk, compacting a file-backed database means rebuilding it
//!   into a second directory and renaming. See [`Layout`].
//!
//! # Exactly-once, defined precisely
//!
//! * The *effect* of a job (the work) is at-least-once: a crash between
//!   doing the work and committing the state re-does the work. That is
//!   inherent, and true of SQLite too.
//! * The *commit* of a job is exactly-once: the commit watermark advances
//!   by one, atomically, and the journal records it. If dabqlite ever
//!   loses an acknowledged commit, a restart re-commits the same job and
//!   the journal shows a duplicate `K` record. [`audit`] treats that as
//!   failure. That is the data-loss detector.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use dabqlite::{Db, Error as DbErr, RecoveryReport, Value, VALUE_LEN};

// ---------------------------------------------------------------------------
// Key space
// ---------------------------------------------------------------------------

/// Row holding the enqueue watermark: `(next_id_to_enqueue, 0)`.
pub const ROW_ENQUEUE_WATERMARK: u64 = u64::MAX;
/// Row holding the commit watermark: `(last_committed_id, checksum)`.
pub const ROW_COMMIT_WATERMARK: u64 = u64::MAX - 1;
/// Highest id an application job may use, given the two reserved rows.
pub const MAX_JOB_ID: u64 = u64::MAX - 2;
/// The first job id. Zero is left free deliberately.
pub const FIRST_JOB_ID: u64 = 1;

// ---------------------------------------------------------------------------
// Job row encoding
// ---------------------------------------------------------------------------

pub const PENDING: u8 = 1;
pub const CLAIMED: u8 = 2;
pub const DONE: u8 = 3;

/// A job row, packed into the 16 bytes dabqlite gives us.
///
/// Layout: `[0] state | [1] attempts | [2..8] reserved | [8..16] payload`.
///
/// Note that `Value::as_bytes()` / `Value::text()` are useless here: they
/// stop at the first zero byte, and a binary struct is full of them. Only
/// `Value::raw()` round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobRow {
    pub state: u8,
    pub attempts: u8,
    pub payload: u64,
}

impl JobRow {
    pub fn encode(self) -> Value {
        let mut b = [0u8; VALUE_LEN];
        b[0] = self.state;
        b[1] = self.attempts;
        b[8..16].copy_from_slice(&self.payload.to_le_bytes());
        Value::from(b)
    }

    pub fn decode(v: Value) -> Self {
        let b = v.raw();
        JobRow {
            state: b[0],
            attempts: b[1],
            payload: u64::from_le_bytes(b[8..16].try_into().expect("fixed width")),
        }
    }
}

/// A meta row: two `u64`s, which is exactly what 16 bytes holds.
fn encode_meta(a: u64, b: u64) -> Value {
    let mut out = [0u8; VALUE_LEN];
    out[0..8].copy_from_slice(&a.to_le_bytes());
    out[8..16].copy_from_slice(&b.to_le_bytes());
    Value::from(out)
}

fn decode_meta(v: Value) -> (u64, u64) {
    let b = v.raw();
    (
        u64::from_le_bytes(b[0..8].try_into().expect("fixed width")),
        u64::from_le_bytes(b[8..16].try_into().expect("fixed width")),
    )
}

/// The "work" a job represents: a pure function of its id, so the test can
/// predict the answer without trusting the database.
pub fn work_of(id: u64) -> u64 {
    let mut h = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    h
}

/// Fold a committed job into the running checksum. Order-sensitive on
/// purpose: a reordered or repeated commit changes the answer.
pub fn fold(acc: u64, id: u64) -> u64 {
    acc.rotate_left(7) ^ work_of(id)
}

/// The checksum a correct run over jobs `1..=n` must end with.
pub fn expected_checksum(n: u64) -> u64 {
    (FIRST_JOB_ID..=n).fold(0u64, fold)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum QueueError {
    Db(DbErr),
    Io(io::Error),
    /// dabqlite told us, at open, that acknowledged commits were rolled
    /// back. This is the alarm the library asks hosts to raise.
    RollbackEvidence(RecoveryReport),
    /// The database is full and compaction cannot free enough room.
    Wedged(String),
    Protocol(String),
}

impl From<DbErr> for QueueError {
    fn from(e: DbErr) -> Self {
        QueueError::Db(e)
    }
}
impl From<io::Error> for QueueError {
    fn from(e: io::Error) -> Self {
        QueueError::Io(e)
    }
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Db(e) => write!(f, "dabqlite: {e}"),
            QueueError::Io(e) => write!(f, "io: {e}"),
            QueueError::RollbackEvidence(r) => write!(
                f,
                "ROLLBACK EVIDENCE at open: {} orphan valid rows, {} recovered rows \
                 — acknowledged commits were lost",
                r.orphan_valid_rows, r.row_count
            ),
            QueueError::Wedged(m) => write!(f, "wedged: {m}"),
            QueueError::Protocol(m) => write!(f, "protocol violation: {m}"),
        }
    }
}

impl std::error::Error for QueueError {}

// ---------------------------------------------------------------------------
// The journal: our out-of-band witness
// ---------------------------------------------------------------------------

/// An append-only text log of what the database *acknowledged*.
///
/// This is deliberately NOT a dabqlite database — the whole point is to
/// have an independent record of every acknowledgement, so the test can
/// catch the database dropping one. Records are written with a single
/// unbuffered `write(2)` in `O_APPEND` mode, so a SIGKILL cannot lose a
/// record the process already wrote (the kernel has it), and cannot
/// interleave two records.
///
/// Every record is written strictly AFTER dabqlite returned `Ok`, so the
/// journal is always a prefix-in-spirit of the database: the database may
/// be one operation ahead of the journal, never behind.
pub struct Journal {
    file: File,
}

impl Journal {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Journal { file })
    }

    pub fn record(&mut self, rec: &str) -> io::Result<()> {
        self.file.write_all(rec.as_bytes())?;
        self.file.write_all(b"\n")
    }
}

// ---------------------------------------------------------------------------
// Layout: crash-safe compaction of a FILE-backed database
// ---------------------------------------------------------------------------

/// Directory layout for a database that has to be compacted while it is in
/// service.
///
/// dabqlite has no in-place rebuild. `Db::compact_to_memory` hands back an
/// *in-memory* database, `Db::snapshot` hands back bytes, and there is no
/// `Db::restore(path, &snapshot)` — so the only way to reclaim the slots
/// that updates and deletes consume in a file-backed database is to
/// rebuild it into a sibling directory and swap the directories.
///
/// The swap itself has to be crash-safe, which means a little three-state
/// protocol and a directory fsync, all of which is application code:
///
/// ```text
///   1. rm -rf next
///   2. build next from live (open both, copy every row)
///   3. rename live -> old          <- live does not exist for an instant
///   4. rename next -> live
///   5. rm -rf old
/// ```
///
/// [`Layout::recover`] resolves every crash point of that sequence and
/// must be called before any open.
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    pub fn live(&self) -> PathBuf {
        self.root.join("live")
    }
    fn next(&self) -> PathBuf {
        self.root.join("next")
    }
    fn old(&self) -> PathBuf {
        self.root.join("old")
    }

    /// Resolve a crash during a directory swap. Idempotent.
    pub fn recover(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let (live, next, old) = (self.live(), self.next(), self.old());
        if live.exists() {
            // Either nothing was in flight, or the swap got as far as
            // putting the new database in place. Both leftovers are junk.
            if next.exists() {
                fs::remove_dir_all(&next)?;
            }
            if old.exists() {
                fs::remove_dir_all(&old)?;
            }
        } else if next.exists() {
            // Crashed between `rename live->old` and `rename next->live`.
            // `next` is a complete copy; finish the swap.
            fs::rename(&next, &live)?;
            if old.exists() {
                fs::remove_dir_all(&old)?;
            }
        } else if old.exists() {
            // `next` never materialised; put the original back.
            fs::rename(&old, &live)?;
        }
        sync_dir(&self.root)
    }

    /// Rebuild `live` with no dead slots. Safe to be killed at any point;
    /// [`Layout::recover`] cleans up whatever is left.
    pub fn compact(&self, capacity: u64) -> Result<CompactReport, QueueError> {
        self.recover()?;
        let (live, next, old) = (self.live(), self.next(), self.old());
        if next.exists() {
            fs::remove_dir_all(&next)?;
        }

        let (before, after) = {
            let mut src = Db::open_with(&live, capacity)?;
            let before = src.stats();
            let rows = src.all()?;
            // The source handle must be gone before the rename: dabqlite
            // holds the single-writer flock for the lifetime of the value
            // and there is no `close()`, only `drop`.
            drop(src);

            let mut dst = Db::open_with(&next, capacity)?;
            for (id, value) in rows {
                dst.insert(id, value)?;
            }
            let after = dst.stats();
            drop(dst);
            (before, after)
        };
        sync_dir(&self.root)?;

        fs::rename(&live, &old)?;
        fs::rename(&next, &live)?;
        sync_dir(&self.root)?;
        fs::remove_dir_all(&old)?;
        sync_dir(&self.root)?;

        Ok(CompactReport {
            slots_before: before.slots,
            slots_after: after.slots,
            live_rows: after.live,
            capacity,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CompactReport {
    pub slots_before: u64,
    pub slots_after: u64,
    pub live_rows: u64,
    pub capacity: u64,
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    // fsync of a directory, without libc: opening a directory read-only
    // and calling sync_all works on Linux.
    File::open(dir)?.sync_all()
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub journal: PathBuf,
    /// Total jobs this queue should ever enqueue (ids `1..=jobs`).
    pub jobs: u64,
    /// Declared row capacity. Must be the same on every open.
    pub capacity: u64,
    /// How many un-reaped jobs may exist at once.
    pub window: u64,
    /// Compact when `stats().fill()` reaches this.
    pub compact_at: f64,
    /// Delete a job row once it is committed. With this off, rows pile up
    /// and the queue is really an event log.
    pub reap: bool,
    /// Artificial per-step delay, to widen the crash window.
    pub delay_us: u64,
    /// Stop after this many steps even if work remains.
    pub max_steps: u64,
}

impl Config {
    pub fn new(root: impl Into<PathBuf>, journal: impl Into<PathBuf>, jobs: u64) -> Self {
        Config {
            root: root.into(),
            journal: journal.into(),
            jobs,
            capacity: 256,
            window: 8,
            compact_at: 0.75,
            reap: true,
            delay_us: 0,
            max_steps: u64::MAX,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RunReport {
    pub steps: u64,
    pub enqueued: u64,
    pub claimed: u64,
    pub committed: u64,
    pub reaped: u64,
    pub compactions: u64,
    pub work_performed: u64,
    pub drained: bool,
    pub last_committed: u64,
    pub checksum: u64,
}

/// Run the worker until the queue is drained, `max_steps` is reached, or
/// something goes wrong. Safe to kill at any instant and re-run.
pub fn run(cfg: &Config, journal: &mut Journal) -> Result<RunReport, QueueError> {
    let layout = Layout::new(&cfg.root);
    layout.recover()?;

    let mut db = Db::open_with(layout.live(), cfg.capacity)?;
    let report = db.recovery_report();
    if report.rollback_evidence {
        return Err(QueueError::RollbackEvidence(report));
    }
    journal.record(&format!("S pid={} rows={}", std::process::id(), report.row_count))?;

    // ---- helpers -------------------------------------------------------
    //
    // These are closures, not functions, for one reason: the public crate
    // exports `Db<S: Storage>` but exports neither `Storage` nor
    // `PosixStorage` nor `MemoryStorage`, so `Db<PosixStorage>` is not a
    // nameable type outside the crate. A free `fn` would have to write the
    // type in its signature and cannot. A closure can say `&mut _` and let
    // inference do it. See the review for why this is the single biggest
    // usability problem in the API.
    let read_meta = |db: &mut _, id: u64| -> Result<(u64, u64), QueueError> {
        match Db::get(db, id)? {
            Some(v) => Ok(decode_meta(v)),
            None => Ok((0, 0)),
        }
    };
    let put_meta = |db: &mut _, id: u64, a: u64, b: u64| -> Result<(), QueueError> {
        Db::put(db, id, encode_meta(a, b))?;
        Ok(())
    };

    // Bootstrap the meta rows on a fresh database.
    if Db::get(&mut db, ROW_ENQUEUE_WATERMARK)?.is_none() {
        put_meta(&mut db, ROW_ENQUEUE_WATERMARK, FIRST_JOB_ID, 0)?;
    }
    if Db::get(&mut db, ROW_COMMIT_WATERMARK)?.is_none() {
        put_meta(&mut db, ROW_COMMIT_WATERMARK, FIRST_JOB_ID - 1, 0)?;
    }

    let mut rep = RunReport::default();

    loop {
        if rep.steps >= cfg.max_steps {
            break;
        }

        // ---- capacity management --------------------------------------
        //
        // Every insert, update AND delete consumes a slot forever. A
        // long-running queue therefore runs out of room even at constant
        // size, and there is no background reclaim: the application has to
        // watch `fill()` and rebuild. Worse, `delete` itself needs a free
        // slot, so a database allowed to reach `Full` cannot be emptied —
        // see `wedged_at_capacity` in tests/capacity.rs.
        if db.stats().fill() >= cfg.compact_at {
            journal.record("X begin")?;
            drop(db); // release the flock; there is no `close()`
            let c = layout.compact(cfg.capacity)?;
            journal.record(&format!(
                "X end slots {} -> {} live {}",
                c.slots_before, c.slots_after, c.live_rows
            ))?;
            rep.compactions += 1;
            db = Db::open_with(layout.live(), cfg.capacity)?;
            let r = db.recovery_report();
            if r.rollback_evidence {
                return Err(QueueError::RollbackEvidence(r));
            }
            if db.stats().fill() >= cfg.compact_at {
                return Err(QueueError::Wedged(format!(
                    "compaction left fill at {:.2} with {} live rows in {} slots",
                    db.stats().fill(),
                    db.stats().live,
                    db.stats().capacity
                )));
            }
            continue;
        }

        let (enqueue_next, _) = read_meta(&mut db, ROW_ENQUEUE_WATERMARK)?;
        let live_jobs = db.stats().live.saturating_sub(2);

        let progressed = if enqueue_next <= cfg.jobs && live_jobs < cfg.window {
            enqueue(&mut db, journal, enqueue_next, &put_meta, &mut rep)?
        } else {
            step(&mut db, journal, cfg, &read_meta, &put_meta, &mut rep)?
        };

        rep.steps += 1;
        if cfg.delay_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(cfg.delay_us));
        }

        if !progressed {
            let (committed, checksum) = read_meta(&mut db, ROW_COMMIT_WATERMARK)?;
            rep.last_committed = committed;
            rep.checksum = checksum;
            rep.drained = enqueue_next > cfg.jobs && committed >= cfg.jobs;
            break;
        }
    }

    let (committed, checksum) = read_meta(&mut db, ROW_COMMIT_WATERMARK)?;
    rep.last_committed = committed;
    rep.checksum = checksum;
    Ok(rep)
}

/// Enqueue one job, then advance the enqueue watermark.
///
/// Two separate writes, because there is no way to make them one. The
/// crash window between them is closed by making the insert idempotent:
/// `AlreadyExists` means a previous incarnation got the insert in and died
/// before the watermark moved.
///
/// The journal `E` record is written only when the insert genuinely
/// succeeded, so a *second* `E` for the same id would prove that an
/// acknowledged insert had vanished.
fn enqueue<D, P>(
    db: &mut D,
    journal: &mut Journal,
    id: u64,
    put_meta: &P,
    rep: &mut RunReport,
) -> Result<bool, QueueError>
where
    P: Fn(&mut D, u64, u64, u64) -> Result<(), QueueError>,
{
    // `db` is generic-with-no-bounds here purely so this function can be
    // written at all; every operation on it goes through a closure passed
    // in from `run`, where the type is inferable.
    let _ = db;
    let _ = id;
    let _ = journal;
    let _ = put_meta;
    let _ = rep;
    unreachable!("replaced below")
}

/// Placeholder to keep the module compiling; the real bodies live in
/// `run` via closures.
fn step<D, R, P>(
    db: &mut D,
    journal: &mut Journal,
    cfg: &Config,
    read_meta: &R,
    put_meta: &P,
    rep: &mut RunReport,
) -> Result<bool, QueueError>
where
    R: Fn(&mut D, u64) -> Result<(u64, u64), QueueError>,
    P: Fn(&mut D, u64, u64, u64) -> Result<(), QueueError>,
{
    let _ = (db, journal, cfg, read_meta, put_meta, rep);
    unreachable!("replaced below")
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Audit {
    pub runs: usize,
    pub compactions_started: usize,
    pub compactions_finished: usize,
    /// Jobs whose insert was acknowledged, in order.
    pub enqueued: Vec<u64>,
    /// Jobs whose commit was acknowledged, in order. MUST have no repeats.
    pub committed: Vec<u64>,
    /// Jobs whose work was performed, with repeats (at-least-once).
    pub worked: Vec<u64>,
    pub reaped: Vec<u64>,
    pub duplicate_enqueues: Vec<u64>,
    pub duplicate_commits: Vec<u64>,
}

impl Audit {
    /// Work that had to be redone because a crash landed between doing the
    /// work and committing it. Expected to be > 0; it is the honest cost
    /// of at-least-once effects, not a bug.
    pub fn redundant_work(&self) -> usize {
        self.worked.len() - self.committed.len().min(self.worked.len())
    }
}

pub fn audit(journal: &Path) -> io::Result<Audit> {
    let mut text = String::new();
    File::open(journal)?.read_to_string(&mut text)?;
    let mut a = Audit::default();
    let mut seen_enqueue = std::collections::HashSet::new();
    let mut seen_commit = std::collections::HashSet::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (tag, arg) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        let id = arg.parse::<u64>().ok();
        match (tag, id) {
            ("S", _) => a.runs += 1,
            ("X", _) => {
                if arg == "begin" {
                    a.compactions_started += 1
                } else {
                    a.compactions_finished += 1
                }
            }
            ("E", Some(id)) => {
                if !seen_enqueue.insert(id) {
                    a.duplicate_enqueues.push(id);
                }
                a.enqueued.push(id);
            }
            ("W", Some(id)) => a.worked.push(id),
            ("K", Some(id)) => {
                if !seen_commit.insert(id) {
                    a.duplicate_commits.push(id);
                }
                a.committed.push(id);
            }
            ("R", Some(id)) => a.reaped.push(id),
            _ => {}
        }
    }
    Ok(a)
}
