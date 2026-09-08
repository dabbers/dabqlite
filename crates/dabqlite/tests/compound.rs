//! Compound search: several predicates, ANDed, in ONE chain walk.
//!
//! Every sample application wanted this and none of them could say it.
//! The bookmark store's `query()` was a full scan plus a chain of Rust
//! filters, because the library could find rows containing a needle but
//! offered no way to combine that with a second condition. So the index
//! was there and the query did not use it.
//!
//! The oracle here is deliberately not a second search implementation.
//! It is the already-tested single-needle surface — `find_text("")`,
//! which is every live row newest-first — filtered by `Match::holds`,
//! which is the one definition of what a match mode means. If a compound
//! search ever disagrees with "scan everything and check every
//! condition", in content OR in order, these tests fail.

use dabqlite::{Db, Match, Op, Predicate, Value, MAX_VALUE_LEN};

type Mem = Db<dabqlite::MemoryStorage>;

fn db(rows: u64) -> Mem {
    Db::in_memory_with(rows).expect("open")
}

fn ids(rows: Vec<(u64, Value)>) -> Vec<u64> {
    rows.into_iter().map(|(id, _)| id).collect()
}

/// Every live row, newest first, checked against every predicate by the
/// same `holds` the engine uses. This is what a compound search MEANS.
fn oracle(db: &Mem, preds: &[Predicate<'_>]) -> Vec<u64> {
    db.find_text("")
        .expect("scan")
        .into_iter()
        .filter(|(_, v)| preds.iter().all(|p| p.mode.holds(v.as_bytes(), p.needle)))
        .map(|(id, _)| id)
        .collect()
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

/// The headline: over a workload of inserts, updates and deletes, a
/// compound search is exactly "scan and check everything" — same rows,
/// same order — for every combination of predicates worth trying.
#[test]
fn a_compound_search_is_the_scan_and_filter_it_replaces() {
    // Words that overlap, share trigrams, appear as substrings of each
    // other, and sometimes land on opposite sides of a slot seam.
    const TAGS: [&str; 4] = ["#rust", "#rusty", "#db", "#x"];
    const WORDS: [&str; 4] = ["alpha", "alphabet", "beta", "be"];

    for seed in 0..6u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9) | 1);
        let mut db = db(4096);

        for round in 0..120u64 {
            let id = rng.below(30);
            match rng.below(8) {
                0 => {
                    db.remove(id).expect("remove");
                }
                _ => {
                    let w = WORDS[rng.below(4) as usize];
                    let t = TAGS[rng.below(4) as usize];
                    // Sometimes long enough to cross slot boundaries, so
                    // the verification is reading an assembled value and
                    // not just a head slot.
                    let pad = "-".repeat(rng.below(60) as usize);
                    let v = format!("{w}{pad}{t} r{round}");
                    db.put(id, Value::from_text(&v).unwrap()).expect("put");
                }
            }

            if round % 7 != 0 {
                continue;
            }
            for w in WORDS {
                for t in TAGS {
                    let preds = [
                        Predicate::contains(w.as_bytes()),
                        Predicate::contains(t.as_bytes()),
                    ];
                    assert_eq!(
                        ids(db.find_and(&preds).expect("find_and")),
                        oracle(&db, &preds),
                        "seed {seed} round {round}: {w:?} AND {t:?}"
                    );
                }
            }
        }
    }
}

