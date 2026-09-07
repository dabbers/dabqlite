//! What happens as the database fills up, and what an application has to do
//! about it. These tests are as much documentation of the behaviour as
//! they are checks on it.

use std::path::PathBuf;

use dabqlite::{Db, Error, Op, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN, VALUE_LEN};
use jobqueue::{
    audit, payload_of, run, slot_cost, Config, Job, Journal, MAX_PAYLOAD, PENDING,
    ROW_COMMIT_WATERMARK, ROW_ENQUEUE_WATERMARK,
};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-cap-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn v(s: &str) -> Value {
    Value::from_text(s).unwrap()
}

/// The slot arithmetic an application has to internalise. Nothing is ever
/// reclaimed in place, so a steady-state queue consumes capacity linearly
/// in the number of operations — and now also in the SIZE of what those
/// operations write.
#[test]
fn a_slot_is_consumed_per_sixteen_bytes_written_and_never_returned() {
    let mut db = Db::in_memory_with(4096).unwrap();
    let short = Job::new(PENDING, 0, vec![1, 2, 3]).encode().unwrap();
    let long = Job::new(PENDING, 0, vec![9; 500]).encode().unwrap();
    assert_eq!(slot_cost(long.len()), 32);

    db.insert(1, short.clone()).unwrap();
    assert_eq!((db.stats().slots, db.stats().dead), (1, 0));

    // The killer for a queue: flipping ONE header byte on a long value
    // rewrites all of it, so the update costs another 32 slots.
    db.insert(2, long.clone()).unwrap();
    assert_eq!(db.stats().slots, 33);
    db.update(2, long.clone()).unwrap();
    assert_eq!(
        db.stats().slots,
        65,
        "an update of a 500-byte value is 32 more slots"
    );

    db.delete(2).unwrap();
    assert_eq!(
        (db.stats().slots, db.stats().live),
        (66, 1),
        "delete = +1 slot whatever the value's size"
    );

    // One full job lifecycle for job id 7, in slots: enqueue batch (job +
    // watermark), claim (rewrite), commit batch (watermark + tombstone).
    let mut db = Db::in_memory_with(65_536).unwrap();
    let job = Job::new(PENDING, 0, payload_of(7)).encode().unwrap();
    let cost = slot_cost(job.len());
    db.batch(&[
        Op::insert(7, job.clone()),
        Op::put(ROW_ENQUEUE_WATERMARK, v("w")),
    ])
    .unwrap();
    db.update(7, job.clone()).unwrap();
    db.batch(&[Op::put(ROW_COMMIT_WATERMARK, v("c")), Op::delete(7)])
        .unwrap();
    assert_eq!(
        db.stats().slots,
        2 * cost as u64 + 3,
        "a job's lifetime costs 2x its payload in slots plus three bookkeeping rows"
    );
    assert_eq!(db.stats().live, 2, "only the two meta rows survive");
}

/// **`Stats::dead` counts RECORDS, not slots, and is therefore wrong for
/// every value that spans more than one row.**
///
/// `Stats::dead` is quoted in ROW SLOTS, the unit capacity is in.
///
/// It used to count retired RECORDS while `slots` and `capacity` counted
/// slots, so after superseding and then deleting one 500-byte value — 65
/// slots, all 65 reclaimable — it reported 3. That was not cosmetic:
/// `dead` is the number an application is told to watch when deciding
/// whether a rebuild is worth doing, and an application compacting when
/// `dead > slots / 2` would never have compacted a database made of long
/// values, and would have hit `Full` with 98% of its slots reclaimable.
///
/// The library fixed it. This crate can trigger on `dead` again rather
/// than on `fill()` alone.
#[test]
fn stats_dead_is_in_slots_like_everything_else() {
    let mut db = Db::in_memory_with(4096).unwrap();
    let long = Value::from_vec(vec![7u8; 500]).unwrap();
    db.insert(1, long.clone()).unwrap();
    assert_eq!((db.stats().slots, db.stats().dead), (32, 0));

    db.update(1, long.clone()).unwrap();
    assert_eq!(
        (db.stats().slots, db.stats().dead),
        (64, 32),
        "superseding a 32-slot value retires 32 slots"
    );

    db.delete(1).unwrap();
    let s = db.stats();
    assert_eq!(
        (s.slots, s.live, s.dead),
        (65, 0, 65),
        "the whole database is dead and `dead` says so"
    );

    // And it predicts exactly what a rebuild returns, which is what the
    // number is for.
    let reclaimable = s.slots - db.compact_to_memory().unwrap().stats().slots;
    assert_eq!(reclaimable, 65);
    assert_eq!(s.dead, reclaimable);
    assert_eq!(s.reclaimable(), s.free() + s.dead);
}

