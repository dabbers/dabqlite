//! The public API's contract, from a caller's point of view.
//!
//! Everything below is written the way an application would write it — no
//! engine types, no `Output` matching, no hand-rolled persistence. If a
//! test here needs a helper that feels like plumbing, that is a signal the
//! library is missing something, not that the test needs more code.

use dabqlite::{Db, Error, Match, Op, Snapshot, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN, VALUE_LEN};

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
    assert_eq!(v.len(), 5, "a value is as long as what you put in it");

    let too_long = "x".repeat(MAX_VALUE_LEN + 1);
    assert_eq!(
        Value::from_text(&too_long),
        Err(Error::ValueTooLong {
            len: MAX_VALUE_LEN + 1,
            max: MAX_VALUE_LEN
        }),
        "a value that does not fit must be refused, never silently cut"
    );
    // Exactly at the ceiling is fine, and round-trips.
    let exact = "y".repeat(MAX_VALUE_LEN);
    assert_eq!(Value::from_text(&exact).unwrap().text(), exact);
    // So does the empty value, which is not the same as an absent row.
    assert_eq!(Value::empty().as_bytes(), b"");
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
    let in_ram = Db::load(&snapshot).expect("load into memory");
    assert_eq!(in_ram.all().unwrap(), db.all().unwrap());

    std::fs::remove_dir_all(&dir).ok();
}

/// Dead weight is counted in SLOTS, the same unit as capacity — which
/// only matters once a value is longer than one slot, and matters a great
/// deal then: a store watching `dead` to decide when to rebuild would
/// under-read an eight-slot value's retirement by a factor of eight.
/// A needle may be as long as a value. It used to be capped at ONE ROW —
/// sixteen bytes — which meant `find_text("developer.mozilla.org")` was
/// refused by a store that could hold the whole URL comfortably. Nothing
/// about search required that; it was the shape of the buffer the needle
/// travelled in.
#[test]
fn a_needle_may_be_as_long_as_a_value() {
    let mut db = Db::in_memory_with(64).expect("open");
    let url = "https://developer.mozilla.org/en-US/docs/Web/API/FileSystemSyncAccessHandle";
    db.insert(1, Value::from_text(url).unwrap()).unwrap();
    db.insert(2, Value::from_text("https://example.com/other").unwrap())
        .unwrap();

    // Needles far longer than a row, matching across slot boundaries.
    for needle in [
        "developer.mozilla.org",
        "/en-US/docs/Web/API/FileSystemSyncAccessHandle",
        url,
    ] {
        let hits = db.find_text(needle).expect("find");
        assert_eq!(hits.len(), 1, "{needle}");
        assert_eq!(hits[0].0, 1);
    }
    assert_eq!(db.find_text("https://").unwrap().len(), 2);

    // The only ceiling left is the one no value can exceed either, and it
    // says so in its own words rather than borrowing the value's.
    let huge = vec![b'z'; MAX_VALUE_LEN + 1];
    match db.find(&huge) {
        Err(Error::NeedleTooLong { len, max }) => {
            assert_eq!((len, max), (MAX_VALUE_LEN + 1, MAX_VALUE_LEN));
        }
        other => panic!("{other:?}"),
    }
    assert!(db
        .find(&huge)
        .unwrap_err()
        .to_string()
        .contains("nothing could contain it"));
}

/// Space is quoted in ROW SLOTS everywhere, and the API says how many a
/// write will take before it is attempted. Without that, "will this fit"
/// is unanswerable: a database at 40% fill legitimately refuses a value
/// that needs more room than the other 60% holds.
#[test]
fn slot_arithmetic_is_answerable_before_the_write() {
    assert_eq!(Op::insert(1, Value::from_text("hi").unwrap()).rows(), 1);
    assert_eq!(
        Op::insert(1, Value::empty()).rows(),
        1,
        "empty still needs a row"
    );
    assert_eq!(
        Op::put(1, Value::from_bytes(&[0u8; VALUE_LEN]).unwrap()).rows(),
        1
    );
    assert_eq!(
        Op::update(1, Value::from_bytes(&[0u8; VALUE_LEN + 1]).unwrap()).rows(),
        2
    );
    assert_eq!(
        Op::insert(1, Value::from_bytes(&[0u8; MAX_VALUE_LEN]).unwrap()).rows(),
        MAX_COMMIT_ROWS,
        "a maximum-length value fills a commit by itself"
    );
    assert_eq!(Op::delete(1).rows(), 1, "a tombstone is a slot");
    assert_eq!(Op::remove(1).rows(), 1);

    let mut db = Db::in_memory_with(10).expect("open");
    assert_eq!(db.stats().free(), 10);
    let five = Op::insert(1, Value::from_bytes(&[b'a'; 5 * VALUE_LEN]).unwrap());
    assert_eq!(five.rows(), 5);
    db.batch(std::slice::from_ref(&five)).unwrap();
    assert_eq!(db.stats().free(), 5);

    // `free` answers what `fill` cannot: this database is at 50% and
    // still cannot take a six-slot value.
    let six = Op::insert(2, Value::from_bytes(&[b'b'; 6 * VALUE_LEN]).unwrap());
    assert!(db.stats().fill() < 0.6);
    assert!(six.rows() > db.stats().free() as usize);
    match db.batch(std::slice::from_ref(&six)) {
        Err(Error::BatchRejected { at: 0, cause }) => {
            assert!(matches!(*cause, Error::Full { .. }), "{cause:?}");
        }
        other => panic!("{other:?}"),
    }
    // And the one that does fit, does.
    let five_more = Op::insert(2, Value::from_bytes(&[b'b'; 5 * VALUE_LEN]).unwrap());
    assert_eq!(five_more.rows(), db.stats().free() as usize);
    db.batch(&[five_more]).unwrap();
    assert_eq!(db.stats().free(), 0);

    // Rebuilding gives back the dead weight, and `reclaimable` says how
    // much that is before you spend the time.
    db.remove(1).ok();
    let s = db.stats();
    assert_eq!(s.reclaimable(), s.free() + s.dead);
}

/// A batch is bounded in SLOTS, not operations — the constant is named
/// for what it counts, and two operations can be too long for one.
#[test]
fn a_batch_is_bounded_in_slots_not_operations() {
    let mut db = Db::in_memory_with(1024).expect("open");
    let half = Value::from_bytes(&[b'x'; (MAX_COMMIT_ROWS / 2) * VALUE_LEN]).unwrap();
    let ops = [
        Op::insert(1, half.clone()),
        Op::insert(2, half.clone()),
        Op::insert(3, Value::from_text("one more slot").unwrap()),
    ];
    assert_eq!(ops.iter().map(Op::rows).sum::<usize>(), MAX_COMMIT_ROWS + 1);
    // Refused, and it names the operation the commit overflowed at —
    // which an op-count limit could not have told anyone, because the
    // count was never the thing that overflowed.
    match db.batch(&ops) {
        Err(Error::BatchRejected { at: 2, cause }) => match *cause {
            Error::BatchTooLong { rows, max } => {
                assert_eq!(max, MAX_COMMIT_ROWS);
                assert_eq!(rows, MAX_COMMIT_ROWS + 1);
            }
            other => panic!("{other:?}"),
        },
        other => panic!("three operations, {} slots: {other:?}", MAX_COMMIT_ROWS + 1),
    }
    // Drop the last one and the same two operations fit exactly.
    db.batch(&ops[..2]).unwrap();
    assert_eq!(db.stats().slots, MAX_COMMIT_ROWS as u64);
}

