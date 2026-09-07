//! The public API's contract, from a caller's point of view.
//!
//! Everything below is written the way an application would write it — no
//! engine types, no `Output` matching, no hand-rolled persistence. If a
//! test here needs a helper that feels like plumbing, that is a signal the
//! library is missing something, not that the test needs more code.

use dabqlite::{Db, Error, Op, Snapshot, Value, MAX_BATCH, MAX_VALUE_LEN, VALUE_LEN};

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
            max: MAX_BATCH,
        },
    ];
    // Every variant must appear above. This match exists to break the
    // build when a new one is added: a variant with no case here is a
    // variant whose message nobody ever read.
    fn covered(e: &Error) -> &'static str {
        match e {
            Error::NotFound { .. } => "NotFound",
            Error::AlreadyExists { .. } => "AlreadyExists",
            Error::Full { .. } => "Full",
            Error::CapacityTooSmall { .. } => "CapacityTooSmall",
            Error::Locked { .. } => "Locked",
            Error::ValueTooLong { .. } => "ValueTooLong",
            Error::Degraded { .. } => "Degraded",
            Error::Corrupt { .. } => "Corrupt",
            Error::SchemaMismatch { .. } => "SchemaMismatch",
            Error::Io { .. } => "Io",
            Error::BatchTooLong { .. } => "BatchTooLong",
            Error::BatchRejected { .. } => "BatchRejected",
        }
    }
    let mut seen: Vec<&'static str> = cases.iter().map(covered).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        cases.len(),
        "the case list has duplicates, so some variant is untested"
    );

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
    let mut reloaded = Db::load(&snapshot).expect("reload");
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
    let ops: Vec<Op> = (0..MAX_BATCH as u64 + 1)
        .map(|i| Op::put(i, Value::from_text("x").unwrap()))
        .collect();
    // The refusal must not claim the DATABASE is full: it is empty, and
    // its capacity is not MAX_BATCH. Three separate sample applications
    // reported the old message as stating the reverse of the truth.
    match db.batch(&ops) {
        Err(Error::BatchRejected { cause, .. }) => match *cause {
            Error::BatchTooLong { rows, max } => {
                assert_eq!(max, MAX_BATCH);
                assert_eq!(rows, MAX_BATCH + 1);
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
        db.stats().capacity > MAX_BATCH as u64,
        "the test needs a database bigger than one commit to be meaningful"
    );

    // Exactly at the limit is fine.
    let ops: Vec<Op> = (0..MAX_BATCH as u64)
        .map(|i| Op::put(i, Value::from_text("x").unwrap()))
        .collect();
    db.batch(&ops)
        .expect("a batch at the limit must be accepted");
    assert_eq!(db.len(), MAX_BATCH as u64);
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
    let mut db = Db::open(&dir).expect("reopen");
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
        let mut db = Db::open(&dir).expect("reopen");
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
        let mut db = Db::open(&dir).expect("reopen after a resize that wrote");
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
    let mut reloaded = Db::load(&snapshot).expect("load");
    assert_eq!(
        reloaded.stats().capacity,
        64,
        "a reloaded snapshot got a different ceiling than the one saved"
    );
    assert_eq!(reloaded.get(1).unwrap().unwrap().text(), "one");
}
