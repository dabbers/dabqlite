#![cfg(unix)]
//! Single-writer enforcement (docs/DESIGN.md §2: "one writer, always").
//! The engine serializes everything within a process; this suite proves no
//! SECOND process (or handle) can reach the files at all — and that the
//! lock itself is crash-safe: it dies with its holder, so a crashed writer
//! can never brick the database behind a stale lock.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use dabqlite_core::{Capacities, Output, VALUE_LEN};
use dabqlite_host::{Host, PosixStorage};

const CAPS: Capacities = Capacities { rows: 8 };

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-lock-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn probe_exe() -> &'static str {
    env!("CARGO_BIN_EXE_lockprobe")
}

#[test]
fn second_open_in_same_process_is_refused() {
    let dir = scratch_dir("same-process");
    let first = PosixStorage::open_dir(&dir).expect("first open");

    let second = PosixStorage::open_dir(&dir);
    let err = second.err().expect("second open must be refused");
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    assert!(
        err.to_string().contains("single-writer"),
        "error should explain the policy: {err}"
    );

    // Releasing the first handle releases the lock.
    drop(first);
    PosixStorage::open_dir(&dir).expect("open after release");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_real_second_process_is_refused_while_the_database_is_open() {
    let dir = scratch_dir("second-process");
    let storage = PosixStorage::open_dir(&dir).expect("open");
    // Not just held — actively in use.
    let mut host = Host::new(CAPS, storage);
    assert!(matches!(
        host.open().expect("probe"),
        Output::OpenDone { result: Ok(0) }
    ));
    host.insert(1, [1; VALUE_LEN]);

    // A genuine second OS process must be refused at the door.
    let out = Command::new(probe_exe())
        .arg(&dir)
        .output()
        .expect("spawn lockprobe");
    assert_eq!(
        out.status.code(),
        Some(2),
        "second process acquired the lock: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("refused"));

    // The refused attempt harmed nothing: our handle still works.
    assert!(matches!(
        host.get(1),
        Output::GetDone { result: Ok(Some(v)), .. } if v == [1; VALUE_LEN]
    ));

    // After we close, a second process succeeds.
    drop(host);
    let out = Command::new(probe_exe())
        .arg(&dir)
        .output()
        .expect("spawn lockprobe");
    assert_eq!(out.status.code(), Some(0), "open after release must work");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn killing_the_lock_holder_never_leaves_a_stale_lock() {
    let dir = scratch_dir("kill-holder");

    // A real process takes the lock and holds it.
    let mut holder = Command::new(probe_exe())
        .arg(&dir)
        .arg("hold")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn holder");
    let mut line = String::new();
    BufReader::new(holder.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read");
    assert_eq!(line.trim(), "acquired", "holder failed to take the lock");

    // While it lives, we are refused.
    assert!(
        PosixStorage::open_dir(&dir).is_err(),
        "lock not held by the holder process"
    );

    // Kill it — simulating a crashed writer — and the kernel releases the
    // lock with it. No stale-lock recovery step exists because none is
    // needed: that is the point of flock over lockfiles.
    holder.kill().expect("kill holder");
    holder.wait().expect("reap holder");

    let storage = PosixStorage::open_dir(&dir).expect("open after holder death");
    // And the database is fully usable.
    let mut host = Host::new(CAPS, storage);
    assert!(matches!(
        host.open().expect("probe"),
        Output::OpenDone { result: Ok(0) }
    ));
    assert!(matches!(
        host.insert(7, [7; VALUE_LEN]),
        Output::InsertDone { result: Ok(()), .. }
    ));
    std::fs::remove_dir_all(&dir).ok();
}
