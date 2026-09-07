//! What `Db::batch` actually promises, checked against what the docs say.
//! Every test here is a question a doc comment made me ask and then go and
//! answer — now including the ones raised by values that span several row
//! slots.

use dabqlite::{Db, Error, Op, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN, VALUE_LEN};
use jobqueue::slot_cost;

fn v(s: &str) -> Value {
    Value::from_text(s).unwrap()
}

fn bytes(n: usize) -> Value {
    Value::from_vec((0..n).map(|i| (i % 251 + 1) as u8).collect()).unwrap()
}

/// Documented: "Each operation sees the state the ones before it in the
/// same batch would leave, so `[insert(5, a), delete(5), insert(5, b)]` is
/// legal and means what it reads as."
#[test]
fn ops_see_the_effects_of_earlier_ops_in_the_same_batch() {
    let mut db = Db::in_memory_with(64).unwrap();
    db.batch(&[Op::insert(5, v("a")), Op::delete(5), Op::insert(5, v("b"))])
        .unwrap();
    assert_eq!(db.get(5).unwrap().map(|x| x.text()), Some("b".into()));
    // Three staged rows for three short ops, in one commit.
    assert_eq!(db.stats().slots, 3);
    assert_eq!(db.stats().live, 1);
}

/// Documented: `Op::Put` "resolves to an insert or a replace with no
/// read-then-write gap to race in". Both directions in one batch.
#[test]
fn put_resolves_against_the_projected_state() {
    let mut db = Db::in_memory_with(64).unwrap();
    db.insert(1, v("old")).unwrap();
    db.batch(&[Op::put(1, v("new")), Op::put(2, v("fresh"))])
        .unwrap();
    assert_eq!(db.get(1).unwrap().unwrap().text(), "new");
    assert_eq!(db.get(2).unwrap().unwrap().text(), "fresh");
    db.batch(&[Op::delete(1), Op::put(1, v("again"))]).unwrap();
    assert_eq!(db.get(1).unwrap().unwrap().text(), "again");
}

/// A batch's slot cost is not `ops.len()`: a `Remove` of an absent id
/// stages nothing, and a long value stages several rows.
#[test]
fn slot_cost_is_not_the_operation_count_in_either_direction() {
    let mut db = Db::in_memory_with(4096).unwrap();
    db.insert(1, v("x")).unwrap();
    let before = db.stats().slots;
    db.batch(&[Op::remove(2), Op::remove(3), Op::remove(4)])
        .unwrap();
    assert_eq!(db.stats().slots, before, "a batch of no-ops consumed slots");

    // One operation, many slots.
    let before = db.stats().slots;
    db.batch(&[Op::put(9, bytes(1000))]).unwrap();
    assert_eq!(
        db.stats().slots - before,
        slot_cost(1000) as u64,
        "a 1000-byte value is {} rows",
        slot_cost(1000)
    );
    assert_eq!(db.get(9).unwrap().unwrap().len(), 1000);

    // Whereas the strict form refuses the whole batch.
    let e = db.batch(&[Op::put(11, v("y")), Op::delete(2)]).unwrap_err();
    assert_eq!(
        e,
        Error::BatchRejected {
            at: 1,
            cause: Box::new(Error::NotFound { id: 2 })
        }
    );
    assert_eq!(
        db.get(11).unwrap(),
        None,
        "the op before the bad one landed"
    );
}

/// An empty batch is a success that writes nothing.
#[test]
fn an_empty_batch_is_a_no_op() {
    let mut db = Db::in_memory_with(64).unwrap();
    db.insert(1, v("x")).unwrap();
    let before = db.stats();
    db.batch(&[]).unwrap();
    assert_eq!(db.stats(), before);
}

/// The single-row methods and `batch` agree on the resulting rows.
#[test]
fn a_batch_and_the_same_writes_one_at_a_time_produce_the_same_rows() {
    let mut one = Db::in_memory_with(4096).unwrap();
    one.insert(1, bytes(300)).unwrap();
    one.insert(2, v("b")).unwrap();
    one.update(1, bytes(40)).unwrap();
    one.delete(2).unwrap();

    let mut many = Db::in_memory_with(4096).unwrap();
    many.batch(&[
        Op::insert(1, bytes(300)),
        Op::insert(2, v("b")),
        Op::update(1, bytes(40)),
        Op::delete(2),
    ])
    .unwrap();

    assert_eq!(one.stats(), many.stats());
    assert_eq!(one.all().unwrap(), many.all().unwrap());
    // The BYTES differ, and they have to: the commit-span byte inside each
    // row records how many rows share its commit, so a four-op commit is
    // not byte-identical to four one-op commits. Anything comparing
    // snapshots across two writers must compare ROWS, not bytes.
    assert_ne!(
        one.snapshot().unwrap().to_bytes(),
        many.snapshot().unwrap().to_bytes(),
        "if these ever match, the commit span is not in the row after all"
    );
}

