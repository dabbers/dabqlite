//! Can a queue ask "give me the next PENDING job" without scanning?
//!
//! This is the one query a job queue actually needs, and the reason the
//! worker in `src/lib.rs` still finds its head positionally. These are the
//! things I tried, re-tried against the reworked `find`.

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
/// The result order changed: `find` is now documented NEWEST FIRST, and it
/// is. For a FIFO queue that is the wrong end — the head of the queue is
/// the oldest match — so the queue has to reverse the whole result, which
/// means materialising all of it, which is exactly what `find_page` exists
/// to avoid.
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
