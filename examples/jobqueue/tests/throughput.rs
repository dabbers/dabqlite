//! What a batch is worth, and what a job costs, measured rather than
//! asserted.
//!
//! Run with `cargo test --test throughput -- --nocapture` to see the
//! numbers. The assertions are loose floors so the file is a measurement
//! that also fails when batching stops saving commits, not a benchmark
//! pretending to be a correctness check.
//!
//! Method, so the numbers are auditable:
//!
//! * Every measurement writes to a fresh file-backed database in
//!   `std::env::temp_dir()`, with real `fsync`s — the same path the worker
//!   uses. Nothing is in memory and nothing is warmed up.
//! * `ROWS` rows are written by every raw variant; the only variable is
//!   how many rows share a commit.
//! * The queue numbers drive the real worker (`jobqueue::run`) end to end,
//!   through enqueue, claim, work and commit, with the real payload
//!   mixture — so they include the cost of writing and re-reading values
//!   that span row slots.
//! * The clock is wall time around the loop, `Instant::now`, single
//!   threaded, nothing else running in the same test binary (these tests
//!   are `#[test]`s in one file and cargo may run them in parallel; the
//!   ratios are what matter and they are measured inside one function).

use std::path::PathBuf;
use std::time::Instant;

use dabqlite::{Db, Op, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN};
use jobqueue::{payload_of, run, Config, Journal};

