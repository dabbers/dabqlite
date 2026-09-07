//! What a bookmark store actually costs on this library.
//!
//! Everything here is measured, not estimated. Run with:
//!
//! ```text
//! cargo run --release --bin bench
//! ```
//!
//! The dataset is synthetic but realistically shaped: ~55-byte URLs,
//! ~30-byte titles, three tags each, drawn from a fixed pool so that
//! needle selectivity is known in advance. It is byte-for-byte the same
//! dataset the previous review measured, so the two tables compare.

// This crate is a CONSUMER of dabqlite, not part of its deterministic
// boundary, and a benchmark needs a clock.
#![allow(clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use bookmarks::{NewBookmark, Order, Query, Store};
use dabqlite::{Db, MemoryStorage, PosixStorage, Value};

const SIZES: [usize; 3] = [1_000, 10_000, 50_000];

const WORDS: [&str; 16] = [
    "rust", "async", "kernel", "graphics", "parser", "database", "network", "crypto", "wasm",
    "linux", "compiler", "search", "storage", "render", "protocol", "runtime",
];
const TAGS: [&str; 8] = [
    "read-later",
    "reference",
    "blog",
    "video",
    "paper",
    "tool",
    "docs",
    "howto",
];

fn make(i: usize) -> NewBookmark {
    let w = WORDS[i % WORDS.len()];
    let w2 = WORDS[(i * 7 + 3) % WORDS.len()];
    // ~55 bytes: four 16-byte slots on its own.
    let url = format!("https://{w}.example.com/{w2}/article-{i:06}");
    let title = format!("{w} {w2}: article number {i}");
    let mut tags = vec![TAGS[i % TAGS.len()].to_string(), w.to_string()];
    // One tag on every thousandth bookmark, for a high-selectivity needle.
    if i.is_multiple_of(1000) {
        tags.push("needle".to_string());
    }
    // Calibrated-selectivity tags: at n = 50_000 these match 1, 10, 100,
    // 1_000 and 10_000 rows respectively.
    if i == 0 {
        tags.push("qaaa".to_string());
    }
    if i.is_multiple_of(5000) {
        tags.push("qbbb".to_string());
    }
    if i.is_multiple_of(500) {
        tags.push("qccc".to_string());
    }
    if i.is_multiple_of(50) {
        tags.push("qddd".to_string());
    }
    if i.is_multiple_of(5) {
        tags.push("qeee".to_string());
    }
    NewBookmark {
        url,
        title,
        tags,
        added: 1_700_000_000 + i as u64,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Run `f` until it has taken at least 50 ms (or 30 times), return the mean.
fn timed<T>(mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut runs = 0u32;
    drop(f()); // warm up: the huge scans above leave the allocator noisy
    let start = Instant::now();
    let mut out = f();
    runs += 1;
    while start.elapsed() < Duration::from_millis(50) && runs < 30 {
        out = f();
        runs += 1;
    }
    (start.elapsed() / runs, out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("dabqlite bookmark benchmarks (release build)");
    println!(
        "VALUE_LEN={} MAX_VALUE_LEN={} MAX_COMMIT_ROWS={} (row slots)",
        dabqlite::VALUE_LEN,
        dabqlite::MAX_VALUE_LEN,
        dabqlite::MAX_COMMIT_ROWS
    );

    let only = std::env::args().nth(1);
    let want = |name: &str| only.as_deref().is_none_or(|o| o == name);
    if want("sizes") {
        for n in SIZES {
            bench_size(n)?;
        }
    }
    if want("raw") {
        bench_read_after_write()?;
    }
    if want("fsync") {
        bench_fsync()?;
    }
    Ok(())
}

/// Is a read cheap, or is the FIRST read after a write expensive?
fn bench_read_after_write() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n================ reads before and after a write ================");
    for n in SIZES {
        let items: Vec<NewBookmark> = (0..n).map(make).collect();
        let mut s: Store<MemoryStorage> = Store::in_memory_with(n as u64 * 14)?;
        s.import(&items)?;
        let db = s.db();
        let key = |i: usize| (i as u64 % n as u64) + 1;

        // 200 reads, no writes in between.
        let t = Instant::now();
        for i in 0..200 {
            db.get(key(i * 37)).unwrap();
        }
        let warm = t.elapsed() / 200;

        // 200 reads, each preceded by one write.
        let t = Instant::now();
        for i in 0..200 {
            db.put(key(i * 37), Value::from_bytes(b"x").unwrap())
                .unwrap();
            db.get(key(i * 37 + 11)).unwrap();
        }
        let mixed = t.elapsed() / 200;

        // One write, then one read, then a second read.
        db.put(key(5), Value::from_bytes(b"y").unwrap()).unwrap();
        let t = Instant::now();
        db.get(key(9)).unwrap();
        let first = t.elapsed();
        let t = Instant::now();
        db.get(key(9)).unwrap();
        let second = t.elapsed();

        println!(
            "n={n:<6} read alone {:>8.4} ms | write+read pair {:>8.4} ms | \
             first read after a write {:>8.4} ms | next read {:>8.4} ms",
            ms(warm),
            ms(mixed),
            ms(first),
            ms(second)
        );
    }
    Ok(())
}

fn bench_size(n: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!("\n================ {n} bookmarks ================");
    let items: Vec<NewBookmark> = (0..n).map(make).collect();

    let capacity = (n as u64) * 14;
    let t = Instant::now();
    let mut s: Store<MemoryStorage> = Store::in_memory_with(capacity)?;
    s.import(&items)?;
    let build = t.elapsed();
    let st = s.stats();
    println!(
        "build          {:>9.1} ms   ({:.1} us/bookmark, ~13 bookmarks per commit)",
        ms(build),
        build.as_secs_f64() * 1e6 / n as f64
    );
    println!(
        "rows           live {} slots {} dead {} ({:.0}% of capacity {})",
        st.live,
        st.slots,
        st.dead,
        st.fill() * 100.0,
        st.capacity
    );
    println!(
        "               {:.1} slots per bookmark ({} live rows for {n} bookmarks + 1 header)",
        st.slots as f64 / n as f64,
        st.live
    );
    let blob = s.to_blob()?;
    println!(
        "blob           {} bytes ({:.0} bytes/bookmark)",
        blob.len(),
        blob.len() as f64 / n as f64
    );

    // --- full scans --------------------------------------------------------
    let (d, rows) = timed(|| s.db().all().unwrap());
    println!(
        "\nDb::all()               {:>9.2} ms   {} rows   ({:.2} us/row)",
        ms(d),
        rows.len(),
        d.as_secs_f64() * 1e6 / rows.len() as f64
    );
    let (d, list) = timed(|| s.list().unwrap());
    println!(
        "Store::list()           {:>9.2} ms   {} bookmarks (scan + decode)",
        ms(d),
        list.len()
    );

    // --- find, by selectivity ---------------------------------------------
    println!("\nDb::find (the library's own substring index)");
    for needle in ["zzqx", "needle", "read-later", "http"] {
        let (d, hits) = timed(|| s.db().find(needle.as_bytes()).unwrap());
        println!(
            "  {:<12} {:>9.3} ms   {:>6} rows matched",
            format!("{:?}", needle),
            ms(d),
            hits.len()
        );
    }

    // The same questions answered by a scan, for comparison.
    println!("\nthe same questions, answered by a full scan in Rust");
    for needle in ["zzqx", "needle", "read-later", "http"] {
        let (d, hits) = timed(|| {
            s.db()
                .all()
                .unwrap()
                .into_iter()
                .filter(|(_, v)| contains(v.as_bytes(), needle.as_bytes()))
                .count()
        });
        println!(
            "  {:<12} {:>9.3} ms   {:>6} rows matched",
            format!("{:?}", needle),
            ms(d),
            hits
        );
    }

    // --- what "stop after the first page" is actually worth ----------------
    //
    // `find_page` is documented as costing the same per page however many
    // matches there are. That holds while every value fits one slot. A
    // bookmark does not, and the engine takes the exhaustive path once any
    // value spans more than one row, so the first page of a RARE needle
    // scans the whole table.
    println!("\nfirst page of 8 vs every match (Db::find_page vs Db::find)");
    for needle in ["zzqx", "needle", "read-later", "http"] {
        let (dp, page) = timed(|| s.db().find_page(needle.as_bytes(), None).unwrap());
        let (da, all) = timed(|| s.db().find(needle.as_bytes()).unwrap());
        println!(
            "  {:<12} first page {:>9.3} ms ({} rows) | all matches {:>9.3} ms ({} rows)",
            format!("{:?}", needle),
            ms(dp),
            page.0.len(),
            ms(da),
            all.len()
        );
    }

    // --- the queries a bookmark manager actually issues --------------------
    println!("\nthe queries a bookmark manager issues");
    let (d, hits) = timed(|| s.by_tag("needle").unwrap());
    println!(
        "  by_tag(\"needle\")      {:>9.2} ms   {} bookmarks (exact tag, index-served)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| s.search_tag("needle").unwrap());
    println!(
        "  search_tag(\"needle\")  {:>9.2} ms   {} bookmarks (tag prefix, index-served)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| s.search("needle").unwrap());
    println!(
        "  search(\"needle\")      {:>9.2} ms   {} bookmarks (full scan, case-folded)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| s.search("article-000500").unwrap());
    println!(
        "  search(\"article-0005\")  {:>7.2} ms   {} bookmarks (14-byte needle: scan)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| s.find_exact("article-000500").unwrap());
    println!(
        "  find_exact(same)      {:>9.2} ms   {} bookmarks (index; needle spans two slots)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| s.search("kernel").unwrap());
    println!(
        "  search(\"kernel\")      {:>9.2} ms   {} bookmarks (full scan)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| s.search_page("kernel", None, 20).unwrap());
    println!(
        "  search_page(\"kernel\",20) {:>6.2} ms   {} bookmarks (index, first 20)",
        ms(d),
        hits.0.len()
    );
    let mid = (n / 2) as u64;
    for (label, id) in [("first", 1u64), ("middle", mid), ("last", n as u64)] {
        let (d, _) = timed(|| s.get_bookmark(id).unwrap());
        let (dg, _) = timed(|| s.db().get(id).unwrap());
        println!(
            "  get_bookmark id={:<7} {:>8.4} ms   ({:>8.4} ms bare Db::get)",
            label,
            ms(d),
            ms(dg)
        );
    }
    let (d, page) = timed(|| s.page(0, 20).unwrap());
    println!(
        "  page(0, 20)           {:>9.4} ms   {} bookmarks",
        ms(d),
        page.len()
    );
    let (d, page) = timed(|| s.page(mid, 20).unwrap());
    println!(
        "  page(n/2, 20)         {:>9.4} ms   {} bookmarks",
        ms(d),
        page.len()
    );
    let (d, hits) = timed(|| {
        s.query(&Query {
            text: Some("kernel".into()),
            tags: vec!["docs".into()],
            since: Some(1_700_000_000),
            order: Order::NewestFirst,
            limit: Some(20),
            ..Query::default()
        })
        .unwrap()
    });
    println!(
        "  query(text+tag+range) {:>9.2} ms   {} bookmarks (scan + sort in Rust)",
        ms(d),
        hits.len()
    );
    let (d, hits) = timed(|| {
        s.query(&Query {
            order: Order::NewestFirst,
            limit: Some(20),
            ..Query::default()
        })
        .unwrap()
    });
    println!(
        "  20 newest             {:>9.2} ms   {} bookmarks (whole table, then sort)",
        ms(d),
        hits.len()
    );

    // --- writes ------------------------------------------------------------
    println!("\nwrites");
    let before = s.stats().slots;
    let (d, _) = timed(|| s.visit(mid).unwrap());
    let per_visit = (s.stats().slots - before) as f64;
    println!(
        "  visit(id)             {:>9.4} ms   read + rewrite of the whole value \
         ({:.0} slots per call)",
        ms(d),
        per_visit / 31.0
    );
    let (d, _) = timed(|| {
        s.set_title(mid, "a different title for this bookmark")
            .unwrap()
    });
    println!("  set_title(id)         {:>9.4} ms   read + one put", ms(d));
    // --- how find scales with the number of MATCHES ------------------------
    println!("\nDb::find by number of matches (same database, same call)");
    println!("  needle        matches      ms      us/match");
    for needle in ["qaaa", "qbbb", "qccc", "qddd", "qeee"] {
        let (d, hits) = timed(|| s.db().find(needle.as_bytes()).unwrap());
        println!(
            "  {:<12} {:>7}  {:>9.3}  {:>9.2}",
            needle,
            hits.len(),
            ms(d),
            if hits.is_empty() {
                0.0
            } else {
                d.as_secs_f64() * 1e6 / hits.len() as f64
            }
        );
    }

    // --- how find scales with NEEDLE LENGTH -------------------------------
    // What a search-as-you-type box does: h, ht, htt, http, ...
    println!("\nDb::find as a user types (every URL starts \"https://\")");
    for needle in ["h", "ht", "htt", "http", "https:/", "s://kern"] {
        let (d, hits) = timed(|| s.db().find(needle.as_bytes()).unwrap());
        let (dp, page) = timed(|| s.db().find_page(needle.as_bytes(), None).unwrap());
        println!(
            "  {:<10} {:>10.3} ms   {:>6} rows matched   (first page of 8: {:>8.3} ms, {} rows)",
            format!("{:?}", needle),
            ms(d),
            hits.len(),
            ms(dp),
            page.0.len()
        );
    }

    // --- bulk delete, last so it does not perturb the tables above --------
    let victims: Vec<u64> = (1..=100).collect();
    let t = Instant::now();
    let removed = s.remove_many(&victims)?;
    println!(
        "\nremove_many(100) {:>7.4} ms   {removed} bookmarks in ONE commit \
(was ~6 bookmarks per commit)",
        ms(t.elapsed())
    );

    // --- compaction --------------------------------------------------------
    let t = Instant::now();
    s.compact()?;
    println!(
        "\ncompact()      {:>9.1} ms   -> slots {} dead {}",
        ms(t.elapsed()),
        s.stats().slots,
        s.stats().dead
    );
    Ok(())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// What `batch` is worth where fsyncs are real: the file backend.
fn bench_fsync() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n================ durable writes (PosixStorage) ================");
    let n = 500usize;
    let items: Vec<NewBookmark> = (0..n).map(make).collect();

    let dir = std::env::temp_dir().join(format!("bookmarks-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let t = Instant::now();
    {
        let mut s: Store<PosixStorage> = Store::open_with(&dir, 100_000)?;
        s.import(&items)?;
    }
    let packed = t.elapsed();
    println!(
        "{n} bookmarks, import (packed commits) {:>7.1} ms   ({:.2} ms/bookmark)",
        ms(packed),
        ms(packed) / n as f64
    );

    // The same bookmarks, one commit each -- what `import` used to be.
    let dir1 = std::env::temp_dir().join(format!("bookmarks-bench1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir1);
    let t = Instant::now();
    {
        let mut s: Store<PosixStorage> = Store::open_with(&dir1, 100_000)?;
        for it in &items {
            s.add(&it.url, &it.title, &it.tags, it.added)?;
        }
    }
    let one_each = t.elapsed();
    println!(
        "the same, one commit per bookmark      {:>7.1} ms   ({:.2} ms/bookmark)",
        ms(one_each),
        ms(one_each) / n as f64
    );

    // And what it cost when a bookmark was fifty rows and each row was its
    // own commit -- the shape this crate had before `Db::batch` existed.
    let dir2 = std::env::temp_dir().join(format!("bookmarks-bench2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir2);
    let t = Instant::now();
    let mut rows = 0u64;
    {
        let mut db: Db<PosixStorage> = Db::open_with(&dir2, 100_000)?;
        for (i, it) in items.iter().enumerate() {
            let base = (i as u64 + 1) << 24;
            let mut k = 0u64;
            for c in it.url.as_bytes().chunks(dabqlite::VALUE_LEN) {
                db.put(base | k, Value::from_bytes(c)?)?;
                k += 1;
            }
            for c in it.title.as_bytes().chunks(dabqlite::VALUE_LEN) {
                db.put(base | 1 << 16 | k, Value::from_bytes(c)?)?;
                k += 1;
            }
            for (j, t) in it.tags.iter().enumerate() {
                db.put(base | 2 << 16 | j as u64, Value::from_text(t)?)?;
            }
            db.put(base | 3 << 16, Value::from_bytes(b"meta")?)?;
            rows = db.stats().slots;
        }
    }
    let chunked = t.elapsed();
    println!(
        "the old shape: {rows} rows, one commit each {:>4.1} ms   ({:.2} ms/bookmark)",
        ms(chunked),
        ms(chunked) / n as f64
    );
    println!(
        "packed import is {:.1}x faster than one commit per bookmark, \
         {:.1}x faster than the old chunked shape",
        one_each.as_secs_f64() / packed.as_secs_f64(),
        chunked.as_secs_f64() / packed.as_secs_f64()
    );
    for d in [&dir, &dir1, &dir2] {
        let _ = std::fs::remove_dir_all(d);
    }
    Ok(())
}
