//! What happens as the database fills up, and what an application has to do
//! about it. These tests are as much documentation of the behaviour as
//! they are checks on it.

use std::path::PathBuf;

use dabqlite::{Db, Error, Value};
use jobqueue::{audit, run, Config, JobRow, Journal, Layout, DONE, PENDING};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-cap-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The slot arithmetic an application has to internalise: nothing is ever
/// reclaimed in place, so a steady-state queue still consumes capacity
/// linearly in the number of operations, not the number of live rows.
#[test]
fn every_operation_consumes_a_slot_forever() {
    let mut db = Db::in_memory_with(1000).unwrap();
    let v = JobRow {
        state: PENDING,
        attempts: 0,
        payload: 7,
    }
    .encode();

    db.insert(1, v).unwrap();
    assert_eq!((db.stats().slots, db.stats().dead), (1, 0));
    db.update(
        1,
        JobRow {
            state: DONE,
            attempts: 1,
            payload: 7,
        }
        .encode(),
    )
    .unwrap();
    assert_eq!(
        (db.stats().slots, db.stats().dead),
        (2, 1),
        "update = +1 slot, +1 dead"
    );
    db.delete(1).unwrap();
    assert_eq!(
        (db.stats().slots, db.stats().dead, db.stats().live),
        (3, 3, 0),
        "delete = +1 slot (the tombstone), and retires the row it hides"
    );

    // One full job lifecycle in this queue is 6 slots: insert, enqueue
    // watermark, claim, done, commit watermark, reap.
    let mut db = Db::in_memory_with(1000).unwrap();
    let before = db.stats().slots;
    db.insert(10, v).unwrap();
    db.put(u64::MAX, v).unwrap();
    db.update(10, v).unwrap();
    db.update(10, v).unwrap();
    db.put(u64::MAX - 1, v).unwrap();
    db.delete(10).unwrap();
    assert_eq!(db.stats().slots - before, 6);
    assert_eq!(db.stats().live, 2, "only the two meta rows survive");
}

/// The sharpest edge in the whole API: **a full database cannot be
/// emptied**. `delete` needs a free slot for its tombstone, so once
/// `Full` is reached every single mutation — including the ones that would
/// free space — is refused. There is no in-place escape.
#[test]
fn a_full_database_cannot_be_emptied() {
    let mut db = Db::in_memory_with(4).unwrap();
    for i in 0..4u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    assert_eq!(
        db.insert(4, Value::from_text("x").unwrap()),
        Err(Error::Full { capacity: 4 })
    );
    assert_eq!(
        db.update(0, Value::from_text("y").unwrap()),
        Err(Error::Full { capacity: 4 })
    );
    assert_eq!(
        db.delete(0),
        Err(Error::Full { capacity: 4 }),
        "DELETE is refused on a full database: the only operation that could \
         make room needs room to run"
    );

    // And it is not about being at 100% live: three live rows in a
    // five-slot database is equally stuck.
    let mut db = Db::in_memory_with(5).unwrap();
    for i in 0..4u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    db.delete(0).unwrap();
    assert_eq!(db.stats().live, 3);
    assert_eq!(
        db.delete(1),
        Err(Error::Full { capacity: 5 }),
        "3 live rows out of a declared 5 and the database is already wedged"
    );
}

/// The documented escape from `Full` for a file-backed database is to
/// reopen with a bigger capacity. It works — and it is the only escape
/// that does not require the application to have built a rebuild
/// protocol of its own.
#[test]
fn reopening_with_more_capacity_escapes_full_and_too_small_is_its_own_error() {
    let dir = scratch("grow").join("db");
    {
        let mut db = Db::open_with(&dir, 4).unwrap();
        for i in 0..4u64 {
            db.insert(i, Value::from_text("x").unwrap()).unwrap();
        }
        assert!(matches!(db.delete(0), Err(Error::Full { .. })));
    }
    let mut db = Db::open_with(&dir, 64).unwrap();
    db.delete(0).unwrap();
    assert_eq!(db.len(), 3);

    // But shrinking back is refused, so capacity is a ratchet: you cannot
    // reclaim the memory afterwards without a rebuild.
    drop(db);
    assert!(matches!(
        Db::open_with(&dir, 4),
        Err(Error::CapacityTooSmall { .. })
    ));
    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
}

