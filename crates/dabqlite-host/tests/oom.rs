#![cfg(unix)]
//! GENUINE out-of-memory, in a real child process under `RLIMIT_AS`.
//!
//! The design confines allocation to init (docs/DESIGN.md §4.2), and the
//! counting-allocator suite (`dabqlite-core/tests/allocation.rs`) proves
//! steady state performs literally zero allocations. This suite closes the
//! loop on the one window that remains: what does a REAL allocator
//! exhaustion at init do to a database on disk?
//!
//! The claims:
//! - a process that declares an oversized capacity under a hard
//!   address-space limit dies by the allocator (abort, not a panic that
//!   something might catch) — the OS-level fail-stop;
//! - the database directory is BYTE-IDENTICAL after the death: OOM at
//!   init is exactly a crash before the first write, a fate the crash
//!   sweeps already cover exhaustively;
//! - the flock died with the process (kernel-released, like any crash),
//!   so a normal open immediately afterwards succeeds and serves every
//!   row — no stale lock, no operator step;
//! - the probe itself is honest: the same binary with a sane capacity
//!   under the same limit opens fine (the death is the allocation's,
//!   not some environmental accident).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use dabqlite_core::{Capacities, Output, VALUE_LEN};
use dabqlite_host::{Host, PosixStorage};

const CAPS: Capacities = Capacities { rows: 64 };

/// Enough rows that arena + index dwarf the limit below: 50M rows is
/// ~1.6 GiB of arena against a 256 MiB address space.
const HUGE_ROWS: u64 = 50_000_000;
const RLIMIT_MB: u64 = 256;

/// Run the probe under an address-space ceiling imposed from OUTSIDE the
/// process, by the shell (`ulimit -v`, in KiB, is `RLIMIT_AS`). The limit
/// arrives before `exec`, so the probe binary itself contains no FFI and
/// no unsafe code — the harness stays as safe as the engine it tests.
fn probe_under_limit(dir: &Path, rows: u64, limit_mb: u64) -> std::process::Output {
    Command::new("sh")
        .args([
            "-c",
            r#"ulimit -v "$1" && exec "$2" "$3" "$4""#,
            "sh",
            &(limit_mb * 1024).to_string(),
            env!("CARGO_BIN_EXE_oomprobe"),
            dir.to_str().expect("utf8 dir"),
            &rows.to_string(),
        ])
        .output()
        .expect("spawn probe")
}

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-oom-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Every file in the directory, name → bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| {
            let e = e.expect("entry");
            let name = e.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(e.path()).expect("read file");
            (name, bytes)
        })
        .collect()
}

#[test]
fn oom_at_init_is_a_crash_before_the_first_write() {
    let dir = scratch_dir("init");

    // A database with real committed data, then closed.
    let mut acked: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
    {
        let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
        assert!(matches!(
            host.open().expect("probe"),
            Output::OpenDone { result: Ok(0) }
        ));
        for i in 0..CAPS.rows {
            let value = [(i as u8).wrapping_mul(41); VALUE_LEN];
            assert!(matches!(
                host.insert(i, value),
                Output::InsertDone { result: Ok(()), .. }
            ));
            acked.push((i, value));
        }
    }
    let before = snapshot(&dir);

    // Honesty control FIRST: the same limit, a sane capacity — the probe
    // binary opens and reads the database fine. Whatever kills the next
    // run is therefore the allocation, not the limit or the environment.
    let out = probe_under_limit(&dir, CAPS.rows, RLIMIT_MB);
    assert!(
        out.status.success(),
        "control probe failed under the limit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("opened {}", CAPS.rows),
        "control probe must serve the whole database"
    );

    // The real thing: an oversized capacity under the same limit. The
    // init allocation fails; Rust's allocator error handler ABORTS —
    // no exit code 0, no unwinding something could swallow.
    let out = probe_under_limit(&dir, HUGE_ROWS, RLIMIT_MB);
    assert!(
        !out.status.success(),
        "a {HUGE_ROWS}-row arena fit inside {RLIMIT_MB} MiB?"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("memory allocation of"),
        "death must be the allocator's own abort, got: {stderr}"
    );

    // OOM ≡ crash-before-first-write: not one byte moved.
    assert_eq!(
        snapshot(&dir),
        before,
        "an init-time OOM modified the database directory"
    );

    // The flock died with the process: a normal open works NOW, no
    // cleanup, and every acked row is served exact.
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open after oom"));
    assert!(matches!(
        host.open().expect("probe"),
        Output::OpenDone { result: Ok(n) } if n == CAPS.rows
    ));
    for &(id, value) in &acked {
        assert!(matches!(
            host.get(id),
            Output::GetDone { result: Ok(Some(v)), .. } if v == value
        ));
    }
    std::fs::remove_dir_all(&dir).ok();
}