/// **A full database cannot be emptied in place.** `delete` needs a free
/// slot for its tombstone, so once `Full` is reached every mutation —
/// including the ones that would free space — is refused.
///
/// That is still true, and it is still the sharpest edge in the API. What
/// changed is that the error now names the way out and the way out
/// depends on `dead`: with slots to reclaim, a rebuild; with none, a
/// larger capacity. Both escapes are exercised below.
#[test]
fn a_full_database_cannot_be_emptied_but_the_error_names_the_way_out() {
    let mut db = Db::in_memory_with(4).unwrap();
    for i in 0..4u64 {
        db.insert(i, v("x")).unwrap();
    }
    assert!(matches!(
        db.insert(4, v("x")),
        Err(Error::Full { capacity: 4, .. })
    ));
    assert!(matches!(
        db.update(0, v("y")),
        Err(Error::Full { capacity: 4, .. })
    ));
    assert_eq!(
        db.delete(0),
        Err(Error::Full {
            capacity: 4,
            dead: 0
        }),
        "DELETE is refused on a full database: the only operation that could \
         make room needs room to run"
    );
    let rejected = db.batch(&[Op::delete(0), Op::delete(1)]).unwrap_err();
    match &rejected {
        Error::BatchRejected { at, cause } => {
            assert_eq!(*at, 0);
            assert!(matches!(**cause, Error::Full { capacity: 4, .. }));
        }
        other => panic!("expected BatchRejected, got {other:?}"),
    }
    let source = std::error::Error::source(&rejected).expect("a rejected batch has a source");
    assert!(
        source.to_string().contains("larger capacity"),
        "with nothing to reclaim the error must send the caller to a bigger \
         database: {source}"
    );
    // Which works: the bytes are the same database with more room.
    let snapshot = db.snapshot().unwrap();
    let mut bigger = Db::load_with(&snapshot, 16).unwrap();
    bigger.delete(0).unwrap();
    assert_eq!(bigger.stats().live, 3);

    // And it is not about being at 100% live: three live rows in a
    // five-slot database is equally stuck for the same reason. But here
    // there IS dead weight, so the error points at a rebuild instead, and
    // the rebuild unsticks it without a bigger database.
    let mut db = Db::in_memory_with(5).unwrap();
    for i in 0..4u64 {
        db.insert(i, v("x")).unwrap();
    }
    db.delete(0).unwrap();
    assert_eq!(db.stats().live, 3);
    let e = db.delete(1).unwrap_err();
    assert!(matches!(
        e,
        Error::Full {
            capacity: 5,
            dead: 2
        }
    ));
    assert!(e.to_string().contains("Db::compact()"), "{e}");
    let mut rebuilt = db.compact_to_memory().unwrap();
    assert_eq!(rebuilt.stats().free(), 2);
    rebuilt.delete(1).unwrap();
    assert_eq!(rebuilt.stats().live, 2);
}

/// Variable-length values make `Full` arrive in units the caller cannot
/// see coming from `Stats` alone: a database with 40 free slots refuses a
/// 41-slot value and accepts a 40-slot one, and nothing in `Stats` is
/// denominated in bytes.
#[test]
fn a_long_value_can_be_refused_while_a_short_one_fits() {
    let mut db = Db::in_memory_with(64).unwrap();
    db.insert(1, Value::from_vec(vec![0; 24 * VALUE_LEN]).unwrap())
        .unwrap();
    let s = db.stats();
    assert_eq!((s.slots, s.capacity), (24, 64));
    // `Stats::free` answers this directly now, and `Op::rows` says what a
    // write will cost — so "will this fit" is answerable before trying.
    assert_eq!(s.free(), 40);
    let too_big = Op::insert(2, Value::from_vec(vec![0; 41 * VALUE_LEN]).unwrap());
    assert_eq!(too_big.rows(), 41);
    assert!(too_big.rows() > s.free() as usize);

    assert!(
        matches!(
            db.insert(2, Value::from_vec(vec![0; 41 * VALUE_LEN]).unwrap()),
            Err(Error::Full { capacity: 64, .. })
        ),
        "41 slots into 40 free"
    );
    db.insert(2, Value::from_vec(vec![0; 40 * VALUE_LEN]).unwrap())
        .unwrap();
    assert_eq!(db.stats().slots, 64);
}

/// A rejected batch performs NO I/O and leaves the database byte-identical.
#[test]
fn a_rejected_batch_changes_nothing() {
    let mut db = Db::in_memory_with(4096).unwrap();
    db.insert(1, v("one")).unwrap();
    let before = db.stats();
    let snap_before = db.snapshot().unwrap().to_bytes();

    let e = db
        .batch(&[
            Op::put(2, Value::from_vec(vec![3; 700]).unwrap()),
            Op::put(3, v("three")),
            Op::insert(1, v("clash")),
        ])
        .unwrap_err();
    assert_eq!(
        e,
        Error::BatchRejected {
            at: 2,
            cause: Box::new(Error::AlreadyExists { id: 1 })
        }
    );
    assert_eq!(db.stats(), before, "a refused batch consumed a slot");
    assert_eq!(db.get(2).unwrap(), None, "an op before the bad one landed");
    assert_eq!(db.snapshot().unwrap().to_bytes(), snap_before);
}