#[test]
fn dead_weight_counts_the_slots_a_long_value_held_not_the_value() {
    let mut db = Db::in_memory_with(64).expect("open");
    // Eight slots: 128 bytes at 16 bytes a slot.
    let long = Value::from_bytes(&[b'x'; 128]).unwrap();
    let short = Value::from_text("small").unwrap();
    db.insert(1, long.clone()).unwrap();
    assert_eq!(db.stats().slots, 8);
    assert_eq!(db.stats().dead, 0);

    // Superseding it retires all eight, not one.
    db.put(1, short.clone()).unwrap();
    let s = db.stats();
    assert_eq!(s.slots, 9);
    assert_eq!(s.live, 1);
    assert_eq!(s.dead, 8, "the retired value held eight slots");

    // And a rebuild returns exactly that many.
    let compacted = db.compact_to_memory().expect("compact");
    assert_eq!(compacted.stats().slots, 1);
    assert_eq!(compacted.stats().dead, 0);
    assert_eq!(compacted.get(1).unwrap(), Some(short));

    // Deleting a long value is the same story plus the tombstone.
    let mut db = Db::in_memory_with(64).expect("open");
    db.insert(1, long).unwrap();
    db.remove(1).unwrap();
    let s = db.stats();
    assert_eq!(s.live, 0);
    assert_eq!(s.slots, 9);
    assert_eq!(s.dead, 9, "eight retired slots plus the tombstone");
    // Dead weight never exceeds what is there to reclaim.
    assert!(s.dead <= s.slots);
}

/// The same in a batch, and across a reopen: recovery rebuilds the
/// accounting from the rows file alone, so it has to reach the same
/// number the write path did.
#[test]
fn dead_weight_survives_a_reopen_and_a_batch() {
    let long = Value::from_bytes(&[b'q'; 100]).unwrap(); // 7 slots
    let mut db = Db::in_memory_with(64).expect("open");
    db.batch(&[
        Op::insert(1, long.clone()),
        Op::insert(2, long.clone()),
        Op::insert(3, Value::from_text("short").unwrap()),
    ])
    .unwrap();
    assert_eq!(db.stats().slots, 15);
    db.batch(&[
        Op::put(1, Value::from_text("now short").unwrap()),
        Op::delete(2),
    ])
    .unwrap();
    let before = db.stats();
    assert_eq!(before.live, 2);
    assert_eq!(before.slots, 17);
    // 7 retired for id 1, 7 + 1 tombstone for id 2.
    assert_eq!(before.dead, 15);

    let snapshot = db.snapshot().expect("snapshot");
    let reopened = Db::load(&snapshot).expect("load");
    assert_eq!(
        reopened.stats(),
        before,
        "recovery reached a different accounting than the write path"
    );
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
    let compacted = db.compact_to_memory().expect("compact");
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
        Err(Error::Full {
            capacity: 4,
            dead: 0
        })
    );
    // With every slot live, the only way on is a bigger database, and the
    // message says exactly that rather than suggesting a rebuild that
    // would reclaim nothing.
    let msg = db
        .insert(4, Value::from_text("x").unwrap())
        .unwrap_err()
        .to_string();
    assert!(msg.contains("larger capacity"), "{msg}");
    assert!(!msg.contains("compact"), "nothing to compact here: {msg}");
    // Everything is still readable at the wall.
    assert_eq!(db.len(), 4);
    assert_eq!(db.all().unwrap().len(), 4);

    // Now make some of it dead weight, and the message changes to the
    // thing that would actually help. A compaction needs no free slot, so
    // it is available exactly when the database is full.
    let mut db = Db::in_memory_with(4).expect("open");
    db.insert(1, Value::from_text("a").unwrap()).unwrap();
    db.put(1, Value::from_text("b").unwrap()).unwrap();
    db.put(1, Value::from_text("c").unwrap()).unwrap();
    db.put(1, Value::from_text("d").unwrap()).unwrap();
    let err = db.insert(2, Value::from_text("x").unwrap()).unwrap_err();
    match err {
        Error::Full { capacity: 4, dead } => {
            assert_eq!(dead, 3, "three superseded records are dead weight");
            let msg = err.to_string();
            assert!(msg.contains("compact"), "{msg}");
            assert!(msg.contains("no free slot"), "{msg}");
        }
        other => panic!("expected Full, got {other:?}"),
    }
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
        Error::Full {
            capacity: 10,
            dead: 0,
        },
        Error::Full {
            capacity: 10,
            dead: 4,
        },
        Error::ValueTooLong { len: 20, max: 16 },
        Error::Degraded { quarantined: 2 },
        Error::Corrupt { what: "test" },
        Error::SchemaMismatch {
            file_schema: 1,
            binary: 2,
        },
        Error::Io {
            kind: std::io::ErrorKind::StorageFull,
            detail: "no space left on device".into(),
        },
        Error::CapacityTooSmall {
            required: 20,
            asked: 5,
        },
        Error::Locked {
            detail: "held".into(),
        },
        Error::BatchRejected {
            at: 3,
            cause: Box::new(Error::NotFound { id: 7 }),
        },
        Error::BatchTooLong {
            rows: 200,
            max: MAX_COMMIT_ROWS,
        },
        Error::NeedleTooLong {
            len: 4096,
            max: MAX_VALUE_LEN,
        },
        Error::Mismatch { id: 12 },
    ];
    // Every variant must appear above. This match exists to break the
    // build when a new one is added: a variant with no case here is a
    // variant whose message nobody ever read.
    const ALL_VARIANTS: &[&str] = &[
        "NotFound",
        "AlreadyExists",
        "Full",
        "CapacityTooSmall",
        "Locked",
        "ValueTooLong",
        "NeedleTooLong",
        "Mismatch",
        "Degraded",
        "Corrupt",
        "SchemaMismatch",
        "Io",
        "BatchTooLong",
        "BatchRejected",
    ];
    fn covered(e: &Error) -> &'static str {
        match e {
            Error::NotFound { .. } => "NotFound",
            Error::AlreadyExists { .. } => "AlreadyExists",
            Error::Full { .. } => "Full",
            Error::CapacityTooSmall { .. } => "CapacityTooSmall",
            Error::Locked { .. } => "Locked",
            Error::ValueTooLong { .. } => "ValueTooLong",
            Error::NeedleTooLong { .. } => "NeedleTooLong",
            Error::Mismatch { .. } => "Mismatch",
            Error::Degraded { .. } => "Degraded",
            Error::Corrupt { .. } => "Corrupt",
            Error::SchemaMismatch { .. } => "SchemaMismatch",
            Error::Io { .. } => "Io",
            Error::BatchTooLong { .. } => "BatchTooLong",
            Error::BatchRejected { .. } => "BatchRejected",
        }
    }
    // Every variant must appear at least once. `covered` breaks the build
    // when a variant is added; this catches the case where it was added
    // there but no example was added here.
    let mut all: Vec<&'static str> = ALL_VARIANTS.to_vec();
    all.sort_unstable();
    let mut seen: Vec<&'static str> = cases.iter().map(covered).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen, all, "some error variant has no example in this test");

    for e in cases {
        let msg = e.to_string();
        assert!(msg.len() > 8, "unhelpful message for {e:?}: {msg:?}");
        assert!(!msg.contains("Err("), "leaked a debug format: {msg}");
    }
}

