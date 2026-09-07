//! Things that still go wrong, each pinned by the smallest test that
//! shows it. These are findings about the library, written from the
//! application's side; the comment on each says what a caller can and
//! cannot do about it.
//!
//! The repository's clippy config disallows a clock inside the
//! deterministic boundary; one test here measures wall time on purpose.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::time::Instant;

use dabqlite::{Db, MemDb, Op, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN, VALUE_LEN};
use kvstore::store::{rebuild, Store};
use kvstore::Config;
use kvstore::KvError;

fn dir_for(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("kvstore-res-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// **A compaction no longer discards writes committed while it runs.**
///
/// This test used to record the sharpest hole in the API. `Db::compact`
/// reclaims dead slots in place but cannot re-place rows, and a hash
/// table has to re-place them: dropping a tombstone re-homes every key
/// that probed past it. The only primitive that put a rebuilt database
/// back on disk was `Db::restore`, and `restore` refuses to run while a
/// writer holds the directory — so `kv compact` had to RELEASE the
/// single-writer lock, rebuild, and swap. Anything committed in that
/// window was acknowledged, fsynced, and then thrown away with no error
/// anywhere.
///
/// `Db::rebuild_with` closed it: the transform sees the live rows and
/// what it returns becomes the database, with the lock held from the read
/// to the reopen. There is no window left to write into, which is what
/// this now proves — a second writer cannot even open the directory while
/// the rebuild is running.
#[test]
fn a_compaction_holds_the_lock_so_there_is_no_window_to_lose_a_write_in() {
    let dir = dir_for("compact-race");
    let cfg = Config::new(&dir).with_rows(4096);
    {
        let mut s = Store::open(&cfg).unwrap();
        s.set("keep", b"1", 0, 0).unwrap();
        s.set("doomed", b"2", 0, 0).unwrap();
        s.del("doomed", 0).unwrap();
        assert!(s.stats().dead > 0);
    }

    // The rebuild is one call made by one holder of the lock, and while
    // it runs nobody else can open the directory to write into it.
    {
        let mut s = Store::open(&cfg).unwrap();
        let dead_before = s.stats().dead;
        s.db()
            .rebuild_with(|rows| {
                // Standing where the old code released the lock: a second
                // writer is refused, so there is no window.
                assert!(
                    matches!(Store::open(&cfg), Err(KvError::Locked { .. })),
                    "the writer lock must be held across the rebuild"
                );
                rows
            })
            .unwrap();
        assert!(s.stats().dead < dead_before);
        assert!(s.get("keep", 0).unwrap().is_some());
    }

    // And the real thing: `kv compact` re-places every record, keeps
    // everything live, and leaves a database another writer can take.
    {
        let mut s = Store::open(&cfg).unwrap();
        s.set("late", b"acknowledged", 0, 0).unwrap();
    }
    let mut warn = Vec::new();
    kvstore::exec::execute(&cfg, &kvstore::exec::Command::Compact, &mut warn).expect("compact");
    let mut after = Store::open(&cfg).unwrap();
    assert!(after.get("keep", 0).unwrap().is_some());
    assert_eq!(
        after.get("late", 0).unwrap().unwrap().value,
        b"acknowledged",
        "a write committed before a compaction must survive it"
    );
    assert!(after.get("doomed", 0).unwrap().is_none());
    assert_eq!(after.stats().dead, 0);
    drop(after);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Reading a value is LINEAR in its length.**
///
/// It was quadratic. `Db::get` reassembles a long value from bounded
/// windows, one engine call per window, and each call re-measured the
/// value's run to learn its total length — so a k-slot read walked the
/// run k times. Measured here at 0.25 us/slot for a 2-slot value against
/// 5.6 us/slot for a 128-slot one: reading `MAX_VALUE_LEN` cost about
/// 720 us against 0.5 us for sixteen bytes, on every single `get`, which
/// for a session store keeping 2 KiB blobs is the whole read path.
///
/// A ratio, not a wall-clock threshold, so it does not depend on the
/// machine. Sixteen times the slots should cost about sixteen times as
/// much, and the bound below is loose enough to survive a noisy box and
/// tight enough that the quadratic shape (which measured 50x) fails it.
#[test]
fn reading_a_long_value_costs_about_linearly_more() {
    let mut db = MemDb::in_memory_with(4096).unwrap();
    let short = 8 * VALUE_LEN;
    let long = MAX_VALUE_LEN;
    let time = |db: &mut MemDb, id: u64, reps: u32| {
        let t = Instant::now();
        for _ in 0..reps {
            assert!(db.get(id).unwrap().is_some());
        }
        t.elapsed().as_secs_f64() / reps as f64
    };
    db.put(1, Value::from_vec(vec![b'x'; short]).unwrap())
        .unwrap();
    db.put(2, Value::from_vec(vec![b'x'; long]).unwrap())
        .unwrap();
    // Warm.
    time(&mut db, 1, 200);
    time(&mut db, 2, 50);

    let t_short = time(&mut db, 1, 2000);
    let t_long = time(&mut db, 2, 500);
    let slot_ratio = (long / VALUE_LEN) as f64 / (short / VALUE_LEN) as f64; // 16x
    let time_ratio = t_long / t_short;
    assert!(
        time_ratio < slot_ratio * 2.5,
        "reading {long} bytes took {time_ratio:.1}x reading {short}, for \
         {slot_ratio:.0}x the slots — that is not linear"
    );
}

/// **Two maximum-length values now share a commit, and the batch bound is
/// stated in the units it is actually enforced in.**
///
/// `MAX_COMMIT_ROWS` used to be 128 row slots and `MAX_VALUE_LEN` 2048
/// bytes — exactly 128 slots — so one full-length value consumed an entire
/// commit and could never be made atomic with anything else. For a
/// key/value store that meant "rotate this session token and write the
/// audit line" was expressible only if both records were small. The two
/// limits are now separate: the commit is eight times the longest value.
///
/// The bound that remains is a commit's worth of SLOTS, and when a batch
/// crosses it the rejection names the operation it crossed at. That
/// operation may be perfectly legal on its own — which the library now
/// says outright on `Error::BatchRejected` ("stopped at", not "was
/// wrong"), rather than leaving the caller to infer it from a `cause`
/// that contradicts the first assertion below.
#[test]
fn two_long_values_share_a_commit_and_the_slot_bound_is_stated_honestly() {
    let mut db = MemDb::in_memory_with(4096).unwrap();
    let big = || Value::from_vec(vec![b'x'; MAX_VALUE_LEN]).unwrap();
    let per_value = Op::put(1, big()).rows();
    assert_eq!(per_value, MAX_VALUE_LEN / VALUE_LEN);
    assert!(per_value < MAX_COMMIT_ROWS, "a value is not a whole commit");
    // On its own, that write is fine.
    db.put(1, big()).unwrap();

    // And so is the pair — the whole point. "Rotate the token and write
    // the audit line" is now expressible at any record size.
    db.batch(&[Op::put(2, big()), Op::put(3, big())])
        .expect("two maximum-length values, atomically");
    assert_eq!(db.get(2).unwrap().unwrap().as_bytes().len(), MAX_VALUE_LEN);
    assert_eq!(db.get(3).unwrap().unwrap().as_bytes().len(), MAX_VALUE_LEN);

    // The bound that IS still there, and the operation it is blamed on.
    let fits = MAX_COMMIT_ROWS / per_value;
    let ops: Vec<Op> = (0..=fits as u64).map(|i| Op::put(100 + i, big())).collect();
    let err = db.batch(&ops).unwrap_err();
    match err {
        dabqlite::Error::BatchRejected { at, ref cause } => {
            assert_eq!(at, fits, "the operation the batch crossed the line at");
            assert!(
                matches!(
                    **cause,
                    dabqlite::Error::BatchTooLong { rows, .. }
                        if rows == (fits + 1) * per_value
                ),
                "{cause:?}"
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    // The remedy is still stated, one `source()` down.
    assert!(err.to_string().contains("split it into several batches"));
    assert_eq!(db.get(100).unwrap(), None, "and nothing in it landed");
}

/// **`Stats::free()` is exact, and a write that uses the last slot lands.**
///
/// Worth pinning because the whole `RESERVED_SLOTS` reserve in this crate
/// is built on believing it: if `free()` over-reported, a `set` that
/// passed the room check would fail mid-command.
#[test]
fn a_write_that_takes_the_last_free_slot_is_accepted() {
    let mut db = MemDb::in_memory_with(16).unwrap();
    db.put(1, Value::from_vec(vec![b'a'; 10 * VALUE_LEN]).unwrap())
        .unwrap();
    let free = db.stats().free();
    assert_eq!(free, 6);
    let exact = Value::from_vec(vec![b'b'; free as usize * VALUE_LEN]).unwrap();
    assert_eq!(Op::put(2, exact.clone()).rows() as u64, free);
    db.put(2, exact).unwrap();
    assert_eq!(db.stats().free(), 0);

    // And at zero free slots even a DELETE is refused, which is why this
    // crate keeps a reserve of its own.
    let err = db.remove(1).unwrap_err();
    assert!(matches!(err, dabqlite::Error::Full { .. }), "{err:?}");
}

/// **An expired session costs its slots until something rewrites it.**
///
/// There is no TTL in the library, so `kv` writes an expiry into the
/// value and hides the record at read time. The rows stay: an expired
/// 2 KiB session holds 128 slots until `purge` tombstones it (costing one
/// more) and `compact` rebuilds. A store whose whole job is sessions
/// therefore has to run two maintenance commands to get its space back.
#[test]
fn expired_sessions_hold_their_slots_until_purge_and_compact() {
    let dir = dir_for("expiry-cost");
    let cfg = Config::new(&dir).with_rows(8192);
    let mut s = Store::open(&cfg).unwrap();
    for i in 0..20 {
        s.set(&format!("session/{i}"), &vec![b's'; 1000], 100, 0)
            .unwrap();
    }
    let full = s.stats().slots;
    assert!(
        full >= 20 * 63,
        "{full} slots for twenty 1000-byte sessions"
    );

    // Every one of them is invisible, and every one still costs.
    assert!(s.entries(200).unwrap().is_empty());
    assert_eq!(s.stats().slots, full);
    assert_eq!(s.stats().dead, 0, "the library sees twenty live rows");

    assert_eq!(s.purge(200).unwrap().len(), 20);
    assert_eq!(s.stats().slots, full + 20, "purge COSTS twenty more slots");
    let entries = s.entries(200).unwrap();
    drop(s);

    let mut fresh = rebuild(&entries, 8192).unwrap();
    MemDb::restore(&dir, &fresh.snapshot().unwrap()).unwrap();
    let rebuilt = Store::open(&cfg).unwrap();
    assert_eq!(rebuilt.stats().slots, 0);
    drop(rebuilt);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A directory CAN be asked whether it holds a database.**
///
/// It could not. `Db::read_only` on an empty directory failed with a raw
/// `Io` error naming a file the public API never mentioned, and
/// `Db::open` would CREATE one — so "read this database if it exists"
/// had to be written by hardcoding the superblock's filename, which
/// `exec.rs` did. A sample reaching for a private detail is the API
/// saying it is missing something.
#[test]
fn a_directory_can_be_asked_whether_it_holds_a_database() {
    let dir = dir_for("empty-dir");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!Db::exists(&dir), "an empty directory holds nothing");

    // And the CLI answers accordingly instead of creating one.
    let cfg = Config::new(&dir);
    let mut warn = Vec::new();
    let out = kvstore::exec::execute(
        &cfg,
        &kvstore::exec::Command::Get {
            key: "nothing".into(),
            raw: false,
        },
        &mut warn,
    )
    .expect("a missing database reads as empty, not as an error");
    assert!(
        matches!(out, kvstore::exec::Outcome::Missing { .. }),
        "{out:?}"
    );
    assert!(
        !Db::exists(&dir),
        "reading a path that holds nothing must not make a database there"
    );

    {
        let mut s = Store::open(&cfg).unwrap();
        s.set("k", b"v", 0, 0).unwrap();
    }
    assert!(Db::exists(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}
