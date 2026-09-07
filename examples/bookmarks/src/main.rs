//! A tiny CLI over the bookmark store.
//!
//! Every invocation is one browser-style session: read the snapshot blob off
//! disk (standing in for IndexedDB), work in memory, write the blob back.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bookmarks::{read_store, with_store, Bookmark, Store, StoreError};

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
usage: bookmarks [--db FILE] <command>

  add <url> <title> [tag ...]   add a bookmark
  list                          every bookmark, in id order
  get <id>                      one bookmark
  search <text>                 substring over url, title and tags (full scan)
  tag <text>                    substring over tags only (uses the index)
  rm <id>                       delete a bookmark
  title <id> <text>             retitle
  url <id> <text>               re-point
  tags <id> [tag ...]           replace the tag set
  stats                         row-slot accounting and blob size
  fsck                          look for half-written bookmarks; repair them
  compact                       rebuild, reclaiming dead slots
  demo                          the save / drop / reload session flow

The database is in memory. --db names the snapshot blob (default bookmarks.dabq).
";

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = PathBuf::from("bookmarks.dabq");
    if args.first().map(String::as_str) == Some("--db") {
        if args.len() < 2 {
            return Err("--db needs a path".into());
        }
        path = PathBuf::from(args.remove(1));
        args.remove(0);
    }
    let Some(cmd) = args.first().cloned() else {
        print!("{USAGE}");
        return Ok(());
    };
    let rest = &args[1..];

    match cmd.as_str() {
        "add" => {
            if rest.len() < 2 {
                return Err("add <url> <title> [tag ...]".into());
            }
            let (url, title, tags) = (&rest[0], &rest[1], rest[2..].to_vec());
            let id = write(&path, |s| s.add(url, title, &tags))?;
            println!("added {id}");
        }
        "list" => {
            let all = read(&path, |s| s.list())?;
            print_list(&all);
        }
        "get" => {
            let id = id_arg(rest)?;
            match read(&path, move |s| s.get_bookmark(id))? {
                Some(b) => print_one(&b),
                None => return Err(format!("no bookmark {id}").into()),
            }
        }
        "search" => {
            let needle = joined(rest)?;
            let hits = read(&path, |s| s.search(&needle))?;
            print_list(&hits);
        }
        "tag" => {
            let needle = joined(rest)?;
            let hits = read(&path, |s| s.search_tag(&needle))?;
            print_list(&hits);
        }
        "rm" => {
            let id = id_arg(rest)?;
            let gone = write(&path, move |s| s.remove(id))?;
            println!("{}", if gone { "removed" } else { "not there" });
        }
        "title" => {
            let id = id_arg(rest)?;
            let text = joined(&rest[1..])?;
            write(&path, move |s| s.set_title(id, &text))?;
            println!("ok");
        }
        "url" => {
            let id = id_arg(rest)?;
            let text = joined(&rest[1..])?;
            write(&path, move |s| s.set_url(id, &text))?;
            println!("ok");
        }
        "tags" => {
            let id = id_arg(rest)?;
            let tags = rest[1..].to_vec();
            write(&path, move |s| s.set_tags(id, &tags))?;
            println!("ok");
        }
        "stats" => {
            let blob = load(&path)?;
            let (n, _, report) = with_store(blob.as_deref(), |s| s.count())?;
            let st = report.stats;
            println!("bookmarks   {n}");
            println!(
                "row slots   {} used of {} ({:.1}% full), {} dead",
                st.slots,
                st.capacity,
                st.fill() * 100.0,
                st.dead
            );
            println!("live rows   {}", st.live);
            println!("blob        {} bytes at {}", report.blob_len, path.display());
        }
        "fsck" => {
            let (before, fixed) = write(&path, |s| {
                let before = s.integrity()?;
                let fixed = s.repair()?;
                Ok((before, fixed))
            })?;
            println!("bookmarks {}", before.bookmarks);
            if before.orphans.is_empty() {
                println!("no half-written bookmarks");
            } else {
                println!(
                    "orphaned entities {:?} ({} stray rows); deleted {fixed} rows",
                    before.orphans, before.stray_rows
                );
            }
        }
        "compact" => {
            let blob = load(&path)?;
            let (_, bytes, report) = with_store(blob.as_deref(), |s| {
                s.request_compaction();
                Ok(())
            })?;
            save(&path, &bytes)?;
            println!(
                "compacted: {} slots used of {}, {} dead",
                report.stats.slots, report.stats.capacity, report.stats.dead
            );
        }
        "demo" => demo()?,
        other => return Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    }
    Ok(())
}

