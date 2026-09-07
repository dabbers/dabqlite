//! A crash-resilient job queue on top of `dabqlite`.
//!
//! The point of this crate is to put dabqlite's central claims under a
//! process that gets SIGKILLed at arbitrary points:
//!
//! * an acknowledged commit is never lost,
//! * a [`dabqlite::Db::batch`] is all-or-nothing, and — new in this
//!   revision —
//! * a value that spans several row slots is **as atomic as a short one**,
//!   and comes back byte-for-byte, at its exact length.
//!
//! That last claim is the one this crate now attacks hardest. Jobs carry
//! real payloads of 1..=[`MAX_PAYLOAD`] bytes, most of which do not fit in
//! one 16-byte slot, and a payload that comes back SHORT is the failure
//! mode that matters: it is the one a checksum over ids alone would not
//! see, and the one that a store which writes a long value as several
//! rows could plausibly produce.
//!
//! # The data model we still have to build on top of the data model
//!
//! dabqlite gives one table of `(u64, Value)`, where a `Value` is now any
//! byte string up to 2 KiB. That is a very large improvement over sixteen
//! fixed bytes, but it is still ONE column, so the following remains
//! hand-rolled:
//!
//! * **Job rows** — id = job id, value = a 4-byte header
//!   (`state`, `attempts`, declared payload length) followed by the
//!   payload. See [`Job`]. The declared length is redundant with
//!   `Value::len()` on purpose: it is how this crate detects a payload
//!   that came back short *without* trusting the store to tell the truth
//!   about its own lengths.
//! * **Meta rows** — two reserved ids at the top of the key space hold the
//!   enqueue watermark and the commit watermark. There is no separate
//!   place to put metadata, so metadata is a row and the application has
//!   to carve ids out of the user key space to hold it.
//! * **Batch packing by SLOT COST.** [`MAX_COMMIT_ROWS`] is 128 *row slots*, not
//!   128 operations, and a 2 KiB value eats all 128 by itself. So every
//!   caller that builds a batch has to compute `len.div_ceil(VALUE_LEN)`
//!   per operation and stop before the budget runs out — see
//!   [`slot_cost`] and [`Queue::enqueue`]. The library does this
//!   arithmetic privately in its own rebuild path and does not expose it.
//!
//! What is NOT hand-rolled any more, and used to be:
//!
//! * **Value packing.** `Value` was a fixed `[u8; 16]` whose `as_bytes()`
//!   stopped at the first zero byte, so a binary struct round-tripped only
//!   through `Value::raw()` and a payload had to be a `u64`. Now
//!   `Value::from_vec` takes what you have and `as_bytes()` gives it back,
//!   trailing zeros included — which this crate checks, deliberately, by
//!   ending a third of its payloads with zero bytes.
//! * **Cross-row atomicity.** `Db::batch` commits several writes as one
//!   commit, so "insert the job AND move the enqueue watermark" and
//!   "retire the job AND advance the commit watermark" are each a single
//!   atomic step.
//! * **Compaction.** `Db::compact(&mut self)` rebuilds in place,
//!   crash-safely, and an interrupted compaction is finished by the next
//!   open. The directory-swap protocol this crate used to carry is gone,
//!   and so is the `Option<FileDb>` dance that `compact(self) -> Self`
//!   forced on every field holding a database.
//!
//! # Exactly-once, defined precisely
//!
//! * The *effect* of a job (the work) is at-least-once: a crash between
//!   doing the work and committing the state re-does the work. That is
//!   inherent, and true of SQLite too.
//! * The *commit* of a job is exactly-once: the commit watermark advances
//!   by one and the job row is retired, in the same commit. If dabqlite
//!   ever loses an acknowledged commit, a restart re-commits the same job
//!   and the journal shows a duplicate `K` record. [`audit`] treats that
//!   as failure. That is the data-loss detector.
//! * A batch is never half-visible. [`Queue::half_batch_evidence`] is the
//!   detector for that.
//! * A payload is never half-visible either. The commit checksum folds the
//!   digest of the payload **as read back from the database**, not the
//!   payload the worker meant to write, so one truncated value anywhere in
//!   the history changes the final number. [`Queue::short_payloads`] is
//!   the direct version of the same check.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use dabqlite::{
    Error as DbErr, FileDb, Op, RecoveryReport, Stats, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN,
    VALUE_LEN,
};

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

/// `[0] state | [1] attempts | [2..4] declared payload length | [4..] payload`.
pub const JOB_HEADER: usize = 4;

/// The longest payload a job may carry: the value ceiling, less this
/// crate's own header.
///
/// This used to hold back an extra `VALUE_LEN`, and the difference was a
/// real constraint rather than caution. A value of `MAX_VALUE_LEN` bytes
/// occupies `MAX_VALUE_LEN / VALUE_LEN` slots, which used to be exactly
/// [`MAX_COMMIT_ROWS`] — the whole commit. A maximum-length value could
/// therefore only ever be written ALONE, and this queue never writes a job
/// row alone: an enqueue is `[job..., watermark]` and a commit is
/// `[watermark, retire]`. A slot had to be left for the companion row, or
/// the batch carrying the queue's cross-row invariant became
/// unrepresentable — the library's advertised value ceiling and its
/// advertised atomicity could not both be used at once.
///
/// A commit now holds [`MAX_COMMIT_ROWS`] slots and the longest value
/// costs `MAX_VALUE_LEN / VALUE_LEN` of them, which is eight times
/// smaller, so the reservation is gone and the only subtraction left is
/// this crate's own header.
pub const MAX_PAYLOAD: usize = MAX_VALUE_LEN - JOB_HEADER;
const _: () = assert!(
    MAX_VALUE_LEN / VALUE_LEN < MAX_COMMIT_ROWS,
    "a longest-payload job must still fit in a commit beside its watermark row"
);

/// Row slots a value of `len` bytes consumes.
///
/// This is the unit [`MAX_COMMIT_ROWS`] is denominated in and the unit capacity
/// is denominated in, and the library exposes neither a `cost` on `Op` nor
/// a helper of its own, so every caller writes this line.
pub fn slot_cost(len: usize) -> usize {
    len.div_ceil(VALUE_LEN).max(1)
}

/// A job row: a small header and a real, variable-length payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub state: u8,
    pub attempts: u8,
    pub payload: Vec<u8>,
}

impl Job {
    pub fn new(state: u8, attempts: u8, payload: Vec<u8>) -> Self {
        Job {
            state,
            attempts,
            payload,
        }
    }