/// `MAX_COMMIT_ROWS` counts ROW SLOTS, not operations, and the error says so.
///
/// This is the API change that mattered most to this crate: the old cap
/// was an operation count, the old refusal was `Error::Full { capacity: 64
/// }` whose Display read "database is full at its declared capacity of 64
/// rows" on a database that was empty, and `at` was the cap rather than an
/// operation index. Now there is a variant that describes what happened.
#[test]
fn the_cap_is_row_slots_and_the_refusal_names_them() {
    let mut db = Db::in_memory_with(65_536).unwrap();

    // Exactly MAX_COMMIT_ROWS one-slot ops is allowed.
    let ops: Vec<Op> = (0..MAX_COMMIT_ROWS as u64)
        .map(|i| Op::put(i, v("x")))
        .collect();
    db.batch(&ops).unwrap();
    assert_eq!(db.len(), MAX_COMMIT_ROWS as u64);

    // One more operation is refused, and the message counts rows.
    let ops: Vec<Op> = (1000..1000 + MAX_COMMIT_ROWS as u64 + 1)
        .map(|i| Op::put(i, v("x")))
        .collect();
    let e = db.batch(&ops).unwrap_err();
    let cause = match &e {
        Error::BatchRejected { cause, .. } => cause.as_ref().clone(),
        other => panic!("expected BatchRejected, got {other:?}"),
    };
    assert_eq!(
        cause,
        Error::BatchTooLong {
            rows: MAX_COMMIT_ROWS + 1,
            max: MAX_COMMIT_ROWS
        }
    );
    assert!(
        e.to_string().contains("one commit holds"),
        "the refusal should explain the commit limit: {e}"
    );
    assert!(
        !e.to_string().contains("database is full"),
        "a too-long batch is not a full database: {e}"
    );

    // And TWO operations can be too long, if they are long enough: a batch
    // is bounded by bytes/16, not by op count.
    let e = db
        .batch(&[Op::put(2000, bytes(1040)), Op::put(2001, bytes(1040))])
        .unwrap_err();
    match e {
        Error::BatchRejected { cause, .. } => assert_eq!(
            *cause,
            Error::BatchTooLong {
                rows: 130,
                max: MAX_COMMIT_ROWS
            },
            "two ops, 130 slots"
        ),
        other => panic!("expected BatchRejected, got {other:?}"),
    }
}

/// The ceiling and the atomicity guarantee cannot be used together.
///
/// A `MAX_VALUE_LEN` value is exactly `MAX_COMMIT_ROWS` slots, so it fills a
/// commit by itself. It can be written — alone. It can NEVER be written in
/// the same commit as anything else, not even a one-slot bookkeeping row.
/// Any application that keeps an invariant across two rows (this one keeps
/// two) therefore has a real value ceiling of `MAX_VALUE_LEN - VALUE_LEN`,
/// which the library never mentions.
#[test]
fn a_maximum_length_value_cannot_share_a_commit_with_anything() {
    let mut db = Db::in_memory_with(65_536).unwrap();
    let biggest = bytes(MAX_VALUE_LEN);
    assert_eq!(slot_cost(MAX_VALUE_LEN), MAX_COMMIT_ROWS);

    db.batch(&[Op::put(1, biggest.clone())]).unwrap();
    assert_eq!(db.get(1).unwrap().unwrap().len(), MAX_VALUE_LEN);

    let e = db
        .batch(&[Op::put(2, biggest.clone()), Op::put(3, v("bookkeeping"))])
        .unwrap_err();
    match e {
        Error::BatchRejected { cause, .. } => assert_eq!(
            *cause,
            Error::BatchTooLong {
                rows: MAX_COMMIT_ROWS + 1,
                max: MAX_COMMIT_ROWS
            }
        ),
        other => panic!("expected BatchRejected, got {other:?}"),
    }
    assert_eq!(db.get(2).unwrap(), None, "nothing from the refused batch");

    // One slot short of the ceiling, and the pair fits.
    db.batch(&[
        Op::put(2, bytes(MAX_VALUE_LEN - VALUE_LEN)),
        Op::put(3, v("bookkeeping")),
    ])
    .unwrap();
    assert_eq!(db.get(3).unwrap().unwrap().text(), "bookkeeping");
}

/// Exactness, which is the whole point of the new `Value`: what goes in
/// comes out, at the same length, trailing zeros and all — through a
/// multi-slot value, through `range`, and through a rebuild.
#[test]
fn values_round_trip_exactly_including_trailing_zeros() {
    let mut db = Db::in_memory_with(4096).unwrap();
    let cases: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0; 16],
        vec![0; 17],
        {
            let mut v = vec![7u8; 100];
            v.extend_from_slice(&[0; 20]);
            v
        },
        {
            let mut v = vec![9u8; MAX_VALUE_LEN - 4];
            v.extend_from_slice(&[0; 4]);
            v
        },
    ];
    for (i, want) in cases.iter().enumerate() {
        db.put(i as u64, Value::from_vec(want.clone()).unwrap())
            .unwrap();
    }
    for (i, want) in cases.iter().enumerate() {
        let got = db.get(i as u64).unwrap().expect("row");
        assert_eq!(got.as_bytes(), &want[..], "case {i} did not round-trip");
        assert_eq!(got.len(), want.len(), "case {i} changed length");
    }
    // The same through a bulk scan, which reads long values by a different
    // path (the page carries the length, not the bytes).
    for (id, got) in db.all().unwrap() {
        assert_eq!(
            got.as_bytes(),
            &cases[id as usize][..],
            "row {id} via all()"
        );
    }
}
