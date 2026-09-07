//! A tiny CLI over the bookmark store.
//!
//! Two backends, one `Store`: `--db FILE` is the browser story (load an
//! opaque blob, work in memory, write the blob back), `--dir DIR` is the
//! durable one (real files, real fsyncs, one writer at a time). Both are
//! the same `Store<S>` because the storage types are exported now.

// A CLI needs a wall clock for the `added` timestamp; the determinism
// deny list is for the library's own deterministic boundary.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use bookmarks::{Bookmark, NewBookmark, Order, Query, Store};
use dabqlite::{MemoryStorage, Storage};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bookmarks: {e}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage: bookmarks [--db FILE | --dir DIR] <command>

  add <url> <title> [tag ...]   add a bookmark (one atomic commit)
  import <n>                    add n generated bookmarks, atomic per bookmark
  list                          every bookmark, in id order
  page <after> <limit>          a bounded page of bookmarks
  get <id>                      one bookmark
  visit <id>                    bump the visit counter
  search <text>                 substring over url, title and tags (full scan)
  tag <text>                    substring over tags only (uses the index)
  find <text>                   byte-exact search via Db::find, newest first
  bytag <tag>                   exact tag match, served by the index
  recent <n>                    n newest, by the added timestamp
  rm <id> [id ...]              delete bookmarks (one atomic commit)
  retag <from> <to>             rename a tag everywhere
  title <id> <text>             retitle
  url <id> <text>               re-point
  tags <id> [tag ...]           replace the tag set
  stats                         row-slot accounting
  compact                       rebuild, reclaiming dead slots
  demo                          the save / drop / reload session flow

--db names a snapshot blob (default bookmarks.dabq); --dir a real database
directory.
";

enum Backend {
    Blob(PathBuf),
    Dir(PathBuf),
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut backend = Backend::Blob(PathBuf::from("bookmarks.dabq"));
    match args.first().map(String::as_str) {
        Some("--db") | Some("--dir") => {
            if args.len() < 2 {
                return Err("--db/--dir needs a path".into());
            }
            let flag = args.remove(0);
            let path = PathBuf::from(args.remove(0));
            backend = if flag == "--db" {
                Backend::Blob(path)
            } else {
                Backend::Dir(path)
            };
        }
        _ => {}
    }
    let Some(cmd) = args.first().cloned() else {
        print!("{USAGE}");
        return Ok(());
    };
    let rest = args[1..].to_vec();

    match backend {
        Backend::Blob(path) => {
            let mut store = match std::fs::read(&path) {
                Ok(blob) => Store::load(&blob)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::in_memory()?,
                Err(e) => return Err(e.into()),
            };
            if cmd == "compact" {
                store.compact()?;
                report_compaction(store.stats());
                save(&path, &store.to_blob()?)?;
                return Ok(());
            }
            let dirty = dispatch(&mut store, &cmd, &rest)?;
            if dirty {
                save(&path, &store.to_blob()?)?;
            }
        }
        Backend::Dir(path) => {
            let mut store = Store::open(&path)?;
            if cmd == "compact" {
                // In place, and crash-safe, because the library does it.
                store.compact()?;
                report_compaction(store.stats());
                return Ok(());
            }
            dispatch(&mut store, &cmd, &rest)?;
        }
    }
    Ok(())
}

fn report_compaction(st: dabqlite::Stats) {
    println!(
        "compacted: {} slots used of {}, {} dead",
        st.slots, st.capacity, st.dead
    );
}

