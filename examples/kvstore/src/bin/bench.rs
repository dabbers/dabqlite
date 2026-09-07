//! What the `kv` commands cost, on a real file-backed database.
//!
//! `cargo run --release --bin bench` runs everything; pass `ops`,
//! `value`, or `capacity` to run one section. All of it goes through the
//! same `Store` the CLI uses, so the numbers are the numbers.
//!
//! The repository's clippy config disallows a clock inside the
//! deterministic boundary. A benchmark is outside it.
#![allow(clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use kvstore::store::Store;
use kvstore::Config;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn us_per(d: Duration, n: usize) -> f64 {
    d.as_secs_f64() * 1e6 / n as f64
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("kv-bench-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn main() {
    let which: Vec<String> = std::env::args().skip(1).collect();
    let want = |s: &str| which.is_empty() || which.iter().any(|w| w == s);
    if want("ops") {
        ops();
    }
    if want("value") {
        value_length();
    }
    if want("capacity") {
        capacity_cost();
    }
}

/// The cost of each CLI operation, at three key counts and three value
/// sizes. Capacity is sized to the data, so `open` is not paying for an
/// arena nobody uses (see `capacity_cost`).
fn ops() {
    println!(
        "\n{:>7} {:>6} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "keys",
        "vbytes",
        "open ms",
        "set us",
        "get us",
        "list ms",
        "find ms",
        "scan ms",
        "purge ms",
        "cmpct ms"
    );
    for n in [100usize, 1_000, 10_000] {
        for vbytes in [16usize, 256, 2000] {
            one(n, vbytes);
        }
    }
}

fn one(n: usize, vbytes: usize) {
    let dir = tempdir(&format!("{n}-{vbytes}"));
    let slots_each = (12 + 16 + vbytes).div_ceil(16) as u64;
    // Room for the records, the versions an update supersedes, and the
    // tombstones a purge writes.
    let cfg = Config::new(&dir).with_rows(n as u64 * slots_each * 3 + 4_096);
    let value = vec![b'x'; vbytes];

    let t = Instant::now();
    let mut store = Store::open(&cfg).expect("open");
    let open = t.elapsed();

    let t = Instant::now();
    for i in 0..n {
        store
            .set(&format!("key/{i:07}"), &value, 0, 0)
            .expect("set");
    }
    let set = t.elapsed();

    let t = Instant::now();
    for i in 0..n {
        assert!(store.get(&format!("key/{i:07}"), 0).expect("get").is_some());
    }
    let get = t.elapsed();

    // `list`: a full scan plus a sort by key, because ids are hashes.
    let t = Instant::now();
    let all = store.entries(0).expect("list");
    let list = t.elapsed();
    assert_eq!(all.len(), n);

    // `search` for something ONE record holds, served by the library's
    // index, against the same answer the way this crate used to get it:
    // materialise every record and compare in Rust.
    let needle = b"one-rare-needle!";
    store
        .set("rare", &[&value[..], &needle[..]].concat(), 0, 0)
        .expect("set rare");
    let t = Instant::now();
    let hits = store.search(needle, false, 0).expect("search");
    let find = t.elapsed();
    assert_eq!(hits.len(), 1);

    let t = Instant::now();
    let scanned = store
        .entries(0)
        .expect("scan")
        .into_iter()
        .filter(|e| e.value.windows(needle.len()).any(|w| w == needle))
        .count();
    let scan = t.elapsed();
    assert_eq!(scanned, hits.len());
    store.del("rare", 0).expect("del rare");

    // Expire half of them and retire the lot in batched commits.
    for i in 0..n / 2 {
        store
            .set(&format!("key/{i:07}"), &value, 100, 0)
            .expect("ttl");
    }
    let t = Instant::now();
    let purged = store.purge(200).expect("purge");
    let purge = t.elapsed();
    assert_eq!(purged.len(), n / 2);

    let slots = store.stats().slots;
    drop(store);

    let t = Instant::now();
    let out = kvstore::exec::execute(&cfg, &kvstore::exec::Command::Compact, &mut std::io::sink())
        .expect("compact");
    let compact = t.elapsed();
    if let kvstore::exec::Outcome::Compacted { after, .. } = out {
        println!(
            "{n:>7} {vbytes:>6} {:>8.2} {:>8.1} {:>8.1} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>9.2} \
             slots {slots} -> {}",
            ms(open),
            us_per(set, n),
            us_per(get, n),
            ms(list),
            ms(find),
            ms(scan),
            ms(purge),
            ms(compact),
            after.slots,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// One read of one key, against the length of its value. A value is
/// reassembled from bounded windows, one engine call each.
fn value_length() {
    println!(
        "\n{:>7} {:>6} {:>10} {:>12}",
        "vbytes", "slots", "get us", "us per slot"
    );
    let dir = tempdir("valuelen");
    let cfg = Config::new(&dir).with_rows(65_536);
    let mut store = Store::open(&cfg).expect("open");
    for vbytes in [16usize, 64, 128, 256, 512, 1024, 2035] {
        let value = vec![b'x'; vbytes];
        store.set("k", &value, 0, 0).expect("set");
        let reps = 200;
        let t = Instant::now();
        for _ in 0..reps {
            assert_eq!(store.get("k", 0).expect("get").unwrap().value.len(), vbytes);
        }
        let each = us_per(t.elapsed(), reps);
        let slots = (12 + 1 + vbytes).div_ceil(16);
        println!(
            "{vbytes:>7} {slots:>6} {each:>10.2} {:>12.3}",
            each / slots as f64
        );
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a DECLARED capacity costs before a single row is written. The
/// arena is allocated at open, so this is memory and latency you pay for
/// room you may never use.
fn capacity_cost() {
    println!(
        "\n{:>12} {:>10} {:>12}",
        "capacity", "open ms", "empty get us"
    );
    for rows in [1_024u64, 65_536, 1_000_000, 4_000_000, 16_000_000] {
        let dir = tempdir(&format!("cap{rows}"));
        let cfg = Config::new(&dir).with_rows(rows);
        let t = Instant::now();
        let mut store = Store::open(&cfg).expect("open");
        let open = t.elapsed();
        let t = Instant::now();
        for _ in 0..100 {
            assert!(store.get("absent", 0).expect("get").is_none());
        }
        let get = us_per(t.elapsed(), 100);
        println!("{rows:>12} {:>10.2} {get:>12.2}", ms(open));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
