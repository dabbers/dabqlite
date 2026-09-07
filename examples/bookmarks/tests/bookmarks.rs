//! Integration tests for the bookmark store.
//!
//! Many of these exist to pin down a *limitation* of the underlying
//! library rather than a feature of this crate. Those are named so, and
//! they are meant to FAIL if the library ever grows the thing they say it
//! lacks — that is the point of writing them down.

use bookmarks::{
    Bookmark, NewBookmark, Order, Query, Store, StoreError, MAX_INDEXED_TAG, MAX_NEEDLE,
    MAX_RECORD, MAX_TAG, MAX_TAGS, MAX_TITLE, MAX_URL, TS,
};
use dabqlite::{
    Error as DbErr, MemDb, MemoryStorage, Op, Value, MAX_COMMIT_ROWS, MAX_VALUE_LEN, VALUE_LEN,
};

const MDN: &str = "https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API";
const RUST: &str = "https://doc.rust-lang.org/std/index.html";
const SQLITE: &str = "https://www.sqlite.org/whentouse.html";

const T0: u64 = 1_700_000_000;

fn tags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn seeded() -> Store<MemoryStorage> {
    let mut s = Store::in_memory().expect("fresh");
    s.add(RUST, "Rust standard library", &tags(&["rust", "docs"]), T0)
        .unwrap();
    s.add(
        SQLITE,
        "When to use SQLite",
        &tags(&["sqlite", "db", "docs"]),
        T0 + 10,
    )
    .unwrap();
    s.add(MDN, "IndexedDB API", &tags(&["browser", "db"]), T0 + 20)
        .unwrap();
    s
}

fn ids(v: &[Bookmark]) -> Vec<u64> {
    v.iter().map(|b| b.id).collect()
}

// ---------------------------------------------------------------------------
// The shape of the thing
// ---------------------------------------------------------------------------

#[test]
fn a_bookmark_is_one_key_and_one_value() {
    // The whole port in one assertion. A bookmark used to be up to fifty
    // rows under a bit-packed key; it is now the value at its id.
    let mut s = seeded();
    assert_eq!(s.db().len(), 4, "three bookmarks and one header row");
    for id in 1..=3u64 {
        assert!(s.db().get(id).unwrap().is_some());
    }
    // ...and a generic function over any backend is writable, because the
    // storage types are exported.
    fn count_any<S: dabqlite::Storage>(s: &mut Store<S>) -> u64 {
        s.count().unwrap()
    }
    assert_eq!(count_any(&mut s), 3);
}

#[test]
fn a_session_round_trips_through_one_opaque_blob() {
    let mut s = seeded();
    let before = s.list().unwrap();
    let blob = s.to_blob().unwrap();
    drop(s);

    let mut s = Store::load(&blob).unwrap();
    assert_eq!(s.list().unwrap(), before, "the reloaded database differs");

    let id = s
        .add("https://example.com", "Example", &[], T0 + 30)
        .unwrap();
    assert_eq!(id, 4);
    assert_eq!(s.list().unwrap().len(), 4);
}

#[test]
fn a_two_kilobyte_url_is_one_value_and_lands_in_one_commit() {
    // The previous version of this test was called
    // `a_realistic_2kb_url_cannot_be_written_atomically`. It can now.
    let real_url = format!("https://example.com/search?q={}", "x".repeat(950));
    assert!(real_url.len() > 900 && real_url.len() <= MAX_URL);

    let mut s = Store::in_memory().unwrap();
    let before = s.stats().slots;
    s.add(&real_url, "Long", &tags(&["long"]), T0).unwrap();
    let cost = s.stats().slots - before;
    assert!(cost > 60, "a 1 KB URL is many slots: {cost}");
    assert!(
        cost as usize <= MAX_COMMIT_ROWS,
        "and all of them are one commit: {cost} > {MAX_COMMIT_ROWS}"
    );
    assert_eq!(s.get_bookmark(1).unwrap().unwrap().url, real_url);
}

#[test]
fn text_survives_exactly_including_bytes_that_used_to_be_padding() {
    // `Value` is length-carrying now, so trailing zeros are data. The old
    // format padded to sixteen bytes and could not tell the difference,
    // which is why this crate carried a length side-car.
    let mut db = MemDb::in_memory_with(256).unwrap();
    let awkward = b"end\0\0\0".to_vec();
    db.put(1, Value::from_vec(awkward.clone()).unwrap())
        .unwrap();
    assert_eq!(db.get(1).unwrap().unwrap().as_bytes(), &awkward[..]);

    // And the store's own text, across a slot boundary, in both scripts.
    let long_title = "Ünïcödé — a title with multi-byte characters that is comfortably \
                      longer than sixteen bytes and lands a code point across a slot edge";
    let mut s = Store::in_memory().unwrap();
    s.add(MDN, long_title, &tags(&["long"]), T0).unwrap();
    let blob = s.to_blob().unwrap();
    let b = Store::load(&blob)
        .unwrap()
        .get_bookmark(1)
        .unwrap()
        .unwrap();
    assert_eq!(b.url, MDN);
    assert_eq!(b.title, long_title);
}

// ---------------------------------------------------------------------------
// Batches: what atomicity buys, and where it stops
// ---------------------------------------------------------------------------

#[test]
fn one_bookmark_is_one_commit() {
    let mut s = Store::in_memory().unwrap();
    let before = s.stats().slots;
    s.add(MDN, "IndexedDB API", &tags(&["browser", "db"]), T0)
        .unwrap();
    let rows = s.stats().slots - before;
    // The value's slots plus the id-counter row.
    let record = s.get_bookmark(1).unwrap().unwrap().encode().len();
    assert_eq!(rows as usize, record.div_ceil(VALUE_LEN) + 1);
    assert!(rows as usize <= MAX_COMMIT_ROWS);
}