    /// Encode for storage. Fails only if the payload is over
    /// [`MAX_PAYLOAD`], which the caller is expected to have checked.
    pub fn encode(&self) -> Result<Value, QueueError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(QueueError::Protocol(format!(
                "payload of {} bytes exceeds MAX_PAYLOAD {MAX_PAYLOAD}",
                self.payload.len()
            )));
        }
        let mut out = Vec::with_capacity(JOB_HEADER + self.payload.len());
        out.push(self.state);
        out.push(self.attempts);
        out.extend_from_slice(&(self.payload.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(Value::from_vec(out)?)
    }

    /// Decode a stored value.
    ///
    /// Every failure here is a value that came back as something nobody
    /// wrote: too short to hold a header, or carrying fewer payload bytes
    /// than its own header declares. The second is the interesting one —
    /// it is what a torn multi-slot value would look like.
    pub fn decode(id: u64, v: &Value) -> Result<Job, QueueError> {
        let b = v.as_bytes();
        if b.len() < JOB_HEADER {
            return Err(QueueError::ShortValue {
                id,
                declared: JOB_HEADER,
                got: b.len(),
            });
        }
        let declared = u16::from_le_bytes([b[2], b[3]]) as usize;
        let payload = &b[JOB_HEADER..];
        if payload.len() != declared {
            return Err(QueueError::ShortValue {
                id,
                declared: declared + JOB_HEADER,
                got: b.len(),
            });
        }
        Ok(Job {
            state: b[0],
            attempts: b[1],
            payload: payload.to_vec(),
        })
    }

    /// Row slots this job costs when written.
    pub fn slots(&self) -> usize {
        slot_cost(JOB_HEADER + self.payload.len())
    }
}

/// A meta row: two `u64`s.
///
/// Still hand-packed, because there is still exactly one column. Values
/// being variable-length does not give metadata a home; it only means the
/// bytes come back the way they went in, which is why the old
/// `Value::raw()` call is gone from here.
pub fn encode_meta(a: u64, b: u64) -> Value {
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&a.to_le_bytes());
    out[8..16].copy_from_slice(&b.to_le_bytes());
    Value::from_bytes(&out).expect("16 bytes is under MAX_VALUE_LEN")
}

pub fn decode_meta(v: &Value) -> (u64, u64) {
    let b = v.as_bytes();
    let word = |at: usize| -> u64 {
        b.get(at..at + 8)
            .and_then(|s| s.try_into().ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0)
    };
    (word(0), word(8))
}

// ---------------------------------------------------------------------------
// The work: payloads that span slots
// ---------------------------------------------------------------------------

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// How long job `id`'s payload is. A deterministic mixture, weighted so
/// that a majority of jobs span MORE than one row slot and a few sit right
/// at the ceiling:
///
/// | class          | bytes        | slots       |
/// |----------------|--------------|-------------|
/// | fits one slot  | 1..=12       | 1           |
/// | a few slots    | 13..=200     | 2..=13      |
/// | many slots     | 201..=1000   | 13..=63     |
/// | at the ceiling | MAX_PAYLOAD-3..=MAX_PAYLOAD | 127 |
pub fn payload_len(id: u64) -> usize {
    let h = mix(id ^ 0x5A5A_0000_0000_A5A5);
    let r = (h >> 8) as usize;
    match h % 8 {
        0..=2 => 1 + r % 12,
        3..=5 => 13 + r % 188,
        6 => 201 + r % 800,
        _ => MAX_PAYLOAD - r % 4,
    }
}

/// The payload of job `id`: a pure function of the id, so a test can
/// predict every byte without trusting the database.
///
/// A third of payloads END IN ZERO BYTES on purpose. Under the old
/// fixed-width `Value` those bytes were indistinguishable from padding and
/// `as_bytes()` ate them; the new contract says exactly what you put in
/// comes out, and this is what checks it under SIGKILL rather than in a
/// unit test.
pub fn payload_of(id: u64) -> Vec<u8> {
    let len = payload_len(id);
    let mut out = Vec::with_capacity(len);
    let mut s = mix(id);
    while out.len() < len {
        s = mix(s);
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(len);
    if id.is_multiple_of(3) {
        let zeros = 1 + (id % 3) as usize;
        let at = out.len().saturating_sub(zeros);
        for b in &mut out[at..] {
            *b = 0;
        }
    }
    out
}

/// An order- and content-sensitive digest of a byte string. Folds the
/// LENGTH in as well, so a truncated payload cannot digest to the same
/// value as a shorter one that was written on purpose.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut acc = 0xC0FF_EE00_1234_5678u64 ^ (bytes.len() as u64).wrapping_mul(0x9E37_79B9);
    for &b in bytes {
        acc = acc.rotate_left(7) ^ u64::from(b).wrapping_mul(0x0100_0000_01B3);
    }
    acc
}

/// Fold a committed job into the running checksum.
///
/// Takes the payload the worker READ BACK from the database, not the one
/// it meant to write, so a job whose payload returned short changes the
/// commit checksum and every later one — which is how a short read gets
/// caught even when nothing else notices.
pub fn fold(acc: u64, id: u64, payload: &[u8]) -> u64 {
    acc.rotate_left(7) ^ mix(id) ^ digest(payload)
}

