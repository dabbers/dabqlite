//! The value-ordered index under the same fault model as everything else
//! (docs/DESIGN.md §4.6, §10): rebuilt at every recovery, held to a free
//! oracle, and asked the question again after a crash at every boundary.
//!
//! The index keeps no bytes of its own and nothing on disk, so a crash
//! cannot damage it directly. What a crash CAN do is leave the rows in a
//! different state from the one the writer thought it left them in — and
//! the index must then describe THAT state, exactly, with no entry for a
//! row the recovery discarded and none missing for a row it kept.

use std::collections::{BTreeMap, BTreeSet};

use dabqlite_core::{BatchOp, Capacities, DbError, Input, Output, MAX_VALUE_LEN, VALUE_LEN};
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 512 };

fn fresh() -> SimHost {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    host
}

/// A value whose first bytes are a byte KEY, which is the whole point of
/// the index: an application puts its real key at the front of the record.
fn keyed(key: &str, pad: usize) -> Vec<u8> {
    let mut v = key.as_bytes().to_vec();
    v.extend(std::iter::repeat_n(b'.', pad));
    v
}

/// The claimed order, spelled out: value bytes, then id.
fn oracle(model: &BTreeMap<u64, Vec<u8>>, lo: &[u8], hi: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let set: BTreeSet<(Vec<u8>, u64)> = model.iter().map(|(&id, v)| (v.clone(), id)).collect();
    set.into_iter()
        .filter(|(k, _)| k.as_slice() >= lo && (hi.is_empty() || k.as_slice() <= hi))
        .map(|(k, id)| (id, k))
        .collect()
}

fn put(host: &mut SimHost, id: u64, value: &[u8]) {
    match host.batch(&[BatchOp::Put { id, value }]) {
        Driven::Done(Output::BatchDone { result: Ok(()), .. }) => {}
        other => panic!("put {id}: {other:?}"),
    }
}

/// Random traffic, every bound, ascending and descending. The base case
/// the crash tests build on.
#[test]
fn value_pages_match_the_oracle_over_random_traffic() {
    use rand::Rng;
    for seed in 0..6u64 {
        let mut rng = crash_rng(0x5641_4C4F, seed); // "VALO"
        let mut host = fresh();
        let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

        for step in 0..80u64 {
            let existing = (!model.is_empty()).then(|| {
                *model
                    .keys()
                    .nth(rng.gen_range(0..model.len()))
                    .expect("nonempty")
            });
            match (existing, rng.gen_range(0..100u32)) {
                (Some(id), r) if r < 15 => {
                    host.run(dabqlite_sim::host::ClientOp::Delete { id });
                    model.remove(&id);
                }
                _ => {
                    let id = rng.gen_range(0..30u64);
                    // Shared prefixes, exact collisions, and values that
                    // cross slot seams — the three shapes that break a
                    // comparison written the easy way.
                    let key = format!(
                        "{}{:02}/",
                        ["ses", "usr", "tok"][step as usize % 3],
                        rng.gen_range(0..8u32)
                    );
                    let v = keyed(&key, rng.gen_range(0..40));
                    put(&mut host, id, &v);
                    model.insert(id, v);
                }
            }

            if step % 7 != 0 {
                continue;
            }
            assert_eq!(
                host.value_all(b"", b""),
                oracle(&model, b"", b""),
                "seed={seed} step={step}: full value scan"
            );
            let mut rev = oracle(&model, b"", b"");
            rev.reverse();
            assert_eq!(
                host.value_all_rev(b"", b""),
                rev,
                "seed={seed} step={step}: descending value scan"
            );
            for lo in [&b""[..], b"ses", b"ses03/", b"tok", b"usr99", b"zzz"] {
                for hi in [&b""[..], b"ses", b"tok00/", b"usr", b"zzz"] {
                    assert_eq!(
                        host.value_all(lo, hi),
                        oracle(&model, lo, hi),
                        "seed={seed} step={step}: [{}, {}]",
                        String::from_utf8_lossy(lo),
                        String::from_utf8_lossy(hi)
                    );
                }
            }
        }
    }
}

/// The index is rebuilt from the committed rows, so after a crash it must
/// describe the RECOVERED database — not the one the writer intended.
#[test]
fn the_value_order_survives_every_crash_boundary() {
    for seed in 0..4u64 {
        for boundary in 0..6u64 {
            let mut host = fresh();
            let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
            for i in 0..6u64 {
                // Descending keys, so a rebuild that kept insertion order
                // instead of value order would be visibly wrong.
                let v = keyed(&format!("k{:02}/", 20 - i), i as usize * 3);
                put(&mut host, i, &v);
                model.insert(i, v);
            }
            let doomed = keyed("k00/last", 40);
            host.crash_after = Some(host.io_count + boundary);
            assert!(matches!(
                host.batch(&[BatchOp::Put {
                    id: 999,
                    value: &doomed
                }]),
                Driven::Crashed
            ));
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(0x5643_5253, seed * 10 + boundary));

            let mut host = SimHost::new(CAPS, disk, None);
            let live = match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("seed={seed} b={boundary}: {other:?}"),
            };
            // The commit either landed whole or not at all; the scan says
            // which, and the oracle follows it rather than guessing.
            if live == 7 {
                model.insert(999, doomed.clone());
            }
            assert_eq!(
                host.value_all(b"", b""),
                oracle(&model, b"", b""),
                "seed={seed} b={boundary}"
            );
            let mut rev = oracle(&model, b"", b"");
            rev.reverse();
            assert_eq!(
                host.value_all_rev(b"", b""),
                rev,
                "seed={seed} b={boundary}"
            );
            // And the database is still writable afterwards, in the order.
            let after = keyed("k99/after", 0);
            put(&mut host, 1000, &after);
            model.insert(1000, after);
            assert_eq!(host.value_all(b"", b""), oracle(&model, b"", b""));
        }
    }
}

