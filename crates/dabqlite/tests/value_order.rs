//! The ordered index over VALUE bytes, held to a `BTreeMap<Vec<u8>, u64>`
//! oracle (docs/DESIGN.md §10, "keys that are not `u64`").
//!
//! This is the answer to the gap all three sample applications named:
//! their real key is a byte string, the library's key is a `u64`, so each
//! of them hashes, probes past collisions, and answers "list everything
//! under `session/`" with a full scan and a sort. Put the key at the
//! front of the record and these scans answer it off an index.
//!
//! Every test here compares against `BTreeMap`, which is the definition
//! of the order being claimed — not a second implementation of it.

use std::collections::{BTreeMap, BTreeSet};

use dabqlite::{Db, Op, Value, VALUE_LEN};

type Mem = Db<dabqlite::MemoryStorage>;

fn db(rows: u64) -> Mem {
    Db::in_memory_with(rows).expect("open")
}

/// Deterministic pseudo-random bytes; no dependency, no clock.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 ^ (self.0 >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// The order this index claims, spelled out as a `BTreeSet` of
/// `(value, id)`: bytes first, id to break a tie. An empty `hi` is "no
/// upper bound".
fn oracle_range(oracle: &BTreeSet<(Vec<u8>, u64)>, lo: &[u8], hi: &[u8]) -> Vec<(u64, Vec<u8>)> {
    oracle
        .iter()
        .filter(|(k, _)| k.as_slice() >= lo && (hi.is_empty() || k.as_slice() <= hi))
        .map(|(k, id)| (*id, k.clone()))
        .collect()
}

/// The oracle for an `id -> value` world: every live row, in the claimed
/// order.
fn oracle_of(live: &BTreeMap<u64, Vec<u8>>) -> BTreeSet<(Vec<u8>, u64)> {
    live.iter().map(|(&id, v)| (v.clone(), id)).collect()
}

fn got(rows: Vec<(u64, Value)>) -> Vec<(u64, Vec<u8>)> {
    rows.into_iter()
        .map(|(id, v)| (id, v.as_bytes().to_vec()))
        .collect()
}

/// The headline: a scan in value order is exactly the oracle's order,
/// over a workload of inserts, updates and deletes, at every bound.
#[test]
fn a_value_ordered_scan_is_the_btreemap_order_after_any_workload() {
    for seed in 0..8u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9) | 1);
        let mut db = db(4096);
        // id -> the value it currently holds. The claimed order is
        // derived from it, so the oracle cannot drift from the workload.
        let mut live: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

        for round in 0..200u64 {
            let id = rng.below(40);
            match rng.below(10) {
                0..=1 if live.contains_key(&id) => {
                    live.remove(&id);
                    db.remove(id).expect("remove");
                }
                _ => {
                    // Values that share prefixes, cross slot seams, and
                    // sometimes collide exactly.
                    let len = rng.below(40) as usize;
                    let head = (b'a' + rng.below(4) as u8) as char;
                    let mut v = format!("{head}{:02}/", rng.below(6)).into_bytes();
                    v.extend((0..len).map(|i| b'0' + ((round + i as u64) % 10) as u8));
                    live.insert(id, v.clone());
                    db.put(id, Value::from_vec(v).unwrap()).expect("put");
                }
            }
            let oracle = oracle_of(&live);

            if round % 17 != 0 {
                continue;
            }
            // Full scan.
            assert_eq!(
                got(db.range_by_value(b"", b"").unwrap()),
                oracle_range(&oracle, b"", b""),
                "seed {seed} round {round}: full value scan"
            );
            // Descending is the same list, reversed.
            let mut rev = oracle_range(&oracle, b"", b"");
            rev.reverse();
            assert_eq!(
                got(db.range_by_value_rev(b"", b"").unwrap()),
                rev,
                "seed {seed} round {round}: descending value scan"
            );
            // Every bound worth trying, including ones between keys.
            for lo in [
                &b""[..],
                b"a",
                b"a00/",
                b"a00/5",
                b"b",
                b"c99",
                b"d",
                b"zzz",
            ] {
                for hi in [&b""[..], b"a", b"a00/", b"b99/", b"c", b"zzz"] {
                    if !hi.is_empty() && lo > hi {
                        assert!(
                            db.range_by_value(lo, hi).unwrap().is_empty(),
                            "inverted bounds must be empty, not an error"
                        );
                        continue;
                    }
                    assert_eq!(
                        got(db.range_by_value(lo, hi).unwrap()),
                        oracle_range(&oracle, lo, hi),
                        "seed {seed} round {round}: [{:?}, {:?}]",
                        String::from_utf8_lossy(lo),
                        String::from_utf8_lossy(hi)
                    );
                }
            }
        }
    }
}