// -- session helpers --------------------------------------------------------

fn load(path: &Path) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    // The library gives us one blob and no durability story for it; writing
    // it safely is entirely on us. Temp file plus rename is the minimum.
    let tmp = path.with_extension("dabq.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write<R>(
    path: &Path,
    body: impl FnOnce(&mut Store) -> Result<R, StoreError>,
) -> Result<R, Box<dyn std::error::Error>> {
    let blob = load(path)?;
    let (out, bytes, _) = with_store(blob.as_deref(), body)?;
    save(path, &bytes)?;
    Ok(out)
}

fn read<R>(
    path: &Path,
    body: impl FnOnce(&mut Store) -> Result<R, StoreError>,
) -> Result<R, Box<dyn std::error::Error>> {
    match load(path)? {
        Some(blob) => Ok(read_store(&blob, body)?),
        None => {
            let (out, _, _) = with_store(None, body)?;
            Ok(out)
        }
    }
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
    println!("({} bookmark{})", all.len(), if all.len() == 1 { "" } else { "s" });
}

// -- the session flow, spelled out ------------------------------------------

fn demo() -> Result<(), Box<dyn std::error::Error>> {
    let seed: Vec<(&str, &str, &[&str])> = vec![
        (
            "https://doc.rust-lang.org/std/index.html",
            "Rust standard library",
            &["rust", "docs"],
        ),
        (
            "https://www.sqlite.org/whentouse.html",
            "When to use SQLite",
            &["sqlite", "db", "docs"],
        ),
        (
            "https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API",
            "IndexedDB API",
            &["browser", "db"],
        ),
    ];

    println!("1. fresh in-memory database, nothing on disk");
    let (ids, blob, report) = with_store(None, |s| {
        let mut ids = Vec::new();
        for (url, title, tags) in &seed {
            let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
            ids.push(s.add(url, title, &tags)?);
        }
        Ok(ids)
    })?;
    println!(
        "   added {ids:?} -> {} row slots for {} bookmarks ({} rows per bookmark, \
         because a 16-byte value holds neither a URL nor a title)",
        report.stats.slots,
        ids.len(),
        report.stats.live / ids.len() as u64
    );

    println!(
        "2. snapshot -> {} bytes; that blob IS the database",
        blob.len()
    );

    println!("3. the Db value was dropped when the session returned: only bytes are left");

    println!("4. reload from the bytes alone");
    let (restored, tagged, spanning, index_hits) = read_store(&blob, |s| {
        // "org/en-US" lands at bytes 26..35 of the third URL, straddling the
        // 32-byte chunk boundary, so no single 16-byte row contains it.
        let index_hits = match s.exec_raw(bookmarks::Cmd::Find(b"org/en-US".to_vec()))? {
            bookmarks::Reply::Rows(r) => r.len(),
            _ => 0,
        };
        Ok((s.list()?, s.search_tag("db")?, s.search("org/en-US")?, index_hits))
    })?;
    for b in &restored {
        print_one(b);
    }

    println!("5. prove nothing changed");
    assert_eq!(restored.len(), seed.len(), "bookmark count survived");
    for (b, (url, title, tags)) in restored.iter().zip(&seed) {
        assert_eq!(&b.url, url, "url survived");
        assert_eq!(&b.title, title, "title survived");
        let mut want: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
        want.sort();
        want.dedup();
        assert_eq!(b.tags, want, "tags survived");
    }
    println!("   every url, title and tag round-tripped exactly");
    println!(
        "   tag \"db\"          -> {:?}   (served by Db::find: a tag fits one row)",
        tagged.iter().map(|b| b.id).collect::<Vec<_>>()
    );
    println!(
        "   search \"org/en-US\" -> {:?}   (full scan; Db::find returned {index_hits} rows for",
        spanning.iter().map(|b| b.id).collect::<Vec<_>>()
    );
    println!("                                the same needle, because it straddles two chunks)");
    Ok(())
}
