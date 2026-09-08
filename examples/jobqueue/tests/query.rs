//! Can a queue ask "give me the next PENDING job" without scanning?
//!
//! This is the one query a job queue actually needs, and the reason the
//! worker in `src/lib.rs` still finds its head positionally. These are the
//! things I tried, re-tried against the reworked `find` — and then
//! against the ordered index over VALUE bytes, which is the one that
//! actually answers it.

use dabqlite::{Db, Error, Op, Value, MAX_VALUE_LEN, VALUE_LEN};
use jobqueue::{Job, CLAIMED, PENDING};

fn job(state: u8, payload: Vec<u8>) -> Value {
    Job::new(state, 0, payload).encode().unwrap()
}

/// `Db::find` matches a byte substring of the value. The queue's state
/// lives in byte 0 of the row, so the obvious query — "rows whose state
/// byte is PENDING" — still cannot be expressed: `find` has no notion of
/// an offset, and a one-byte needle matches that byte ANYWHERE in the row.
///
/// Variable-length values make this strictly worse: a job now carries a
/// real payload, so there are hundreds of bytes for a one-byte needle to
/// collide with instead of eight.
#[test]
fn find_cannot_express_a_field_equality() {
    let mut db = Db::in_memory_with(4096).unwrap();
    // A pending job whose payload happens to contain the CLAIMED byte.
    db.insert(1, job(PENDING, vec![0x00, 0x02, 0xFF, 0x02]))
        .unwrap();
    // A genuinely claimed job.
    db.insert(2, job(CLAIMED, vec![0xFF; 200])).unwrap();

    let hits = db.find(&[CLAIMED]).unwrap();
    let ids: Vec<u64> = hits.iter().map(|(id, _)| *id).collect();
    assert!(
        ids.contains(&1),
        "the PENDING job matched a search for CLAIMED because its payload \
         contains the byte 0x02 — `find` is substring search, not a field \
         predicate: {ids:?}"
    );
}

/// With the state widened to a distinctive multi-byte tag, `find` DOES
/// answer the question. This is the closest thing to a "where state = ?"
/// index, and the cost is the tag bytes plus the risk above.
///
/// The result order is NEWEST FIRST, and for a FIFO queue that is the
/// wrong end — the head of the queue is the OLDEST match — so a queue
/// built on this has to reverse the whole result, materialising all of
/// it, which is exactly what `find_page` exists to avoid.
///
/// That is not a defect in `find` to be fixed; it is `find` being asked
/// to be an ordered secondary index, which it is not. See
/// `the_value_ordered_index_is_the_state_index_find_was_never_going_to_be`
/// below for the query this test wanted, answered properly.
#[test]
fn a_multi_byte_state_tag_is_the_closest_thing_to_an_index_and_it_is_newest_first() {
    let mut db = Db::in_memory_with(65_536).unwrap();
    let tag = |state: u8, id: u64| {
        let mut b = vec![0xE7, 0xE7, state];
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&[0x11; 40]);
        Value::from_vec(b).unwrap()
    };
    for id in 1..=40u64 {
        let state = if id % 3 == 0 { CLAIMED } else { PENDING };
        db.insert(id, tag(state, id)).unwrap();
    }

    let pending = db.find(&[0xE7, 0xE7, PENDING]).unwrap();
    let ids: Vec<u64> = pending.iter().map(|(id, _)| *id).collect();
    let mut expected: Vec<u64> = (1..=40u64).filter(|id| id % 3 != 0).collect();
    expected.reverse();
    assert_eq!(ids, expected, "exact, and newest first");
    assert_eq!(pending.len(), 27);

    // `find_page` does stop early, which is the real improvement: the head
    // of a search box costs one page, not the whole match set.
    let (page, cursor) = db.find_page(&[0xE7, 0xE7, PENDING], None).unwrap();
    assert!(!page.is_empty());
    assert!(page.len() <= 27);
    assert_eq!(page[0].0, 40, "the newest match first");
    // Paging reaches every match and repeats none.
    let mut seen = page.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let mut cursor = cursor;
    while let Some(c) = cursor {
        let (page, next) = db.find_page(&[0xE7, 0xE7, PENDING], Some(c)).unwrap();
        seen.extend(page.iter().map(|(id, _)| *id));
        cursor = next;
    }
    assert_eq!(seen, expected, "paging is the same sequence as find()");
}

/// The claim that a match straddling a slot boundary is still found. It
/// holds — checked at every offset across the first boundary of a
/// multi-slot value.
#[test]
fn find_matches_across_the_boundary_between_row_slots() {
    let mut db = Db::in_memory_with(65_536).unwrap();
    let needle: [u8; 8] = *b"\xC0FFEE\xC0\xDE!";
    for (i, at) in (VALUE_LEN - 7..VALUE_LEN + 8).enumerate() {
        let mut payload = vec![0x2Au8; 400];
        payload[at..at + needle.len()].copy_from_slice(&needle);
        db.insert(i as u64, Value::from_vec(payload).unwrap())
            .unwrap();
    }
    let hits = db.find(&needle).unwrap();
    assert_eq!(
        hits.len(),
        15,
        "a needle straddling the 16-byte slot boundary was missed: found {:?}",
        hits.iter().map(|(id, _)| *id).collect::<Vec<_>>()
    );
    // ...and it does not invent matches out of the seam between two rows.
    let mut db = Db::in_memory_with(4096).unwrap();
    db.batch(&[
        Op::put(1, Value::from_vec(vec![0xAA; 16]).unwrap()),
        Op::put(2, Value::from_vec(vec![0xBB; 16]).unwrap()),
    ])
    .unwrap();
    assert!(
        db.find(&[0xAA, 0xAA, 0xBB, 0xBB]).unwrap().is_empty(),
        "a needle spanning TWO DIFFERENT ROWS matched; the seam is not a value"
    );
}

