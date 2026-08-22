#![cfg(unix)]
//! The inspector CLI on real files: deterministic golden output, a
//! byte-level read-only guarantee, and coexistence with a LIVE writer —
//! the inspector takes no lock, because forensics on a wedged process's
//! directory is exactly when you need it.

use std::path::{Path, PathBuf};
use std::process::Command;

use dabqlite_core::{Capacities, SCHEMA_HASH, VALUE_LEN};
use dabqlite_host::posix::rows_file_name;
use dabqlite_host::{Host, PosixStorage};

const CAPS: Capacities = Capacities { rows: 8 };

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-inspect-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn inspect_exe() -> &'static str {
    env!("CARGO_BIN_EXE_dabqlite-inspect")
}

fn build_db(dir: &Path, n: u64) {
    let mut host = Host::new(CAPS, PosixStorage::open_dir(dir).expect("open dir"));
    host.open().expect("probe");
    for i in 0..n {
        host.insert(i * 3, [i as u8; VALUE_LEN]);
    }
}

#[test]
fn output_is_deterministic_and_names_the_right_things() {
    let dir = scratch_dir("golden");
    build_db(&dir, 3);

    let run = || {
        let out = Command::new(inspect_exe())
            .arg(&dir)
            .output()
            .expect("run inspector");
        assert!(out.status.success());
        String::from_utf8(out.stdout).expect("utf8")
    };
    let first = run();
    let second = run();
    assert_eq!(first, second, "inspector output must be deterministic");

    // The load-bearing facts, present verbatim.
    for needle in [
        &format!("binary schema   0x{SCHEMA_HASH:016X}"),
        "verdict: healthy - open recovers 3 rows",
        "committed valid    3",
        "rollback evidence  none",
        "superblock.dabq                   256 B  superblock",
        "<- LIVE",
    ] {
        assert!(first.contains(needle), "missing {needle:?} in:\n{first}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn inspection_changes_zero_bytes() {
    let dir = scratch_dir("readonly");
    build_db(&dir, 5);
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("readdir")
        .map(|e| e.expect("entry").path())
        .collect();
    let before: Vec<Vec<u8>> = files
        .iter()
        .map(|p| std::fs::read(p).expect("read"))
        .collect();

    let out = Command::new(inspect_exe())
        .arg(&dir)
        .arg("--verify")
        .output()
        .expect("run inspector");
    assert!(out.status.success(), "healthy db must verify clean");

    let after: Vec<Vec<u8>> = files
        .iter()
        .map(|p| std::fs::read(p).expect("read"))
        .collect();
    assert_eq!(before, after, "the inspector wrote to the database");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn inspects_while_a_writer_holds_the_lock() {
    let dir = scratch_dir("live");
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
    host.open().expect("probe");
    host.insert(1, [1; VALUE_LEN]);
    // The writer is alive and holds the flock. The inspector must still
    // work: it takes no lock at all.
    let out = Command::new(inspect_exe())
        .arg(&dir)
        .output()
        .expect("run inspector");
    assert!(out.status.success(), "inspector blocked by the writer lock");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("open recovers 1 rows"), "{text}");
    drop(host);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn verify_flags_a_damaged_database_via_exit_code() {
    let dir = scratch_dir("damaged");
    build_db(&dir, 4);
    // Flip one bit inside a committed row, at rest.
    let rows_path = dir.join(rows_file_name(SCHEMA_HASH));
    let mut bytes = std::fs::read(&rows_path).expect("read rows");
    bytes[2 * 32 + 3] ^= 0x08;
    std::fs::write(&rows_path, bytes).expect("write damage");

    let out = Command::new(inspect_exe())
        .arg(&dir)
        .arg("--verify")
        .output()
        .expect("run inspector");
    assert_eq!(out.status.code(), Some(2), "damage must fail --verify");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("verdict: CORRUPT - committed row failed checksum"));
    assert!(text.contains("corrupt slot at byte offset 64"), "{text}");
    std::fs::remove_dir_all(&dir).ok();
}