/// `compact_to_memory` is the only compaction the library offers, and it
/// hands back an *in-memory* database. There is no way to put it back on
/// disk, so a file-backed application has to build a directory-swap
/// protocol. This is that protocol, and it works.
#[test]
fn compacting_a_file_backed_database_is_application_code() {
    let root = scratch("compact");
    let layout = Layout::new(&root);
    layout.recover().unwrap();

    {
        let mut db = Db::open_with(layout.live(), 512).unwrap();
        for i in 0..50u64 {
            db.insert(i, Value::from_text(&format!("v{i}")).unwrap())
                .unwrap();
        }
        for i in 0..50u64 {
            db.put(i, Value::from_text("touched").unwrap()).unwrap();
        }
        for i in 0..25u64 {
            db.remove(i).unwrap();
        }
        let s = db.stats();
        assert_eq!((s.live, s.slots), (25, 125));

        // What the library actually gives you: a copy in RAM, and no way
        // to write it back down.
        let mem = db.compact_to_memory().unwrap();
        assert_eq!(mem.stats().slots, 25);
        assert_eq!(mem.stats().dead, 0);
        // `mem` is a `Db<MemoryStorage>` -- a type this crate cannot even
        // name -- and there is no `Db::restore(path, snapshot)`, so it is
        // a dead end for a file-backed application.
    }

    let report = layout.compact(512).unwrap();
    assert_eq!(report.slots_before, 125);
    assert_eq!(report.slots_after, 25);

    let mut db = Db::open_with(layout.live(), 512).unwrap();
    assert_eq!(db.len(), 25);
    assert_eq!(db.stats().dead, 0);
    assert_eq!(db.get(30).unwrap().unwrap().text(), "touched");
    assert_eq!(db.get(3).unwrap(), None);
    std::fs::remove_dir_all(&root).ok();
}

/// Every crash point of the directory-swap protocol resolves to a
/// consistent database. This is the part an application must get right
/// and the library gives no help with.
#[test]
fn the_swap_protocol_recovers_from_every_crash_point() {
    let root = scratch("swap");
    let layout = Layout::new(&root);

    let seed = |dir: &str, ids: &[u64]| {
        let mut db = Db::open_with(root.join(dir), 64).unwrap();
        for &i in ids {
            db.insert(i, Value::from_text(&format!("v{i}")).unwrap())
                .unwrap();
        }
    };
    let ids_of = |dir: &str| {
        let mut db = Db::open_with(root.join(dir), 64).unwrap();
        db.all()
            .unwrap()
            .iter()
            .map(|&(k, _)| k)
            .collect::<Vec<_>>()
    };
    let clear = || {
        for d in ["live", "next", "old"] {
            let _ = std::fs::remove_dir_all(root.join(d));
        }
    };

    // Crash during step 2 (building `next`): `next` is partial junk.
    clear();
    seed("live", &[1, 2, 3]);
    seed("next", &[1]);
    layout.recover().unwrap();
    assert!(!root.join("next").exists());
    assert_eq!(ids_of("live"), vec![1, 2, 3]);

    // Crash between step 3 and 4: `live` is gone, `next` is complete.
    clear();
    seed("old", &[1, 2, 3]);
    seed("next", &[1, 2, 3]);
    layout.recover().unwrap();
    assert_eq!(ids_of("live"), vec![1, 2, 3]);
    assert!(!root.join("old").exists() && !root.join("next").exists());

    // Crash between step 4 and 5: `live` is new, `old` is a stale copy.
    clear();
    seed("live", &[1, 2, 3]);
    seed("old", &[1, 2, 3]);
    layout.recover().unwrap();
    assert_eq!(ids_of("live"), vec![1, 2, 3]);
    assert!(!root.join("old").exists());

    // Pathological: only `old` survives.
    clear();
    seed("old", &[9]);
    layout.recover().unwrap();
    assert_eq!(ids_of("live"), vec![9]);

    std::fs::remove_dir_all(&root).ok();
}

/// A long-running queue at constant depth: capacity is consumed by
/// throughput, not by data, so the application compacts many times over
/// the life of a workload that never holds more than a handful of rows.
#[test]
fn a_long_running_queue_compacts_repeatedly_at_constant_depth() {
    let root = scratch("long");
    let journal_path = root.join("j.log");
    let mut cfg = Config::new(root.join("db"), &journal_path, 300);
    cfg.capacity = 128;
    cfg.window = 4;
    cfg.compact_at = 0.75;

    let mut journal = Journal::open(&journal_path).unwrap();
    let rep = run(&cfg, &mut journal).unwrap();

    assert!(rep.drained, "{rep:?}");
    assert_eq!(rep.committed, 300);
    assert_eq!(rep.checksum, jobqueue::expected_checksum(300));
    assert!(
        rep.compactions >= 15,
        "300 jobs at 6 slots each in a 128-slot database must compact often, got {}",
        rep.compactions
    );

    let a = audit(&journal_path).unwrap();
    assert!(a.duplicate_commits.is_empty());
    assert_eq!(a.committed.len(), 300);
    assert_eq!(a.committed, (1..=300).collect::<Vec<u64>>(), "FIFO order");
    std::fs::remove_dir_all(&root).ok();
}