/// A crash in the middle of a LONG value's run is the case a
/// dereferencing comparison can get wrong: the head is on disk and the
/// continuation is not. Recovery discards the whole run, and the index
/// must have no entry for it — not an entry that compares against a
/// truncated value.
#[test]
fn a_crash_inside_a_long_value_leaves_no_entry_behind() {
    for boundary in 0..8u64 {
        let mut host = fresh();
        let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for i in 0..4u64 {
            let v = keyed(&format!("a{i}/"), 4);
            put(&mut host, i, &v);
            model.insert(i, v);
        }
        // A value many slots long whose bytes sort in the MIDDLE of the
        // others, so a stale entry would be visible in the order rather
        // than at one end where it might be missed.
        let long = keyed("a2x/", MAX_VALUE_LEN - 8);
        assert!(long.len() > VALUE_LEN * 4);
        host.crash_after = Some(host.io_count + boundary);
        let outcome = host.batch(&[BatchOp::Put {
            id: 99,
            value: &long,
        }]);
        let crashed = matches!(outcome, Driven::Crashed);
        let mut disk = std::mem::take(&mut host.disk);
        if crashed {
            disk.crash(&mut crash_rng(0x4C4F_4E47, boundary));
        }

        let mut host = SimHost::new(CAPS, disk, None);
        let live = match host.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
            other => panic!("b={boundary}: {other:?}"),
        };
        if live == 5 {
            model.insert(99, long.clone());
        }
        assert_eq!(
            host.value_all(b"", b""),
            oracle(&model, b"", b""),
            "b={boundary}"
        );
        // The bytes that came back are the WHOLE value, not a prefix.
        if live == 5 {
            let got = host.value_all(b"a2x/", b"a2x0");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].1, long, "a truncated value would still sort here");
        }
    }
}

/// Salvage mode answers with what it can verify and says the answer is
/// partial. A value-ordered scan is no different from any other read.
#[test]
fn a_quarantined_row_is_missing_and_the_page_says_so() {
    let mut host = fresh();
    for i in 0..5u64 {
        put(&mut host, i, &keyed(&format!("q{i}/"), 2));
    }
    let mut disk = std::mem::take(&mut host.disk);
    // Corrupt one row's checksum so strict open refuses and salvage
    // quarantines exactly it.
    disk.corrupt(dabqlite_core::FileId::Rows, 2 * 32 + 4, 0xff);

    let mut strict = SimHost::new(CAPS, disk.clone(), None);
    assert!(
        matches!(
            strict.open(),
            Driven::Done(Output::OpenDone {
                result: Err(DbError::Corrupt { .. })
            })
        ),
        "a damaged row must not open strictly"
    );

    let mut host = SimHost::new(CAPS, disk, None);
    match host.open_salvage() {
        Driven::Done(Output::OpenDone { result: Ok(_) }) => {}
        other => panic!("salvage open: {other:?}"),
    }
    let page = match host.run_input(Input::RangeByValue {
        lo: b"",
        hi: b"",
        after: None,
        descending: false,
    }) {
        Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
        other => panic!("salvage scan: {other:?}"),
    };
    assert!(
        page.incomplete,
        "a scan that silently omits quarantined rows is indistinguishable \
         from data loss"
    );
    let rows = host.value_all(b"", b"");
    assert_eq!(rows.len(), 4, "exactly the quarantined row is missing");
    assert!(rows.iter().all(|(id, _)| *id != 2));
    // What IS returned is exact.
    for (id, v) in rows {
        assert_eq!(v, keyed(&format!("q{id}/"), 2));
    }
}

/// The scan is bounded work per call in this order too: a page is a page,
/// and the cursor makes progress. Without both, a caller paging a large
/// database either never finishes or loops.
#[test]
fn value_pages_are_bounded_and_the_cursor_advances() {
    let mut host = fresh();
    for i in 0..60u64 {
        put(&mut host, i, &keyed(&format!("p{i:03}/"), 1));
    }
    let mut seen = 0usize;
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = match host.run_input(Input::RangeByValue {
            lo: b"",
            hi: b"",
            after,
            descending: false,
        }) {
            Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
            other => panic!("{other:?}"),
        };
        assert!(
            page.count as usize <= dabqlite_core::RANGE_PAGE,
            "page longer than the protocol allows"
        );
        seen += page.count as usize;
        pages += 1;
        assert!(pages <= 20, "the cursor is not making progress");
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(seen, 60);
    assert!(pages >= 8, "60 rows cannot be one page");
}