#[test]
fn a_refused_add_leaves_nothing_behind_at_all() {
    let mut s = Store::in_memory_with(64).unwrap();
    let mut added = 0u64;
    while s
        .add(
            &format!("https://example.com/{added}"),
            "Example",
            &tags(&["x"]),
            T0,
        )
        .is_ok()
    {
        added += 1;
    }
    assert!(added > 0);
    let rows = s.db().all().unwrap().len();
    assert_eq!(s.count().unwrap(), added);
    assert_eq!(rows as u64, added + 1, "no half-written bookmark survived");
    // Every id from 1..=added reads back whole.
    for id in 1..=added {
        assert!(s.get_bookmark(id).unwrap().is_some());
    }
}

#[test]
fn an_oversized_batch_now_says_batch_too_long_and_not_database_full() {
    // FIXED. This database has room for 4096 rows and holds none. The old
    // library answered a 65-op batch with "database is full at its
    // declared capacity of 64 rows" — both halves false. It now names the
    // thing that is actually too big.
    let mut db = MemDb::in_memory_with(4096).unwrap();
    assert_eq!(db.stats().capacity, 4096);
    assert_eq!(db.len(), 0);
    let ops: Vec<Op> = (0..=MAX_COMMIT_ROWS as u64)
        .map(|i| Op::put(i, Value::from_bytes(b"x").unwrap()))
        .collect();
    let e = db.batch(&ops).unwrap_err();
    assert_eq!(
        e,
        DbErr::BatchRejected {
            at: MAX_COMMIT_ROWS,
            cause: Box::new(DbErr::BatchTooLong {
                rows: MAX_COMMIT_ROWS + 1,
                max: MAX_COMMIT_ROWS
            })
        }
    );
    assert_eq!(
        e.to_string(),
        format!(
            "batch refused at operation {max} (this batch needs {over} row slots \
             and one commit holds {max}; split it into several batches, each \
             still atomic in itself); nothing in it was applied",
            max = MAX_COMMIT_ROWS,
            over = MAX_COMMIT_ROWS + 1,
        )
    );
    assert!(db.batch(&ops[..MAX_COMMIT_ROWS]).is_ok());
    assert_eq!(db.len(), MAX_COMMIT_ROWS as u64);
}

#[test]
fn a_full_length_value_still_leaves_room_for_the_counter_beside_it() {
    // FIXED, and it was the sharpest edge in the port. MAX_VALUE_LEN used
    // to be exactly MAX_COMMIT_ROWS * VALUE_LEN, so a maximum-length value
    // consumed the whole commit and "one record plus the counter that
    // names it, atomically" — the most ordinary two-row invariant there
    // is — was impossible at the top of the value range. This crate
    // reserved a slot (MAX_RECORD = MAX_VALUE_LEN - VALUE_LEN) rather than
    // discover it in production. The commit is now eight times the longest
    // value, so the reservation is gone.
    assert_eq!(MAX_VALUE_LEN * 8, MAX_COMMIT_ROWS * VALUE_LEN);
    assert_eq!(MAX_RECORD, MAX_VALUE_LEN, "no slot is held back any more");

    let mut db = MemDb::in_memory_with(4096).unwrap();
    let full = Value::from_vec(vec![b'x'; MAX_VALUE_LEN]).unwrap();
    assert!(db.put(1, full.clone()).is_ok(), "alone, it fits");
    db.batch(&[
        Op::put(2, full.clone()),
        Op::put(3, Value::from_bytes(b"!").unwrap()),
    ])
    .expect("and so does a companion beside it");
    assert_eq!(db.get(2).unwrap().unwrap().as_bytes(), full.as_bytes());
    assert_eq!(db.get(3).unwrap().unwrap().as_bytes(), b"!");

    // Several of them at once, too — the whole point of decoupling the
    // two limits. Eight maximum-length values exactly fill one commit.
    let mut db = MemDb::in_memory_with(4096).unwrap();
    let ops: Vec<Op> = (0..(MAX_COMMIT_ROWS / (MAX_VALUE_LEN / VALUE_LEN)) as u64)
        .map(|i| Op::put(i, full.clone()))
        .collect();
    assert_eq!(ops.len(), 8);
    db.batch(&ops).expect("a commit's worth of longest values");
    assert_eq!(db.len(), 8);

    // And the store's ceiling is now the library's, applied to the whole
    // encoded record and still refused before any write.
    let mut s = Store::in_memory().unwrap();
    let long_tags: Vec<String> = (0..MAX_TAGS).map(|i| format!("{i:0>63}")).collect();
    let e = s
        .add(&"u".repeat(MAX_URL), &"t".repeat(MAX_TITLE), &long_tags, T0)
        .unwrap_err();
    assert!(
        matches!(e, StoreError::RecordTooLong { max, .. } if max == MAX_RECORD),
        "{e:?}"
    );
    assert_eq!(s.count().unwrap(), 0);
}

#[test]
fn add_many_is_one_commit_for_a_whole_import() {
    let mut s = Store::in_memory().unwrap();
    let batch: Vec<NewBookmark> = (0..12)
        .map(|i| {
            NewBookmark::new(
                &format!("https://example.com/article/{i}"),
                &format!("Article {i}"),
                &["import"],
                T0 + i,
            )
        })
        .collect();
    let before = s.stats().slots;
    let got = s.add_many(&batch).unwrap();
    assert_eq!(got, (1..=12).collect::<Vec<_>>());
    let cost = s.stats().slots - before;
    assert!(cost as usize <= MAX_COMMIT_ROWS, "{cost} slots");
    assert_eq!(s.count().unwrap(), 12);

    // The ceiling is row SLOTS, not bookmarks, so it depends on how long
    // the bookmarks are — it used to be about a dozen of these and is now
    // about a hundred. Past it, whatever it is, the library says exactly
    // that. Stated against MAX_COMMIT_ROWS so the number moves with the
    // format instead of going quietly stale: one bookmark costs at least
    // one slot, so that many of them cannot fit alongside the counter.
    let mut s = Store::in_memory_with(65_536).unwrap();
    let too_many: Vec<NewBookmark> = (0..MAX_COMMIT_ROWS as u64)
        .map(|i| NewBookmark::new(&format!("https://example.com/{i}"), "T", &["x"], T0))
        .collect();
    let e = s.add_many(&too_many).unwrap_err();
    assert!(
        matches!(
            e,
            StoreError::Db(DbErr::BatchRejected { cause: ref c, .. })
                if matches!(**c, DbErr::BatchTooLong { .. })
        ),
        "{e:?}"
    );
    assert_eq!(s.count().unwrap(), 0, "and nothing in it was applied");
}