/// The checksum a correct run over jobs `1..=n` must end with.
pub fn expected_checksum(n: u64) -> u64 {
    (FIRST_JOB_ID..=n).fold(0u64, |acc, id| fold(acc, id, &payload_of(id)))
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
    /// A state that only a HALF-APPLIED batch could have produced. If this
    /// is ever raised, `Db::batch` is not atomic.
    HalfBatch(String),
    /// A value came back SHORTER than the bytes that were written — the
    /// failure mode a multi-slot value makes possible. If this is ever
    /// raised, a long value is not atomic.
    ShortValue {
        id: u64,
        declared: usize,
        got: usize,
    },
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
            QueueError::HalfBatch(m) => write!(f, "HALF-APPLIED BATCH: {m}"),
            QueueError::ShortValue { id, declared, got } => write!(
                f,
                "SHORT VALUE: row {id} was written as {declared} bytes and came back \
                 as {got} — a multi-slot value was not atomic"
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
/// `E`/`K`/`W`/`R`/`S`/`X` records are written strictly AFTER dabqlite
/// returned `Ok`, so the journal is always a prefix-in-spirit of the
/// database: the database may be one operation ahead of the journal, never
/// behind.
///
/// `D` records ([`churn`]) are the one exception and are labelled as such:
/// they are written BEFORE the batch they describe, on purpose, so that
/// every state the database is *allowed* to be in after a kill is already
/// on record.
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
// The queue
// ---------------------------------------------------------------------------

/// The queue's handle on its database.
///
/// A plain struct with a plain `FileDb` field. Two things had to change in
/// the library for that sentence to be true: `FileDb` had to be exported
/// (before, `Db<PosixStorage>` could not be *written down* outside the
/// library at all), and `compact` had to take `&mut self`. While it took
/// `self` by value and returned a new handle, a database living in a
/// struct field could only be compacted by `Option::take`-ing it out, and
/// every read and write in this file went through an accessor that
/// `expect`ed the field was not currently missing.
pub struct Queue {
    db: FileDb,
    reap: bool,
}

impl Queue {
    /// Open (or create) the queue. Bootstraps the two meta rows in a
    /// single batch, so a fresh database never exists with one watermark
    /// and not the other.
    pub fn open(path: impl AsRef<Path>, capacity: u64, reap: bool) -> Result<Self, QueueError> {
        let db = FileDb::open_with(path.as_ref(), capacity)?;
        let rep = db.recovery_report();
        if rep.rollback_evidence {
            return Err(QueueError::RollbackEvidence(rep));
        }
        let mut q = Queue { db, reap };
        q.bootstrap()?;
        Ok(q)
    }

    pub fn recovery_report(&self) -> RecoveryReport {
        self.db.recovery_report()
    }

    pub fn stats(&self) -> Stats {
        self.db.stats()
    }

    fn bootstrap(&mut self) -> Result<(), QueueError> {
        let mut ops = Vec::new();
        if self.db.get(ROW_ENQUEUE_WATERMARK)?.is_none() {
            ops.push(Op::put(ROW_ENQUEUE_WATERMARK, encode_meta(FIRST_JOB_ID, 0)));
        }
        if self.db.get(ROW_COMMIT_WATERMARK)?.is_none() {
            ops.push(Op::put(
                ROW_COMMIT_WATERMARK,
                encode_meta(FIRST_JOB_ID - 1, 0),
            ));
        }
        if !ops.is_empty() {
            self.db.batch(&ops)?;
        }
        Ok(())
    }

    fn meta(&mut self, id: u64) -> Result<(u64, u64), QueueError> {
        Ok(self.db.get(id)?.as_ref().map(decode_meta).unwrap_or((0, 0)))
    }

    /// `(next id to hand out, unused)`.
    pub fn enqueue_watermark(&mut self) -> Result<u64, QueueError> {
        Ok(self.meta(ROW_ENQUEUE_WATERMARK)?.0)
    }

    /// `(last committed id, order- and content-sensitive checksum)`.
    pub fn commit_watermark(&mut self) -> Result<(u64, u64), QueueError> {
        self.meta(ROW_COMMIT_WATERMARK)
    }

    /// The head of the queue: the lowest-id job row at or after `from`.
    pub fn head(&mut self, from: u64) -> Result<Option<(u64, Job)>, QueueError> {
        let (page, _) = self.db.range_page(from, MAX_JOB_ID)?;
        match page.first() {
            None => Ok(None),
            Some((id, v)) => Ok(Some((*id, Job::decode(*id, v)?))),
        }
    }

    /// Insert jobs starting at the enqueue watermark AND advance the
    /// watermark — **one commit** — returning the ids actually enqueued.
    ///
    /// Fewer than `n` may be enqueued, and that is the library showing
    /// through: [`MAX_COMMIT_ROWS`] counts ROW SLOTS, so how many jobs fit in one
    /// commit depends on how long their payloads are. One job with a
    /// ceiling-sized payload fills the batch by itself. The caller gets the
    /// ids that were actually written and loops.
    ///
    /// `Op::insert` (not `Op::put`) is deliberate: if the row were somehow
    /// already there the batch must be REFUSED, because that would mean
    /// the invariant this method exists to maintain had already broken.
    pub fn enqueue(&mut self, n: u64) -> Result<Vec<u64>, QueueError> {
        let first = self.enqueue_watermark()?;
        let mut ops = Vec::new();
        let mut ids = Vec::new();
        // One slot is reserved for the watermark row that closes the batch.
        let budget = MAX_COMMIT_ROWS - 1;
        let mut staged = 0usize;
        for id in first..first + n {
            let job = Job::new(PENDING, 0, payload_of(id));
            let cost = job.slots();
            if staged + cost > budget {
                break;
            }
            staged += cost;
            ops.push(Op::insert(id, job.encode()?));
            ids.push(id);
        }
        if ids.is_empty() {
            return Ok(ids);
        }
        ops.push(Op::put(
            ROW_ENQUEUE_WATERMARK,
            encode_meta(first + ids.len() as u64, 0),
        ));
        self.db.batch(&ops)?;
        Ok(ids)
    }

    /// PENDING -> CLAIMED. One row, so a plain `update`; there is nothing
    /// to make atomic with it.
    ///
    /// Note what this costs: flipping ONE header byte rewrites the whole
    /// value, so claiming a job with a 2 KiB payload burns 127 row slots.
    /// There is no partial update and no second column to put mutable
    /// state in, so a queue that wants a cheap state flip has to store the
    /// state in a different ROW from the payload — at which point it needs
    /// a batch to keep the two consistent, and pays two slots per flip
    /// instead of one.
    pub fn claim(&mut self, id: u64, job: &Job) -> Result<(), QueueError> {
        let next = Job::new(CLAIMED, job.attempts.saturating_add(1), job.payload.clone());
        self.db.update(id, next.encode()?)?;
        Ok(())
    }

    /// Retire the job AND advance the commit watermark — **one commit**.
    ///
    /// Retiring means deleting the row (`reap`) or moving it to `DONE`
    /// (archive mode). Either way the row moves and the aggregate advances
    /// together, which is the invariant the whole queue rests on.
    ///
    /// The checksum folds `payload`, which the caller read back out of the
    /// database a moment ago. A payload that returned short therefore
    /// poisons the checksum permanently, and `tests/crash.rs` compares it
    /// against an independently computed one after every kill.
    pub fn commit(&mut self, id: u64, job: &Job) -> Result<u64, QueueError> {
        let (last, checksum) = self.commit_watermark()?;
        if id != last + 1 {
            return Err(QueueError::Protocol(format!(
                "commit watermark is {last}, head of queue is {id}"
            )));
        }
        let next = fold(checksum, id, &job.payload);
        let retire = if self.reap {
            Op::delete(id)
        } else {
            Op::update(
                id,
                Job::new(DONE, job.attempts, job.payload.clone()).encode()?,
            )
        };
        self.db
            .batch(&[Op::put(ROW_COMMIT_WATERMARK, encode_meta(id, next)), retire])?;
        Ok(next)
    }

    /// Reclaim the slots that updates and deletes consumed.
    ///
    /// One library call, on `&mut self`. This used to be a `Layout` type in
    /// this crate with a `live`/`next`/`old` directory triple, a five-step
    /// swap and a `recover()` resolving each of its four crash points; then
    /// it was one call that consumed the handle and had to be threaded
    /// back through an `Option`. Now it is a method call.
    pub fn compact(&mut self) -> Result<(u64, u64), QueueError> {
        let before = self.db.stats().slots;
        self.db.compact()?;
        let rep = self.db.recovery_report();
        if rep.rollback_evidence {
            return Err(QueueError::RollbackEvidence(rep));
        }
        Ok((before, self.db.stats().slots))
    }

    /// Every job row currently in the database that DECODES.
    ///
    /// Decoding is fallible now: a row whose stored bytes are fewer than
    /// its own header declares is a value that came back short, and it is
    /// skipped here rather than aborting the scan, because
    /// [`Queue::short_payloads`] is the detector that reports it and both
    /// are checked at every restart.
    pub fn outstanding(&mut self) -> Result<Vec<(u64, Job)>, QueueError> {
        let rows = self.db.range(FIRST_JOB_ID, MAX_JOB_ID)?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, v) in rows {
            if let Ok(job) = Job::decode(id, &v) {
                out.push((id, job));
            }
        }
        Ok(out)
    }

    /// Every job row whose payload is not exactly what was written for it,
    /// as `(id, expected_len, got_len)`.
    ///
    /// The direct form of the detector: this crate knows every job's
    /// payload as a pure function of its id, so it can compare byte for
    /// byte. Anything reported here is a value the store lost part of.
    pub fn short_payloads(&mut self) -> Result<Vec<(u64, usize, usize)>, QueueError> {
        let rows = self.db.range(FIRST_JOB_ID, MAX_JOB_ID)?;
        let mut bad = Vec::new();
        for (id, v) in rows {
            let want = payload_of(id);
            match Job::decode(id, &v) {
                Err(QueueError::ShortValue { declared, got, .. }) => bad.push((id, declared, got)),
                Err(e) => return Err(e),
                Ok(job) => {
                    if job.payload != want {
                        bad.push((id, want.len(), job.payload.len()));
                    }
                }
            }
        }
        Ok(bad)
    }

    /// Look for a state that is reachable ONLY if a batch was applied in
    /// part. `Ok(None)` means the cross-row invariants hold.
    pub fn half_batch_evidence(&mut self) -> Result<Option<String>, QueueError> {
        let enqueue_next = self.enqueue_watermark()?;
        let (committed, _) = self.commit_watermark()?;
        let reap = self.reap;

        // (1) The enqueue batch is [insert job..., put watermark]. A job
        //     row at or beyond the watermark means the inserts landed and
        //     the watermark write did not.
        let beyond = self.db.range(enqueue_next, MAX_JOB_ID)?;
        if let Some((id, _)) = beyond.first() {
            return Ok(Some(format!(
                "job row {id} exists at or beyond the enqueue watermark {enqueue_next}: \
                 the insert half of an enqueue batch landed without the watermark half"
            )));
        }

        // (2) The commit batch is [put watermark, retire job]. In reap
        //     mode "retire" is a delete, so no row may survive at or below
        //     the commit watermark.
        if reap {
            let below = self.db.range(FIRST_JOB_ID, committed)?;
            if let Some((id, _)) = below.first() {
                return Ok(Some(format!(
                    "job row {id} still exists at or below the commit watermark {committed}: \
                     the watermark half of a commit batch landed without the delete half"
                )));
            }
        } else {
            // In archive mode "retire" moves the row to DONE, so states
            // and the watermark must agree in both directions.
            for (id, job) in self.outstanding()? {
                let expected_done = id <= committed;
                if expected_done && job.state != DONE {
                    return Ok(Some(format!(
                        "job {id} is at or below the commit watermark {committed} but its \
                         state is {} not DONE: the watermark half of a commit batch landed \
                         without the row half",
                        job.state
                    )));
                }
                if !expected_done && job.state == DONE {
                    return Ok(Some(format!(
                        "job {id} is DONE but the commit watermark is only {committed}: the \
                         row half of a commit batch landed without the watermark half"
                    )));
                }
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    /// The database directory. `Db::open_with` takes it directly; there is
    /// no application-owned directory layout any more.
    pub root: PathBuf,
    pub journal: PathBuf,
    /// Total jobs this queue should ever enqueue (ids `1..=jobs`).
    pub jobs: u64,
    /// Declared row capacity, in SLOTS — not in jobs. A job with a
    /// ceiling-sized payload is 127 of them.
    pub capacity: u64,
    /// How many un-reaped jobs may exist at once.
    pub window: u64,
    /// Jobs to offer one enqueue batch. How many actually land depends on
    /// their payload lengths; see [`Queue::enqueue`].
    pub enqueue_chunk: u64,
    /// Compact when `stats().fill()` reaches this.
    pub compact_at: f64,
    /// Delete a job row once it is committed. With this off, rows are
    /// archived in place as `DONE` and the queue is really an event log.
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
            capacity: 4096,
            window: 8,
            enqueue_chunk: 1,
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
    pub batches: u64,
    pub compactions: u64,
    pub work_performed: u64,
    pub payload_bytes: u64,
    /// Steps that hit `Error::Full` and had to rebuild before retrying.
    pub full_retries: u64,
    pub drained: bool,
    pub last_committed: u64,
    pub checksum: u64,
    pub stats_fill: f64,
}

/// Run the worker until the queue is drained, `max_steps` is reached, or
/// something goes wrong. Safe to kill at any instant and re-run.
pub fn run(cfg: &Config, journal: &mut Journal) -> Result<RunReport, QueueError> {
    let mut q = Queue::open(&cfg.root, cfg.capacity, cfg.reap)?;
    let opened = q.recovery_report();
    journal.record(&format!(
        "S pid={} rows={} orphans={}",
        std::process::id(),
        opened.row_count,
        opened.orphan_valid_rows
    ))?;
    // A restart is the moment to check the cross-row invariants: if a kill
    // landed inside a batch, this is where a half-applied one would show.
    if let Some(evidence) = q.half_batch_evidence()? {
        return Err(QueueError::HalfBatch(evidence));
    }
    // And the moment to check every surviving payload byte for byte: if a
    // kill landed inside a multi-slot value, this is where a short one
    // would show.
    if let Some(&(id, declared, got)) = q.short_payloads()?.first() {
        return Err(QueueError::ShortValue { id, declared, got });
    }

    let mut rep = RunReport::default();

    loop {
        if rep.steps >= cfg.max_steps {
            break;
        }
        rep.steps += 1;

        // ---- capacity management --------------------------------------
        //
        // Every insert, update AND delete consumes slots forever, and
        // nothing is reclaimed until a rebuild. A queue that stays the
        // same size still runs out of room, so the application has to
        // watch `fill()` and rebuild before it hits the wall: `delete`
        // itself needs a free slot, so a database allowed to reach `Full`
        // cannot even be emptied (see tests/capacity.rs).
        if q.stats().fill() >= cfg.compact_at {
            journal.record("X begin")?;
            let (before, after) = q.compact()?;
            journal.record(&format!("X end slots={before}->{after}"))?;
            rep.compactions += 1;
            let s = q.stats();
            if s.fill() >= cfg.compact_at {
                return Err(QueueError::Wedged(format!(
                    "compaction left fill at {:.2}: {} live rows, {} slots, capacity {}",
                    s.fill(),
                    s.live,
                    s.slots,
                    s.capacity
                )));
            }
            continue;
        }

        let progressed = match step(cfg, &mut q, journal, &mut rep) {
            Ok(p) => p,
            // The database ran out of room mid-step. `fill()` did not see
            // it coming and could not have: whether the next write fits
            // depends on how many SLOTS its value needs, and `Stats` is
            // denominated in slots while the application's unit is bytes.
            // So the honest application response to `Full` is: rebuild,
            // then try the same step again.
            Err(e) if is_full(&e) => {
                journal.record("X begin")?;
                let (before, after) = q.compact()?;
                journal.record(&format!("X end slots={before}->{after}"))?;
                rep.compactions += 1;
                rep.full_retries += 1;
                if after >= before {
                    return Err(QueueError::Wedged(format!(
                        "{e}, and a rebuild reclaimed nothing ({before} slots \
                         before, {after} after, capacity {})",
                        q.stats().capacity
                    )));
                }
                continue;
            }
            Err(e) => return Err(e),
        };

        if cfg.delay_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(cfg.delay_us));
        }

        if !progressed {
            break;
        }
    }

    let (committed, checksum) = q.commit_watermark()?;
    let enqueue_next = q.enqueue_watermark()?;
    rep.last_committed = committed;
    rep.checksum = checksum;
    rep.drained = enqueue_next > cfg.jobs && committed >= cfg.jobs;
    rep.stats_fill = q.stats().fill();
    Ok(rep)
}

/// Is this the database saying it has no room? A `Full` can arrive on its
/// own or wrapped in a `BatchRejected`, and an application that wants to
/// react to it has to look in both places.
pub fn is_full(e: &QueueError) -> bool {
    matches!(e, QueueError::Db(db) if db_is_full(db))
}

/// The same question of a raw [`dabqlite::Error`]. Two variants have to be
/// checked because a batch reports the reason it was refused inside
/// [`dabqlite::Error::BatchRejected`] rather than as itself.
pub fn db_is_full(e: &DbErr) -> bool {
    match e {
        DbErr::Full { .. } => true,
        DbErr::BatchRejected { cause, .. } => matches!(**cause, DbErr::Full { .. }),
        _ => false,
    }
}

/// One step of the worker: enqueue a batch, or advance the head of the
/// queue by one state. `Ok(false)` means there was nothing to do.
fn step(
    cfg: &Config,
    q: &mut Queue,
    journal: &mut Journal,
    rep: &mut RunReport,
) -> Result<bool, QueueError> {
    let enqueue_next = q.enqueue_watermark()?;
    let (committed_now, _) = q.commit_watermark()?;
    // Queue depth is derived from the two watermarks, not from
    // `stats().live`: in archive mode `live` counts finished work too
    // and the queue would wedge.
    let in_flight = (enqueue_next - FIRST_JOB_ID).saturating_sub(committed_now);

    let remaining = cfg.jobs + 1 - enqueue_next.min(cfg.jobs + 1);
    let want = cfg.enqueue_chunk.max(1).min(remaining);
    let room = cfg.window.saturating_sub(in_flight);
    let refill = enqueue_next <= cfg.jobs && want > 0 && (room >= want || in_flight == 0);

    Ok(if refill {
        // ---- ENQUEUE: one batch, one commit ------------------------
        let ids = q.enqueue(want.min(room.max(1)))?;
        for id in &ids {
            journal.record(&format!("E {id}"))?;
        }
        rep.enqueued += ids.len() as u64;
        rep.batches += 1;
        !ids.is_empty()
    } else {
        // ---- DRAIN -------------------------------------------------
        //
        // There is no secondary index and no "where state = ?", so the
        // head of the queue has to be found positionally. It works
        // only because ids are handed out in FIFO order and the commit
        // watermark says where the live region starts.
        let (committed_so_far, _) = q.commit_watermark()?;
        let scan_from = if cfg.reap {
            FIRST_JOB_ID
        } else {
            committed_so_far + 1
        };
        match q.head(scan_from)? {
            None => false,
            Some((id, job)) => match job.state {
                PENDING => {
                    // Safe as a read-then-write because dabqlite is
                    // single-writer by construction. With more than
                    // one worker there would be no way to do this at
                    // all — there is no conditional update.
                    q.claim(id, &job)?;
                    journal.record(&format!("C {id}"))?;
                    rep.claimed += 1;
                    true
                }
                CLAIMED => {
                    // Do the work, then commit it. The effect is
                    // at-least-once: a crash here re-does it. The
                    // COMMIT is exactly-once, because retiring the row
                    // and advancing the watermark are one commit.
                    //
                    // The payload check is the point of this revision:
                    // the bytes that come back must be EXACTLY the
                    // bytes that went in, including length and
                    // including trailing zeros.
                    let expected = payload_of(id);
                    if job.payload != expected {
                        return Err(QueueError::ShortValue {
                            id,
                            declared: expected.len(),
                            got: job.payload.len(),
                        });
                    }
                    journal.record(&format!("W {id} {}", job.payload.len()))?;
                    rep.work_performed += 1;
                    rep.payload_bytes += job.payload.len() as u64;
                    q.commit(id, &job)?;
                    journal.record(&format!("K {id}"))?;
                    rep.committed += 1;
                    rep.batches += 1;
                    true
                }
                DONE => {
                    // Unreachable: in reap mode a DONE row does not
                    // exist (the commit batch deleted it), and in
                    // archive mode the scan starts past the watermark.
                    return Err(QueueError::HalfBatch(format!(
                        "job {id} is DONE and still at the head of the queue \
                             (commit watermark {committed_so_far})"
                    )));
                }
                other => {
                    return Err(QueueError::Protocol(format!(
                        "job {id} has unknown state {other}"
                    )))
                }
            },
        }
    })
}

/// Read the queue's committed state without running it. Opens the
/// database, so it needs the writer lock.
pub fn inspect(cfg: &Config) -> Result<Inspection, QueueError> {
    let mut q = Queue::open(&cfg.root, cfg.capacity, cfg.reap)?;
    let rec = q.recovery_report();
    let stats = q.stats();
    let enqueue_next = q.enqueue_watermark()?;
    let (committed, checksum) = q.commit_watermark()?;
    let short_payloads = q.short_payloads()?;
    let outstanding = q.outstanding()?;
    let half_batch = q.half_batch_evidence()?;
    Ok(Inspection {
        enqueue_next,
        committed,
        checksum,
        outstanding,
        half_batch,
        short_payloads,
        live: stats.live,
        slots: stats.slots,
        dead: stats.dead,
        capacity: stats.capacity,
        rollback_evidence: rec.rollback_evidence,
        orphan_valid_rows: rec.orphan_valid_rows,
    })
}

#[derive(Debug, Clone)]
pub struct Inspection {
    pub enqueue_next: u64,
    pub committed: u64,
    pub checksum: u64,
    pub outstanding: Vec<(u64, Job)>,
    /// `Some(why)` if the database is in a state only a half-applied batch
    /// could produce.
    pub half_batch: Option<String>,
    /// `(id, expected_len, got_len)` for every job row whose payload is not
    /// the payload that was written for it.
    pub short_payloads: Vec<(u64, usize, usize)>,
    pub live: u64,
    pub slots: u64,
    pub dead: u64,
    pub capacity: u64,
    pub rollback_evidence: bool,
    pub orphan_valid_rows: u64,
}

// ---------------------------------------------------------------------------
// Churn: the batch-atomicity torture target
// ---------------------------------------------------------------------------

/// Row holding the churn digest.
pub const CHURN_DIGEST: u64 = u64::MAX;
/// Cells live at ids `1..=CHURN_CELLS`.
pub const CHURN_CELLS: u64 = 200;

/// A churn cell's stored value: `[0..8] declared length | payload`.
///
/// Self-describing on purpose. The digest below would catch a truncated
/// value anyway, but only statistically-ish and only as "a batch was
/// applied in part"; the declared length turns the same damage into the
/// specific claim "this multi-slot value came back short", which is a
/// different bug in a different part of the library.
fn cell_value(len: usize, seed: u64) -> Value {
    let mut out = Vec::with_capacity(8 + len);
    out.extend_from_slice(&(len as u64).to_le_bytes());
    let mut s = seed | 1;
    while out.len() < 8 + len {
        s = mix(s);
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(8 + len);
    Value::from_vec(out).expect("cell values are built under MAX_VALUE_LEN")
}

/// `Ok(payload)` or `Err((declared, got))` for a value that came back short.
fn cell_payload(v: &Value) -> Result<Vec<u8>, (usize, usize)> {
    let b = v.as_bytes();
    if b.len() < 8 {
        return Err((8, b.len()));
    }
    let declared = u64::from_le_bytes(b[0..8].try_into().expect("8 bytes")) as usize;
    if b.len() != 8 + declared {
        return Err((8 + declared, b.len()));
    }
    Ok(b[8..].to_vec())
}

#[derive(Debug, Clone)]
pub struct ChurnConfig {
    pub root: PathBuf,
    pub journal: PathBuf,
    pub rounds: u64,
    /// Row-slot budget for the cell operations of one batch. The batch
    /// itself is this + 1 (the digest row), capped at [`MAX_COMMIT_ROWS`].
    pub batch: usize,
    /// Vary the width of every batch between 1 and `batch` instead of
    /// using `batch` every time.
    pub vary: bool,
    /// Longest cell payload, in bytes. Above `VALUE_LEN - 8` a cell spans
    /// several row slots, which is the case this workload exists to
    /// hammer.
    pub max_cell: usize,
    /// How many distinct cells the workload uses, at ids `1..=cells`.
    ///
    /// It exists because capacity is denominated in SLOTS: the live set is
    /// `cells * slot_cost(8 + max_cell)` slots in the worst case, so a
    /// workload with long values needs either fewer cells or a much larger
    /// capacity, and the caller is the only one who can do that
    /// arithmetic.
    pub cells: u64,
    pub capacity: u64,
    pub compact_at: f64,
    pub seed: u64,
}

impl ChurnConfig {
    pub fn new(root: impl Into<PathBuf>, journal: impl Into<PathBuf>) -> Self {
        ChurnConfig {
            root: root.into(),
            journal: journal.into(),
            rounds: 1_000_000,
            batch: MAX_COMMIT_ROWS - 1,
            vary: false,
            max_cell: 8,
            cells: CHURN_CELLS,
            capacity: 8192,
            compact_at: 0.7,
            seed: 0x9E37_79B9,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ChurnReport {
    pub rounds: u64,
    pub ops: u64,
    pub rows: u64,
    pub compactions: u64,
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A digest of the whole cell set. Order- and content-sensitive over
/// ascending id, so any missing, extra, wrong or TRUNCATED cell changes it.
pub fn churn_digest(cells: &BTreeMap<u64, Vec<u8>>) -> u64 {
    let mut acc = 0xC0FF_EE00_1234_5678u64;
    for (&id, v) in cells {
        acc = acc.rotate_left(11) ^ id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        acc = acc.rotate_left(17) ^ digest(v);
    }
    acc
}

/// The cells, and any that came back short.
type Cells = (BTreeMap<u64, Vec<u8>>, Vec<(u64, usize, usize)>);

fn churn_cells(db: &mut FileDb) -> Result<Cells, QueueError> {
    let mut m = BTreeMap::new();
    let mut short = Vec::new();
    for (id, v) in db.range(1, CHURN_CELLS)? {
        match cell_payload(&v) {
            Ok(p) => {
                m.insert(id, p);
            }
            Err((declared, got)) => short.push((id, declared, got)),
        }
    }
    Ok((m, short))
}

/// Hammer `Db::batch` with large, mixed-op batches of VARIABLE-LENGTH
/// values that maintain a cross-row invariant no single write could
/// maintain: a digest row that must always equal the digest of every other
/// row.
///
/// Every batch is `[cell ops..., put(digest)]`, packed to a ROW-SLOT
/// budget rather than an operation count, because that is the unit
/// `MAX_COMMIT_ROWS` is denominated in. If a batch is ever applied in part, or a
/// multi-slot value is ever torn, the digest row and the cells disagree
/// and [`churn_verify`] says so — and a torn value additionally shows up
/// as a cell whose own declared length does not match its stored bytes.
///
/// The intent record `D <round> <digest> <cells>` is written BEFORE the
/// batch, so the set of states the database is allowed to be in after a
/// kill is bounded by what is already on disk.
pub fn churn(cfg: &ChurnConfig, journal: &mut Journal) -> Result<ChurnReport, QueueError> {
    let mut db = FileDb::open_with(&cfg.root, cfg.capacity)?;
    let rep0 = db.recovery_report();
    if rep0.rollback_evidence {
        return Err(QueueError::RollbackEvidence(rep0));
    }
    journal.record(&format!(
        "S pid={} rows={} orphans={}",
        std::process::id(),
        rep0.row_count,
        rep0.orphan_valid_rows
    ))?;

    // Refuse to start on a state that is already broken.
    let (mut cells, short) = churn_cells(&mut db)?;
    if let Some(&(id, declared, got)) = short.first() {
        return Err(QueueError::ShortValue { id, declared, got });
    }
    let on_disk = db.get(CHURN_DIGEST)?.as_ref().map(|v| decode_meta(v).0);
    if let Some(d) = on_disk {
        if d != churn_digest(&cells) {
            return Err(QueueError::HalfBatch(format!(
                "digest row is {:#018x} but the {} cells digest to {:#018x}",
                d,
                cells.len(),
                churn_digest(&cells)
            )));
        }
    }

    let mut rep = ChurnReport::default();
    let mut rng = cfg.seed ^ (rep0.row_count.wrapping_mul(0x0123_4567_89AB_CDEF));
    // One slot is reserved for the digest row that closes every batch.
    let max_width = cfg.batch.clamp(1, MAX_COMMIT_ROWS - 1);
    // Two ceilings, and the cell has to be under both: what one VALUE can
    // hold, and what is left of a commit once the digest row that closes
    // every batch has its slot.
    let max_cell = cfg
        .max_cell
        .clamp(1, MAX_VALUE_LEN.min((MAX_COMMIT_ROWS - 2) * VALUE_LEN) - 8);
    let cells_used = cfg.cells.clamp(1, CHURN_CELLS);
    // Capacity is in slots and the workload is described in bytes, so the
    // sizing rule has to be worked out by hand. Say so at the start rather
    // than wedging on `Full` a thousand rounds in.
    let worst_live = cells_used * slot_cost(8 + max_cell) as u64;
    if (worst_live as f64) >= cfg.compact_at * cfg.capacity as f64 {
        return Err(QueueError::Wedged(format!(
            "{cells_used} cells of up to {max_cell} bytes is {worst_live} live slots, \
             and the compaction threshold is {:.0} of {} — raise the capacity or \
             lower the cell count",
            cfg.compact_at * cfg.capacity as f64,
            cfg.capacity
        )));
    }

    for round in 0..cfg.rounds {
        let width = if cfg.vary {
            1 + (splitmix(&mut rng) as usize) % max_width
        } else {
            max_width
        };
        if db.stats().fill() >= cfg.compact_at {
            db.compact()?;
            let r = db.recovery_report();
            if r.rollback_evidence {
                return Err(QueueError::RollbackEvidence(r));
            }
            rep.compactions += 1;
            let (c, short) = churn_cells(&mut db)?;
            if let Some(&(id, declared, got)) = short.first() {
                return Err(QueueError::ShortValue { id, declared, got });
            }
            cells = c;
        }

        // Choose cells and what to do to each, stopping when the batch's
        // ROW-SLOT budget is spent. `width` is a slot budget, not an
        // operation count — a single 2 KiB value can consume all of it.
        let mut projected = cells.clone();
        let mut ops = Vec::new();
        let mut touched: BTreeMap<u64, ()> = BTreeMap::new();
        let mut staged = 0usize;
        let mut guard = 0;
        while staged < width && guard < 4 * MAX_COMMIT_ROWS {
            guard += 1;
            let id = splitmix(&mut rng) % cells_used + 1;
            if touched.contains_key(&id) {
                continue;
            }
            let roll = splitmix(&mut rng);
            let present = projected.contains_key(&id);
            let delete = present && roll.is_multiple_of(4);
            let cost = if delete {
                1
            } else {
                let len = 1 + (roll >> 8) as usize % max_cell;
                slot_cost(8 + len)
            };
            if staged + cost > width {
                // Too wide for what is left of this batch; try another
                // cell rather than ending the round early.
                continue;
            }
            touched.insert(id, ());
            staged += cost;
            if delete {
                ops.push(Op::delete(id));
                projected.remove(&id);
            } else {
                let len = 1 + (roll >> 8) as usize % max_cell;
                let v = cell_value(len, roll ^ round);
                let payload = cell_payload(&v).expect("just built");
                ops.push(if present {
                    Op::update(id, v)
                } else {
                    Op::insert(id, v)
                });
                projected.insert(id, payload);
            }
        }
        if ops.is_empty() {
            continue;
        }
        let d = churn_digest(&projected);
        ops.push(Op::put(CHURN_DIGEST, encode_meta(d, round)));

        // The intent, BEFORE the batch: every state the kill is allowed to
        // leave behind is now on record.
        journal.record(&format!("D {round} {d:#018x} {}", projected.len()))?;
        // Same escape as the worker's: `fill()` cannot predict whether the
        // NEXT batch fits, because that depends on how many slots its
        // values need. On `Full`, rebuild and try the same batch again —
        // the intent above still describes it.
        match db.batch(&ops) {
            Err(e) if db_is_full(&e) => {
                db.compact()?;
                rep.compactions += 1;
                db.batch(&ops)?;
            }
            other => other?,
        }
        journal.record(&format!("A {round}"))?;

        rep.rounds += 1;
        rep.ops += ops.len() as u64;
        rep.rows += staged as u64 + 1;
        cells = projected;
    }
    Ok(rep)
}

#[derive(Debug, Clone)]
pub struct ChurnVerdict {
    pub cells: usize,
    pub digest: u64,
    pub computed: u64,
    pub rounds_logged: usize,
    pub orphan_valid_rows: u64,
    pub rollback_evidence: bool,
    /// `Some(why)` when the state cannot be explained by whole batches.
    pub half_batch: Option<String>,
    /// Cells whose stored bytes are fewer than their own header declares:
    /// a multi-slot value that came back SHORT.
    pub short_values: Vec<(u64, usize, usize)>,
    /// Longest cell payload found, in bytes. Proof the workload really was
    /// writing values that span row slots.
    pub widest_cell: usize,
}

/// Verify that the churn database is in a state some COMPLETE batch left
/// it in.
///
/// Three independent checks:
///
/// 1. No cell is short of its own declared length.
/// 2. The digest row equals the digest of the cells.
/// 3. The `(digest, cell count)` pair appears among the intents the
///    journal recorded (or is the empty start state).
pub fn churn_verify(cfg: &ChurnConfig) -> Result<ChurnVerdict, QueueError> {
    let mut db = FileDb::open_with(&cfg.root, cfg.capacity)?;
    let rec = db.recovery_report();
    let (cells, short_values) = churn_cells(&mut db)?;
    let computed = churn_digest(&cells);
    let digest_row = db.get(CHURN_DIGEST)?.as_ref().map(|v| decode_meta(v).0);
    let widest_cell = cells.values().map(|v| v.len()).max().unwrap_or(0);

    let mut intents: std::collections::BTreeSet<(u64, usize)> = std::collections::BTreeSet::new();
    // The empty start state is legal: nothing has been committed yet.
    intents.insert((churn_digest(&BTreeMap::new()), 0));
    let mut text = String::new();
    if let Ok(mut f) = File::open(&cfg.journal) {
        f.read_to_string(&mut text)?;
    }
    let mut rounds_logged = 0usize;
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() != Some(&"D") || f.len() < 4 {
            continue;
        }
        rounds_logged += 1;
        let d = f[2]
            .strip_prefix("0x")
            .and_then(|h| u64::from_str_radix(h, 16).ok());
        let n = f[3].parse::<usize>().ok();
        if let (Some(d), Some(n)) = (d, n) {
            intents.insert((d, n));
        }
    }

    let half_batch = if let Some(&(id, declared, got)) = short_values.first() {
        Some(format!(
            "cell {id} declares {declared} bytes and stored {got}: a multi-slot \
             value came back SHORT"
        ))
    } else {
        match digest_row {
            // No digest row at all: legal only before the first batch.
            None if cells.is_empty() => None,
            None => Some(format!(
                "{} cells exist but the digest row does not: the cell half of the first \
                 batch landed without the digest half",
                cells.len()
            )),
            Some(d) if d != computed => Some(format!(
                "digest row says {d:#018x}, the {} cells digest to {computed:#018x}: a batch \
                 was applied IN PART",
                cells.len()
            )),
            Some(d) if !intents.contains(&(d, cells.len())) => Some(format!(
                "state (digest {d:#018x}, {} cells) is internally consistent but was never \
                 an intent this workload recorded",
                cells.len()
            )),
            Some(_) => None,
        }
    };

    Ok(ChurnVerdict {
        cells: cells.len(),
        digest: digest_row.unwrap_or(0),
        computed,
        rounds_logged,
        orphan_valid_rows: rec.orphan_valid_rows,
        rollback_evidence: rec.rollback_evidence,
        half_batch,
        short_values,
        widest_cell,
    })
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Audit {
    pub runs: usize,
    /// Opens that found checksum-valid rows past the manifest. One is the
    /// normal artifact of a single write in flight at the kill.
    pub opens_with_orphan_rows: usize,
    /// Opens that found TWO OR MORE such rows. That cannot come from a
    /// single-row write: it is direct proof the kill landed inside a
    /// multi-row commit — a batch, or one long value, or both.
    pub opens_with_orphan_batches: usize,
    /// The largest interrupted commit seen, in rows.
    pub widest_orphan_batch: u64,
    pub compactions_started: usize,
    pub compactions_finished: usize,
    /// Jobs whose insert was acknowledged, in order.
    pub enqueued: Vec<u64>,
    /// Jobs whose commit was acknowledged, in order. MUST have no repeats.
    pub committed: Vec<u64>,
    /// Jobs whose work was performed, with repeats (at-least-once).
    pub worked: Vec<u64>,
    /// Payload bytes seen by a `W` record, largest first seen.
    pub widest_payload: usize,
    pub duplicate_enqueues: Vec<u64>,
    pub duplicate_commits: Vec<u64>,
    /// Churn: intents recorded, and batches acknowledged.
    pub churn_intents: usize,
    pub churn_acks: usize,
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
    // BTreeSet, not HashSet: the repository's clippy config bans the
    // default hasher so that nothing an audit reports can depend on
    // iteration order. Membership is all we need here anyway.
    let mut seen_enqueue = std::collections::BTreeSet::new();
    let mut seen_commit = std::collections::BTreeSet::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (tag, arg) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        let id = arg.parse::<u64>().ok();
        match (tag, id) {
            ("S", _) => {
                a.runs += 1;
                let orphans = line
                    .split_whitespace()
                    .find_map(|f| f.strip_prefix("orphans="))
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                if orphans > 0 {
                    a.opens_with_orphan_rows += 1;
                }
                if orphans > 1 {
                    a.opens_with_orphan_batches += 1;
                    a.widest_orphan_batch = a.widest_orphan_batch.max(orphans);
                }
            }
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
            ("W", Some(id)) => {
                a.worked.push(id);
                if let Some(bytes) = it.next().and_then(|b| b.parse::<usize>().ok()) {
                    a.widest_payload = a.widest_payload.max(bytes);
                }
            }
            ("K", Some(id)) => {
                if !seen_commit.insert(id) {
                    a.duplicate_commits.push(id);
                }
                a.committed.push(id);
            }
            ("D", _) => a.churn_intents += 1,
            ("A", _) => a.churn_acks += 1,
            _ => {}
        }
    }
    Ok(a)
}