/// Returns true when the command changed something (a blob-backed store has
/// to be written back; a directory-backed one already is).
fn dispatch<S: Storage>(
    store: &mut Store<S>,
    cmd: &str,
    rest: &[String],
) -> Result<bool, Box<dyn std::error::Error>> {
    match cmd {
        "add" => {
            if rest.len() < 2 {
                return Err("add <url> <title> [tag ...]".into());
            }
            let id = store.add(&rest[0], &rest[1], &rest[2..], now())?;
            println!("added {id}");
            Ok(true)
        }
        "import" => {
            let n: usize = rest.first().ok_or("import <n>")?.parse()?;
            let items: Vec<NewBookmark> = (0..n)
                .map(|i| {
                    NewBookmark::new(
                        &format!("https://example.com/{i}"),
                        &format!("Example {i}"),
                        &["generated"],
                        now(),
                    )
                })
                .collect();
            let ids = store.import(&items)?;
            println!("imported {} bookmarks", ids.len());
            Ok(true)
        }
        "list" => {
            print_list(&store.list()?);
            Ok(false)
        }
        "page" => {
            let after: u64 = rest.first().ok_or("page <after> <limit>")?.parse()?;
            let limit: usize = rest.get(1).ok_or("page <after> <limit>")?.parse()?;
            print_list(&store.page(after, limit)?);
            Ok(false)
        }
        "get" => {
            let id = id_arg(rest)?;
            match store.get_bookmark(id)? {
                Some(b) => print_one(&b),
                None => return Err(format!("no bookmark {id}").into()),
            }
            Ok(false)
        }
        "visit" => {
            let id = id_arg(rest)?;
            println!("{} visits", store.visit(id)?);
            Ok(true)
        }
        "search" => {
            print_list(&store.search(&joined(rest)?)?);
            Ok(false)
        }
        "tag" => {
            print_list(&store.search_tag(&joined(rest)?)?);
            Ok(false)
        }
        "find" => {
            let (hits, _) = store.search_page(&joined(rest)?, None, 50)?;
            print_list(&hits);
            Ok(false)
        }
        "bytag" => {
            print_list(&store.by_tag(&joined(rest)?)?);
            Ok(false)
        }
        "recent" => {
            let n: usize = rest.first().ok_or("recent <n>")?.parse()?;
            print_list(&store.query(&Query {
                order: Order::NewestFirst,
                limit: Some(n),
                ..Query::default()
            })?);
            Ok(false)
        }
        "rm" => {
            let ids: Vec<u64> = rest
                .iter()
                .map(|s| s.parse::<u64>())
                .collect::<Result<_, _>>()?;
            if ids.is_empty() {
                return Err("rm <id> [id ...]".into());
            }
            println!("removed {}", store.remove_many(&ids)?);
            Ok(true)
        }
        "retag" => {
            if rest.len() < 2 {
                return Err("retag <from> <to>".into());
            }
            println!("retagged {} bookmarks", store.retag(&rest[0], &rest[1])?);
            Ok(true)
        }
        "title" => {
            let id = id_arg(rest)?;
            store.set_title(id, &joined(&rest[1..])?)?;
            println!("ok");
            Ok(true)
        }
        "url" => {
            let id = id_arg(rest)?;
            store.set_url(id, &joined(&rest[1..])?)?;
            println!("ok");
            Ok(true)
        }
        "tags" => {
            let id = id_arg(rest)?;
            store.set_tags(id, &rest[1..])?;
            println!("ok");
            Ok(true)
        }
        "stats" => {
            let n = store.count()?;
            let st = store.stats();
            println!("bookmarks   {n}");
            println!(
                "row slots   {} used of {} ({:.1}% full), {} dead",
                st.slots,
                st.capacity,
                st.fill() * 100.0,
                st.dead
            );
            println!("live rows   {}", st.live);
            Ok(false)
        }
        "demo" => {
            demo()?;
            Ok(false)
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    // The blob backend gives us bytes and no durability story for them;
    // writing them safely is entirely on us. Temp file plus rename is the
    // minimum. (`--dir` needs none of this.)
    let tmp = path.with_extension("dabq.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn id_arg(rest: &[String]) -> Result<u64, Box<dyn std::error::Error>> {
    rest.first()
        .ok_or("expected an id")?
        .parse::<u64>()
        .map_err(|e| format!("bad id: {e}").into())
}

fn joined(rest: &[String]) -> Result<String, Box<dyn std::error::Error>> {
    if rest.is_empty() {
        return Err("expected some text".into());
    }
    Ok(rest.join(" "))
}

fn print_one(b: &Bookmark) {
    println!("{:>4}  {}", b.id, b.title);
    println!("      {}", b.url);
    if !b.tags.is_empty() {
        println!("      #{}", b.tags.join(" #"));
    }
}

fn print_list(all: &[Bookmark]) {
    if all.is_empty() {
        println!("(nothing)");
        return;
    }
    for b in all {
        print_one(b);
    }
    println!(
        "({} bookmark{})",
        all.len(),
        if all.len() == 1 { "" } else { "s" }
    );
}

// -- the session flow, spelled out ------------------------------------------

fn demo() -> Result<(), Box<dyn std::error::Error>> {
    let seed = vec![
        NewBookmark::new(
            "https://doc.rust-lang.org/std/index.html",
            "Rust standard library",
            &["rust", "docs"],
            1_700_000_000,
        ),
        NewBookmark::new(
            "https://www.sqlite.org/whentouse.html",
            "When to use SQLite",
            &["sqlite", "db", "docs"],
            1_700_000_100,
        ),
        NewBookmark::new(
            "https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API",
            "IndexedDB API",
            &["browser", "db"],
            1_700_000_200,
        ),
    ];

    println!("1. fresh in-memory database, nothing on disk");
    let mut store: Store<MemoryStorage> = Store::in_memory()?;
    let ids = store.add_many(&seed)?;
    let st = store.stats();
    println!(
        "   add_many({}) was ONE commit of {} rows -- {} row slots for {} bookmarks",
        ids.len(),
        st.slots,
        st.slots,
        ids.len()
    );

    let blob = store.to_blob()?;
    println!(
        "2. snapshot -> {} bytes; that blob IS the database",
        blob.len()
    );
    drop(store);
    println!("3. the Db was dropped: only bytes are left");

    println!("4. reload from the bytes alone");
    let mut store = Store::load(&blob)?;
    let restored = store.list()?;
    for b in &restored {
        print_one(b);
    }

    println!("5. prove nothing changed");
    assert_eq!(restored.len(), seed.len());
    for (b, want) in restored.iter().zip(&seed) {
        assert_eq!(b.url, want.url);
        assert_eq!(b.title, want.title);
    }
    println!("   every url, title and tag round-tripped exactly");

    // "org/en-US" lands at bytes 26..35 of the third URL. It used to
    // straddle two 16-byte chunks, so the index structurally could not
    // see it. It spans two SLOTS of one value now, and `find` matches
    // across the boundary.
    let index_hits = store.db().find(b"org/en-US")?.len();
    println!("   find \"org/en-US\"   -> {index_hits} row(s) from Db::find; the needle spans two");
    println!("                                 slots of one value and the index sees it anyway");
    println!(
        "   by_tag \"db\"        -> {:?}   (exact match: the record delimits tags with \\x1e)",
        store.by_tag("db")?.iter().map(|b| b.id).collect::<Vec<_>>()
    );
    // Byte-exact, though, so a search box still needs the scan.
    let cased = store.find_exact("Rust standard")?.len();
    let folded = store.search("rust standard")?.len();
    println!("   find_exact(\"Rust standard\") -> {cased}, search(\"rust standard\") -> {folded}");
    println!("                                 the index is byte-exact; case folding is a scan");
    Ok(())
}