#[test]
fn a_bulk_delete_of_a_hundred_bookmarks_is_one_commit() {
    // One op per bookmark now instead of up to fifty rows, so a
    // "select all, delete" of a realistic selection cannot half-happen.
    let mut s = Store::in_memory_with(4096).unwrap();
    for i in 0..120u64 {
        s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]), T0)
            .unwrap();
    }
    let before = s.stats().slots;
    assert_eq!(s.remove_many(&(1..=100).collect::<Vec<_>>()).unwrap(), 100);
    assert_eq!(
        s.stats().slots - before,
        100,
        "one tombstone each, one commit"
    );
    assert_eq!(s.count().unwrap(), 20);

    // One tombstone more than a commit holds does not fit, and is refused
    // whole. That number is MAX_COMMIT_ROWS + 1 rather than a literal, so
    // the test keeps testing the boundary when the boundary moves.
    let over = MAX_COMMIT_ROWS as u64 + 1;
    let mut s = Store::in_memory_with(65_536).unwrap();
    for i in 0..over + 8 {
        s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]), T0)
            .unwrap();
    }
    let e = s.remove_many(&(1..=over).collect::<Vec<_>>()).unwrap_err();
    assert!(
        matches!(e, StoreError::Db(DbErr::BatchRejected { .. })),
        "{e:?}"
    );
    assert_eq!(s.count().unwrap(), over + 8);
}

#[test]
fn delete_inside_a_batch_is_strict_and_remove_is_not() {
    let mut db = MemDb::in_memory_with(64).unwrap();
    assert_eq!(
        db.batch(&[Op::delete(99)]).unwrap_err(),
        DbErr::BatchRejected {
            at: 0,
            cause: Box::new(DbErr::NotFound { id: 99 })
        },
        "Op::delete of an absent row kills the batch"
    );
    assert!(db.batch(&[Op::remove(99)]).is_ok());
    let mut s = seeded();
    assert_eq!(s.remove_many(&[2, 2, 99]).unwrap(), 1);
}

#[test]
fn renaming_a_tag_across_the_collection_is_still_not_atomic() {
    // LIMITATION, though a much narrower one than it was. A rename used to
    // cost one commit per bookmark; it then packed ~13 per commit; a
    // commit now holds MAX_COMMIT_ROWS slots, so a few hundred bookmarks
    // of this size rename in ONE commit and are atomic outright. That is
    // an improvement and not a fix: past a commit's worth of slots the
    // rename still needs more than one commit, a crash between them still
    // leaves it half-applied, and nothing in the library expresses "all of
    // these commits or none".
    let mut s = Store::in_memory_with(8192).unwrap();
    for i in 0..300u64 {
        s.add(
            &format!("https://example.com/{i}"),
            &format!("Example {i}"),
            &tags(&["db", "keep"]),
            T0 + i,
        )
        .unwrap();
    }
    assert_eq!(s.retag("db", "databases").unwrap(), 300);
    assert_eq!(s.by_tag("databases").unwrap().len(), 300);
    assert!(s.by_tag("db").unwrap().is_empty());

    // Prove the claim rather than assert it: a collection past the commit
    // bound cannot be renamed as one batch, and the refusal names the
    // number of slots it needed. One record of this length is three slots,
    // so MAX_COMMIT_ROWS of them is three commits' worth.
    let mut db = MemDb::in_memory_with(65_536).unwrap();
    let ops: Vec<Op> = (0..MAX_COMMIT_ROWS as u64)
        .map(|i| {
            Op::put(
                i,
                Value::from_text("a record of about forty bytes, like a bookmark").unwrap(),
            )
        })
        .collect();
    assert!(matches!(
        db.batch(&ops).unwrap_err(),
        DbErr::BatchRejected { cause, .. } if matches!(*cause, DbErr::BatchTooLong { .. })
    ));
}

// ---------------------------------------------------------------------------
// Search: what the index can and cannot answer
// ---------------------------------------------------------------------------

#[test]
fn find_now_matches_across_a_slot_boundary() {
    // FIXED. "org/en-US" sits at bytes 26..35 of the MDN URL. It used to
    // straddle two 16-byte VALUES, which the index structurally could not
    // see; it now straddles two slots of one value, and `find` matches it.
    let mut s = seeded();
    assert_eq!(
        s.db().find(b"org/en-US").unwrap().len(),
        1,
        "if this returns 0 the library lost cross-slot matching"
    );
    assert_eq!(ids(&s.find_exact("org/en-US").unwrap()), vec![3]);
    assert_eq!(ids(&s.search("org/en-US").unwrap()), vec![3]);
}

