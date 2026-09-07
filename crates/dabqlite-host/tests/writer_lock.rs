//! The single-writer lock, and the two ways a lock primitive can be
//! subtly wrong.
//!
//! "One writer, always" (docs/DESIGN.md §2) is the premise the whole
//! engine rests on: recovery, the append-only rows file, and the
//! superblock flip all assume nothing else is writing. A lock that is too
//! LAX breaks that premise silently. A lock that is too STRICT refuses a
//! caller who is entitled to the database, which is its own kind of
//! failure — an application that cannot reopen its own closed database is
//! broken whether or not any byte was lost.
//!
//! The obvious implementation, `flock` via `File::try_lock`, is too
//! strict in a way that is easy to miss: `flock` belongs to the open file
//! description, `fork` duplicates it, and `O_CLOEXEC` only closes the
//! copy at `exec`. So any program that spawns a subprocess hands its
//! writer lock to a child that has never heard of the database. These
//! tests pin both directions.

#![cfg(unix)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use dabqlite_host::PosixStorage;

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-wlock-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Not too strict: a closed database reopens, even while the process is
/// busy spawning unrelated children.
///
/// With `flock` this failed on 891 of 1500 attempts from 39 spawns of
/// `/bin/true` — the fork-to-exec window is long enough to swallow
/// thousands of reopens.
#[test]
fn a_closed_database_reopens_while_the_process_spawns_children() {
    let dir = scratch("fork");
    drop(PosixStorage::open_dir(&dir).expect("first open"));

    let stop = Arc::new(AtomicBool::new(false));
    let spawns = Arc::new(AtomicU64::new(0));
    let spawner = {
        let stop = stop.clone();
        let spawns = spawns.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::process::Command::new("/bin/true").status();
                spawns.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let mut refused = 0u64;
    let attempts = 1500u64;
    for _ in 0..attempts {
        match PosixStorage::open_dir(&dir) {
            Ok(s) => drop(s),
            Err(_) => refused += 1,
        }
    }
    stop.store(true, Ordering::Relaxed);
    spawner.join().unwrap();
    let spawned = spawns.load(Ordering::Relaxed);

    std::fs::remove_dir_all(&dir).ok();
    assert!(
        spawned > 0,
        "the test spawned no children, so it proved nothing"
    );
    assert_eq!(
        refused, 0,
        "the writer lock leaked into unrelated children: {refused} of \
         {attempts} reopens of a CLOSED database were refused, alongside \
         {spawned} spawns"
    );
}

/// Not too lax: a second handle in the SAME process is refused.
///
/// This is the half a POSIX record lock cannot do on its own — record
/// locks are owned by the process, so the kernel would happily hand the
/// same process two — and it is why the backend keeps a registry of the
/// directories it holds.
#[test]
fn a_second_handle_in_the_same_process_is_refused() {
    let dir = scratch("same-process");
    let first = PosixStorage::open_dir(&dir).expect("first open");
    let second = PosixStorage::open_dir(&dir);
    match second {
        Err(e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{e}");
            assert!(e.to_string().contains("single-writer"), "{e}");
        }
        Ok(_) => panic!("two writers in one process: the engine's premise is gone"),
    }
    // And it comes back once the first is closed.
    drop(first);
    drop(PosixStorage::open_dir(&dir).expect("reopen after close"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Two threads racing for the same database: exactly one wins at a time,
/// nobody sees a spurious refusal once the winner lets go, and the
/// registry does not leak an entry on any path.
#[test]
fn threads_racing_for_one_database_never_leak_the_claim() {
    let dir = Arc::new(scratch("threads"));
    let wins = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let dir = dir.clone();
        let wins = wins.clone();
        threads.push(std::thread::spawn(move || {
            for _ in 0..200 {
                if let Ok(s) = PosixStorage::open_dir(&dir) {
                    wins.fetch_add(1, Ordering::Relaxed);
                    drop(s);
                }
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    assert!(
        wins.load(Ordering::Relaxed) > 0,
        "every attempt was refused; the claim leaked"
    );
    // The decisive check: after all that contention the database is free.
    drop(PosixStorage::open_dir(&dir).expect("the lock must be free at the end"));
    std::fs::remove_dir_all(dir.as_path()).ok();
}
