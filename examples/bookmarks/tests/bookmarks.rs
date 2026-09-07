//! Integration tests for the bookmark store.
//!
//! Several of these exist to pin down a *limitation* of the underlying
//! library rather than a feature of this crate. Those are named so.

use bookmarks::{
    meta_key, read_store, url_chunk_key, with_store, with_store_capacity, Bookmark, Cmd, Reply,
    Store, StoreError,
};
use dabqlite::{Error as DbErr, VALUE_LEN};

const MDN: &str = "https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API";
const RUST: &str = "https://doc.rust-lang.org/std/index.html";
const SQLITE: &str = "https://www.sqlite.org/whentouse.html";

fn tags(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Three bookmarks in a fresh database; returns the blob.
fn seeded() -> Vec<u8> {
    let (_, blob, _) = with_store(None, |s| {
        s.add(RUST, "Rust standard library", &tags(&["rust", "docs"]))?;
        s.add(SQLITE, "When to use SQLite", &tags(&["sqlite", "db", "docs"]))?;
        s.add(MDN, "IndexedDB API", &tags(&["browser", "db"]))?;
        Ok(())
    })
    .expect("seed");
    blob
}

fn rows_in(s: &mut Store, lo: u64, hi: u64) -> usize {
    match s.exec_raw(Cmd::Range(lo, hi)).expect("range") {
        Reply::Rows(r) => r.len(),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------

#[test]
fn a_session_round_trips_through_one_opaque_blob() {
    let (before, blob, report) = with_store(None, |s| {
        s.add(RUST, "Rust standard library", &tags(&["rust", "docs"]))?;
        s.add(SQLITE, "When to use SQLite", &tags(&["sqlite", "db"]))?;
        s.add(MDN, "IndexedDB API", &tags(&["browser", "db"]))?;
        s.list()
    })
    .expect("build");

    assert_eq!(before.len(), 3);
    assert_eq!(report.blob_len, blob.len());

    // The database is gone; only `blob` survives. Reload from it alone.
    let after = read_store(&blob, |s| s.list()).expect("reload");
    assert_eq!(after, before, "the reloaded database is not the same database");

    // And it is still writable after a reload, with ids that continue where
    // the previous session left off rather than colliding.
    let (id, blob2, _) = with_store(Some(&blob), |s| s.add("https://example.com", "Example", &[]))
        .expect("write after reload");
    assert_eq!(id, 4);
    let all = read_store(&blob2, |s| s.list()).expect("reload again");
    assert_eq!(all.len(), 4);
    assert_eq!(all[3].url, "https://example.com");
}

#[test]
fn urls_and_titles_are_far_longer_than_a_row_and_survive_exactly() {
    let long_url = format!("https://example.com/{}", "a/".repeat(400)); // > 800 bytes
    let long_title = "Ünïcödé — a title with multi-byte characters that is comfortably \
                      longer than sixteen bytes and lands a code point across a chunk edge"
        .to_string();
    assert!(long_url.len() > VALUE_LEN * 50);

    let (_, blob, _) = with_store(None, |s| s.add(&long_url, &long_title, &tags(&["long"])))
        .expect("add");
    let b = read_store(&blob, |s| s.get_bookmark(1))
        .expect("read")
        .expect("present");
    assert_eq!(b.url, long_url, "a long URL did not survive chunking");
    assert_eq!(b.title, long_title, "multi-byte text split across chunks");
}

#[test]
fn tag_search_is_served_by_the_libraries_own_index() {
    let blob = seeded();
    let hits = read_store(&blob, |s| s.search_tag("db")).expect("tag search");
    let ids: Vec<u64> = hits.iter().map(|b| b.id).collect();
    assert_eq!(ids, vec![2, 3]);

    // Substring, not equality: "ocs" matches the tag "docs".
    let ids: Vec<u64> = read_store(&blob, |s| s.search_tag("ocs"))
        .unwrap()
        .iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(ids, vec![1, 2]);

    // Case folding is ours, not the library's: it is applied at write time.
    assert_eq!(
        read_store(&blob, |s| s.search_tag("DB")).unwrap().len(),
        2,
        "tags are folded on write, so an uppercase needle still matches"
    );
    assert!(read_store(&blob, |s| s.search_tag("nope")).unwrap().is_empty());
}

#[test]
fn full_text_search_finds_what_the_builtin_index_structurally_cannot() {
    let blob = seeded();

    // "org/en-US" sits at bytes 26..35 of the MDN URL, straddling the
    // boundary between chunk 1 and chunk 2. `Db::find` only ever looks
    // inside one 16-byte value, so it cannot see it...
    let raw_hits = read_store(&blob, |s| {
        Ok(match s.exec_raw(Cmd::Find(b"org/en-US".to_vec()))? {
            Reply::Rows(r) => r.len(),
            other => panic!("{other:?}"),
        })
    })
    .expect("raw find");
    assert_eq!(
        raw_hits, 0,
        "if this ever becomes non-zero the library grew cross-row matching"
    );

    // ...so our search is a full scan, and it does find it.
    let hits = read_store(&blob, |s| s.search("org/en-US")).expect("search");
    assert_eq!(hits.iter().map(|b| b.id).collect::<Vec<_>>(), vec![3]);

    // Same for a needle longer than a whole row, which `find` refuses
    // outright.
    let long = "developer.mozilla.org/en-US/docs";
    assert!(long.len() > VALUE_LEN);
    assert_eq!(
        read_store(&blob, |s| Ok(s.exec_raw(Cmd::Find(long.as_bytes().to_vec())))).unwrap(),
        Err(DbErr::ValueTooLong {
            len: long.len(),
            max: VALUE_LEN
        })
    );
    assert_eq!(
        read_store(&blob, |s| s.search(long)).unwrap().len(),
        1,
        "the scan handles needles of any length"
    );

    // Search covers titles and tags too, case-insensitively.
    assert_eq!(read_store(&blob, |s| s.search("SQLite")).unwrap().len(), 1);
    assert_eq!(read_store(&blob, |s| s.search("docs")).unwrap().len(), 3);
}

#[test]
fn deleting_removes_every_row_a_bookmark_owned() {
    let blob = seeded();
    let (report, blob, _) = with_store(Some(&blob), |s| {
        let before = rows_in(s, 2 << 24, (2 << 24) | 0xFF_FFFF);
        assert!(before > 1, "a bookmark is many rows: {before}");
        assert!(s.remove(2)?);
        assert!(!s.remove(2)?, "a second delete is a no-op");
        assert!(!s.remove(999)?);
        let after = rows_in(s, 2 << 24, (2 << 24) | 0xFF_FFFF);
        Ok((before, after))
    })
    .expect("delete");
    assert_eq!(report.1, 0, "{} rows survived a delete", report.1);

    let left = read_store(&blob, |s| s.list()).expect("list");
    assert_eq!(left.iter().map(|b| b.id).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(read_store(&blob, |s| s.get_bookmark(2)).unwrap(), None);
    // A deleted bookmark drops out of the index-backed search as well.
    assert_eq!(
        read_store(&blob, |s| s.search_tag("db"))
            .unwrap()
            .iter()
            .map(|b| b.id)
            .collect::<Vec<_>>(),
        vec![3]
    );
    // Its id is not handed out again after a reload.
    let (id, _, _) = with_store(Some(&blob), |s| s.add("https://x.test", "X", &[])).unwrap();
    assert_eq!(id, 4, "a deleted id must not be reused");
}

#[test]
fn updating_rewrites_fields_and_releases_the_chunks_it_no_longer_needs() {
    let long = format!("https://example.com/{}", "z".repeat(200));
    let (_, blob, _) = with_store(None, |s| s.add(&long, "Long", &tags(&["a", "b", "c"])))
        .expect("add");

    let (counts, blob, _) = with_store(Some(&blob), |s| {
        let wide = rows_in(s, url_chunk_key(1, 0), url_chunk_key(1, 0xFFFF));
        s.set_url(1, "https://x.test")?;
        s.set_title(1, "Short")?;
        s.set_tags(1, &tags(&["only"]))?;
        let narrow = rows_in(s, url_chunk_key(1, 0), url_chunk_key(1, 0xFFFF));
        Ok((wide, narrow))
    })
    .expect("update");
    assert!(counts.0 > 10, "a 220-byte URL needs many rows: {}", counts.0);
    assert_eq!(counts.1, 1, "shrinking a URL must free its extra chunks");

    let b = read_store(&blob, |s| s.get_bookmark(1)).unwrap().unwrap();
    assert_eq!(
        b,
        Bookmark {
            id: 1,
            url: "https://x.test".into(),
            title: "Short".into(),
            tags: tags(&["only"]),
        }
    );
    // The old value is really gone, not merely unreferenced.
    assert!(read_store(&blob, |s| s.search("zzz")).unwrap().is_empty());
    assert!(read_store(&blob, |s| s.search_tag("a")).unwrap().is_empty());

    // Updating something absent is an error, not a silent insert.
    let e = with_store(Some(&blob), |s| s.set_title(42, "nope")).unwrap_err();
    assert_eq!(e, StoreError::NotFound(42));
}

#[test]
fn a_snapshot_written_by_a_previous_process_reloads_intact() {
    // A real previous run: the CLI binary, in its own process, twice.
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

    assert_eq!(run(&["add", RUST, "Rust standard library", "rust", "docs"]).trim(), "added 1");
    assert_eq!(run(&["add", MDN, "IndexedDB API", "browser", "db"]).trim(), "added 2");
    // A separate process again: mutate what run #1 wrote.
    run(&["title", "2", "IndexedDB, revisited"]);
    run(&["rm", "1"]);
    run(&["add", SQLITE, "When to use SQLite", "sqlite", "db"]);

    // Now open the file this library never saw in *this* process.
    let blob = std::fs::read(&db).expect("the previous run's snapshot");
    let all = read_store(&blob, |s| s.list()).expect("load a foreign snapshot");
    assert_eq!(
        all.iter().map(|b| (b.id, b.title.as_str())).collect::<Vec<_>>(),
        vec![(2, "IndexedDB, revisited"), (3, "When to use SQLite")]
    );
    assert_eq!(all[0].url, MDN);
    assert_eq!(all[1].tags, tags(&["db", "sqlite"]));

    // The CLI agrees with us.
    let listed = run(&["list"]);
    assert!(listed.contains("IndexedDB, revisited"), "{listed}");
    assert!(!listed.contains("Rust standard"), "{listed}");
    assert!(run(&["tag", "db"]).contains("(2 bookmarks)"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_half_finished_multi_row_write_is_ours_to_detect_and_repair() {
    // There are no transactions: a bookmark is 5-30 separate commits. This
    // fakes the crash by deleting the meta row that `add` writes last.
    let (_, blob, _) = with_store(None, |s| {
        s.add(RUST, "Rust standard library", &tags(&["rust"]))?;
        s.add(MDN, "IndexedDB API", &tags(&["browser"]))?;
        Ok(())
    })
    .unwrap();

    let (_, torn, _) = with_store(Some(&blob), |s| {
        s.exec_raw(Cmd::Delete(meta_key(2)))?;
        Ok(())
    })
    .unwrap();

    let (found, repaired, _) = with_store(Some(&torn), |s| {
        // The truncated bookmark reads as absent, not as a bookmark with an
        // empty URL — that is what writing meta last buys.
        assert_eq!(s.get_bookmark(2)?, None);
        assert_eq!(s.list()?.len(), 1);
        let before = s.integrity()?;
        let deleted = s.repair()?;
        Ok((before, deleted))
    })
    .unwrap();
    assert_eq!(found.0.bookmarks, 1);
    assert_eq!(found.0.orphans, vec![2]);
    assert!(found.0.stray_rows >= 3, "{:?}", found.0);
    assert_eq!(found.1, found.0.stray_rows);

    let clean = read_store(&repaired, |s| s.integrity()).unwrap();
    assert_eq!(clean.orphans, Vec::<u64>::new());
    assert_eq!(clean.bookmarks, 1);
}

#[test]
fn dead_slots_pile_up_until_something_compacts() {
    let (_, blob, report) = with_store_capacity(None, 4096, |s| {
        for i in 0..20 {
            s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]))?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(report.stats.dead, 0, "inserts alone leave no dead weight");
    assert_eq!(report.stats.capacity, 4096);
    let clean_slots = report.stats.slots;
    // 20 bookmarks * (1 meta + 2 url chunks + 1 title + 1 tag) + 1 header row.
    // Five rows for a 21-byte URL and a one-letter title is the tax a
    // 16-byte value charges, and it is charged against the row capacity.
    assert_eq!(clean_slots, 101, "a bookmark costs several rows: {clean_slots}");

    // Rewriting the same bookmark 30 times burns slots that stay burnt.
    let (_, blob, report) = with_store_capacity(Some(&blob), 4096, |s| {
        for i in 0..30 {
            s.set_title(1, &format!("title {i}"))?;
        }
        Ok(())
    })
    .unwrap();
    assert!(
        report.stats.dead > 50,
        "updates should leave dead slots: {:?}",
        report.stats
    );
    assert!(report.stats.slots > clean_slots + 50);

    // Compaction gives them back, and changes nothing a caller can see.
    let before = read_store(&blob, |s| s.list()).unwrap();
    let (_, blob, report) = with_store_capacity(Some(&blob), 4096, |s| {
        s.request_compaction();
        Ok(())
    })
    .unwrap();
    assert!(report.compacted);
    assert_eq!(report.stats.dead, 0);
    assert_eq!(report.stats.slots, report.stats.live);
    assert_eq!(read_store(&blob, |s| s.list()).unwrap(), before);
}

#[test]
fn the_snapshot_does_not_carry_the_capacity_it_was_written_with() {
    // This is a finding, not a feature: the row capacity is declared at open
    // and is *not* in the blob, so a caller has to remember it out of band or
    // silently get a different database back.
    let (_, blob, small) = with_store_capacity(None, 512, |s| {
        s.add(RUST, "Rust standard library", &tags(&["rust"]))?;
        Ok(())
    })
    .unwrap();
    assert_eq!(small.stats.capacity, 512);

    let (_, _, reloaded) = with_store(Some(&blob), |_| Ok(())).unwrap();
    assert_eq!(
        reloaded.stats.capacity,
        dabqlite::DEFAULT_ROWS,
        "the reloaded database has a different ceiling than the one that was saved"
    );
    assert_ne!(small.stats.capacity, reloaded.stats.capacity);
}

#[test]
fn a_full_database_refuses_rather_than_corrupting_anything() {
    // 64 slots is a handful of bookmarks. The wall is reported honestly and
    // everything written before it is still readable.
    let err = with_store_capacity(None, 64, |s| {
        for i in 0..50 {
            s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]))?;
        }
        Ok(())
    })
    .unwrap_err();
    assert_eq!(err, StoreError::Db(DbErr::Full { capacity: 64 }));

    // ...but note what a caller loses: the session aborted, so the partial
    // work has no blob. Recovering means replaying from a bigger capacity.
    let (n, _, _) = with_store_capacity(None, 4096, |s| {
        for i in 0..50 {
            s.add(&format!("https://example.com/{i}"), "T", &tags(&["x"]))?;
        }
        s.count()
    })
    .unwrap();
    assert_eq!(n, 50);
}

#[test]
fn a_damaged_blob_is_refused_instead_of_guessed_at() {
    let blob = seeded();
    assert!(matches!(
        read_store(b"this is not a snapshot at all", |s| s.list()),
        Err(StoreError::Db(DbErr::Corrupt { .. }))
    ));
    let mut truncated = blob.clone();
    truncated.truncate(blob.len() - 1);
    assert!(matches!(
        read_store(&truncated, |s| s.list()),
        Err(StoreError::Db(DbErr::Corrupt { .. }))
    ));
    // The intact one still works, so the test is testing what it thinks.
    assert_eq!(read_store(&blob, |s| s.count()).unwrap(), 3);
}

#[test]
fn the_row_shaped_limits_are_reported_not_silently_applied() {
    let e = with_store(None, |s| s.add(RUST, "T", &tags(&["a-tag-far-longer-than-a-row"])))
        .unwrap_err();
    assert_eq!(
        e,
        StoreError::TagTooLong {
            tag: "a-tag-far-longer-than-a-row".into(),
            max: VALUE_LEN
        }
    );
    // Exactly a row's worth is fine.
    let sixteen = "0123456789abcdef";
    assert_eq!(sixteen.len(), VALUE_LEN);
    let (_, blob, _) = with_store(None, |s| s.add(RUST, "T", &tags(&[sixteen]))).unwrap();
    assert_eq!(
        read_store(&blob, |s| s.get_bookmark(1)).unwrap().unwrap().tags,
        tags(&[sixteen])
    );
}