/// The escape from `Full` for a file-backed database is to reopen with a
/// bigger capacity — and a database now REMEMBERS its capacity, so the
/// plain `Db::open` no longer silently hands back a different ceiling than
/// the one the data was written under. That was a real trap: `open` used
/// to mean `open_with(DEFAULT_ROWS)`, so a queue that had grown to 200,000
/// slots reopened at 65,536 and refused itself.
#[test]
fn capacity_is_remembered_and_growing_is_the_escape_from_full() {
    let dir = scratch("grow").join("db");
    {
        let mut db = Db::open_with(&dir, 4).unwrap();
        for i in 0..4u64 {
            db.insert(i, v("x")).unwrap();
        }
        assert!(matches!(db.delete(0), Err(Error::Full { .. })));
    }
    // The plain open honours the 4 that was recorded, rather than the
    // 65,536 default.
    {
        let mut db = Db::open(&dir).unwrap();
        assert_eq!(db.stats().capacity, 4);
        assert!(matches!(db.delete(0), Err(Error::Full { .. })));
    }

    let mut db = Db::open_with(&dir, 64).unwrap();
    db.delete(0).unwrap();
    assert_eq!(db.len(), 3);
    drop(db);
    // ...and the new capacity is what a plain reopen now uses, because
    // `open_with` recorded it at the next commit.
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.stats().capacity, 64);
    drop(db);

    // But shrinking back is refused, so capacity is a ratchet.
    let e = Db::open_with(&dir, 4).unwrap_err();
    assert_eq!(
        e,
        Error::CapacityTooSmall {
            required: 5,
            asked: 4
        }
    );
    assert!(e.to_string().contains("reopen with at least 5"), "{e}");
    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
}

/// `Db::compact()` reclaims dead slots in place, on `&mut self`.
#[test]
fn compact_reclaims_dead_slots_in_place() {
    let root = scratch("compact");
    let dir = root.join("db");

    let mut db = Db::open_with(&dir, 65_536).unwrap();
    for i in 0..50u64 {
        db.insert(i, Value::from_vec(payload_of(i + 1)).unwrap())
            .unwrap();
    }
    for i in 0..50u64 {
        db.put(i, v("touched")).unwrap();
    }
    for i in 0..25u64 {
        db.remove(i).unwrap();
    }
    let before = db.stats();
    assert!(before.dead > 0);

    // The whole compaction, on a handle held in a local: no `Option`, no
    // move out and back, no rebinding.
    db.compact().unwrap();
    assert_eq!(db.stats().slots, 25);
    assert_eq!(db.stats().dead, 0);
    assert_eq!(db.len(), 25);
    assert_eq!(db.get(30).unwrap().unwrap().text(), "touched");
    assert_eq!(db.get(3).unwrap(), None);
    drop(db);
    let reopened = Db::open_with(&dir, 65_536).unwrap();
    assert_eq!(reopened.len(), 25);
    assert_eq!(reopened.stats().dead, 0);
    std::fs::remove_dir_all(&root).ok();
}