/// A compound search is not required to be all-`Contains`: the modes mix,
/// and the index still narrows on whichever needle is longest, because
/// EVERY mode implies containment.
#[test]
fn the_match_modes_mix_inside_one_compound_search() {
    let mut db = db(256);
    db.insert(1, Value::from_text("https://example.com/alpha").unwrap())
        .unwrap();
    db.insert(2, Value::from_text("http://example.com/alpha").unwrap())
        .unwrap();
    db.insert(3, Value::from_text("https://other.test/alpha").unwrap())
        .unwrap();
    db.insert(4, Value::from_text("https://example.com/beta").unwrap())
        .unwrap();

    // Prefix AND Suffix: neither alone answers it.
    let preds = [
        Predicate {
            needle: b"https://",
            mode: Match::Prefix,
        },
        Predicate {
            needle: b"/alpha",
            mode: Match::Suffix,
        },
    ];
    assert_eq!(ids(db.find_and(&preds).unwrap()), oracle(&db, &preds));
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![3, 1]);

    // Three of them, including a Contains in the middle.
    let preds = [
        Predicate {
            needle: b"https://",
            mode: Match::Prefix,
        },
        Predicate::contains(b"example.com"),
        Predicate {
            needle: b"/alpha",
            mode: Match::Suffix,
        },
    ];
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![1]);

    // An `Exact` predicate narrows to at most the rows holding that
    // value, and contradicting it with another predicate empties the
    // answer rather than confusing it.
    let exact = [Predicate {
        needle: b"https://example.com/beta",
        mode: Match::Exact,
    }];
    assert_eq!(ids(db.find_and(&exact).unwrap()), vec![4]);
    let contradiction = [
        Predicate {
            needle: b"https://example.com/beta",
            mode: Match::Exact,
        },
        Predicate::contains(b"alpha"),
    ];
    assert!(db.find_and(&contradiction).unwrap().is_empty());
}

/// One predicate is the search the library already had. The compound
/// surface must not be a second, subtly different implementation of it.
#[test]
fn one_predicate_is_the_single_needle_search_it_generalises() {
    let mut db = db(512);
    for id in 0..40u64 {
        let v = format!("row-{id}-{}", "abcde".repeat((id % 7) as usize + 1));
        db.insert(id, Value::from_text(&v).unwrap()).unwrap();
    }
    for id in (0..40u64).step_by(3) {
        db.remove(id).unwrap();
    }

    for needle in [&b""[..], b"a", b"ab", b"abc", b"row-1", b"bcdea", b"zzz"] {
        for mode in [Match::Contains, Match::Prefix, Match::Suffix, Match::Exact] {
            let preds = [Predicate { needle, mode }];
            assert_eq!(
                ids(db.find_and(&preds).unwrap()),
                ids(db.find_matching(needle, mode).unwrap()),
                "{:?} in {mode:?}",
                String::from_utf8_lossy(needle)
            );
        }
    }
}

/// No conditions is not an error and not an empty answer: it is every
/// row, which is what a query with no `WHERE` means.
#[test]
fn no_predicates_at_all_is_every_row() {
    let mut db = db(256);
    for id in 0..12u64 {
        db.insert(id, Value::from_text(&format!("v{id}")).unwrap())
            .unwrap();
    }
    db.remove(5).unwrap();
    assert_eq!(ids(db.find_and(&[]).unwrap()), oracle(&db, &[]));
    assert_eq!(db.find_and(&[]).unwrap().len(), 11);

    // Predicates that are individually vacuous are the same thing again.
    let vacuous = [
        Predicate::contains(b""),
        Predicate::contains(b""),
        Predicate::contains(b""),
    ];
    assert_eq!(ids(db.find_and(&vacuous).unwrap()), oracle(&db, &vacuous));
}

/// A needle too short to have a trigram cannot drive the chain. Paired
/// with a long one it must not disable the index, and alone it must
/// still be exact — the walk falls back to a bounded scan.
#[test]
fn a_needle_shorter_than_a_trigram_is_still_exact() {
    let mut db = db(512);
    for id in 0..30u64 {
        let v = format!(
            "item-{id:02}-payload-{}",
            if id % 2 == 0 { "x" } else { "y" }
        );
        db.insert(id, Value::from_text(&v).unwrap()).unwrap();
    }

    // Short alone: no chain, a scan, still right.
    let short = [Predicate::contains(b"x")];
    assert_eq!(ids(db.find_and(&short).unwrap()), oracle(&db, &short));

    // Short AND long: the long one drives the chain, the short one only
    // verifies. The answer is the intersection either way.
    let mixed = [Predicate::contains(b"x"), Predicate::contains(b"payload")];
    assert_eq!(ids(db.find_and(&mixed).unwrap()), oracle(&db, &mixed));
    assert_eq!(ids(db.find_and(&mixed).unwrap()).len(), 15);

    // Two short ones: still a scan, still the intersection.
    let both_short = [Predicate::contains(b"5"), Predicate::contains(b"y")];
    assert_eq!(
        ids(db.find_and(&both_short).unwrap()),
        oracle(&db, &both_short)
    );
}