const ROWS: u64 = 600;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-tput-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `Instant::now` is banned inside the deterministic boundary by the
/// repository's clippy config. A stopwatch is exactly what this file is.
#[allow(clippy::disallowed_methods)]
fn timed(tag: &str, width: usize, value: &Value) -> f64 {
    let dir = scratch(tag).join("db");
    let slots = value.len().div_ceil(16).max(1) as u64;
    let mut db = Db::open_with(&dir, ROWS * slots + 16).unwrap();

    let start = Instant::now();
    if width <= 1 {
        for id in 0..ROWS {
            db.put(id, value.clone()).unwrap();
        }
    } else {
        let mut id = 0u64;
        while id < ROWS {
            let n = (width as u64).min(ROWS - id);
            let ops: Vec<Op> = (id..id + n).map(|k| Op::put(k, value.clone())).collect();
            db.batch(&ops).unwrap();
            id += n;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    assert_eq!(db.len(), ROWS);
    drop(db);
    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    ROWS as f64 / elapsed
}

#[test]
fn a_batch_of_n_costs_one_commit_not_n() {
    let short = Value::from_bytes(&[0x5A; 16]).unwrap();
    let single = timed("single", 1, &short);
    let pair = timed("pair", 2, &short);
    let eight = timed("eight", 8, &short);
    let full = timed("full", MAX_COMMIT_ROWS, &short);

    eprintln!("rows/sec writing {ROWS} 16-byte rows to a file-backed database:");
    eprintln!("  1 row  per commit : {single:>10.0}");
    eprintln!(
        "  2 rows per commit : {pair:>10.0}  ({:.2}x)",
        pair / single
    );
    eprintln!(
        "  8 rows per commit : {eight:>10.0}  ({:.2}x)",
        eight / single
    );
    eprintln!(
        "  {MAX_COMMIT_ROWS} rows per commit: {full:>10.0}  ({:.2}x)",
        full / single
    );

    // Loose floors. The absolute numbers move by 6x between runs on a
    // shared machine; the RATIO is what is being asserted, and only
    // loosely, so that this fails when batching stops saving commits
    // rather than when the box is busy.
    assert!(
        pair > single * 1.15,
        "two rows per commit ({pair:.0}/s) was not faster than one \
         ({single:.0}/s) — batching is not saving a commit"
    );
    assert!(
        full > single * 4.0,
        "{MAX_COMMIT_ROWS} rows per commit ({full:.0}/s) was not 4x one per commit \
         ({single:.0}/s) — a batch is costing per-row fsyncs"
    );
}

/// A long value is one commit, so it costs one commit's fsyncs however
/// many slots it fills. That is the whole reason a job can carry a real
/// payload: 2 KiB in one write is not 128 writes.
#[test]
fn a_long_value_costs_one_commit_not_one_per_slot() {
    let one_slot = Value::from_bytes(&[0x11; 16]).unwrap();
    let full_slot_run = Value::from_bytes(&vec![0x22; MAX_VALUE_LEN]).unwrap();

    let short_rows = timed("short1", 1, &one_slot);
    let long_rows = timed("long1", 1, &full_slot_run);

    eprintln!("values/sec, one value per commit:");
    eprintln!("  16-byte value       : {short_rows:>10.0}");
    eprintln!(
        "  {MAX_VALUE_LEN}-byte value ({} slots): {long_rows:>10.0}  ({:.2}x)",
        MAX_VALUE_LEN / 16,
        long_rows / short_rows
    );
    eprintln!(
        "  ...which is {:>10.0} bytes/sec against {:.0} bytes/sec",
        long_rows * MAX_VALUE_LEN as f64,
        short_rows * 16.0
    );

    assert!(
        long_rows > short_rows * 0.25,
        "a 128-slot value cost more than 4x a 1-slot value ({long_rows:.0}/s vs \
         {short_rows:.0}/s) — the slots are not sharing one commit"
    );
}

/// The number a job queue actually cares about: jobs per second through
/// the whole pipeline, before and after batching the enqueue.
///
/// The ceiling is structural, not a tuning problem. A job costs three
/// commits — enqueue, claim, commit — and only the enqueue can be batched,
/// because claim and commit are decided one job at a time by reading the
/// head of the queue. So batching moves the cost per job from 3 commits to
/// 2 + 1/chunk, and 2 is the floor for this design on this API.
///
/// That is why the numbers below are printed rather than asserted on: the
/// honest headline is that `Db::batch` is worth 25-60x on raw writes and
/// about 1.1x on this queue, and the difference between those two numbers
/// is the part of the workload the API cannot batch.
#[allow(clippy::disallowed_methods)]
#[test]
fn jobs_per_second_before_and_after_batching_the_enqueue() {
    let jobs = 150u64;

    let measure = |tag: &str, chunk: u64| -> (f64, u64) {
        let root = scratch(tag);
        let journal_path = root.join("j.log");
        let mut cfg = Config::new(root.join("db"), &journal_path, jobs);
        cfg.capacity = 65_536;
        cfg.window = jobs;
        cfg.enqueue_chunk = chunk;
        cfg.compact_at = 0.9;
        let mut journal = Journal::open(&journal_path).unwrap();
        let start = Instant::now();
        let rep = run(&cfg, &mut journal).unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        assert!(rep.drained, "{rep:?}");
        assert_eq!(rep.committed, jobs);
        std::fs::remove_dir_all(&root).ok();
        (jobs as f64 / elapsed, rep.batches)
    };

    let (before, before_batches) = measure("q1", 1);
    let (after, after_batches) = measure("q32", 32);

    let bytes: u64 = (1..=jobs).map(|id| payload_of(id).len() as u64).sum();
    eprintln!("end-to-end queue throughput, {jobs} jobs, {bytes} payload bytes:");
    eprintln!("  enqueue one at a time : {before:>8.1} jobs/sec ({before_batches} commits)");
    eprintln!(
        "  enqueue 32 at a time  : {after:>8.1} jobs/sec ({after_batches} commits, {:.2}x)",
        after / before
    );

    // The commit count is the deterministic part and is what is asserted.
    // The jobs/sec ratio is NOT asserted, and the reason is the finding:
    // batching the enqueue removes at most one of the three commits a job
    // costs, so the end-to-end gain is around 1.0-1.3x and disappears into
    // the noise of a shared machine. There is no API shape that would let
    // this queue batch the other two, because claim and commit are decided
    // one job at a time from the head of the queue.
    assert!(
        after_batches < before_batches,
        "batching did not reduce the commit count: {before_batches} -> {after_batches}"
    );
    assert!(
        before > 1.0 && after > 1.0,
        "the queue did not make progress at all: {before:.1} / {after:.1} jobs/sec"
    );
}