/// The query the samples actually wanted: everything under a prefix, in
/// order, without a scan and a sort.
#[test]
fn a_prefix_scan_returns_exactly_the_rows_under_it() {
    let mut db = db(1024);
    let keys: Vec<&[u8]> = vec![
        b"session/",
        b"session/aaa",
        b"session/zzz",
        b"session0",
        b"sessio",
        b"session",
        b"user/1",
        b"",
        b"\xff\xff",
        b"\xff\xfe",
    ];
    for (i, k) in keys.iter().enumerate() {
        db.put(i as u64, Value::from_bytes(k).unwrap()).unwrap();
    }
    let oracle: BTreeSet<(Vec<u8>, u64)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.to_vec(), i as u64))
        .collect();
    for prefix in [
        &b"session/"[..],
        b"session",
        b"user/",
        b"",
        b"\xff",
        b"\xff\xff",
        b"nothing",
    ] {
        let expect: Vec<(u64, Vec<u8>)> = oracle
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, id)| (*id, k.clone()))
            .collect();
        assert_eq!(
            got(db.prefix(prefix).unwrap()),
            expect,
            "prefix {:?}",
            String::from_utf8_lossy(prefix)
        );
    }
}

/// A prefix that is all `0xff` has no upper bound at all, and the naive
/// "append 0xff" version of this bound gets it wrong. Pinned because the
/// failure is silent: rows simply go missing.
#[test]
fn the_prefix_bound_is_right_at_the_top_of_the_byte_range() {
    let mut db = db(256);
    let keys: Vec<Vec<u8>> = vec![
        vec![0xfe],
        vec![0xff],
        vec![0xff, 0x00],
        vec![0xff, 0xff],
        vec![0xff, 0xff, 0xff],
        vec![0xff, 0xff, 0x01],
    ];
    for (i, k) in keys.iter().enumerate() {
        db.put(i as u64, Value::from_vec(k.clone()).unwrap())
            .unwrap();
    }
    let under_ff = got(db.prefix(&[0xff]).unwrap());
    assert_eq!(under_ff.len(), 5, "{under_ff:?}");
    assert!(under_ff.iter().all(|(_, v)| v[0] == 0xff));
    let under_ffff = got(db.prefix(&[0xff, 0xff]).unwrap());
    assert_eq!(under_ffff.len(), 3, "{under_ffff:?}");
}

/// Values longer than one slot order by their WHOLE bytes, including the
/// parts past the first slot. A comparison that stopped at the slot seam
/// would sort these three identically and nothing else would notice.
#[test]
fn long_values_order_by_every_byte_not_just_the_first_slot() {
    let mut db = db(1024);
    let base = vec![b'x'; VALUE_LEN * 3];
    let mut a = base.clone();
    let mut b = base.clone();
    let mut c = base.clone();
    a[VALUE_LEN * 2 + 5] = b'a';
    b[VALUE_LEN * 2 + 5] = b'b';
    c[VALUE_LEN + 1] = b'c';
    for (id, v) in [(1u64, &a), (2, &b), (3, &c)] {
        db.put(id, Value::from_vec(v.clone()).unwrap()).unwrap();
    }
    let oracle: BTreeSet<(Vec<u8>, u64)> =
        [(a, 1u64), (b, 2), (c.clone(), 3)].into_iter().collect();
    let expect: Vec<(u64, Vec<u8>)> = oracle.iter().map(|(k, id)| (*id, k.clone())).collect();
    assert_eq!(got(db.range_by_value(b"", b"").unwrap()), expect);
    // And a prefix reaching PAST the seam c differs at selects exactly
    // the two values that still agree with it there.
    assert_eq!(c[VALUE_LEN + 1], b'c', "c differs just past the seam");
    let long_prefix = vec![b'x'; VALUE_LEN + 2];
    assert_eq!(db.prefix(&long_prefix).unwrap().len(), 2, "only a and b");
}

/// Two records holding IDENTICAL bytes are two entries in an index whose
/// key is those bytes. The tie-break has to keep them distinct, or the
/// second one is either rejected as a duplicate or silently replaces the
/// first.
#[test]
fn identical_values_under_different_ids_both_come_back() {
    let mut db = db(256);
    for id in 0..5u64 {
        db.put(id, Value::from_bytes(b"same").unwrap()).unwrap();
    }
    let rows = got(db.range_by_value(b"same", b"same").unwrap());
    assert_eq!(rows.len(), 5, "{rows:?}");
    let mut ids: Vec<u64> = rows.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    assert!(rows.iter().all(|(_, v)| v == b"same"));
}

/// A value-ordered scan sees the CURRENT value of each row and nothing
/// else. The index is append-only, so a superseded record keeps its
/// entry; if liveness were not consulted, an updated row would appear
/// twice — once under its old bytes.
#[test]
fn superseded_and_deleted_values_leave_the_scan() {
    let mut db = db(256);
    db.put(1, Value::from_bytes(b"aaa").unwrap()).unwrap();
    db.put(2, Value::from_bytes(b"bbb").unwrap()).unwrap();
    assert_eq!(
        got(db.range_by_value(b"", b"").unwrap()),
        vec![(1, b"aaa".to_vec()), (2, b"bbb".to_vec())]
    );
    db.put(1, Value::from_bytes(b"zzz").unwrap()).unwrap();
    assert_eq!(
        got(db.range_by_value(b"", b"").unwrap()),
        vec![(2, b"bbb".to_vec()), (1, b"zzz".to_vec())],
        "the old bytes must not still be in the order"
    );
    db.remove(2).unwrap();
    assert_eq!(
        got(db.range_by_value(b"", b"").unwrap()),
        vec![(1, b"zzz".to_vec())]
    );
    // Reinserting the retired id puts it back exactly once.
    db.put(2, Value::from_bytes(b"bbb").unwrap()).unwrap();
    assert_eq!(
        got(db.range_by_value(b"", b"").unwrap()),
        vec![(2, b"bbb".to_vec()), (1, b"zzz".to_vec())]
    );
}