#[test]
fn the_index_takes_a_needle_as_long_as_the_record_it_searches() {
    // This used to be the LIMITATION that most limited a search box: a
    // value could be 2048 bytes and a needle 16, so asking the index for
    // a phrase was impossible and every long needle became a scan. The
    // ceilings are the same number now.
    let mut s = seeded();
    assert_eq!(MAX_NEEDLE, MAX_VALUE_LEN);
    let long = "developer.mozilla.org/en-US/docs";
    assert!(long.len() > VALUE_LEN, "longer than a row slot");
    assert_eq!(s.db().find_text(long).unwrap().len(), 1);
    assert_eq!(s.find_exact(long).unwrap().len(), 1);
    // Only a needle no value could contain is refused, and it says so as
    // a question about the NEEDLE rather than about a value.
    let absurd = "x".repeat(MAX_VALUE_LEN + 1);
    assert_eq!(
        s.db().find_text(&absurd),
        Err(DbErr::NeedleTooLong {
            len: absurd.len(),
            max: MAX_VALUE_LEN
        })
    );
    // And the scan agrees with the index, as it always did.
    assert_eq!(s.search(long).unwrap().len(), 1);
}

#[test]
fn the_index_is_byte_exact_so_a_search_box_still_has_to_scan() {
    // LIMITATION. `find` compares bytes. There is no collation, no case
    // folding, no normalisation, and no hook to supply one — so the
    // ordinary behaviour of every search box on earth is a full scan.
    let mut s = seeded();
    assert_eq!(s.find_exact("Rust standard").unwrap().len(), 1);
    assert_eq!(
        s.find_exact("rust standard").unwrap().len(),
        0,
        "if this becomes 1 the library grew case folding"
    );
    assert_eq!(s.search("rust standard").unwrap().len(), 1);
    assert_eq!(s.search("RUST STANDARD").unwrap().len(), 1);
}

#[test]
fn exact_tag_match_works_only_because_we_encoded_delimiters_ourselves() {
    // The library has no notion of a field, so "the tag is exactly rust"
    // is spelled as a substring of a record we deliberately wrote with
    // \x1e on both sides of every tag. It works, and it is a trick.
    let mut s = Store::in_memory().unwrap();
    s.add("https://a.test", "A", &tags(&["rust"]), T0).unwrap();
    s.add("https://b.test", "B", &tags(&["rustaceans"]), T0)
        .unwrap();

    assert_eq!(ids(&s.search_tag("rust").unwrap()), vec![1, 2], "substring");
    assert_eq!(ids(&s.by_tag("rust").unwrap()), vec![1], "equality");

    // The trick used to have a ceiling: the needle is \x1e + tag + \x1e,
    // and that had to fit ONE ROW, so any tag over fourteen characters
    // fell back to a scan. It does not any more — a needle may be as long
    // as the record it looks inside, so every tag is index-served.
    const _: () = assert!(MAX_INDEXED_TAG > MAX_TAG, "no tag can reach the ceiling");
    let long_tag = "a".repeat(MAX_TAG);
    let mut probe = vec![TS];
    probe.extend_from_slice(long_tag.as_bytes());
    probe.push(TS);
    assert!(
        s.db().find(&probe).is_ok(),
        "a delimited needle longer than a row is now a needle"
    );
    s.add("https://c.test", "C", &tags(&[long_tag.as_str()]), T0)
        .unwrap();
    assert_eq!(ids(&s.by_tag(&long_tag).unwrap()), vec![3]);
}

#[test]
fn there_is_no_prefix_search_and_no_way_to_anchor_one() {
    // LIMITATION. `find` is substring and only substring. "URLs on
    // example.com" is not expressible: the needle matches wherever it
    // occurs, and there is no anchor, no prefix mode, and no ordered index
    // over value bytes to range-scan instead.
    let mut s = Store::in_memory().unwrap();
    s.add("https://example.com/a", "A", &[], T0).unwrap();
    s.add("https://evil.test/?u=example.com", "B", &[], T0)
        .unwrap();
    assert_eq!(
        ids(&s.find_exact("example.com").unwrap()).len(),
        2,
        "the index cannot tell host from query string"
    );
    // Anchoring is only possible where the store put a delimiter of its
    // own — which is exactly the tag trick above, and nothing else.
    assert_eq!(s.db().find(b"://example.com").unwrap().len(), 1);
}

/// "The twenty newest" is a page of work now, not a scan and a sort.
///
/// Ids ascend with insertion, so the newest bookmarks are the highest
/// ones — and until the library could read its ordered index backwards,
/// reaching them meant walking every bookmark below them first.
/// `Store::query` with `Order::NewestFirst` still does exactly that
/// (it orders by the `added` timestamp, which is not the key); `newest`
/// does not.
#[test]
fn the_newest_bookmarks_are_a_page_of_work() {
    let mut s = Store::in_memory_with(8192).unwrap();
    let items: Vec<NewBookmark> = (0..300)
        .map(|i| NewBookmark::new(&format!("https://example.com/{i}"), "T", &["x"], T0 + i))
        .collect();
    s.import(&items).unwrap();

    for n in [0usize, 1, 8, 9, 20, 300, 400] {
        let got = s.newest(n).unwrap();
        assert_eq!(got.len(), n.min(300), "n={n}");
        let ids: Vec<u64> = got.iter().map(|b| b.id).collect();
        let want: Vec<u64> = (1..=300u64).rev().take(n.min(300)).collect();
        assert_eq!(ids, want, "n={n}");
    }

    // It agrees with the scan-and-sort it replaces.
    let sorted = s
        .query(&Query {
            order: Order::NewestFirst,
            ..Query::default()
        })
        .unwrap();
    let by_scan: Vec<u64> = sorted.iter().take(20).map(|b| b.id).collect();
    let by_index: Vec<u64> = s.newest(20).unwrap().iter().map(|b| b.id).collect();
    assert_eq!(by_index, by_scan);

    // And it does not trip over the header row when the store is small.
    let mut tiny = Store::in_memory().unwrap();
    assert!(tiny.newest(10).unwrap().is_empty());
    tiny.add("https://a.test", "A", &[], T0).unwrap();
    assert_eq!(tiny.newest(10).unwrap().len(), 1);
}