/// Paging a compound search returns the same rows in the same order as
/// draining it, and the cursor is honoured page by page.
#[test]
fn paging_a_compound_search_is_the_whole_answer_in_pieces() {
    let mut db = db(4096);
    for id in 0..300u64 {
        let v = format!("tag-alpha item-{id} kind-{}", id % 3);
        db.insert(id, Value::from_text(&v).unwrap()).unwrap();
    }
    let preds = [
        Predicate::contains(b"tag-alpha"),
        Predicate::contains(b"kind-1"),
    ];
    let whole = ids(db.find_and(&preds).unwrap());
    assert_eq!(whole, oracle(&db, &preds));
    assert!(whole.len() > 90, "expected a multi-page answer");

    let mut paged = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let (rows, next) = db.find_page_and(&preds, after).expect("page");
        pages += 1;
        paged.extend(ids(rows));
        match next {
            Some(c) => after = Some(c),
            None => break,
        }
        assert!(pages < 100, "paging did not terminate");
    }
    assert!(pages > 1, "the answer must actually span pages");
    assert_eq!(paged, whole);
}

/// A needle longer than any value could be is refused rather than
/// silently answered with an empty page — the same judgement the
/// single-needle search makes, applied to whichever predicate is at
/// fault.
#[test]
fn an_impossible_needle_is_refused_not_answered() {
    let db = db(256);
    let huge = vec![b'x'; MAX_VALUE_LEN + 1];
    let preds = [Predicate::contains(b"ok"), Predicate::contains(&huge)];
    match db.find_and(&preds) {
        Err(dabqlite::Error::NeedleTooLong { len, max }) => {
            assert_eq!(len, MAX_VALUE_LEN + 1);
            assert_eq!(max, MAX_VALUE_LEN);
        }
        other => panic!("expected NeedleTooLong, got {other:?}"),
    }
    // At the limit exactly it is a legal question with an empty answer.
    let at_limit = vec![b'x'; MAX_VALUE_LEN];
    assert!(db
        .find_and(&[Predicate::contains(&at_limit)])
        .expect("legal")
        .is_empty());
}

/// The values a compound search verifies are WHOLE values, assembled
/// across slots — a predicate whose needle straddles a slot seam must
/// still match, and so must one that only appears in a later slot.
#[test]
fn predicates_are_checked_against_whole_values_not_head_slots() {
    let mut db = db(512);
    // VALUE_LEN is 16, so a needle at offset 14 straddles the first seam
    // and a tag at the very end lives many slots in.
    let body = "z".repeat(300);
    db.insert(
        1,
        Value::from_text(&format!("head-seam-{body}-#tail")).unwrap(),
    )
    .unwrap();
    db.insert(
        2,
        Value::from_text(&format!("head-seam-{body}-#other")).unwrap(),
    )
    .unwrap();

    let preds = [
        Predicate::contains(b"head-seam-zzz"),
        Predicate::contains(b"#tail"),
    ];
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![1]);
    assert_eq!(ids(db.find_and(&preds).unwrap()), oracle(&db, &preds));
}

