//! The public API's contract, from a caller's point of view.
//!
//! Everything below is written the way an application would write it — no
//! engine types, no `Output` matching, no hand-rolled persistence. If a
//! test here needs a helper that feels like plumbing, that is a signal the
//! library is missing something, not that the test needs more code.

use dabqlite::{Db, Error, Snapshot, Value, VALUE_LEN};

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-api-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_whole_crud_surface_reads_the_way_you_would_expect() {
    let mut db = Db::in_memory().expect("open");
    assert!(db.is_empty());

    db.insert(1, Value::from_text("one").unwrap())
        .expect("insert");
    db.insert(2, Value::from_text("two").unwrap())
        .expect("insert");
    assert_eq!(db.len(), 2);
    assert!(db.contains(1).unwrap());

    // Insert is not an overwrite; put is.
    assert_eq!(
        db.insert(1, Value::from_text("uno").unwrap()),
        Err(Error::AlreadyExists { id: 1 })
    );
    db.put(1, Value::from_text("uno").unwrap()).expect("put");
    assert_eq!(db.get(1).unwrap().unwrap().text(), "uno");
    // put also inserts.
    db.put(3, Value::from_text("three").unwrap())
        .expect("put new");
    assert_eq!(db.len(), 3);

    // Update requires existence; delete/remove differ in forgiveness.
    assert_eq!(
        db.update(99, Value::from_text("nope").unwrap()),
        Err(Error::NotFound { id: 99 })
    );
    assert_eq!(db.delete(99), Err(Error::NotFound { id: 99 }));
    assert!(!db.remove(99).unwrap());
    assert!(db.remove(2).unwrap());
    assert_eq!(db.get(2).unwrap(), None);
    assert_eq!(db.len(), 2);

    // Ordered listing and substring search.
    let all = db.all().unwrap();
    assert_eq!(all.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(db.range(3, 100).unwrap().len(), 1);
    let hits = db.find_text("ree").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, 3);
}

#[test]
fn values_refuse_to_truncate_and_round_trip_text() {
    let v = Value::from_text("hello").unwrap();
    assert_eq!(v.text(), "hello");
    assert_eq!(v.as_bytes(), b"hello");
    assert_eq!(v.raw().len(), VALUE_LEN);

    let too_long = "x".repeat(VALUE_LEN + 1);
    assert_eq!(
        Value::from_text(&too_long),
        Err(Error::ValueTooLong {
            len: VALUE_LEN + 1,
            max: VALUE_LEN
        }),
        "a value that does not fit must be refused, never silently cut"
    );
    // Exactly full is fine, and round-trips.
    let exact = "y".repeat(VALUE_LEN);
    assert_eq!(Value::from_text(&exact).unwrap().text(), exact);
}