#[test]
fn find_orders_by_last_written_not_by_anything_a_user_can_see() {
    // TRAP. `find` returns matches newest-first, which reads like "most
    // recent". It is the order rows were WRITTEN, so editing an old
    // bookmark moves it to the front of every future search, and a
    // bookmark added later but never touched sinks below it.
    let mut s = Store::in_memory().unwrap();
    for i in 1..=5u64 {
        s.add(
            &format!("https://example.com/{i}"),
            "Example",
            &tags(&["find"]),
            T0 + i,
        )
        .unwrap();
    }
    let first = s.find_exact("Example").unwrap();
    assert_eq!(ids(&first), vec![5, 4, 3, 2, 1], "newest written first");

    s.set_title(1, "Example, revised").unwrap();
    let after = s.find_exact("Example").unwrap();
    assert_eq!(
        ids(&after),
        vec![1, 5, 4, 3, 2],
        "an edit re-dates a bookmark as far as the index is concerned"
    );
    // Ordering by something a user can see is a scan and a sort.
    let by_added = s
        .query(&Query {
            order: Order::NewestFirst,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(ids(&by_added), vec![5, 4, 3, 2, 1]);
}

#[test]
fn the_index_matches_bytes_the_user_never_typed_into_a_field() {
    // The record holds the timestamp and the visit counter as well as the
    // text, and `find` searches all of it, because there is no such thing
    // as a field. The store re-checks every hit; the index still paid to
    // produce them.
    let mut s = seeded();
    let raw = s.db().find(b"1700000010").unwrap();
    assert_eq!(raw.len(), 1, "matched a timestamp, not a title");
    assert!(
        s.find_exact("1700000010").unwrap().is_empty(),
        "the store throws it away"
    );
}

#[test]
fn the_index_cannot_be_combined_with_anything_else() {
    // LIMITATION. "bookmarks tagged `docs`, added in this window,
    // mentioning `rust`, newest first, at most ten" is one SQL statement.
    // Here it is a full scan with the whole predicate written in Rust:
    // `find` cannot be intersected with a range, a tag, an ordering, or a
    // limit.
    let mut s = seeded();
    s.visit(2).unwrap();
    s.visit(2).unwrap();
    s.visit(3).unwrap();

    let hits = s
        .query(&Query {
            text: Some("rust".into()),
            tags: tags(&["docs"]),
            since: Some(T0),
            until: Some(T0 + 5),
            order: Order::NewestFirst,
            limit: Some(10),
        })
        .unwrap();
    assert_eq!(ids(&hits), vec![1]);

    let visited = s
        .query(&Query {
            order: Order::MostVisited,
            limit: Some(2),
            ..Query::default()
        })
        .unwrap();
    assert_eq!(
        visited.iter().map(|b| (b.id, b.visits)).collect::<Vec<_>>(),
        vec![(2, 2), (3, 1)]
    );

    let by_title = s
        .query(&Query {
            order: Order::TitleAsc,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(
        ids(&by_title),
        vec![3, 1, 2],
        "IndexedDB API, Rust standard library, When to use SQLite"
    );
}

#[test]
fn a_limit_is_only_honoured_by_the_one_query_the_index_serves() {
    // `find_page` can stop early. Nothing else can: `range`, `all` and
    // therefore every ordered or compound query materialise the whole
    // table first and truncate afterwards.
    let mut s = Store::in_memory_with(4096).unwrap();
    for i in 0..60u64 {
        s.add(
            &format!("https://example.com/{i}"),
            "Example",
            &tags(&["gen"]),
            T0 + i,
        )
        .unwrap();
    }
    let (page, cursor) = s.search_page("Example", None, 10).unwrap();
    assert_eq!(page.len(), 10);
    assert!(cursor.is_some(), "and it says where to continue");
    let (next, _) = s.search_page("Example", cursor, 10).unwrap();
    assert_eq!(next.len(), 10);
    assert!(next[0].id < page[9].id, "strictly descending across pages");

    // The ordered query has no such thing; `limit` is a truncate.
    let ten = s
        .query(&Query {
            order: Order::NewestFirst,
            limit: Some(10),
            ..Query::default()
        })
        .unwrap();
    assert_eq!(ten.len(), 10);
    assert_eq!(ten[0].id, 60);
}

#[test]
fn paging_walks_forward_only_and_the_page_size_is_not_ours_to_choose() {
    let mut s = Store::in_memory_with(4096).unwrap();
    for i in 0..30u64 {
        s.add(
            &format!("https://example.com/{i}"),
            &format!("Example {i}"),
            &tags(&["gen"]),
            T0 + i,
        )
        .unwrap();
    }
    let first = s.page(0, 10).unwrap();
    assert_eq!(ids(&first), (1..=10).collect::<Vec<_>>());
    let second = s.page(first[9].id, 10).unwrap();
    assert_eq!(ids(&second), (11..=20).collect::<Vec<_>>());

    s.remove_many(&(11..=15).collect::<Vec<_>>()).unwrap();
    assert_eq!(ids(&s.page(10, 10).unwrap()), (16..=25).collect::<Vec<_>>());

    // LIMITATION: there is no backwards page and no "last page". `page`
    // only goes up, and the engine's own step is a fixed 8 rows the caller
    // does not choose.
    assert_eq!(s.page(25, 10).unwrap().len(), 5);
}

#[test]
fn there_is_no_way_to_ask_for_the_largest_key() {
    // LIMITATION, and the reason this crate keeps a header row at all.
    // Without it, "what id comes next" is a full scan: no descending
    // range, no max key, no last row.
    let mut s = seeded();
    let blob = s.to_blob().unwrap();
    let reopened = Store::load(&blob).unwrap();
    assert_eq!(reopened.next_id(), 4, "the header answers it in one get");

    // Delete the header and the store still gets the right answer — by
    // reading every row.
    let mut db = MemDb::load(&dabqlite::Snapshot::from_bytes(&blob).unwrap()).unwrap();
    db.remove(0).unwrap();
    let scanned = db.all().unwrap();
    let recomputed = scanned.iter().map(|(k, _)| *k).max().unwrap() + 1;
    assert_eq!(recomputed, 4);
    assert_eq!(scanned.len(), 3, "and it had to look at all of them");
}

// ---------------------------------------------------------------------------
// Updates, deletes, upkeep — what the new shape costs
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_bookmark_costs_exactly_one_row() {
    let mut s = seeded();
    let before = s.stats().slots;
    assert!(s.remove(2).unwrap());
    assert_eq!(s.stats().slots - before, 1);
    assert_eq!(s.get_bookmark(2).unwrap(), None);
    assert_eq!(ids(&s.list().unwrap()), vec![1, 3]);
    assert!(!s.remove(2).unwrap());
}

#[test]
fn updating_supersedes_the_whole_value_and_retires_the_old_one() {
    // No stale chunks to retire by hand any more: whatever the relative
    // lengths, the new value replaces the old one in one commit.
    let mut s = Store::in_memory().unwrap();
    s.add(&"https://example.com/".repeat(20), "T", &tags(&["a"]), T0)
        .unwrap();
    let long_slots = s.stats().slots;
    s.set_url(1, "https://x.test").unwrap();
    let b = s.get_bookmark(1).unwrap().unwrap();
    assert_eq!(b.url, "https://x.test");
    assert_eq!(b.tags, tags(&["a"]));
    assert_eq!(s.db().len(), 2, "one bookmark, one header");
    assert!(
        s.stats().slots > long_slots,
        "the old value is still on disk"
    );
    // `slots` and `dead` are in the same unit, so they subtract: the
    // superseded value's slots are all dead, not just its head.
    let st = s.stats();
    assert!(
        st.dead >= 20,
        "a twenty-slot value retired {} slots",
        st.dead
    );
    // Same unit, so they subtract: what is left is exactly the live rows.
    let live_slots: u64 = s
        .list()
        .unwrap()
        .iter()
        .map(|b| b.encode().len().div_ceil(VALUE_LEN).max(1) as u64)
        .sum();
    assert_eq!(st.slots - st.dead, live_slots + 1, "plus the header row");
}

#[test]
fn bumping_a_counter_rewrites_the_whole_bookmark() {
    // COST, and the one place the new shape is worse than the old one.
    // The visit counter shares a value with the URL, the title and the
    // tags, so `visit` rewrites every slot of it. The old design gave the
    // counter its own 16-byte row and cost exactly one slot per bump.
    // dabqlite has no partial update — no "write bytes 40..44 of this
    // value" — so the choice is one value and this amplification, or
    // several values and the reassembly this port just deleted.
    let mut s = seeded();
    let record = s.get_bookmark(1).unwrap().unwrap().encode().len();
    let expect = record.div_ceil(VALUE_LEN);
    assert!(expect >= 6, "a bookmark is {expect} slots");

    let before = s.stats().slots;
    assert_eq!(s.visit(1).unwrap(), 1);
    let cost = s.stats().slots - before;
    assert_eq!(cost as usize, expect, "one bump, {expect} slots");
    assert_eq!(s.get_bookmark(1).unwrap().unwrap().visits, 1);
}

#[test]
fn dead_slots_pile_up_until_something_compacts() {
    let mut s = Store::in_memory_with(4096).unwrap();
    s.add(RUST, "Rust standard library", &tags(&["rust"]), T0)
        .unwrap();
    for i in 0..40 {
        s.set_title(1, &format!("title {i}")).unwrap();
    }
    let dirty = s.stats();
    assert!(
        dirty.dead >= 40 * 4,
        "forty superseded multi-slot records left {} dead slots",
        dirty.dead
    );
    let before = s.list().unwrap();

    s.compact().unwrap();
    assert_eq!(s.stats().dead, 0);
    assert!(s.stats().slots < dirty.slots);
    assert_eq!(s.list().unwrap(), before);
    assert_eq!(
        s.add(MDN, "MDN", &[], T0).unwrap(),
        2,
        "the counter survived"
    );
}

#[test]
fn dead_reports_the_slots_a_rebuild_will_actually_return() {
    // This test used to be a FINDING. `Stats::dead` counted retired
    // RECORDS while `slots` and `capacity` counted row SLOTS, so a store
    // whose bookmarks span five slots each under-reported reclaimable
    // space five-fold — and the number an application watches to decide
    // when to compact was the one in the wrong unit. The library fixed
    // it; this now pins the agreement instead of the discrepancy.
    let mut s = Store::in_memory_with(4096).unwrap();
    s.add(RUST, "Rust standard library", &tags(&["rust"]), T0)
        .unwrap();
    let per_record = s.get_bookmark(1).unwrap().unwrap().encode().len() / VALUE_LEN + 1;
    assert!(per_record >= 5, "a bookmark is {per_record} slots");

    let clean = s.stats().slots;
    for i in 0..40 {
        s.set_title(1, &format!("Rust standard library {i}"))
            .unwrap();
    }
    let dirty = s.stats();
    let wasted = dirty.slots - clean;
    assert!(wasted > 40 * 4, "forty edits of a multi-slot bookmark");
    assert_eq!(
        dirty.dead, wasted,
        "dead weight is quoted in the same unit as capacity"
    );

    s.compact().unwrap();
    let reclaimed = dirty.slots - s.stats().slots;
    assert_eq!(
        reclaimed, wasted,
        "a rebuild returns exactly what `dead` promised"
    );
    assert_eq!(s.stats().dead, 0);
}

#[test]
fn the_id_counter_costs_one_dead_row_per_commit_not_per_bookmark() {
    // It used to be one per insert, because an insert was one commit.
    // `import` packs, so the overhead is amortised across the pack.
    let mut s = Store::in_memory_with(8192).unwrap();
    let items: Vec<NewBookmark> = (0..100)
        .map(|i| NewBookmark::new(&format!("https://example.com/{i}"), "T", &["x"], T0))
        .collect();
    s.import(&items).unwrap();
    assert_eq!(s.count().unwrap(), 100);
    assert!(
        s.stats().dead < 30,
        "100 bookmarks left {} dead header rows",
        s.stats().dead
    );
}

#[test]
fn a_value_that_is_not_a_record_reads_as_malformed_not_as_a_wrong_bookmark() {
    // The library will hand back whatever bytes are at a key. Deciding
    // they are not a bookmark is ours, and the store says so rather than
    // inventing fields.
    let mut s = seeded();
    s.db().put(2, Value::from_text("garbage").unwrap()).unwrap();
    assert_eq!(
        s.get_bookmark(2).unwrap_err(),
        StoreError::Malformed { id: 2 }
    );
    assert_eq!(
        s.get_bookmark(1).unwrap().unwrap().id,
        1,
        "and only that one"
    );
}

// ---------------------------------------------------------------------------
// Backends, capacity, durability
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_now_carries_the_capacity_it_was_written_with() {
    // FIXED. It used to not, so `Store::load` had to guess, be told
    // `CapacityTooSmall`, and load again with the number the error
    // carried. That dance is gone.
    let mut s = Store::in_memory_with(512).unwrap();
    s.add(RUST, "Rust standard library", &tags(&["rust"]), T0)
        .unwrap();
    assert_eq!(s.stats().capacity, 512);
    let blob = s.to_blob().unwrap();

    let reloaded = Store::load(&blob).unwrap();
    assert_eq!(reloaded.stats().capacity, 512);
    assert_ne!(reloaded.stats().capacity, dabqlite::DEFAULT_ROWS);
}

#[test]
fn asking_for_less_room_than_the_data_says_exactly_how_much_it_needs() {
    let mut s = Store::in_memory_with(4096).unwrap();
    for i in 0..20 {
        s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]), T0)
            .unwrap();
    }
    let rows = s.stats().slots;
    let blob = s.to_blob().unwrap();
    let snap = dabqlite::Snapshot::from_bytes(&blob).unwrap();
    assert_eq!(
        MemDb::load_with(&snap, 10).err(),
        Some(DbErr::CapacityTooSmall {
            required: rows,
            asked: 10
        })
    );
    // And the plain `load` no longer needs telling at all.
    assert_eq!(Store::load(&blob).unwrap().stats().capacity, 4096);
}

#[test]
fn a_full_database_refuses_rather_than_corrupting_anything() {
    let mut s = Store::in_memory_with(64).unwrap();
    let mut added = 0;
    let err = loop {
        match s.add(
            &format!("https://example.com/{added}"),
            "T",
            &tags(&["x"]),
            T0,
        ) {
            Ok(_) => added += 1,
            Err(e) => break e,
        }
    };
    assert!(
        matches!(
            err,
            StoreError::Db(DbErr::BatchRejected { cause: ref c, .. })
                if matches!(**c, DbErr::Full { capacity: 64, .. })
        ),
        "{err:?}"
    );
    assert_eq!(s.count().unwrap(), added);
    assert_eq!(s.list().unwrap().len() as u64, added);
}

#[test]
fn a_damaged_blob_is_refused_instead_of_guessed_at() {
    let mut s = seeded();
    let blob = s.to_blob().unwrap();
    assert!(matches!(
        Store::load(b"this is not a snapshot at all"),
        Err(StoreError::Db(DbErr::Corrupt { .. }))
    ));
    let mut truncated = blob.clone();
    truncated.truncate(blob.len() - 1);
    assert!(matches!(
        Store::load(&truncated),
        Err(StoreError::Db(DbErr::Corrupt { .. }))
    ));
    assert_eq!(Store::load(&blob).unwrap().count().unwrap(), 3);
}

#[test]
fn opening_a_database_that_lost_nothing_reports_no_rollback() {
    // FIXED. Recovery drops residue past the manifest, so a clean
    // close-and-reopen no longer raises the alarm that means "an
    // acknowledged write was lost".
    let dir = std::env::temp_dir().join(format!("bookmarks-recov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut s: Store<dabqlite::PosixStorage> = Store::open_with(&dir, 4096).unwrap();
        for i in 0..20u64 {
            s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]), T0)
                .unwrap();
        }
        s.set_title(3, "edited").unwrap();
        s.remove(4).unwrap();
    }
    for _ in 0..3 {
        let s: Store<dabqlite::PosixStorage> = Store::open(&dir).unwrap();
        let report = s.recovery_report();
        assert!(
            !report.rollback_evidence,
            "a database that lost nothing claimed rollback: {report:?}"
        );
        assert_eq!(report.declared_capacity, 4096, "capacity is remembered");
    }
    let mut s: Store<dabqlite::PosixStorage> = Store::open(&dir).unwrap();
    assert_eq!(s.count().unwrap(), 19);
    assert_eq!(s.get_bookmark(3).unwrap().unwrap().title, "edited");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn the_file_backend_is_the_same_store_and_locks_against_a_second_writer() {
    let dir = std::env::temp_dir().join(format!("bookmarks-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut s: Store<dabqlite::PosixStorage> = Store::open_with(&dir, 4096).unwrap();
    s.add(RUST, "Rust standard library", &tags(&["rust"]), T0)
        .unwrap();

    match Store::<dabqlite::PosixStorage>::open(&dir) {
        Err(StoreError::Db(DbErr::Locked { detail })) => {
            assert!(detail.contains("locked"), "{detail}");
        }
        other => panic!("expected Locked, got {:?}", other.map(|_| ())),
    }
    drop(s);

    // Reopen without naming a capacity — it is remembered — mutate, and
    // compact in place. `compact` takes `&mut self` now, so the store is
    // not consumed and rebuilt around it.
    let mut s = Store::<dabqlite::PosixStorage>::open(&dir).unwrap();
    assert_eq!(s.stats().capacity, 4096);
    for i in 0..10 {
        s.set_title(1, &format!("title {i}")).unwrap();
    }
    assert!(s.stats().dead > 0);
    let before = s.list().unwrap();
    s.compact().unwrap();
    assert_eq!(s.stats().dead, 0);
    assert_eq!(s.list().unwrap(), before);
    assert_eq!(
        s.add(MDN, "MDN", &[], T0).unwrap(),
        2,
        "the id counter survived"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn the_writer_lock_does_not_leak_into_a_process_spawned_while_it_is_held() {
    // FIXED, and this is the shape of the bug: a child forked while the
    // lock was held inherited the descriptor, so the lock outlived the
    // parent's `Db` and the next open failed with `Locked` for no visible
    // reason. `sleep` here stands in for any subprocess an application
    // starts — a helper, an editor, a browser.
    let dir = std::env::temp_dir().join(format!("bookmarks-fork-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut child = {
        let mut s: Store<dabqlite::PosixStorage> = Store::open_with(&dir, 4096).unwrap();
        s.add(RUST, "Rust", &tags(&["rust"]), T0).unwrap();
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        drop(s);
        child
    };

    let reopened = Store::<dabqlite::PosixStorage>::open(&dir);
    let outcome = match reopened {
        Ok(mut s) => Ok(s.count().unwrap()),
        Err(e) => Err(e),
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        outcome,
        Ok(1),
        "the lock outlived the Db that took it, in a child that never touched the database"
    );
}

#[test]
fn a_snapshot_written_by_a_previous_process_reloads_intact() {
    let dir = std::env::temp_dir().join(format!("bookmarks-prev-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("prev.dabq");
    let bin = env!("CARGO_BIN_EXE_bookmarks");

    let run = |args: &[&str]| -> String {
        let out = std::process::Command::new(bin)
            .arg("--db")
            .arg(&db)
            .args(args)
            .output()
            .expect("spawn");
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8")
    };

    assert_eq!(
        run(&["add", RUST, "Rust standard library", "rust", "docs"]).trim(),
        "added 1"
    );
    assert_eq!(
        run(&["add", MDN, "IndexedDB API", "browser", "db"]).trim(),
        "added 2"
    );
    run(&["title", "2", "IndexedDB, revisited"]);
    run(&["rm", "1"]);
    run(&["add", SQLITE, "When to use SQLite", "sqlite", "db"]);

    let blob = std::fs::read(&db).expect("the previous run's snapshot");
    let mut s = Store::load(&blob).expect("load a foreign snapshot");
    let all = s.list().unwrap();
    assert_eq!(
        all.iter()
            .map(|b| (b.id, b.title.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "IndexedDB, revisited"), (3, "When to use SQLite")]
    );
    assert_eq!(all[0].url, MDN);
    assert_eq!(all[1].tags, tags(&["db", "sqlite"]));

    let listed = run(&["list"]);
    assert!(listed.contains("IndexedDB, revisited"), "{listed}");
    assert!(!listed.contains("Rust standard"), "{listed}");
    assert!(run(&["bytag", "db"]).contains("(2 bookmarks)"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_record_shaped_limits_are_reported_not_silently_applied() {
    let mut s = Store::in_memory().unwrap();
    // A tag may be four rows long now, not one.
    assert_eq!(MAX_TAG, 64);
    s.add(RUST, "T", &tags(&["a-tag-far-longer-than-a-row"]), T0)
        .unwrap();
    assert_eq!(
        s.get_bookmark(1).unwrap().unwrap().tags,
        tags(&["a-tag-far-longer-than-a-row"])
    );
    assert_eq!(
        s.add(RUST, "T", &tags(&[&"x".repeat(MAX_TAG + 1)]), T0)
            .unwrap_err(),
        StoreError::TagTooLong {
            tag: "x".repeat(MAX_TAG + 1),
            max: MAX_TAG
        }
    );

    let many: Vec<String> = (0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect();
    assert_eq!(
        s.add(RUST, "T", &many, T0).unwrap_err(),
        StoreError::TooManyTags {
            got: MAX_TAGS + 1,
            max: MAX_TAGS
        }
    );

    // And the format's own byte is refused rather than silently corrupting
    // a record — a hazard this crate owns because the library has no
    // fields.
    assert_eq!(
        s.add("https://e.test/\u{1f}x", "T", &[], T0).unwrap_err(),
        StoreError::SeparatorInText { field: "url" }
    );
}

#[cfg(unix)]
#[test]
fn contention_is_refused_honestly_and_never_corrupts_anything() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("bookmarks-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    drop(Store::<dabqlite::PosixStorage>::open_with(&dir, 50_000).unwrap());

    let wrote = Arc::new(AtomicU64::new(0));
    let unexpected = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let dir = dir.clone();
        let (wrote, unexpected) = (wrote.clone(), unexpected.clone());
        handles.push(std::thread::spawn(move || {
            for i in 0..40 {
                match Store::<dabqlite::PosixStorage>::open(&dir) {
                    Ok(mut s) => {
                        s.add(&format!("https://t{t}.test/{i}"), "x", &tags(&["t"]), T0)
                            .unwrap();
                        wrote.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(StoreError::Db(DbErr::Locked { .. })) => {}
                    Err(_) => {
                        unexpected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        unexpected.load(Ordering::Relaxed),
        0,
        "contention produced an error that was not `Locked`"
    );
    let mut s = Store::<dabqlite::PosixStorage>::open(&dir).unwrap();
    assert_eq!(
        s.count().unwrap(),
        wrote.load(Ordering::Relaxed),
        "the database disagrees with the writers about what landed"
    );
    assert_eq!(s.list().unwrap().len() as u64, s.count().unwrap());
    // Note what is NOT here: any retry or backoff from the library. A loser
    // gets `Locked` and is on its own.
    let _ = std::fs::remove_dir_all(&dir);
}