/// The index is derived state. A reopen rebuilds it from the committed
/// rows, so the order after recovery must be the order before it — that
/// is the property that makes it safe to keep no on-disk form of it.
#[test]
fn the_order_survives_a_reopen_because_it_is_rebuilt() {
    let mut db = db(1024);
    let mut rng = Rng(0xD1CE);
    let mut live: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for id in 0..60u64 {
        let mut v = format!("k{:02}/", rng.below(20)).into_bytes();
        v.extend(std::iter::repeat_n(b'-', rng.below(30) as usize));
        live.insert(id, v.clone());
        db.put(id, Value::from_vec(v).unwrap()).unwrap();
    }
    let oracle = oracle_of(&live);
    let before = got(db.range_by_value(b"", b"").unwrap());
    let snap = db.snapshot().expect("snapshot");
    let reopened = Mem::load(&snap).expect("reopen");
    assert_eq!(
        got(reopened.range_by_value(b"", b"").unwrap()),
        before,
        "the rebuilt index must order the same way"
    );
    assert_eq!(before, oracle_range(&oracle, b"", b""));
}

/// Paging is not a different answer from scanning. Every page boundary is
/// a place a cursor can be wrong by one, in either direction.
#[test]
fn paging_a_value_scan_returns_the_scan() {
    let mut db = db(1024);
    for id in 0..50u64 {
        db.put(
            id,
            Value::from_vec(format!("v{id:03}").into_bytes()).unwrap(),
        )
        .unwrap();
    }
    let whole = got(db.range_by_value(b"", b"").unwrap());
    assert_eq!(whole.len(), 50);
    // Walk it a page at a time and rebuild the same list.
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let (page, next) = db.range_page_by_value(b"", b"", cursor).unwrap();
        assert!(page.len() <= dabqlite_core::RANGE_PAGE, "page too long");
        paged.extend(got(page));
        match next {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    assert_eq!(paged, whole);

    let mut rev_paged = Vec::new();
    let mut cursor = None;
    loop {
        let (page, next) = db.range_page_by_value_rev(b"", b"", cursor).unwrap();
        rev_paged.extend(got(page));
        match next {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    let mut expect = whole.clone();
    expect.reverse();
    assert_eq!(rev_paged, expect);
}

/// A bound longer than any value can be is refused by name rather than
/// answered with an empty page that looks like a real result.
#[test]
fn a_bound_longer_than_any_value_is_refused_by_name() {
    let mut db = db(64);
    db.put(1, Value::from_bytes(b"x").unwrap()).unwrap();
    let too_long = vec![b'x'; dabqlite::MAX_VALUE_LEN + 1];
    assert!(matches!(
        db.range_by_value(&too_long, b"").unwrap_err(),
        dabqlite::Error::ValueTooLong { .. }
    ));
    assert!(matches!(
        db.range_by_value(b"", &too_long).unwrap_err(),
        dabqlite::Error::ValueTooLong { .. }
    ));
    // Exactly at the ceiling is a legal bound.
    let at_max = vec![b'x'; dabqlite::MAX_VALUE_LEN];
    assert!(db.range_by_value(&at_max, b"").is_ok());
}

/// The index must not change what a batch means: a value-ordered scan
/// sees a whole commit or none of it, like every other read.
#[test]
fn a_batch_lands_in_the_value_order_all_at_once() {
    let mut db = db(1024);
    db.batch(&[
        Op::insert(1, Value::from_bytes(b"ccc").unwrap()),
        Op::insert(2, Value::from_bytes(b"aaa").unwrap()),
        Op::insert(3, Value::from_bytes(b"bbb").unwrap()),
    ])
    .unwrap();
    assert_eq!(
        got(db.range_by_value(b"", b"").unwrap()),
        vec![
            (2, b"aaa".to_vec()),
            (3, b"bbb".to_vec()),
            (1, b"ccc".to_vec())
        ]
    );
    // A refused batch changes nothing, in this order as in every other.
    let err = db.batch(&[
        Op::insert(4, Value::from_bytes(b"ddd").unwrap()),
        Op::insert(1, Value::from_bytes(b"eee").unwrap()),
    ]);
    assert!(err.is_err(), "duplicate insert must be refused");
    assert_eq!(db.range_by_value(b"", b"").unwrap().len(), 3);
}