/// Compaction of a database full of LONG values: the rebuild has to pack
/// its own batches by slot cost, and it does — including a value that
/// fills a whole commit by itself.
#[test]
fn compact_rebuilds_multi_slot_values_exactly() {
    let root = scratch("compact-long");
    let dir = root.join("db");
    let mut db = Db::open_with(&dir, 65_536).unwrap();

    let lens = [1usize, 17, MAX_PAYLOAD, 999, MAX_VALUE_LEN];
    for (i, &len) in lens.iter().enumerate() {
        let mut val = vec![(i + 1) as u8; len];
        // Trailing zeros, to check the rebuild does not trim them either.
        if let Some(tail) = val.last_mut() {
            *tail = 0;
        }
        db.put(i as u64, Value::from_vec(val).unwrap()).unwrap();
        db.put(i as u64, Value::from_vec(vec![(i + 1) as u8; len]).unwrap())
            .unwrap();
    }
    let before: Vec<_> = db.all().unwrap();
    assert!(db.stats().dead > 0);

    db.compact().unwrap();
    assert_eq!(db.stats().dead, 0);
    assert_eq!(db.all().unwrap(), before, "a rebuild changed the rows");
    for (i, &len) in lens.iter().enumerate() {
        assert_eq!(db.get(i as u64).unwrap().unwrap().len(), len);
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Compaction cannot rescue a database that is full of LIVE rows, and says
/// so honestly rather than looking like it worked.
#[test]
fn compact_does_not_rescue_a_database_full_of_live_rows() {
    let root = scratch("compact-live");
    let dir = root.join("db");
    let mut db = Db::open_with(&dir, 8).unwrap();
    for i in 0..8u64 {
        db.insert(i, v("x")).unwrap();
    }
    assert!(matches!(db.delete(0), Err(Error::Full { .. })));
    db.compact().unwrap();
    assert_eq!(db.stats().slots, 8, "there was nothing dead to reclaim");
    assert_eq!(db.stats().dead, 0);
    assert!(
        matches!(db.delete(0), Err(Error::Full { .. })),
        "still wedged, correctly"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// A crash during `Db::compact` is the library's problem, not the
/// application's. Reconstruct every leftover the swap can produce and
/// check the next `open` resolves it.
///
/// The staging and retired directory names (`<db>.compacting`,
/// `<db>.retired`) are internal and undocumented, so this test has to know
/// them by construction — which is itself a finding: an operator staring
/// at those two directories after a crash has nothing to look up.
#[test]
fn an_interrupted_compaction_is_resolved_by_the_next_open() {
    let root = scratch("compact-crash");
    let dir = root.join("db");
    let staging = root.join("db.compacting");
    let retired = root.join("db.retired");

    let mut n = 0;
    let mut seed = |target: &PathBuf, ids: &[u64]| {
        n += 1;
        let tmp = root.join(format!("seed{n}"));
        let mut db = Db::open_with(&tmp, 4096).unwrap();
        for &i in ids {
            db.insert(i, Value::from_vec(payload_of(i)).unwrap())
                .unwrap();
        }
        drop(db);
        std::fs::rename(&tmp, target).unwrap();
    };
    let ids_of = |p: &PathBuf| {
        let mut db = Db::open_with(p, 4096).unwrap();
        db.all()
            .unwrap()
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
    };
    let clear = || {
        for p in [&dir, &staging, &retired] {
            let _ = std::fs::remove_dir_all(p);
        }
    };

    // Crashed while building the staging copy.
    clear();
    seed(&dir, &[1, 2, 3]);
    seed(&staging, &[1]);
    assert_eq!(ids_of(&dir), vec![1, 2, 3]);
    assert!(!staging.exists(), "the junk copy was left behind");

    // Crashed between the two renames: the live directory does not exist.
    clear();
    seed(&retired, &[1, 2, 3]);
    seed(&staging, &[1, 2, 3]);
    assert_eq!(
        ids_of(&dir),
        vec![1, 2, 3],
        "the database did not come back"
    );
    assert!(!staging.exists() && !retired.exists());

    // Crashed after the swap, before the retired copy was removed.
    clear();
    seed(&dir, &[1, 2, 3]);
    seed(&retired, &[1, 2, 3]);
    assert_eq!(ids_of(&dir), vec![1, 2, 3]);
    assert!(!retired.exists());

    // Pathological: only the retired copy survives.
    clear();
    seed(&retired, &[9]);
    assert_eq!(ids_of(&dir), vec![9], "the original was not put back");

    std::fs::remove_dir_all(&root).ok();
}

/// A long-running queue at constant depth: capacity is consumed by
/// throughput AND by payload size, so the application compacts many times
/// over the life of a workload that never holds more than a handful of
/// rows.
#[test]
fn a_long_running_queue_compacts_repeatedly_at_constant_depth() {
    let root = scratch("long");
    let journal_path = root.join("j.log");
    let mut cfg = Config::new(root.join("db"), &journal_path, 120);
    // Eight commits' worth of slots. It has to be at least twice the
    // largest single job's slot cost plus headroom, because a job at the
    // payload ceiling is 127 slots to insert and 127 more to claim — a
    // sizing rule the library states nowhere, since capacity is denominated
    // in slots and an application thinks in payloads.
    cfg.capacity = 8 * MAX_COMMIT_ROWS as u64;
    cfg.window = 3;
    cfg.compact_at = 0.6;

    let mut journal = Journal::open(&journal_path).unwrap();
    let rep = run(&cfg, &mut journal).unwrap();

    assert!(rep.drained, "{rep:?}");
    assert_eq!(rep.committed, 120);
    assert_eq!(rep.checksum, jobqueue::expected_checksum(120));
    assert!(
        rep.compactions >= 10,
        "120 jobs through a {}-slot database must compact often, got {}",
        cfg.capacity,
        rep.compactions
    );

    let a = audit(&journal_path).unwrap();
    assert!(a.duplicate_commits.is_empty());
    assert_eq!(a.committed.len(), 120);
    assert_eq!(a.committed, (1..=120).collect::<Vec<u64>>(), "FIFO order");
    assert!(
        a.widest_payload > VALUE_LEN,
        "the workload never wrote a value that spans slots: {a:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}
