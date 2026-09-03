#![cfg(unix)]
//! The infinitely pegged CPU, made literal: a REAL writer process is
//! SIGSTOPed — repeatedly, at kernel-chosen points inside its commit
//! stream — held frozen, and resumed. The clockless core cannot even
//! observe that this happened (docs/DESIGN.md §7.1: no clock, no
//! timeouts), so the claims are absolute, not statistical:
//!
//! - the paused writer resumes and completes its workload EXACTLY — every
//!   row present, byte-for-byte, no matter where in a commit's five I/Os
//!   the freeze landed or how long it lasted;
//! - the single-writer flock protects the database for the entire pause:
//!   a frozen writer still owns the store, and a second process is
//!   refused at the door the whole time (no "it looks dead, steal the
//!   lock" hazard — flock releases on death, not on stillness);
//! - the inspector still works while the writer is frozen mid-commit —
//!   it takes no lock and reads bytes as they are, torn or not.
//!
//! Together with `dabqlite-sim/tests/saturation.rs` (bit-identical
//! lifetimes under saturated cores) this closes the CPU-starvation
//! surface: slow is indistinguishable from fast, and stopped is just
//! very slow.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use dabqlite_core::{Capacities, Output, VALUE_LEN};
use dabqlite_host::{Host, PosixStorage};

const N: u64 = 512;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-sigstop-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Send a named signal via the POSIX shell's `kill` builtin — the same
/// syscall, reached without FFI, so the harness stays as safe as the
/// engine it tests.
fn signal(pid: u32, sig: &str) -> bool {
    Command::new("sh")
        .args(["-c", r#"kill -s "$1" "$2""#, "sh", sig, &pid.to_string()])
        .status()
        .expect("spawn kill")
        .success()
}

fn proc_state(pid: u32) -> char {
    // /proc/<pid>/stat field 3 is the state letter; the comm field before
    // it is parenthesized and may contain spaces, so split after ')'.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let after = stat.rsplit_once(')').expect("comm").1;
    after
        .split_whitespace()
        .next()
        .and_then(|s| s.chars().next())
        .expect("state")
}

#[test]
fn a_writer_frozen_mid_commit_resumes_and_completes_exactly() {
    let dir = scratch_dir("freeze");

    let mut churner = Command::new(env!("CARGO_BIN_EXE_churner"))
        .arg(&dir)
        .arg(N.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn churner");
    let pid = churner.id();
    let mut stdout = BufReader::new(churner.stdout.take().expect("stdout"));

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read ready");
    assert_eq!(line.trim(), "ready");
    // Go: the writer starts committing 512 rows with real fsyncs.
    let mut stdin = churner.stdin.take().expect("stdin");
    writeln!(stdin, "go").expect("go");

    // Freeze/thaw cycles at arbitrary points in the commit stream. Each
    // SIGSTOP lands wherever the kernel finds the process — before, after,
    // or in the middle of a commit's writes and fsyncs; the staggered
    // offsets sample several such points per run, on slow disks and fast.
    // (The churner holds its lock until dismissed, so even a disk that
    // finishes the whole workload instantly cannot race these cycles.)
    for (cycle, delay_ms) in [0u64, 2, 5, 15, 40].into_iter().enumerate() {
        std::thread::sleep(Duration::from_millis(delay_ms));
        assert!(signal(pid, "STOP"), "SIGSTOP failed on cycle {cycle}");
        // The process is truly frozen (state T), not merely signaled.
        let mut state = proc_state(pid);
        for _ in 0..100 {
            if state == 'T' {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            state = proc_state(pid);
        }
        assert_eq!(state, 'T', "churner not stopped on cycle {cycle}");

        // Held frozen — mid-commit, as far as anyone knows.
        std::thread::sleep(Duration::from_millis(50));

        // The frozen writer still owns the database: a second process is
        // refused for the entire pause. Stillness is not death.
        match PosixStorage::open_dir(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("cycle {cycle}: wrong refusal: {e}"),
            Ok(_) => panic!("cycle {cycle}: a frozen writer lost the lock"),
        }

        // The inspector needs no lock and mutates nothing: it works on
        // the frozen writer's directory, torn mid-commit bytes and all.
        let out = Command::new(env!("CARGO_BIN_EXE_dabqlite-inspect"))
            .arg(&dir)
            .output()
            .expect("run inspector");
        assert!(
            out.status.success(),
            "cycle {cycle}: inspector failed on a frozen writer's files:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        assert!(signal(pid, "CONT"), "SIGCONT failed on cycle {cycle}");
    }

    // Thawed for the last time: the writer finishes as if nothing at all
    // had happened — because, from where it stands, nothing did.
    line.clear();
    stdout.read_line(&mut line).expect("read done");
    assert_eq!(line.trim(), format!("done {N}"), "churner did not finish");
    writeln!(stdin, "exit").expect("dismiss");
    assert!(churner.wait().expect("reap").success());

    // Every row, byte-exact.
    let mut host = Host::new(
        Capacities { rows: N },
        PosixStorage::open_dir(&dir).expect("open after churn"),
    );
    assert!(matches!(
        host.open().expect("probe"),
        Output::OpenDone { result: Ok(n) } if n == N
    ));
    for i in 0..N {
        let want = [(i as u8).wrapping_mul(37) % 251; VALUE_LEN];
        assert!(matches!(
            host.get(i),
            Output::GetDone { result: Ok(Some(v)), .. } if v == want
        ));
    }
    std::fs::remove_dir_all(&dir).ok();
}