#[test]
fn a_snapshot_round_trips_through_plain_bytes() {
    let mut db = Db::in_memory().expect("open");
    for i in 0..50u64 {
        db.insert(i, Value::from_text(&format!("row-{i}")).unwrap())
            .expect("insert");
    }
    db.remove(7).unwrap();
    db.put(9, Value::from_text("changed").unwrap()).unwrap();

    // The application persists ONE blob wherever it likes.
    let blob = db.snapshot().expect("snapshot").to_bytes();
    let restored = Snapshot::from_bytes(&blob).expect("parse");
    let mut db2 = Db::load(&restored).expect("load");

    assert_eq!(db2.len(), db.len());
    assert_eq!(db2.all().unwrap(), db.all().unwrap());
    assert_eq!(db2.get(7).unwrap(), None, "a deletion did not survive");
    assert_eq!(db2.get(9).unwrap().unwrap().text(), "changed");
    // ...and it keeps working.
    db2.insert(1000, Value::from_text("after").unwrap())
        .unwrap();

    // Garbage is refused rather than guessed at.
    assert!(matches!(
        Snapshot::from_bytes(b"not a snapshot at all......."),
        Err(Error::Corrupt { .. })
    ));
    let mut truncated = blob.clone();
    truncated.truncate(blob.len() - 1);
    assert!(matches!(
        Snapshot::from_bytes(&truncated),
        Err(Error::Corrupt { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_file_backed_database_persists_and_shares_bytes_with_memory() {
    let dir = scratch("files");
    {
        let mut db = Db::open(&dir).expect("open");
        for i in 0..20u64 {
            db.insert(i, Value::from_text(&format!("v{i}")).unwrap())
                .expect("insert");
        }
        db.remove(3).unwrap();
        db.put(4, Value::from_text("updated").unwrap()).unwrap();
    }
    // Reopen: everything is there.
    let mut db = Db::open(&dir).expect("reopen");
    assert_eq!(db.len(), 19);
    assert_eq!(db.get(3).unwrap(), None);
    assert_eq!(db.get(4).unwrap().unwrap().text(), "updated");
    assert!(!db.recovery_report().rollback_evidence);

    // The same database moves into memory as a snapshot and back —
    // without the caller knowing anything about the file layout.
    let snapshot = db.snapshot().expect("snapshot a file-backed database");
    let mut in_ram = Db::load(&snapshot).expect("load into memory");
    assert_eq!(in_ram.all().unwrap(), db.all().unwrap());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stats_expose_the_dead_weight_that_deletes_and_updates_create() {
    let mut db = Db::in_memory_with(64).expect("open");
    for i in 0..20u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    assert_eq!(db.stats().live, 20);
    assert_eq!(db.stats().dead, 0);
    assert_eq!(db.stats().slots, 20);

    for i in 0..5u64 {
        db.remove(i).unwrap();
    }
    for i in 5..10u64 {
        db.put(i, Value::from_text("y").unwrap()).unwrap();
    }
    let s = db.stats();
    assert_eq!(s.live, 15);
    // 5 deletes = 5 retired + 5 tombstones; 5 updates = 5 retired.
    assert_eq!(s.dead, 15);
    assert_eq!(s.slots, 30);
    assert_eq!(s.capacity, 64);
    assert!(s.fill() > 0.4 && s.fill() < 0.5);

    // Compaction gives the space back, and loses nothing.
    let before = db.all().unwrap();
    let mut compacted = db.compact_to_memory().expect("compact");
    assert_eq!(compacted.all().unwrap(), before);
    assert_eq!(compacted.stats().dead, 0);
    assert_eq!(compacted.stats().slots, 15);
}

#[test]
fn a_full_database_says_so_instead_of_failing_obscurely() {
    let mut db = Db::in_memory_with(4).expect("open");
    for i in 0..4u64 {
        db.insert(i, Value::from_text("x").unwrap()).unwrap();
    }
    assert_eq!(
        db.insert(4, Value::from_text("x").unwrap()),
        Err(Error::Full { capacity: 4 })
    );
    // The message tells you what to do about it.
    let msg = Error::Full { capacity: 4 }.to_string();
    assert!(msg.contains("full") && msg.contains('4'), "{msg}");
    // Everything is still readable at the wall.
    assert_eq!(db.len(), 4);
    assert_eq!(db.all().unwrap().len(), 4);
}

#[cfg(unix)]
#[test]
fn a_damaged_database_still_gives_up_its_surviving_rows() {
    use dabqlite_core::ROW_SIZE;
    let dir = scratch("salvage");
    {
        let mut db = Db::open(&dir).expect("open");
        for i in 0..10u64 {
            db.insert(i, Value::from_text(&format!("keep{i}")).unwrap())
                .unwrap();
        }
    }
    // Rot one row.
    let rows_path = dir.join(dabqlite_host::rows_file_name(dabqlite_core::SCHEMA_HASH));
    let mut bytes = std::fs::read(&rows_path).unwrap();
    bytes[4 * ROW_SIZE + 2] ^= 0x40;
    std::fs::write(&rows_path, bytes).unwrap();

    // A normal open refuses, and says what to try.
    match Db::open(&dir) {
        Err(Error::Corrupt { what }) => assert!(what.contains("salvage"), "{what}"),
        other => panic!("a damaged database must not open normally: {other:?}"),
    }

    // Salvage reads the rest.
    let mut db = Db::salvage(&dir).expect("salvage");
    assert!(db.is_degraded());
    assert_eq!(db.len(), 9);
    for i in 0..10u64 {
        if i == 4 {
            // The damaged row is unanswerable, and says so rather than
            // claiming the row never existed.
            assert!(matches!(db.get(i), Err(Error::Degraded { .. })));
        } else {
            assert_eq!(db.get(i).unwrap().unwrap().text(), format!("keep{i}"));
        }
    }
    // Writes are refused on a degraded database.
    assert!(matches!(
        db.insert(999, Value::from_text("no").unwrap()),
        Err(Error::Degraded { .. })
    ));
    // And the survivors can be lifted into a clean database in one call.
    let mut rescued = db.compact_to_memory().expect("compact out of degraded");
    assert_eq!(rescued.len(), 9);
    assert!(!rescued.is_degraded());
    rescued
        .insert(999, Value::from_text("yes").unwrap())
        .unwrap();

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn errors_are_all_displayable_and_say_something_useful() {
    let cases: Vec<Error> = vec![
        Error::NotFound { id: 5 },
        Error::AlreadyExists { id: 5 },
        Error::Full { capacity: 10 },
        Error::ValueTooLong { len: 20, max: 16 },
        Error::Degraded { quarantined: 2 },
        Error::Corrupt { what: "test" },
        Error::SchemaMismatch {
            file_schema: 1,
            binary: 2,
        },
        Error::Io {
            detail: "disk".into(),
        },
    ];
    for e in cases {
        let msg = e.to_string();
        assert!(msg.len() > 8, "unhelpful message for {e:?}: {msg:?}");
        assert!(!msg.contains("Err("), "leaked a debug format: {msg}");
    }
}
