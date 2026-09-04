#![cfg(unix)]
//! Repair by REBUILD, never by surgery.
//!
//! When a row cannot be verified there is nothing on a single disk to
//! repair it from, so "repair" here means: read everything that still
//! verifies, and write a clean database somewhere new. The shape carries
//! the safety — the source is opened through a handle that physically
//! cannot write, and the destination is a fresh directory — so the only
//! copy of the truth is never overwritten and an interrupted repair costs
//! nothing but the partial copy.
//!
//! This is also the answer to compaction: the rebuilt database contains
//! exactly the live rows, densely packed, with quarantined slots and any
//! post-migration legacy file left behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use dabqlite_core::{Capacities, DbError, Output, ROW_SIZE, VALUE_LEN};
use dabqlite_host::{rows_file_name, Host, PosixStorage, ReadOnlyDir, SUPERBLOCK_FILE};

const CAPS: Capacities = Capacities { rows: 64 };
const N: u64 = 20;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-repair-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| {
            let e = e.expect("entry");
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("read"),
            )
        })
        .collect()
}

fn value_for(i: u64) -> [u8; VALUE_LEN] {
    [(i as u8).wrapping_mul(31) ^ 0x5C; VALUE_LEN]
}

/// A populated database, then a bit flipped inside row `victim`.
fn damaged_db(tag: &str, victim: u64) -> PathBuf {
    let dir = scratch(tag);
    {
        let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open"));
        assert!(matches!(
            host.open().expect("probe"),
            Output::OpenDone { result: Ok(0) }
        ));
        for i in 0..N {
            assert!(matches!(
                host.insert(i, value_for(i)),
                Output::InsertDone { result: Ok(()), .. }
            ));
        }
    }
    let rows_path = dir.join(rows_file_name(dabqlite_core::SCHEMA_HASH));
    let mut bytes = std::fs::read(&rows_path).expect("read rows");
    bytes[victim as usize * ROW_SIZE + 5] ^= 0x40;
    std::fs::write(&rows_path, bytes).expect("write rows");
    dir
}

fn inspect(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dabqlite-inspect"))
        .args(args)
        .output()
        .expect("run inspector")
}

#[test]
fn a_damaged_database_is_rebuilt_clean_and_the_loss_is_reported() {
    let src = damaged_db("rebuild", 7);
    let dest = scratch("rebuild-out");

    // Strict open refuses it — that is still the default.
    {
        let mut host = Host::new(CAPS, PosixStorage::open_dir(&src).expect("open"));
        assert!(matches!(
            host.open().expect("probe"),
            Output::OpenDone {
                result: Err(DbError::Corrupt { .. })
            }
        ));
    }

    let out = inspect(&[src.as_os_str(), "--repair-to".as_ref(), dest.as_os_str()]);
    assert!(
        out.status.success(),
        "repair failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("wrote {} rows", N - 1)),
        "repair must report what it recovered:\n{stdout}"
    );
    assert!(
        stdout.contains("DROPPED 1"),
        "repair must report what it discarded:\n{stdout}"
    );

    // The rebuilt database opens STRICTLY — it is a clean database, not a
    // degraded one — and holds exactly the rows that survived.
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&dest).expect("open dest"));
    match host.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, N - 1),
        other => panic!("rebuilt database did not open cleanly: {other:?}"),
    }
    assert!(!host.engine.is_degraded());
    assert_eq!(host.engine.quarantined(), 0);
    for i in 0..N {
        let got = match host.get(i) {
            Output::GetDone { result: Ok(v), .. } => v,
            other => panic!("get {i}: {other:?}"),
        };
        if i == 7 {
            assert_eq!(got, None, "the unverifiable row must not be resurrected");
        } else {
            assert_eq!(got, Some(value_for(i)), "row {i}");
        }
    }
    // And it is a normal, writable database again.
    assert!(matches!(
        host.insert(9_999, [1; VALUE_LEN]),
        Output::InsertDone { result: Ok(()), .. }
    ));

    std::fs::remove_dir_all(&src).ok();
    std::fs::remove_dir_all(&dest).ok();
}

/// The source is evidence. Repair must leave it byte-identical, so a
/// second opinion is always still possible.
#[test]
fn repair_leaves_the_source_byte_identical() {
    let src = damaged_db("source-intact", 3);
    let dest = scratch("source-intact-out");
    let before = snapshot(&src);

    let out = inspect(&[src.as_os_str(), "--repair-to".as_ref(), dest.as_os_str()]);
    assert!(out.status.success());

    assert_eq!(
        snapshot(&src),
        before,
        "repair modified the database it was reading"
    );
    std::fs::remove_dir_all(&src).ok();
    std::fs::remove_dir_all(&dest).ok();
}

/// Repair writes a NEW database and refuses to overwrite an existing one:
/// the one destructive operation in the system does not get to be
/// accidentally destructive as well.
#[test]
fn repair_refuses_a_non_empty_destination() {
    let src = damaged_db("refuse", 2);
    let dest = scratch("refuse-out");
    std::fs::create_dir_all(&dest).expect("mkdir");
    std::fs::write(dest.join("something.txt"), b"do not clobber me").expect("write");

    let out = inspect(&[src.as_os_str(), "--repair-to".as_ref(), dest.as_os_str()]);
    assert!(!out.status.success(), "repair overwrote a non-empty target");
    assert_eq!(
        std::fs::read(dest.join("something.txt")).expect("still there"),
        b"do not clobber me"
    );
    std::fs::remove_dir_all(&src).ok();
    std::fs::remove_dir_all(&dest).ok();
}