/// A compound search sees committed state, and it sees it whole: a batch
/// that writes several matching rows contributes all of them or none.
#[test]
fn a_compound_search_reads_committed_state() {
    let mut db = db(512);
    db.batch(&[
        Op::insert(1, Value::from_text("apple pie #dessert").unwrap()),
        Op::insert(2, Value::from_text("apple sauce #side").unwrap()),
        Op::insert(3, Value::from_text("cherry pie #dessert").unwrap()),
    ])
    .expect("batch");

    let preds = [
        Predicate::contains(b"apple"),
        Predicate::contains(b"#dessert"),
    ];
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![1]);

    // Reopening changes nothing: the index is rebuilt from the rows and
    // the answer is the same.
    let snapshot = db.snapshot().expect("snapshot");
    let db: Mem = Db::load(&snapshot).expect("load");
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![1]);
    assert_eq!(ids(db.find_and(&preds).unwrap()), oracle(&db, &preds));
}

/// Naming a second condition makes a search FASTER, not just narrower.
///
/// Every predicate's chain is a superset of the answer, so the engine is
/// free to walk whichever is cheapest — and it measures rather than
/// guesses, because a chain is keyed on a needle's first three bytes and
/// the longer needle is routinely the more common one. This is the test
/// that tells the two apart: both choices return the same rows, so only
/// the verification count says which chain was walked.
#[test]
fn a_compound_search_walks_its_cheapest_condition() {
    let mut db = db(8192);
    for id in 0..2000u64 {
        // Every row carries the common tag; exactly one also carries the
        // rare one, and the rare needle is the SHORTER of the two.
        let v = if id == 777 {
            format!("common-tag item-{id} zqx-rare")
        } else {
            format!("common-tag item-{id}")
        };
        db.insert(id, Value::from_text(&v).unwrap()).unwrap();
    }

    // The common needle alone: the chain is every row, and every one of
    // them gets verified.
    let before = db.find_verifications();
    assert_eq!(db.find_text("common-tag").unwrap().len(), 2000);
    let common_cost = db.find_verifications() - before;
    assert!(
        common_cost >= 2000,
        "the common needle should verify every row, verified {common_cost}"
    );

    // Both, ANDed. The answer is one row either way; the cost is not.
    let preds = [
        Predicate::contains(b"common-tag"),
        Predicate::contains(b"zqx-rare"),
    ];
    let before = db.find_verifications();
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![777]);
    let compound_cost = db.find_verifications() - before;
    assert_eq!(ids(db.find_and(&preds).unwrap()), oracle(&db, &preds));
    assert!(
        compound_cost <= 8,
        "the rare condition should drive the walk, verified {compound_cost}"
    );

    // Order of the predicates does not decide it either — the measurement
    // does.
    let reversed = [
        Predicate::contains(b"zqx-rare"),
        Predicate::contains(b"common-tag"),
    ];
    let before = db.find_verifications();
    assert_eq!(ids(db.find_and(&reversed).unwrap()), vec![777]);
    assert!(db.find_verifications() - before <= 8);
}

/// A needle without a trigram has no chain, so it must never be chosen
/// over a predicate that has one — a scan is the most expensive walk
/// available, not the cheapest.
#[test]
fn a_short_needle_never_wins_the_chain_over_a_real_one() {
    let mut db = db(8192);
    for id in 0..1500u64 {
        let v = format!("payload-{id}-e");
        db.insert(id, Value::from_text(&v).unwrap()).unwrap();
    }
    db.insert(4242, Value::from_text("payload-zzz-marker-e").unwrap())
        .unwrap();

    // "e" has no trigram; "zzz-marker" does, and it is rare.
    let preds = [
        Predicate::contains(b"e"),
        Predicate::contains(b"zzz-marker"),
    ];
    let before = db.find_verifications();
    assert_eq!(ids(db.find_and(&preds).unwrap()), vec![4242]);
    let cost = db.find_verifications() - before;
    assert!(
        cost <= 8,
        "a chainless needle must not turn this into a scan, verified {cost}"
    );
    assert_eq!(ids(db.find_and(&preds).unwrap()), oracle(&db, &preds));
}