/// A needle may be as long as a value, so `find` takes the things a
/// caller actually wants to look for.
///
/// This used to be the sharpest remaining edge: a value could be 2048
/// bytes and a needle 16, so a URL, a 32-byte hash or a UUID string —
/// perfectly ordinary values in this store — could not be handed to the
/// one search primitive it has. The workaround was a 16-byte prefix
/// search plus a hand-written re-scan of the candidates. Both ceilings
/// are the same number now, and an over-long needle says so as a question
/// about the NEEDLE rather than borrowing the value's error.
#[test]
fn a_needle_may_be_as_long_as_a_value() {
    let mut db = Db::in_memory_with(4096).unwrap();
    let hash = vec![0x5Au8; 32];
    let mut payload = vec![0u8; 100];
    payload[10..42].copy_from_slice(&hash);
    db.insert(1, Value::from_vec(payload).unwrap()).unwrap();

    assert_eq!(db.find(&hash[..VALUE_LEN]).unwrap().len(), 1);
    assert_eq!(
        db.find(&hash).unwrap().len(),
        1,
        "the whole 32-byte hash, straddling two slots"
    );

    // Only a needle no value could contain is refused.
    let absurd = vec![0x5Au8; MAX_VALUE_LEN + 1];
    assert_eq!(
        db.find(&absurd).unwrap_err(),
        Error::NeedleTooLong {
            len: MAX_VALUE_LEN + 1,
            max: MAX_VALUE_LEN
        }
    );

    // And the search can be anchored, so a payload that merely mentions
    // the hash is distinguishable from one that IS it.
    db.insert(2, Value::from_vec(hash.clone()).unwrap())
        .unwrap();
    assert_eq!(db.find(&hash).unwrap().len(), 2, "both contain it");
    let exact: Vec<u64> = db
        .find_exact(&hash)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(exact, vec![2], "only one IS it");
}

/// **The query this file was written to look for, answered.**
///
/// Every attempt above bends `find` into a secondary index and every one
/// of them fails in a different way: a one-byte needle matches the byte
/// anywhere, a distinctive tag costs bytes and still collides in
/// principle, and the order is newest-first when a queue wants oldest.
/// They fail because substring search is not an ordered index and no
/// amount of care makes it one.
///
/// The ordered index over VALUE bytes IS one. Put the state first and the
/// id right behind it, and "the oldest PENDING job" is a prefix scan that
/// reads ONE page: exact (no collisions — a prefix is anchored, unlike a
/// substring), ordered (FIFO within the state, because the id follows the
/// state), and bounded (a page, not the match set).
#[test]
fn the_value_ordered_index_is_the_state_index_find_was_never_going_to_be() {
    // [state | id, big-endian | payload]. Big-endian because byte order
    // IS the order: the index compares bytes, so a little-endian id would
    // sort by its least significant byte first and FIFO would be noise.
    let row = |state: u8, id: u64, payload: &[u8]| {
        let mut b = vec![state];
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(payload);
        Value::from_vec(b).unwrap()
    };
    let mut db = Db::in_memory_with(65_536).unwrap();
    for id in 1..=40u64 {
        // Payloads that would collide with a one-byte state needle, so
        // this is the same trap `find_cannot_express_a_field_equality`
        // walks into.
        let payload = vec![if id % 2 == 0 { PENDING } else { CLAIMED }; 30];
        let state = if id % 3 == 0 { CLAIMED } else { PENDING };
        db.insert(id, row(state, id, &payload)).unwrap();
    }

    // One page, and it is the HEAD of the queue.
    let (page, _) = db.prefix_page(&[PENDING], None).unwrap();
    assert!(!page.is_empty());
    let head = page[0].1.as_bytes();
    assert_eq!(head[0], PENDING);
    assert_eq!(
        u64::from_be_bytes(head[1..9].try_into().unwrap()),
        1,
        "the oldest pending job, first, from one page"
    );

    // The whole set, in FIFO order, and exact: no payload byte collides
    // its way in, because a prefix is anchored where a substring is not.
    let pending: Vec<u64> = db
        .prefix(&[PENDING])
        .unwrap()
        .into_iter()
        .map(|(_, v)| u64::from_be_bytes(v.as_bytes()[1..9].try_into().unwrap()))
        .collect();
    let expected: Vec<u64> = (1..=40u64).filter(|id| id % 3 != 0).collect();
    assert_eq!(pending, expected, "every pending job, oldest first");

    // Which `find` cannot do at either end: it matches payload bytes...
    let found = db.find(&[CLAIMED]).unwrap();
    assert!(
        found.len() > 40 - expected.len(),
        "find matched payload bytes as well as state bytes: {} hits",
        found.len()
    );
    // ...and hands back the newest first when the queue wants the oldest.
    let anchored = db.find_prefix(&[PENDING]).unwrap();
    assert_eq!(
        anchored.first().map(|(id, _)| *id),
        Some(40),
        "even anchored, find is newest-first"
    );

    // Claiming the head moves it out of the pending prefix and into the
    // claimed one, and the next call returns the next job — no scan, no
    // reversal, no materialised match set.
    let (id, _) = (1u64, ());
    let payload = vec![PENDING; 30];
    db.put(id, row(CLAIMED, id, &payload)).unwrap();
    let (page, _) = db.prefix_page(&[PENDING], None).unwrap();
    assert_eq!(
        u64::from_be_bytes(page[0].1.as_bytes()[1..9].try_into().unwrap()),
        2,
        "the queue advanced"
    );
    // And the page is a PAGE: bounded, whatever the match set costs.
    assert!(page.len() <= dabqlite::RANGE_PAGE);
}