/// A healthy database rebuilds to an exact copy: repair is not a
/// destructive operation that happens to preserve data, it is a copy that
/// happens to skip what it cannot verify.
#[test]
fn repairing_a_healthy_database_loses_nothing() {
    let src = scratch("healthy");
    {
        let mut host = Host::new(CAPS, PosixStorage::open_dir(&src).expect("open"));
        host.open().expect("probe");
        for i in 0..N {
            host.insert(i, value_for(i));
        }
    }
    let dest = scratch("healthy-out");
    let out = inspect(&[src.as_os_str(), "--repair-to".as_ref(), dest.as_os_str()]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("nothing was dropped"), "{stdout}");

    // Byte-identical to the original: same rows, same order, same layout.
    assert_eq!(
        std::fs::read(dest.join(rows_file_name(dabqlite_core::SCHEMA_HASH))).expect("dest rows"),
        std::fs::read(src.join(rows_file_name(dabqlite_core::SCHEMA_HASH))).expect("src rows"),
        "a clean rebuild should reproduce the rows file exactly"
    );
    std::fs::remove_dir_all(&src).ok();
    std::fs::remove_dir_all(&dest).ok();
}

/// The read-only handle is not a convention, it is a wall: writes through
/// it fail, so no code path — present or future — can mutate a database
/// it was only supposed to inspect.
#[test]
fn the_readonly_handle_physically_refuses_writes() {
    use dabqlite_host::Storage;
    let src = damaged_db("readonly", 1);
    let mut ro = ReadOnlyDir::open_dir(&src).expect("open read-only");
    let err = ro
        .write(dabqlite_core::FileId::Rows, 0, &[0u8; 4])
        .expect_err("a read-only handle must refuse writes");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    // Reads work, and a missing file reads as empty rather than failing.
    assert!(!ro
        .read(dabqlite_core::FileId::Rows, 0, 32)
        .expect("read")
        .is_empty());
    let empty = scratch("readonly-empty");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let mut ro = ReadOnlyDir::open_dir(&empty).expect("open empty");
    assert_eq!(ro.len(dabqlite_core::FileId::Superblock).expect("len"), 0);
    assert!(ro
        .read(dabqlite_core::FileId::Superblock, 0, 64)
        .expect("read")
        .is_empty());
    // It takes no lock, so it never blocks a live writer.
    let _live = PosixStorage::open_dir(&src).expect("writer still gets the lock");
    let _second_ro = ReadOnlyDir::open_dir(&src).expect("inspection beside a live writer");

    std::fs::remove_dir_all(&src).ok();
    std::fs::remove_dir_all(&empty).ok();
}

/// Repair works with SUPERBLOCK_FILE present but a corrupt manifest? No —
/// and it says so instead of writing a plausible-looking empty database.
#[test]
fn repair_refuses_when_the_manifest_itself_is_unreadable() {
    let src = damaged_db("no-manifest", 1);
    let sb = src.join(SUPERBLOCK_FILE);
    let mut bytes = std::fs::read(&sb).expect("read sb");
    for b in bytes.iter_mut() {
        *b ^= 0xFF;
    }
    std::fs::write(&sb, bytes).expect("write sb");

    let dest = scratch("no-manifest-out");
    let out = inspect(&[src.as_os_str(), "--repair-to".as_ref(), dest.as_os_str()]);
    assert!(
        !out.status.success(),
        "repair must not invent a database from an unreadable manifest"
    );
    std::fs::remove_dir_all(&src).ok();
    std::fs::remove_dir_all(&dest).ok();
}

/// `--gc` reclaims the dead legacy file left by a completed migration —
/// and, far more importantly, REFUSES to touch it when the migration has
/// not happened, because then it is not dead at all: it is the only copy
/// of the data.
#[test]
fn gc_reclaims_the_legacy_file_only_after_migration_completed() {
    use dabqlite_core::migration::V1_SCHEMA_HASH;

    // A pre-migration database: legacy rows file present, superblock on
    // the legacy schema. gc MUST refuse — this is the dangerous case.
    let pre = scratch("gc-pre");
    std::fs::create_dir_all(&pre).expect("mkdir");
    let legacy_name = rows_file_name(V1_SCHEMA_HASH);
    std::fs::write(pre.join(&legacy_name), vec![0xAB; 480]).expect("legacy rows");
    let out = inspect(&[pre.as_os_str(), "--gc".as_ref()]);
    assert!(
        !out.status.success(),
        "gc must refuse a non-recovering directory"
    );
    assert_eq!(
        std::fs::read(pre.join(&legacy_name))
            .expect("still there")
            .len(),
        480,
        "gc deleted data it could not prove was dead"
    );

    // A healthy CURRENT-schema database with a leftover legacy file: now
    // the file really is inert, and gc reclaims it.
    let post = scratch("gc-post");
    {
        let mut host = Host::new(CAPS, PosixStorage::open_dir(&post).expect("open"));
        host.open().expect("probe");
        for i in 0..N {
            host.insert(i, value_for(i));
        }
    }
    std::fs::write(post.join(&legacy_name), vec![0xCD; 960]).expect("legacy leftovers");
    let out = inspect(&[post.as_os_str(), "--gc".as_ref()]);
    assert!(
        out.status.success(),
        "gc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("reclaimed 960 bytes"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The live data is untouched and still opens strictly.
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&post).expect("reopen"));
    match host.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, N),
        other => panic!("gc damaged the database: {other:?}"),
    }
    for i in 0..N {
        assert!(matches!(
            host.get(i),
            Output::GetDone { result: Ok(Some(v)), .. } if v == value_for(i)
        ));
    }
    std::fs::remove_dir_all(&pre).ok();
    std::fs::remove_dir_all(&post).ok();
}
