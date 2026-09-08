//! Tests that pin what happens at size, and what `Db::find` and
//! `Db::all` cost.
//!
//! These are timing tests, which are normally a bad idea. They are here
//! because the effects they measure are not marginal. The previous
//! version of this file existed because a needle matching 50,000 of
//! 50,000 rows took 31 SECONDS against 58 ms for a brute-force scan.
//! That is fixed, and these tests now pin the fix — and the two costs
//! that came with it.

// A timing test needs a clock; the determinism deny list is for the
// library's own deterministic boundary, not for a consumer's tests.
#![allow(clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use bookmarks::{NewBookmark, Store};
use dabqlite::{MemDb, Value};

fn dataset(n: usize) -> Vec<NewBookmark> {
    (0..n)
        .map(|i| {
            let mut tags = vec!["wideeee".to_string()];
            if i % 4 == 0 {
                tags.push("narrowww".to_string());
            }
            NewBookmark {
                url: format!("https://example.com/article-{i:06}"),
                title: format!("article number {i}"),
                tags,
                added: 1_700_000_000 + i as u64,
            }
        })
        .collect()
}

fn best_of<T>(n: u32, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::from_secs(9999);
    let mut out = f();
    for _ in 0..n {
        let t = Instant::now();
        out = f();
        best = best.min(t.elapsed());
    }
    (best, out)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn find_is_no_longer_quadratic_in_the_number_of_matches() {
    // FIXED, and this is the assertion that used to say the opposite:
    // "4x the matches should cost far more than 4x the time".
    const N: usize = 4000;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    s.import(&dataset(N)).unwrap();

    let (narrow_t, narrow) = best_of(3, || s.db().find(b"narrowww").unwrap());
    let (wide_t, wide) = best_of(3, || s.db().find(b"wideeee").unwrap());
    assert_eq!(narrow.len(), N / 4);
    assert_eq!(wide.len(), N);

    // Exactness first: the index is not approximate, and that is real.
    let scanned = s
        .db()
        .all()
        .unwrap()
        .into_iter()
        .filter(|(_, v)| contains(v.as_bytes(), b"wideeee"))
        .count();
    assert_eq!(
        wide.len(),
        scanned,
        "find must agree with a brute-force scan"
    );

    eprintln!(
        "find: {} matches in {narrow_t:?}, {} matches in {wide_t:?} ({:.1}x the time)",
        narrow.len(),
        wide.len(),
        wide_t.as_secs_f64() / narrow_t.as_secs_f64()
    );
    // The claim is LINEAR, and the failure it guards against is
    // quadratic: 4x the matches used to cost ~16x. The bound is
    // deliberately loose because this is wall-clock on a shared machine —
    // tight enough that the quadratic shape fails it by a mile, loose
    // enough that it does not fail on noise.
    assert!(
        wide_t < narrow_t * 8,
        "4x the matches must not cost the quadratic 16x any more; got {:.1}x \
         ({narrow_t:?} -> {wide_t:?})",
        wide_t.as_secs_f64() / narrow_t.as_secs_f64()
    );
}

#[test]
fn the_index_is_no_longer_hundreds_of_times_slower_than_a_scan_but_it_is_still_slower() {
    // FIXED, with an asterisk. "ttps" matches every bookmark — the query a
    // search box issues on the fourth keystroke. The index used to lose to
    // a hand-written scan by a factor that GREW with the table: 4.7x at
    // 1,000 bookmarks, 65x at 10,000, 505x at 50,000 (29,237 ms against
    // 57.9 ms).
    //
    // That growth is gone. What is left is a flat ~1.2x: measured in
    // release, find("http") against the same scan is 5.06 vs 4.18 ms at
    // 1k, 55.8 vs 46.8 at 10k, 276 vs 235 at 50k. So the index is not a
    // speed-up for this store at any size — it is merely no longer a
    // catastrophe, and what it actually buys is exactness across slot
    // boundaries and a cheap first page when matches are dense.
    const N: usize = 4000;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    s.import(&dataset(N)).unwrap();

    let (index_t, index_hits) = best_of(3, || s.db().find(b"ttps").unwrap());
    let (scan_t, scan_hits) = best_of(3, || {
        s.db()
            .all()
            .unwrap()
            .into_iter()
            .filter(|(_, v)| contains(v.as_bytes(), b"ttps"))
            .count()
    });
    assert_eq!(index_hits.len(), scan_hits);
    eprintln!(
        "find {index_t:?} vs scan {scan_t:?} for {scan_hits} matches ({:.2}x)",
        index_t.as_secs_f64() / scan_t.as_secs_f64()
    );
    assert!(
        index_t < scan_t * 3,
        "the index ({index_t:?}) is still a large multiple of the scan \
         ({scan_t:?}); it used to be 505x at 50k and should now be ~1.2x"
    );
}

#[test]
fn selectivity_pays_again_now_that_long_values_keep_the_index() {
    // This test used to record the price of variable-length values. The
    // engine took the exhaustive path — a descending scan of every row
    // slot — as soon as ANY value spanned more than one slot, and every
    // bookmark does, so the trigram index was off for the entire life of
    // a bookmark database. A needle matching nothing cost the same order
    // as reading every row: 69.7 ms at 50,000 bookmarks against 0.000 ms
    // before long values existed.
    //
    // The cause was a missing back-pointer, not a missing invariant: a
    // posting is filed under the row its window STARTS in, so a match
    // past the first slot of a value was filed under a continuation row
    // and the chain walk dropped it. Resolving a continuation back to its
    // head made the chain a superset again. Selectivity is worth
    // something once more, and this now pins that.
    const N: usize = 4000;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    s.import(&dataset(N)).unwrap();

    let (miss_t, miss) = best_of(5, || s.db().find(b"zzqxzzqx").unwrap());
    let (all_t, all_rows) = best_of(5, || s.db().all().unwrap());
    assert!(miss.is_empty());
    assert_eq!(all_rows.len(), N + 1);
    eprintln!("find(0 matches) {miss_t:?} vs reading the whole table {all_t:?}");
    assert!(
        miss_t * 20 < all_t,
        "a needle matching NOTHING should be far cheaper than reading every \
         row ({miss_t:?} vs {all_t:?})"
    );

    // And it costs what it costs over SHORT values: the index is on in
    // both cases now, so the identical miss is the same order of
    // magnitude whether or not the values span slots.
    let mut short = MemDb::in_memory_with(N as u64 * 2).unwrap();
    for i in 0..N as u64 {
        short
            .put(i, Value::from_text(&format!("tiny-{i:06}")).unwrap())
            .unwrap();
    }
    let (short_t, short_hits) = best_of(5, || short.find(b"zzqxzzqx").unwrap());
    assert!(short_hits.is_empty());
    eprintln!("the same miss over single-slot values: {short_t:?}");
    assert!(
        miss_t < short_t * 50,
        "long values should no longer switch the index off: {miss_t:?} vs \
         {short_t:?} for the identical miss"
    );
}

#[test]
fn one_long_value_does_not_turn_the_index_off_for_the_database() {
    // The sharpest form of the fixed regression. `exhaustive` used to be
    // `self.long_values > 0` — a property of the DATABASE, not of the
    // query or the row — so ONE value of seventeen bytes anywhere in the
    // file put every subsequent search of every other row on the scan
    // path, for the life of the database. Measured here at 618 ns before
    // and 61.7 ms after: a 99,800x regression from inserting one row.
    const N: u64 = 20_000;
    let mut db = MemDb::in_memory_with(N + 64).unwrap();
    for i in 0..N {
        db.put(i, Value::from_text(&format!("tiny-{i:06}")).unwrap())
            .unwrap();
    }
    let (before, hits) = best_of(5, || db.find(b"zzqxzzqx").unwrap());
    assert!(hits.is_empty());

    // One value that does not fit a slot. Nothing else changes.
    db.put(N, Value::from_text("seventeen bytes!!").unwrap())
        .unwrap();
    let (after, hits) = best_of(5, || db.find(b"zzqxzzqx").unwrap());
    assert!(hits.is_empty());

    eprintln!("the same miss over {N} rows: {before:?}, then {after:?} after ONE long value");
    assert!(
        after < before * 20,
        "one 17-byte value should not cost every later search the whole \
         table: {before:?} -> {after:?}"
    );
}

#[test]
fn the_first_page_of_a_rare_search_still_costs_the_whole_table() {
    // LIMITATION, and one the index being back on does NOT fix. A page
    // is filled by walking the candidate chain from its head, so a needle
    // whose only match is the oldest row means walking the chain past
    // every newer candidate first. `Db::find_page` is documented as
    // costing "the same per page however many matches there are", which
    // is true of the SECOND page and every one after it; the first page
    // of a rare needle costs the distance to its first match. A search
    // box gets its first results instantly for the needles a person
    // actually types and slowly for the ones that match one old row.
    const N: usize = 4000;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    s.import(&dataset(N)).unwrap();

    let (common_t, common) = best_of(5, || s.db().find_page(b"wideeee", None).unwrap());
    let (rare_t, rare) = best_of(5, || s.db().find_page(b"article-000", None).unwrap());
    assert_eq!(common.0.len(), 8);
    assert_eq!(rare.0.len(), 8);
    eprintln!("first page: common needle {common_t:?}, rare needle {rare_t:?}");

    // A needle that matches only the OLDEST row is the worst case: the
    // scan runs the length of the table before it can fill a page.
    let (oldest_t, oldest) = best_of(5, || s.db().find_page(b"article-000000", None).unwrap());
    assert_eq!(oldest.0.len(), 1);
    eprintln!("first page matching only the oldest row: {oldest_t:?}");
    assert!(
        oldest_t > common_t * 10,
        "paging is supposed to cost the same per page; the first page of a \
         needle matching one old row cost {oldest_t:?} against {common_t:?}"
    );
}

#[test]
fn a_full_scan_pays_an_extra_round_trip_for_every_long_value() {
    // COST of variable-length values, and it is large. A scan page carries
    // a row's value inline only while it fits one slot (`RowRef::value`
    // returns `None` otherwise), and the read-back is one engine call per
    // additional 16 bytes. So scanning N rows of 128-byte values costs
    // ~9 round trips each where N rows of 8-byte values cost one.
    //
    // Measured on the bookmark dataset at 50,000 bookmarks: `Db::all()`
    // went from 48.4 ms over 487,889 short rows to 244.2 ms over 50,001
    // long ones — a tenth of the rows and five times the wall clock.
    const N: u64 = 5000;
    let long = "x".repeat(128);

    let mut shortdb = MemDb::in_memory_with(N * 2).unwrap();
    for i in 0..N {
        shortdb
            .put(i, Value::from_text("shortval").unwrap())
            .unwrap();
    }
    let mut longdb = MemDb::in_memory_with(N * 16).unwrap();
    for i in 0..N {
        longdb.put(i, Value::from_text(&long).unwrap()).unwrap();
    }

    let (short_t, short_rows) = best_of(3, || shortdb.all().unwrap());
    let (long_t, long_rows) = best_of(3, || longdb.all().unwrap());
    assert_eq!(short_rows.len(), N as usize);
    assert_eq!(long_rows.len(), N as usize);
    eprintln!(
        "all() over {N} rows: {short_t:?} short values, {long_t:?} long values \
         ({:.1}x)",
        long_t.as_secs_f64() / short_t.as_secs_f64()
    );
    assert!(
        long_t > short_t * 3,
        "the same number of rows should not cost {:.1}x more to scan just \
         because the values are longer ({short_t:?} -> {long_t:?}); if this \
         starts failing, scan pages learned to carry long values",
        long_t.as_secs_f64() / short_t.as_secs_f64()
    );
}

#[test]
fn find_matches_bytes_because_it_has_no_idea_what_a_field_is() {
    // LIMITATION, unchanged. `find` searches raw value bytes. This crate's
    // records carry a timestamp and a visit counter next to the text, and
    // a short needle can match inside either — so every `find` result has
    // to be re-checked field by field in Rust, or the store returns
    // bookmarks whose text does not contain the needle at all.
    let mut db = MemDb::in_memory_with(64).unwrap();
    db.insert(1, Value::from_text("a title with tag in it").unwrap())
        .unwrap();
    db.insert(
        2,
        Value::from_bytes(&[0x00, b't', b'a', b'g', 0x07]).unwrap(),
    )
    .unwrap();

    let hits = db.find(b"tag").unwrap();
    assert_eq!(
        hits.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        vec![2, 1],
        "the binary row matched the text query (and note the order: newest first)"
    );
}

#[test]
fn a_scan_of_the_whole_collection_is_the_price_of_every_ordered_query() {
    // Not a micro-benchmark, a shape check: `20 newest` has to materialise
    // and sort the entire collection because ids are the only order the
    // library can iterate in, and `added` is not the id. `find`'s own
    // newest-first order is by row, which is write order, not `added`.
    const N: usize = 5000;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    let mut items = dataset(N);
    // Shuffle `added` so that id order is NOT date order, as it would be
    // after any import of an existing browser profile.
    for (i, it) in items.iter_mut().enumerate() {
        it.added = 1_700_000_000 + ((i * 7919) % N) as u64;
    }
    s.import(&items).unwrap();

    let newest = s
        .query(&bookmarks::Query {
            order: bookmarks::Order::NewestFirst,
            limit: Some(20),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(newest.len(), 20);
    assert!(newest[0].added > newest[19].added);
    let (scan_t, all) = best_of(2, || s.list().unwrap());
    assert_eq!(all.len(), N);
    eprintln!("{N} bookmarks materialised in {scan_t:?} to answer LIMIT 20");
}

/// The review's last open item, closed: a tag query is served by the
/// index instead of by materialising the whole collection.
///
/// This store used to answer "tagged rust AND wasm" with `list()` and a
/// chain of Rust filters — every bookmark decoded to find the handful
/// that matched. The library grew compound predicates, so the same
/// question is now one chain walk over the RAREST of the tags named.
/// The assertion is a count, not a clock: the library says how many rows
/// a search verified, and that number is the same on every machine.
#[test]
fn a_tag_query_is_a_chain_walk_and_no_longer_the_whole_collection() {
    const N: usize = 5000;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    s.import(&dataset(N)).unwrap();

    // `wideeee` is on every bookmark, `narrowww` on a quarter of them.
    // Naming both must cost the NARROW one, not the wide one and not the
    // collection.
    let before = s.db().find_verifications();
    let hits = s
        .query(&bookmarks::Query {
            tags: vec!["wideeee".into(), "narrowww".into()],
            ..Default::default()
        })
        .unwrap();
    let verified = s.db().find_verifications() - before;

    assert_eq!(hits.len(), N / 4);
    assert!(
        verified <= (N / 4) as u64 + 8,
        "a two-tag query verified {verified} rows for {} hits — it walked \
         the wide tag, or the whole store",
        hits.len()
    );
    // And the honest comparison: the scan it replaced touches every row.
    assert!(
        verified * 3 < N as u64,
        "verified {verified} of {N}, which is not an index"
    );
    eprintln!(
        "{N} bookmarks, two tags, {verified} rows verified for {} hits",
        hits.len()
    );
}

/// The parts of a query the library still cannot serve stay honest.
///
/// A case-insensitive text match and a date range are Rust filters over
/// decoded fields, and pretending otherwise would lose rows. This pins
/// that they still agree with the naive answer once the index has
/// narrowed on the tags.
#[test]
fn the_filters_the_index_cannot_serve_still_agree_with_the_scan() {
    const N: usize = 800;
    let mut s = Store::in_memory_with(N as u64 * 14).unwrap();
    let mut items = dataset(N);
    // Mixed case, so a byte-exact index could not answer the text part.
    for (i, it) in items.iter_mut().enumerate() {
        it.title = format!("Article Number {i}");
    }
    s.import(&items).unwrap();

    let q = bookmarks::Query {
        text: Some("ARTICLE NUMBER 4".into()),
        tags: vec!["narrowww".into()],
        since: Some(1_700_000_000 + 100),
        until: Some(1_700_000_000 + 600),
        ..Default::default()
    };
    let got: Vec<u64> = s.query(&q).unwrap().into_iter().map(|b| b.id).collect();

    // The same question, answered by decoding everything.
    let want: Vec<u64> = s
        .list()
        .unwrap()
        .into_iter()
        .filter(|b| b.matches("article number 4"))
        .filter(|b| b.tags.contains(&"narrowww".to_string()))
        .filter(|b| b.added >= 1_700_000_000 + 100 && b.added <= 1_700_000_000 + 600)
        .map(|b| b.id)
        .collect();

    assert!(
        !want.is_empty(),
        "the fixture must actually match something"
    );
    assert_eq!(got, want);
}
