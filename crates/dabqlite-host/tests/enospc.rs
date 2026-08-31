#![cfg(unix)]
//! GENUINE ENOSPC on real files: a size-capped tmpfs filled to the brim,
//! with the kernel — not a simulator — refusing the writes.
//!
//! (`/dev/full` itself can't host a database: it is a single character
//! device, not a filesystem. A tiny tmpfs gives the same ENOSPC physics
//! for a whole directory of real files.)
//!
//! The claims, mirroring the sim regime in `dabqlite-sim/tests/disk_full.rs`:
//! - the wall surfaces as a real `ENOSPC` through `Host::last_error` and
//!   the engine fail-stops;
//! - already-acknowledged reads keep working on the fail-stopped state's
//!   recovery — and RECOVERY ITSELF succeeds on the still-full disk,
//!   because recovery never allocates: its reads are reads, its fsyncs
//!   flush existing pages, and its repair write overwrites a slot that
//!   already exists (on extent/CoW filesystems where overwrite can also
//!   ENOSPC, recovery would instead fail loudly — the sim regime covers
//!   that shape);
//! - the inspector works on the full disk (it takes no lock and writes
//!   no byte);
//! - freeing space ends the episode: inserts resume, and every row acked
//!   before OR after the wall is present, exact. Zero loss.
//!
//! Environment-gated, honestly: needs root (mounts its own tmpfs) or a
//! pre-mounted directory in `DABQLITE_ENOSPC_DIR` (CI does this with
//! sudo). Anywhere else the test prints why and passes vacuously.

use std::path::PathBuf;
use std::process::Command;

use dabqlite_core::{Capacities, DbError, Output, VALUE_LEN};
use dabqlite_host::{Host, PosixStorage};

const CAPS: Capacities = Capacities { rows: 4096 };

struct TestFs {
    dir: PathBuf,
    mounted: bool,
}

impl Drop for TestFs {
    fn drop(&mut self) {
        if self.mounted {
            let _ = Command::new("umount").arg(&self.dir).status();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// A directory on a 256 KiB filesystem, or None (with the reason printed)
/// when this environment can't provide one.
fn tiny_fs() -> Option<TestFs> {
    if let Ok(dir) = std::env::var("DABQLITE_ENOSPC_DIR") {
        return Some(TestFs {
            dir: PathBuf::from(dir),
            mounted: false,
        });
    }
    // SAFETY-free root check via effective uid through /proc.
    let uid_is_root = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
        })
        .unwrap_or(false);
    if !uid_is_root {
        eprintln!(
            "enospc test SKIPPED: needs root (to mount a tiny tmpfs) or \
             DABQLITE_ENOSPC_DIR pointing at a small pre-mounted filesystem"
        );
        return None;
    }
    let dir = std::env::temp_dir().join(format!("dabqlite-enospc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let ok = Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=256k", "tmpfs"])
        .arg(&dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("enospc test SKIPPED: tmpfs mount failed");
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    Some(TestFs { dir, mounted: true })
}

#[test]
fn real_enospc_episode_loses_nothing() {
    let Some(fs) = tiny_fs() else { return };
    let db = fs.dir.join("db");

    // Eat most of the filesystem so the database hits the wall early.
    // (tmpfs allocates page-granularly, so the exact wall position is the
    // kernel's business — the test only requires that it arrives.)
    let filler = fs.dir.join("filler");
    std::fs::write(&filler, vec![0u8; 220 * 1024]).expect("filler");

    let mut host = Host::new(CAPS, PosixStorage::open_dir(&db).expect("open dir"));
    assert!(matches!(
        host.open().expect("probe"),
        Output::OpenDone { result: Ok(0) }
    ));

    // Insert until the kernel says no.
    let mut acked: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
    let mut hit_wall = false;
    for i in 0..CAPS.rows {
        let value = [(i % 251) as u8; VALUE_LEN];
        match host.insert(i, value) {
            Output::InsertDone { result: Ok(()), .. } => acked.push((i, value)),
            Output::InsertDone {
                result: Err(DbError::IoFailed { .. }),
                ..
            } => {
                hit_wall = true;
                break;
            }
            other => panic!("insert {i}: {other:?}"),
        }
    }
    assert!(
        hit_wall,
        "the 256K filesystem never filled — enlarge filler"
    );
    assert!(!acked.is_empty(), "the wall arrived before a single commit");
    // The surfaced error is the real thing.
    let err = host.last_error.as_ref().expect("storage error recorded");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::ENOSPC),
        "expected raw ENOSPC, got {err:?}"
    );
    // Fail-stopped: reads on the wedged handle are refused too.
    assert!(matches!(
        host.get(acked[0].0),
        Output::GetDone {
            result: Err(DbError::IoFailed { .. }),
            ..
        }
    ));

    // The inspector works on the full disk: no lock, no writes.
    let out = Command::new(env!("CARGO_BIN_EXE_dabqlite-inspect"))
        .arg(&db)
        .output()
        .expect("run inspector");
    assert!(out.status.success(), "inspector failed on a full disk");

    // Recovery on the STILL-FULL disk: recovery never allocates, so it
    // succeeds — and every acknowledged row is served, exact. (The one
    // in-flight row resolves all-or-nothing, as always.)
    drop(host);
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&db).expect("reopen full"));
    let n = match host.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => n,
        other => panic!("recovery on full disk: {other:?}"),
    };
    assert!(
        n == acked.len() as u64 || n == acked.len() as u64 + 1,
        "{n} rows vs {} acked",
        acked.len()
    );
    for &(id, value) in &acked {
        assert!(matches!(
            host.get(id),
            Output::GetDone { result: Ok(Some(v)), .. } if v == value
        ));
    }
    // Still full: the next insert is refused with ENOSPC again.
    // (tmpfs may allow a few appends into the tail page first.)
    let mut refused_again = false;
    for i in 0..200u64 {
        match host.insert(1_000_000 + i, [9; VALUE_LEN]) {
            Output::InsertDone { result: Ok(()), .. } => {}
            Output::InsertDone {
                result: Err(DbError::IoFailed { .. }),
                ..
            } => {
                refused_again = true;
                break;
            }
            other => panic!("post-recovery insert: {other:?}"),
        }
    }
    assert!(refused_again, "the disk should still be full");

    // Space frees: the episode ends. Everything acked at ANY point —
    // before the wall, between the walls — is present and writable again.
    std::fs::remove_file(&filler).expect("free space");
    drop(host);
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&db).expect("reopen freed"));
    let final_n = match host.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => n,
        other => panic!("recovery after freeing space: {other:?}"),
    };
    assert!(final_n >= n, "rows went backwards after freeing space");
    for &(id, value) in &acked {
        assert!(matches!(
            host.get(id),
            Output::GetDone { result: Ok(Some(v)), .. } if v == value
        ));
    }
    assert!(matches!(
        host.insert(2_000_000, [5; VALUE_LEN]),
        Output::InsertDone { result: Ok(()), .. }
    ));
}