#[cfg(unix)]
#[test]
fn compaction_reclaims_dead_slots_in_place_and_survives_interruption() {
    use dabqlite::FileDb;
    let dir = scratch("compact");

    let survivors: Vec<u64> = (0..60u64).filter(|i| i % 4 == 0).collect();
    let mut db: FileDb = Db::open_with(&dir, 512).expect("open");
    for i in 0..60u64 {
        db.insert(i, Value::from_text(&format!("v{i}")).unwrap())
            .unwrap();
    }
    for i in 0..60u64 {
        if !survivors.contains(&i) {
            assert!(db.remove(i).unwrap());
        }
    }
    let before = db.stats();
    assert!(before.dead > 0, "the workload should have left dead weight");

    // In place: same path, same data, no dead weight.
    db.compact().expect("compact");
    let after = db.stats();
    assert_eq!(after.live, before.live);
    assert_eq!(after.dead, 0, "compaction left dead slots behind");
    assert!(
        after.slots < before.slots,
        "compaction did not shrink the database"
    );
    for i in 0..60u64 {
        let got = db.get(i).unwrap();
        if survivors.contains(&i) {
            assert_eq!(got.unwrap().text(), format!("v{i}"));
        } else {
            assert_eq!(got, None, "row {i} came back from the dead");
        }
    }
    // Still the same database on disk, and still writable.
    db.insert(1000, Value::from_text("after").unwrap()).unwrap();
    drop(db);
    let reopened: FileDb = Db::open_with(&dir, 512).expect("reopen");
    assert_eq!(reopened.len(), survivors.len() as u64 + 1);

    // An interrupted swap resolves itself. Simulate a crash landing
    // between the two renames: the live directory is gone and the old one
    // is sitting beside it.
    let retired = dir.with_file_name(format!(
        "{}.retired",
        dir.file_name().unwrap().to_string_lossy()
    ));
    drop(reopened);
    std::fs::rename(&dir, &retired).expect("simulate crash mid-swap");
    assert!(!dir.exists());
    let recovered: FileDb = Db::open_with(&dir, 512).expect("open must resolve the swap");
    assert_eq!(
        recovered.len(),
        survivors.len() as u64 + 1,
        "data lost mid-swap"
    );
    assert!(
        !retired.exists(),
        "the retired copy should have been reclaimed"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn a_database_can_be_held_in_a_struct_now() {
    use dabqlite::{FileDb, MemDb};
    // The point of this test is that it COMPILES: `Db<S>` has to be
    // nameable for anyone to build anything on it.
    struct App {
        memory: MemDb,
        disk: Option<FileDb>,
    }
    fn count<S: dabqlite::Storage>(db: &mut Db<S>) -> u64 {
        db.len()
    }

    let dir = scratch("nameable");
    let mut app = App {
        memory: Db::in_memory().expect("memory"),
        disk: Some(Db::open(&dir).expect("disk")),
    };
    app.memory
        .insert(1, Value::from_text("a").unwrap())
        .unwrap();
    app.disk
        .as_mut()
        .unwrap()
        .insert(2, Value::from_text("b").unwrap())
        .unwrap();
    assert_eq!(count(&mut app.memory), 1);
    assert_eq!(count(app.disk.as_mut().unwrap()), 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// A value is exactly what you put in it. Not "what you put in, minus
/// trailing zeros" — the whole point of storing a length is that no byte
/// pattern is special.
///
/// The two earlier versions of this both lost data: the first truncated at
/// the first zero ANYWHERE, silently cutting the tail off any binary
/// payload; the second kept interior zeros but still trimmed trailing
/// ones, so a value that ended in a zero came back short.
#[test]
fn a_value_is_exactly_the_bytes_you_gave_it() {
    for case in [
        &b"ab\0cd"[..],
        b"",
        b"\0",
        b"trailing\0\0\0",
        &[0u8; VALUE_LEN][..],
        &[7u8; VALUE_LEN][..],
        // Longer than one row, and ending in zeros, so both the length
        // byte and the multi-slot path have to be exact.
        &[0u8; VALUE_LEN * 3][..],
    ] {
        let v = Value::from_bytes(case).unwrap();
        assert_eq!(v.as_bytes(), case, "in-memory value changed");
        assert_eq!(v.len(), case.len());
    }
}

/// And the same through an actual round trip to storage and back, which
/// is where a length that lives in the row rather than in the caller's
/// head earns its keep.
#[test]
fn a_value_survives_storage_byte_for_byte_at_every_length() {
    let mut db = Db::in_memory().expect("open");
    let cases: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        b"ab\0cd".to_vec(),
        b"trailing\0\0\0".to_vec(),
        vec![0u8; VALUE_LEN],
        vec![7u8; VALUE_LEN],
        vec![9u8; VALUE_LEN + 1],
        vec![0u8; VALUE_LEN * 3],
        (0..255u8).cycle().take(1000).collect(),
        vec![0xFF; MAX_VALUE_LEN],
    ];
    for (i, case) in cases.iter().enumerate() {
        let id = i as u64;
        db.put(id, Value::from_bytes(case).unwrap()).expect("put");
        assert_eq!(
            db.get(id).unwrap().unwrap().as_bytes(),
            &case[..],
            "value {i} ({} bytes) changed in storage",
            case.len()
        );
    }
    // And again after a reload from the raw bytes, so the file — not the
    // live engine — is what is being trusted.
    let snapshot = db.snapshot().unwrap();
    let reloaded = Db::load(&snapshot).expect("reload");
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(
            reloaded.get(i as u64).unwrap().unwrap().as_bytes(),
            &case[..],
            "value {i} changed across a reload"
        );
    }
}

#[cfg(unix)]
#[test]
fn opening_too_small_says_so_instead_of_claiming_the_database_is_full() {
    let dir = scratch("too-small");
    {
        let mut db = Db::open_with(&dir, 64).expect("open");
        for i in 0..20u64 {
            db.insert(i, Value::from_text("x").unwrap()).unwrap();
        }
    }
    match Db::open_with(&dir, 5) {
        Err(Error::CapacityTooSmall { required, asked }) => {
            assert_eq!(asked, 5);
            assert!(required >= 20, "required {required}");
            let msg = Error::CapacityTooSmall { required, asked }.to_string();
            assert!(msg.contains("reopen with at least"), "{msg}");
            assert!(!msg.contains("full"), "this is the opposite of full: {msg}");
        }
        other => panic!("expected CapacityTooSmall, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn lock_contention_is_its_own_error_not_an_io_failure() {
    let dir = scratch("locked");
    let _held = Db::open(&dir).expect("first writer");
    match Db::open(&dir) {
        Err(Error::Locked { detail }) => {
            assert!(detail.contains("single-writer"), "{detail}");
        }
        other => panic!("expected Locked, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------
// Batches, from the caller's side
// ---------------------------------------------------------------------

#[test]
fn a_batch_reads_like_a_list_of_writes_and_lands_as_one() {
    let mut db = Db::in_memory().expect("open");
    db.batch(&[
        Op::put(1, Value::from_text("one").unwrap()),
        Op::put(2, Value::from_text("two").unwrap()),
        Op::put(3, Value::from_text("three").unwrap()),
    ])
    .expect("batch");
    assert_eq!(db.len(), 3);

    // A second batch that reads and rewrites what the first wrote.
    db.batch(&[
        Op::update(1, Value::from_text("uno").unwrap()),
        Op::delete(2),
        Op::remove(999),
        Op::insert(4, Value::from_text("four").unwrap()),
    ])
    .expect("batch");

    assert_eq!(db.get(1).unwrap().unwrap().text(), "uno");
    assert_eq!(db.get(2).unwrap(), None);
    assert_eq!(db.get(3).unwrap().unwrap().text(), "three");
    assert_eq!(db.get(4).unwrap().unwrap().text(), "four");
}

/// The property an application actually leans on: a refused batch is not a
/// mess to clean up. It names the operation that stopped it, and the
/// database is byte-for-byte what it was.
#[test]
fn a_refused_batch_changes_nothing_and_says_which_operation_refused_it() {
    let mut db = Db::in_memory().expect("open");
    db.insert(1, Value::from_text("one").unwrap()).unwrap();
    let before = db.snapshot().unwrap().to_bytes();

    let err = db
        .batch(&[
            Op::put(2, Value::from_text("two").unwrap()),
            Op::put(3, Value::from_text("three").unwrap()),
            Op::update(77, Value::from_text("nope").unwrap()),
            Op::put(4, Value::from_text("four").unwrap()),
        ])
        .expect_err("a batch with an impossible op must be refused");

    match &err {
        Error::BatchRejected { at, cause } => {
            assert_eq!(*at, 2, "the refusal must name the operation that failed");
            assert_eq!(**cause, Error::NotFound { id: 77 });
        }
        other => panic!("expected BatchRejected, got {other:?}"),
    }
    // The message is readable on its own, and `source()` reaches the cause
    // without the caller having to destructure.
    let text = err.to_string();
    assert!(text.contains("operation 2"), "{text}");
    assert!(text.contains("nothing in it was applied"), "{text}");
    assert_eq!(
        std::error::Error::source(&err).map(|e| e.to_string()),
        Some("no row with id 77".to_string())
    );

    // Nothing was written — not even the two operations that came first.
    assert_eq!(db.get(2).unwrap(), None);
    assert_eq!(db.get(3).unwrap(), None);
    assert_eq!(db.len(), 1);
    assert_eq!(
        db.snapshot().unwrap().to_bytes(),
        before,
        "a refused batch changed the database on disk"
    );
}

#[test]
fn a_batch_sees_its_own_earlier_operations() {
    let mut db = Db::in_memory().expect("open");
    db.batch(&[
        Op::insert(5, Value::from_text("first").unwrap()),
        Op::delete(5),
        Op::insert(5, Value::from_text("second").unwrap()),
        Op::put(5, Value::from_text("third").unwrap()),
    ])
    .expect("batch");
    assert_eq!(db.get(5).unwrap().unwrap().text(), "third");
    assert_eq!(db.len(), 1);
}

#[test]
fn an_over_long_batch_is_refused_rather_than_silently_split() {
    let mut db = Db::in_memory().expect("open");
    let ops: Vec<Op> = (0..MAX_COMMIT_ROWS as u64 + 1)
        .map(|i| Op::put(i, Value::from_text("x").unwrap()))
        .collect();
    // The refusal must not claim the DATABASE is full: it is empty, and
    // its capacity is not MAX_COMMIT_ROWS. Three separate sample applications
    // reported the old message as stating the reverse of the truth.
    match db.batch(&ops) {
        Err(Error::BatchRejected { cause, .. }) => match *cause {
            Error::BatchTooLong { rows, max } => {
                assert_eq!(max, MAX_COMMIT_ROWS);
                assert_eq!(rows, MAX_COMMIT_ROWS + 1);
                let msg = cause.to_string();
                assert!(!msg.contains("full"), "this database is not full: {msg}");
                assert!(msg.contains("atomic"), "{msg}");
            }
            other => panic!("expected BatchTooLong, got {other:?}"),
        },
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(db.is_empty(), "an over-long batch wrote rows anyway");
    assert!(
        db.stats().capacity > MAX_COMMIT_ROWS as u64,
        "the test needs a database bigger than one commit to be meaningful"
    );

    // Exactly at the limit is fine.
    let ops: Vec<Op> = (0..MAX_COMMIT_ROWS as u64)
        .map(|i| Op::put(i, Value::from_text("x").unwrap()))
        .collect();
    db.batch(&ops)
        .expect("a batch at the limit must be accepted");
    assert_eq!(db.len(), MAX_COMMIT_ROWS as u64);
}

#[test]
fn an_empty_batch_is_allowed_and_does_nothing() {
    let mut db = Db::in_memory().expect("open");
    db.insert(1, Value::from_text("one").unwrap()).unwrap();
    let before = db.snapshot().unwrap().to_bytes();
    db.batch(&[])
        .expect("an empty batch is a no-op, not an error");
    assert_eq!(db.snapshot().unwrap().to_bytes(), before);
}

#[cfg(unix)]
#[test]
fn a_batch_is_durable_as_a_unit_across_a_reopen() {
    let dir = scratch("batch-durable");
    {
        let mut db = Db::open(&dir).expect("open");
        db.batch(&[
            Op::put(10, Value::from_text("ten").unwrap()),
            Op::put(20, Value::from_text("twenty").unwrap()),
            Op::put(30, Value::from_text("thirty").unwrap()),
        ])
        .expect("batch");
    }
    let db = Db::open(&dir).expect("reopen");
    assert_eq!(db.len(), 3);
    assert_eq!(db.get(10).unwrap().unwrap().text(), "ten");
    assert_eq!(db.get(20).unwrap().unwrap().text(), "twenty");
    assert_eq!(db.get(30).unwrap().unwrap().text(), "thirty");
    assert!(
        !db.recovery_report().rollback_evidence,
        "a clean reopen after a batch must not report lost data"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Compare-and-set: the read inside the commit.
///
/// Every read-then-write a sample application wrote — "take the job at
/// the head of the queue if it is still pending", "increment this
/// counter", "save if nobody else edited it" — was a `get` followed by a
/// `put`, with a gap between them, correct only because this store admits
/// one writer. `Op::expect` closes the gap: the check and the writes it
/// guards are decided together, in one commit, and it costs no row slot
/// to ask.
#[test]
fn a_batch_can_assert_what_a_row_holds_before_writing() {
    let mut db = Db::in_memory_with(64).expect("open");
    db.insert(1, Value::from_text("pending").unwrap()).unwrap();
    let slots = db.stats().slots;

    // An assertion is checked, not written: no slot, and a batch of
    // nothing but assertions that hold is a no-op that succeeds.
    assert_eq!(
        Op::expect(1, Value::from_text("pending").unwrap()).rows(),
        0
    );
    assert_eq!(Op::expect_absent(9).rows(), 0);
    db.batch(&[
        Op::expect(1, Value::from_text("pending").unwrap()),
        Op::expect_absent(9),
    ])
    .expect("both assertions hold");
    assert_eq!(db.stats().slots, slots, "an assertion spends nothing");

    // Claim it, guarded. This is the whole operation, atomically.
    db.batch(&[
        Op::expect(1, Value::from_text("pending").unwrap()),
        Op::put(1, Value::from_text("claimed").unwrap()),
    ])
    .expect("the row was still pending");
    assert_eq!(db.get(1).unwrap().unwrap().text(), "claimed");

    // A second claimant loses, and loses cleanly: the batch is refused
    // whole, it names the row, and nothing was written.
    let before = db.all().unwrap();
    let e = db
        .batch(&[
            Op::expect(1, Value::from_text("pending").unwrap()),
            Op::put(1, Value::from_text("claimed by someone else").unwrap()),
            Op::insert(2, Value::from_text("side effect").unwrap()),
        ])
        .expect_err("the row is no longer pending");
    match &e {
        Error::BatchRejected { at, cause } => {
            assert_eq!(*at, 0);
            assert!(matches!(**cause, Error::Mismatch { id: 1 }), "{cause:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(db.all().unwrap(), before, "a lost race changes nothing");
    assert!(db.get(2).unwrap().is_none());

    // Absence is assertable in both directions, which is how "create it
    // only if nobody has" is written without a separate `insert`.
    db.batch(&[
        Op::expect_absent(5),
        Op::put(5, Value::from_text("mine").unwrap()),
    ])
    .expect("nobody had it");
    assert!(matches!(
        db.batch(&[Op::expect_absent(5), Op::put(5, Value::empty())]),
        Err(Error::BatchRejected { at: 0, .. })
    ));
    assert_eq!(db.get(5).unwrap().unwrap().text(), "mine");

    // It works on a value too long for one row slot, and the assertion
    // compares the whole value rather than its head.
    let long = Value::from_bytes(&[b'q'; 200]).unwrap();
    let mut nearly = vec![b'q'; 200];
    nearly[199] = b'Z';
    db.insert(6, long.clone()).unwrap();
    db.batch(&[Op::expect(6, long.clone()), Op::delete(6)])
        .expect("the whole 200 bytes match");
    db.insert(6, long.clone()).unwrap();
    assert!(
        matches!(
            db.batch(&[
                Op::expect(6, Value::from_bytes(&nearly).unwrap()),
                Op::delete(6)
            ]),
            Err(Error::BatchRejected { .. })
        ),
        "one byte different in the last slot is different"
    );
    assert!(db.get(6).unwrap().is_some());
}

/// Reads take `&self`, so one open database can serve many readers at
/// once — and, more to the point, so the compiler proves reads are pure.
///
/// Every read here is answered from state recovery built in memory: no
/// I/O is requested, no state machine advances, nothing is written. A
/// read path that broke any of that would not compile against `&self`,
/// which turns "reads never touch the disk" from a claim in a comment
/// into a property of the types.
#[test]
fn reads_take_a_shared_borrow_so_many_can_run_at_once() {
    let mut db = Db::in_memory_with(256).expect("open");
    for i in 0..40u64 {
        db.insert(i, Value::from_text(&format!("value {i}")).unwrap())
            .unwrap();
    }
    db.insert(99, Value::from_bytes(&[b'L'; 100]).unwrap())
        .unwrap();

    // Several reads alive at the same time, from one handle, with no
    // mutable borrow anywhere.
    let a = db.get(7).unwrap().unwrap();
    let b = db.range(0, 5).unwrap();
    let c = db.find_text("value 3").unwrap();
    let d = db.last(3).unwrap();
    let e = db.get(99).unwrap().unwrap();
    assert_eq!(a.text(), "value 7");
    assert_eq!(b.len(), 6);
    assert!(!c.is_empty());
    assert_eq!(d[0].0, 99);
    assert_eq!(
        e.len(),
        100,
        "a multi-slot value reads back through &self too"
    );

    // A shared reference is enough for the whole read surface, so a
    // reader can be handed out behind one.
    fn count_everything(db: &Db<dabqlite::MemoryStorage>) -> usize {
        db.all().unwrap().len()
            + db.range_rev(0, u64::MAX).unwrap().len()
            + db.find_prefix(b"value").unwrap().len()
            + db.find_page(b"value", None).unwrap().0.len()
            + db.range_page_rev(0, u64::MAX).unwrap().0.len()
            + usize::from(db.get(1).unwrap().is_some())
    }
    let n = count_everything(&db);
    assert!(n > 0);
    // The same handle is still writable afterwards: a shared read borrow
    // ends where it ends.
    db.insert(1000, Value::empty()).unwrap();
    assert_eq!(db.len(), 42);
}

/// Anchored search: a host is not a query parameter, and a tag is not a
/// longer tag that contains it.
#[test]
fn a_search_can_be_anchored_to_either_end_or_both() {
    let mut db = Db::in_memory_with(256).expect("open");
    let urls = [
        "https://example.com/page",
        "https://other.test/?ref=example.com",
        "http://example.com.evil.test/",
        "example.com",
    ];
    for (i, u) in urls.iter().enumerate() {
        db.insert(i as u64, Value::from_text(u).unwrap()).unwrap();
    }
    let ids = |rows: Vec<(u64, Value)>| {
        let mut v: Vec<u64> = rows.into_iter().map(|(id, _)| id).collect();
        v.sort_unstable();
        v
    };

    // The substring search reaches all four, which is exactly the problem
    // an unanchored search has.
    assert_eq!(ids(db.find_text("example.com").unwrap()), vec![0, 1, 2, 3]);
    assert_eq!(
        ids(db.find_prefix(b"https://example.com").unwrap()),
        vec![0]
    );
    assert_eq!(ids(db.find_suffix(b"/page").unwrap()), vec![0]);
    assert_eq!(ids(db.find_exact(b"example.com").unwrap()), vec![3]);

    // Needles too short to have a trigram are anchored too, and so is the
    // empty one: every value starts and ends with nothing, and only an
    // empty value equals it.
    db.insert(9, Value::empty()).unwrap();
    assert_eq!(ids(db.find_prefix(b"h").unwrap()), vec![0, 1, 2]);
    assert_eq!(ids(db.find_suffix(b"/").unwrap()), vec![2]);
    assert_eq!(ids(db.find_prefix(b"").unwrap()).len(), 5);
    assert_eq!(ids(db.find_exact(b"").unwrap()), vec![9]);

    // Paging works in any mode, and the modes agree with `find_matching`.
    let (page, _) = db
        .find_page_matching(b"https://", Match::Prefix, None)
        .expect("page");
    assert_eq!(page.len(), 2);
    assert_eq!(
        ids(db.find_matching(b"https://", Match::Prefix).unwrap()),
        vec![0, 1]
    );

    // Anchored search works across a slot boundary like everything else.
    let long = format!("prefix-{}-suffix", "x".repeat(200));
    db.insert(20, Value::from_text(&long).unwrap()).unwrap();
    assert_eq!(ids(db.find_prefix(b"prefix-xxx").unwrap()), vec![20]);
    assert_eq!(ids(db.find_suffix(b"xxx-suffix").unwrap()), vec![20]);
    assert_eq!(ids(db.find_exact(long.as_bytes()).unwrap()), vec![20]);
}

/// "The n newest" — the query every sample application wrote, and the
/// one none of them could express.
///
/// Ids ascend in a log, an outbox, a feed or a job queue, so the newest
/// rows are the highest ones. Reaching them through an ascending scan
/// means walking everything below them first: three separate samples
/// materialised the whole database and sorted it to answer this.
#[test]
fn the_newest_rows_are_a_page_of_work_not_a_scan() {
    let mut db = Db::in_memory_with(4096).expect("open");
    for i in 0..2000u64 {
        db.insert(i, Value::from_text(&format!("row {i}")).unwrap())
            .unwrap();
    }

    // The n highest ids, greatest first, for every n across a page
    // boundary and past the end.
    for n in [0usize, 1, 7, 8, 9, 20, 2000, 2500] {
        let got = db.last(n).expect("last");
        assert_eq!(got.len(), n.min(2000), "n={n}");
        let ids: Vec<u64> = got.iter().map(|(id, _)| *id).collect();
        let want: Vec<u64> = (0..2000u64).rev().take(n.min(2000)).collect();
        assert_eq!(ids, want, "n={n}");
        if n > 0 {
            assert_eq!(got[0].1.text(), "row 1999", "n={n}");
        }
    }

    // Descending equals ascending reversed, over bounded windows too.
    let mut ascending = db.range(500, 540).expect("range");
    ascending.reverse();
    assert_eq!(db.range_rev(500, 540).expect("range_rev"), ascending);
    let mut everything = db.all().expect("all");
    everything.reverse();
    assert_eq!(db.range_rev(0, u64::MAX).expect("range_rev"), everything);

    // And a page stops where it says it does.
    let (page, next) = db.range_page_rev(0, u64::MAX).expect("page");
    assert_eq!(page.len(), dabqlite_core::RANGE_PAGE);
    assert_eq!(page[0].0, 1999);
    let next = next.expect("more rows below");
    let (page2, _) = db.range_page_rev(0, next).expect("page");
    assert_eq!(page2[0].0, 1999 - dabqlite_core::RANGE_PAGE as u64);

    // Deletes and updates move rows, and the descending view follows.
    db.remove(1999).unwrap();
    db.remove(1998).unwrap();
    assert_eq!(db.last(1).unwrap()[0].0, 1997);
    db.put(1997, Value::from_text("edited").unwrap()).unwrap();
    assert_eq!(db.last(1).unwrap()[0].1.text(), "edited");

    // An empty database has no newest row rather than an error.
    let empty = Db::in_memory().expect("open");
    assert!(empty.last(10).unwrap().is_empty());
    assert!(empty.range_rev(0, u64::MAX).unwrap().is_empty());
}

/// Getting out of a full database, both ways.
///
/// A deletion is recorded by APPENDING a tombstone, so it needs a slot
/// like any other write — which means "just delete something" is not the
/// escape from `Error::Full`, and a caller who assumes it is finds their
/// database wedged. There are exactly two escapes, they depend on whether
/// any slot is reclaimable, and `Error::Full` carries the number that
/// decides which: rebuild, or reopen bigger.
#[test]
fn a_full_database_has_a_way_out_and_the_error_names_it() {
    // Churned full: four rows in six slots, two of them dead weight.
    let mut db = Db::in_memory_with(6).expect("open");
    for i in 0..4u64 {
        db.insert(i, Value::from_text("v").unwrap()).unwrap();
    }
    db.put(0, Value::from_text("w").unwrap()).unwrap();
    db.put(1, Value::from_text("x").unwrap()).unwrap();
    assert_eq!(db.stats().free(), 0);
    assert_eq!(db.stats().dead, 2);

    let e = db.remove(2).expect_err("a tombstone needs a slot too");
    match e {
        Error::Full { capacity, dead } => assert_eq!((capacity, dead), (6, 2)),
        other => panic!("{other:?}"),
    }
    assert!(
        e.to_string().contains("Db::compact()"),
        "the error must name the escape: {e}"
    );

    // The escape it names works, and needs no free slot to do it.
    let mut db = db.compact_to_memory().expect("rebuild");
    assert_eq!(db.stats().slots, 4);
    assert_eq!(db.stats().free(), 2);
    db.remove(2).expect("now there is room for the tombstone");
    assert_eq!(db.len(), 3);

    // Genuinely full: every slot live, nothing to reclaim. Rebuilding
    // cannot help and the error says so instead of suggesting it.
    let mut db = Db::in_memory_with(4).expect("open");
    for i in 0..4u64 {
        db.insert(i, Value::from_text("v").unwrap()).unwrap();
    }
    let e = db.remove(0).expect_err("full is full");
    match e {
        Error::Full { capacity, dead } => assert_eq!((capacity, dead), (4, 0)),
        other => panic!("{other:?}"),
    }
    assert!(
        e.to_string().contains("larger capacity"),
        "with nothing to reclaim the error must send the caller to a \
         bigger database, not to a rebuild that would return nothing: {e}"
    );
    assert_eq!(db.compact_to_memory().unwrap().stats().free(), 0);

    // And that escape works: the bytes are the same database, reopened
    // with more room.
    let snapshot = db.snapshot().expect("snapshot");
    let mut bigger = Db::load_with(&snapshot, 16).expect("reopen larger");
    assert_eq!(bigger.len(), 4);
    bigger.remove(0).expect("room now");
    assert_eq!(bigger.len(), 3);
}

/// A damaged BLOB is salvageable, not fatal.
///
/// `Db::salvage` opens a damaged directory. A snapshot had no equivalent,
/// so one flipped byte anywhere in a blob cost the whole database — and
/// the error said to reopen in salvage mode, which was advice that could
/// not be taken: there was no directory, and the rows file's name is not
/// part of the API. That is precisely the browser deployment, where a
/// database lives in memory and is snapshotted into IndexedDB, and
/// precisely where "corruption is contained, not fatal" has to hold.
#[test]
fn a_damaged_snapshot_is_salvageable_rather_than_fatal() {
    let mut db = Db::in_memory_with(64).expect("open");
    for i in 0..8u64 {
        db.insert(i, Value::from_text(&format!("row-{i}")).unwrap())
            .unwrap();
    }
    // One long value too, so the salvage covers a multi-slot run.
    db.insert(100, Value::from_bytes(&[b'L'; 100]).unwrap())
        .unwrap();
    let clean = db.snapshot().expect("snapshot");

    // A snapshot round-trips as a blob and back, undamaged.
    let bytes = clean.to_bytes();
    assert_eq!(
        Db::load(&Snapshot::from_bytes(&bytes).unwrap())
            .unwrap()
            .len(),
        9
    );

    // Flip one bit inside a committed row. Strict load refuses the whole
    // database — detection over availability, unchanged.
    // The rows image starts after the 24-byte header and the superblock.
    let mut damaged = bytes.clone();
    let sb_len = u64::from_le_bytes(damaged[8..16].try_into().unwrap()) as usize;
    let rows_at = 24 + sb_len;
    damaged[rows_at + 3 * 32 + 5] ^= 0x40;
    let damaged = Snapshot::from_bytes(&damaged).expect("still a snapshot");
    assert!(
        matches!(Db::load(&damaged), Err(Error::Corrupt { .. })),
        "a strict load must refuse a damaged blob"
    );

    // Salvage load: the damage is contained, everything else is served.
    let mut rescued = Db::load_salvaged(&damaged).expect("salvage load");
    assert!(rescued.is_degraded());
    assert_eq!(rescued.recovery_report().quarantined_rows, 1);
    let survivors = rescued.all().expect("scan what survived");
    assert_eq!(survivors.len(), 8, "one row lost, the rest readable");
    assert!(
        !survivors.iter().any(|(id, _)| *id == 3),
        "the damaged row must not be served"
    );
    // The long value came through whole.
    assert_eq!(
        rescued.get(100).unwrap().unwrap().as_bytes(),
        &[b'L'; 100][..]
    );
    // And it is read-only, like every salvage.
    assert!(rescued.insert(500, Value::empty()).is_err());

    // Rebuild from what survived, in one call, and the result is a
    // healthy database that can be snapshotted straight back out.
    let mut rebuilt = rescued.compact_to_memory().expect("rebuild");
    assert_eq!(rebuilt.len(), 8);
    assert!(!rebuilt.is_degraded());
    assert_eq!(rebuilt.recovery_report().quarantined_rows, 0);
    assert_eq!(rebuilt.all().unwrap(), survivors);
    let healthy = rebuilt.snapshot().expect("snapshot");
    assert_eq!(Db::load(&healthy).expect("load").len(), 8);
}

/// Looking at a database must not change it — including the one thing
/// that only exists until someone looks.
///
/// A commit interrupted by a crash leaves checksum-valid rows past the
/// manifest. The first read-WRITE open truncates them, which is what
/// keeps the next open's claim rule sound; but it also means the evidence
/// is consumed by whoever opens first. A monitoring probe, a `stat`
/// command, an operator having a look — any of them would silently disarm
/// the alarm for the process that actually needed it.
///
/// A read-only open leaves no next state to keep sound, so it truncates
/// nothing. Two things follow: it can open a database that crashed
/// mid-commit at all (it would otherwise be refused a truncate it must
/// not perform, and fail-stop on the spot), and the evidence survives
/// being read.
#[cfg(unix)]
#[test]
fn a_read_only_open_does_not_consume_the_evidence_of_an_interrupted_commit() {
    let dir = scratch("residue-readonly");
    let superblock = {
        let mut db = Db::open(&dir).expect("open");
        for i in 0..3u64 {
            db.insert(i, Value::from_text("committed").unwrap())
                .unwrap();
        }
        // The manifest as it stands with three rows committed.
        std::fs::read(dir.join("superblock.dabq")).expect("superblock")
    };
    {
        let mut db = Db::open(&dir).expect("reopen");
        db.insert(99, Value::from_text("interrupted").unwrap())
            .unwrap();
    }
    // Rewind the manifest over the fourth row: exactly the state a crash
    // between the row fsync and the superblock flip leaves behind.
    std::fs::write(dir.join("superblock.dabq"), &superblock).expect("rewind");

    // Read-only, twice: the same answer both times, and no writer lock.
    for pass in 0..2 {
        let db = Db::read_only(&dir).expect("read-only open");
        assert_eq!(db.len(), 3, "pass {pass}");
        assert_eq!(
            db.recovery_report().orphan_valid_rows,
            1,
            "pass {pass}: reading the database consumed the evidence"
        );
        assert!(!db.recovery_report().rollback_evidence);
    }

    // The read-write open still sees it, and only then is it cleared.
    {
        let db = Db::open(&dir).expect("open");
        assert_eq!(db.recovery_report().orphan_valid_rows, 1);
        assert_eq!(db.len(), 3);
    }
    let db = Db::open(&dir).expect("open");
    assert_eq!(
        db.recovery_report().orphan_valid_rows,
        0,
        "the residue is gone once a writer has recovered past it"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A database remembers how big it was declared, so reopening it does not
/// have to be told again.
///
/// Every sample application built against this library before the
/// capacity was recorded ended up keeping a sidecar file to remember it —
/// and one of them then discovered that compaction deleted the sidecar.
/// The number belongs in the database.
#[cfg(unix)]
#[test]
fn a_database_remembers_the_capacity_it_was_created_with() {
    let dir = scratch("remembered-capacity");
    {
        let mut db = Db::open_with(&dir, 100).expect("open");
        db.insert(1, Value::from_text("one").unwrap()).unwrap();
        assert_eq!(db.stats().capacity, 100);
    }
    // Reopened without saying anything: the same ceiling, not the default.
    {
        let db = Db::open(&dir).expect("reopen");
        assert_eq!(
            db.stats().capacity,
            100,
            "reopening forgot the declared capacity"
        );
        assert_eq!(db.recovery_report().declared_capacity, 100);
        assert_eq!(db.get(1).unwrap().unwrap().text(), "one");
    }
    // And it survives a compaction, which rebuilds the directory.
    {
        let mut db = Db::open(&dir).expect("reopen");
        db.compact().expect("compact");
        assert_eq!(db.stats().capacity, 100, "compaction forgot the capacity");
    }
    // Asking for a different one explicitly still wins for that session,
    // and the file learns it at the next commit — a capacity is recorded
    // by writing, not by opening, so an open never rewrites a database it
    // was only asked to read.
    {
        let db = Db::open_with(&dir, 250).expect("resize");
        assert_eq!(db.stats().capacity, 250);
    }
    {
        let db = Db::open(&dir).expect("reopen after a resize that wrote nothing");
        assert_eq!(
            db.stats().capacity,
            100,
            "an open that wrote nothing should not have changed the file"
        );
    }
    {
        let mut db = Db::open_with(&dir, 250).expect("resize");
        db.insert(2, Value::from_text("two").unwrap()).unwrap();
    }
    {
        let db = Db::open(&dir).expect("reopen after a resize that wrote");
        assert_eq!(
            db.stats().capacity,
            250,
            "the new capacity was not recorded"
        );
        assert_eq!(db.get(2).unwrap().unwrap().text(), "two");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The same for a snapshot: the bytes carry the capacity, so a round trip
/// through `to_bytes`/`from_bytes` does not quietly resize the database.
#[test]
fn a_snapshot_carries_the_capacity_it_was_written_with() {
    let mut db = Db::in_memory_with(64).expect("open");
    db.insert(1, Value::from_text("one").unwrap()).unwrap();
    let bytes = db.snapshot().unwrap().to_bytes();

    let snapshot = Snapshot::from_bytes(&bytes).expect("parse");
    let reloaded = Db::load(&snapshot).expect("load");
    assert_eq!(
        reloaded.stats().capacity,
        64,
        "a reloaded snapshot got a different ceiling than the one saved"
    );
    assert_eq!(reloaded.get(1).unwrap().unwrap().text(), "one");
}

/// A snapshot can go back onto disk, atomically.
///
/// `snapshot()` had no inverse: a snapshot IS the two files, but putting
/// them back safely meant writing your own temp-file-and-rename, and every
/// sample application that saved one did exactly that.
#[cfg(unix)]
#[test]
fn a_snapshot_can_be_restored_onto_a_directory() {
    let dir = scratch("restore");
    let source = {
        let mut db = Db::in_memory_with(200).expect("open");
        for i in 0..30u64 {
            db.put(i, Value::from_bytes(&vec![b'v'; 40 + i as usize]).unwrap())
                .unwrap();
        }
        db.remove(3).unwrap();
        db.snapshot().unwrap()
    };

    // Onto a directory that does not exist yet.
    {
        let db = Db::restore(&dir, &source).expect("restore");
        assert_eq!(db.len(), 29);
        assert_eq!(db.get(3).unwrap(), None);
        assert_eq!(db.get(7).unwrap().unwrap().len(), 47);
        assert_eq!(db.stats().capacity, 200, "the capacity came with it");
        // Restoring rebuilds, so the dead slot the delete left is gone.
        assert_eq!(db.stats().dead, 0);
    }

    // And over a directory that already holds a different database.
    {
        let mut existing = Db::open(&dir).expect("reopen");
        existing
            .put(999, Value::from_text("later").unwrap())
            .unwrap();
        assert!(existing.contains(999).unwrap());
    }
    {
        let db = Db::restore(&dir, &source).expect("restore over");
        assert_eq!(
            db.get(999).unwrap(),
            None,
            "the old contents should be gone"
        );
        assert_eq!(db.len(), 29);
    }

    // It refuses while another writer holds the target, rather than
    // pulling the directory out from under them.
    {
        let _held = Db::open(&dir).expect("hold the lock");
        match Db::restore(&dir, &source) {
            Err(Error::Locked { .. }) => {}
            other => panic!("expected Locked, got {other:?}"),
        }
    }
    // And the database it refused to overwrite is untouched.
    {
        let db = Db::open(&dir).expect("still there");
        assert_eq!(db.len(), 29);
    }
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(dir.with_extension("compacting")).ok();
}

/// Readers, at last: any number of them, alongside the writer, taking no
/// lock and writing nothing.
///
/// All three sample applications reported the absence of this as a hard
/// limitation — a status endpoint, a dashboard, or a second CLI could not
/// read a database that anything was writing. The honest read-only mode
/// already existed for damaged databases; this is the same machinery,
/// named for what it does.
#[cfg(unix)]
#[test]
fn readers_run_alongside_the_writer_and_never_see_a_half_commit() {
    let dir = scratch("readers");
    let mut writer = Db::open_with(&dir, 4096).expect("writer");
    for i in 0..20u64 {
        writer
            .put(i, Value::from_bytes(&[b'a'; 40]).unwrap())
            .unwrap();
    }

    // Several readers at once, while the writer still holds its lock.
    let mut readers: Vec<_> = (0..4)
        .map(|_| Db::read_only(&dir).expect("a reader must not need the lock"))
        .collect();
    for r in &mut readers {
        assert_eq!(r.len(), 20);
        assert_eq!(r.get(7).unwrap().unwrap().len(), 40);
        assert!(!r.is_degraded(), "a healthy database is not degraded");
    }

    // The writer keeps working, in batches and singly, including values
    // that span several row slots. Each reader sees a COMMITTED state —
    // the one it opened on — never a partial one.
    for round in 0..30u64 {
        writer
            .batch(&[
                Op::put(100 + round, Value::from_bytes(&[b'b'; 300]).unwrap()),
                Op::put(200 + round, Value::from_text("small").unwrap()),
                Op::remove(round),
            ])
            .unwrap();
        // A reader opened NOW sees a whole number of commits: every id it
        // can see carries a whole value, never a prefix.
        let fresh = Db::read_only(&dir).expect("reader");
        for (id, value) in fresh.all().unwrap() {
            let expect = if id >= 200 {
                5
            } else if id >= 100 {
                300
            } else {
                40
            };
            assert_eq!(
                value.len(),
                expect,
                "reader saw a torn value for id {id} after round {round}"
            );
        }
    }

    // The readers opened at the start still show the state they opened on:
    // a snapshot, not a moving target.
    for r in &mut readers {
        assert_eq!(r.len(), 20, "a reader's view moved under it");
    }

    // And none of them disturbed the writer or the files.
    assert!(writer.contains(129).unwrap());
    drop(readers);
    drop(writer);
    std::fs::remove_dir_all(&dir).ok();
}

/// `max_id` answers "what id comes next" without a scan.
///
/// A sample application needed it and had no way to ask: it scanned every
/// row for the maximum, then cached the answer in a row of its own, which
/// every insert then rewrote — 49,999 dead slots after 50,000 inserts, 9%
/// of its arena, to store a number the index already knew.
#[test]
fn the_largest_id_is_a_question_you_can_ask() {
    let mut db = Db::in_memory_with(256).expect("open");
    assert_eq!(db.max_id(), None, "an empty database has no largest id");

    for id in [5u64, 100, 42] {
        db.put(id, Value::from_text("x").unwrap()).unwrap();
    }
    assert_eq!(db.max_id(), Some(100));

    // It is the largest id EVER used, not the largest live one: an
    // allocator that reused 100 after it was deleted would collide with
    // rows that still remember it.
    db.remove(100).unwrap();
    assert_eq!(db.get(100).unwrap(), None);
    assert_eq!(
        db.max_id(),
        Some(100),
        "a deleted id must not be handed out again"
    );

    // It survives a restart, because the tree is rebuilt from the rows.
    let snapshot = db.snapshot().unwrap();
    let reloaded = Db::load(&snapshot).expect("reload");
    assert_eq!(reloaded.max_id(), Some(100));

    // And it tracks growth across enough inserts to span several tree
    // levels, so the descent is exercised rather than a single leaf.
    let mut db = Db::in_memory_with(1024).expect("open");
    for id in 0..500u64 {
        db.put(id * 7, Value::from_text("x").unwrap()).unwrap();
        assert_eq!(db.max_id(), Some(id * 7), "after inserting {}", id * 7);
    }
}
