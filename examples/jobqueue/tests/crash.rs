//! The point of the whole exercise: kill the worker with SIGKILL at
//! arbitrary points, restart it, and check that every acknowledged job is
//! accounted for exactly once — none lost, none duplicated.
//!
//! Three independent detectors run against every generation:
//!
//! 1. **The database never regresses.** After every kill we reopen and
//!    read the two watermarks. Neither may ever go backwards.
//! 2. **The journal is never ahead of the database.** Journal records are
//!    written strictly *after* dabqlite returned `Ok`, so if the database
//!    comes back missing something the journal saw acknowledged, that is
//!    lost acknowledged data.
//! 3. **The commit checksum is exact.** The commit watermark row carries
//!    an order-sensitive fold over every committed job id. It must equal
//!    the fold over `1..=committed` computed independently by the test.
//!    A single dropped, repeated or reordered commit changes it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jobqueue::{audit, expected_checksum, inspect, Config};

const EXE: &str = env!("CARGO_BIN_EXE_jobqueue");

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-crash-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn(cfg: &Config) -> Child {
    let mut cmd = Command::new(EXE);
    cmd.arg("run")
        .arg("--root")
        .arg(&cfg.root)
        .arg("--journal")
        .arg(&cfg.journal)
        .arg("--jobs")
        .arg(cfg.jobs.to_string())
        .arg("--capacity")
        .arg(cfg.capacity.to_string())
        .arg("--window")
        .arg(cfg.window.to_string())
        .arg("--compact-at")
        .arg(cfg.compact_at.to_string())
        .arg("--delay-us")
        .arg(cfg.delay_us.to_string());
    if !cfg.reap {
        cmd.arg("--no-reap");
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker")
}

fn journal_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[derive(Debug, Default)]
struct Tally {
    cycles: u64,
    killed: u64,
    killed_by_kill_binary: u64,
    exited_first: u64,
    jobs_committed: u64,
    redundant_work: u64,
    compactions: u64,
    interrupted_compactions: u64,
    /// Restarts that found an in-flight row past the manifest: proof the
    /// kill landed inside a dabqlite commit.
    opens_with_orphan_rows: u64,
    runs: u64,
}

/// One generation's knobs. A struct rather than eight positional
/// arguments: the call sites below differ only in these numbers, and
/// `("g2", 150, 256, 12, true, 25, ...)` tells a reader nothing about
/// which number is the capacity and which is the window.
struct Gen {
    tag: &'static str,
    jobs: u64,
    capacity: u64,
    window: u64,
    reap: bool,
    cycles: u64,
    seed: u64,
}

/// One generation: many kill/restart cycles against a single database and
/// a single journal, then a clean drain, then a full audit.
///
/// `Instant::now` appears below. The repository's clippy config bans it
/// inside the deterministic boundary — the engine has no clock, by design
/// §7.1 — but this is a test *harness* whose whole job is to kill a real
/// child process at a real wall-clock moment. The determinism that matters
/// here is the seeded `Rng`, which chooses every decision the harness makes;
/// the clock only bounds how long it waits for a child that has wedged.
#[allow(clippy::disallowed_methods)]
fn generation(g: Gen, t: &mut Tally) {
    let Gen {
        tag,
        jobs,
        capacity,
        window,
        reap,
        cycles,
        seed,
    } = g;
    let root = scratch(tag);
    let journal = root.join("journal.log");
    let mut cfg = Config::new(root.join("db"), &journal, jobs);
    cfg.capacity = capacity;
    cfg.window = window;
    cfg.reap = reap;
    cfg.compact_at = 0.7;

    let mut rng = Rng(seed);
    let (mut prev_committed, mut prev_enqueued) = (0u64, 0u64);

    for cycle in 0..cycles {
        let mut child = spawn(&cfg);
        let start_len = journal_len(&journal);
        let mut killed = false;

        match rng.below(3) {
            // Wait for the worker to acknowledge a few operations, then
            // kill it a random sliver later — lands inside an operation
            // rather than tidily between two.
            0 | 1 => {
                let grow_by = 8 + rng.below(180);
                let deadline = Instant::now() + Duration::from_millis(400);
                loop {
                    if journal_len(&journal) >= start_len + grow_by {
                        std::thread::sleep(Duration::from_micros(rng.below(900)));
                        killed = child.kill().is_ok();
                        break;
                    }
                    if child.try_wait().expect("try_wait").is_some() {
                        break;
                    }
                    if Instant::now() > deadline {
                        killed = child.kill().is_ok();
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(120));
                }
            }
            // A real external `kill -9`, to prove the signal is not
            // something Rust is softening on the way out.
            _ => {
                std::thread::sleep(Duration::from_millis(1 + rng.below(40)));
                if child.try_wait().expect("try_wait").is_none() {
                    let out = Command::new("kill")
                        .arg("-9")
                        .arg(child.id().to_string())
                        .status()
                        .expect("run kill(1)");
                    assert!(out.success(), "kill -9 failed");
                    killed = true;
                    t.killed_by_kill_binary += 1;
                }
            }
        }

        let out = child.wait_with_output().expect("wait");
        // `kill()` can win the race against a worker that was already on
        // its way out; only count a death by signal as a real kill.
        let killed = killed && out.status.code().is_none();
        if killed {
            t.killed += 1;
            assert!(
                out.status.code().is_none(),
                "cycle {cycle}: expected death by signal, got {:?}",
                out.status
            );
        } else {
            t.exited_first += 1;
        }
        // A worker that exits on its own must never report the rollback
        // alarm (exit 3) or a hard error (exit 1).
        if let Some(code) = out.status.code() {
            assert!(
                code == 0 || code == 10,
                "cycle {cycle}: worker exited {code}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // ---- detectors, after every single kill ----------------------
        let ins = inspect(&cfg).expect("reopen after kill");
        assert!(
            !ins.rollback_evidence,
            "cycle {cycle}: dabqlite reports rolled-back acknowledged commits: {ins:?}"
        );
        assert!(
            ins.committed >= prev_committed,
            "cycle {cycle}: commit watermark went BACKWARDS {prev_committed} -> {}",
            ins.committed
        );
        assert!(
            ins.enqueue_next >= prev_enqueued,
            "cycle {cycle}: enqueue watermark went BACKWARDS {prev_enqueued} -> {}",
            ins.enqueue_next
        );
        assert_eq!(
            ins.checksum,
            expected_checksum(ins.committed),
            "cycle {cycle}: commit checksum does not match an exact in-order \
             commit of jobs 1..={} — a commit was lost, repeated or reordered",
            ins.committed
        );

        let a = audit(&journal).expect("audit");
        assert!(
            a.duplicate_commits.is_empty(),
            "cycle {cycle}: job(s) {:?} were committed TWICE — the database lost \
             an acknowledged commit and the restart redid it",
            a.duplicate_commits
        );
        assert!(
            a.duplicate_enqueues.is_empty(),
            "cycle {cycle}: job(s) {:?} were inserted TWICE — the database lost \
             an acknowledged insert",
            a.duplicate_enqueues
        );
        if let Some(&last) = a.committed.last() {
            assert!(
                last <= ins.committed,
                "cycle {cycle}: the journal saw commit {last} acknowledged but the \
                 database came back at {} — ACKNOWLEDGED DATA LOST",
                ins.committed
            );
        }
        if let Some(&last) = a.enqueued.last() {
            // The insert is acknowledged before the watermark moves, so
            // the watermark is legitimately at `last` or `last + 1`.
            assert!(
                last <= ins.enqueue_next,
                "cycle {cycle}: the journal saw insert {last} acknowledged but the \
                 enqueue watermark came back at {} — ACKNOWLEDGED DATA LOST",
                ins.enqueue_next
            );
            // The strong form: every job the journal saw inserted and did
            // not see committed must still be sitting in the database.
            let present: std::collections::BTreeSet<u64> =
                ins.outstanding.iter().map(|&(id, _)| id).collect();
            for id in (ins.committed + 1)..=last {
                assert!(
                    present.contains(&id),
                    "cycle {cycle}: job {id} was inserted and acknowledged, is not \
                     committed (watermark {}), and is GONE from the database",
                    ins.committed
                );
            }
        }

        prev_committed = ins.committed;
        prev_enqueued = ins.enqueue_next;
        t.cycles += 1;
    }

    // ---- drain cleanly ------------------------------------------------
    let mut attempts = 0;
    loop {
        let out = Command::new(EXE)
            .arg("run")
            .arg("--root")
            .arg(&cfg.root)
            .arg("--journal")
            .arg(&cfg.journal)
            .arg("--jobs")
            .arg(cfg.jobs.to_string())
            .arg("--capacity")
            .arg(cfg.capacity.to_string())
            .arg("--window")
            .arg(cfg.window.to_string())
            .arg("--compact-at")
            .arg(cfg.compact_at.to_string())
            .args(if reap { vec![] } else { vec!["--no-reap"] })
            .output()
            .expect("final drain");
        attempts += 1;
        if out.status.code() == Some(0) {
            break;
        }
        assert!(
            attempts < 5,
            "queue would not drain: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // ---- the full accounting -----------------------------------------
    let ins = inspect(&cfg).expect("final inspect");
    assert!(!ins.rollback_evidence);
    assert_eq!(ins.committed, jobs, "not every job committed");
    assert_eq!(ins.checksum, expected_checksum(jobs));
    if reap {
        assert!(
            ins.outstanding.is_empty(),
            "reaped queue should be empty, got {:?}",
            ins.outstanding
        );
        assert_eq!(ins.live, 2, "only the two meta rows should remain");
    }

    // The journal is allowed to be BEHIND the database — a kill landing
    // between dabqlite's `Ok` and our `write(2)` loses the record but not
    // the data — so what it must be is a strictly increasing, duplicate-free
    // subsequence of 1..=jobs. Completeness is proved by the database's own
    // watermark and checksum above; the journal proves nothing was done twice.
    let a = audit(&journal).expect("audit");
    let subsequence_of_jobs = |v: &[u64], what: &str| {
        assert!(
            v.windows(2).all(|w| w[0] < w[1]),
            "{what} records are not strictly increasing: {v:?}"
        );
        assert!(
            v.iter().all(|&id| (1..=jobs).contains(&id)),
            "{what} records contain an id outside 1..={jobs}: {v:?}"
        );
    };
    subsequence_of_jobs(&a.committed, "commit");
    subsequence_of_jobs(&a.enqueued, "insert");
    assert!(
        a.committed.len() as u64 >= jobs - t.cycles,
        "far too many commit records missing: {} of {jobs}",
        a.committed.len()
    );

    t.jobs_committed += jobs;
    t.redundant_work += (a.worked.len() as u64).saturating_sub(jobs);
    t.compactions += a.compactions_finished as u64;
    t.interrupted_compactions += (a.compactions_started - a.compactions_finished) as u64;
    t.opens_with_orphan_rows += a.opens_with_orphan_rows as u64;
    t.runs += a.runs as u64;

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn sigkill_never_loses_or_duplicates_an_acknowledged_job() {
    let mut t = Tally::default();
    // Four independent lifetimes, each killed 25 times mid-flight. The
    // capacities are deliberately tiny so compaction runs constantly and
    // the kills land inside it as often as inside ordinary writes.
    generation(
        Gen {
            tag: "g0",
            jobs: 200,
            capacity: 128,
            window: 6,
            reap: true,
            cycles: 25,
            seed: 0xD1CE_D1CE,
        },
        &mut t,
    );
    generation(
        Gen {
            tag: "g1",
            jobs: 200,
            capacity: 96,
            window: 4,
            reap: true,
            cycles: 25,
            seed: 0x5EED_0001,
        },
        &mut t,
    );
    generation(
        Gen {
            tag: "g2",
            jobs: 150,
            capacity: 256,
            window: 12,
            reap: true,
            cycles: 25,
            seed: 0x00A1_1CE5,
        },
        &mut t,
    );
    generation(
        Gen {
            tag: "g3",
            jobs: 120,
            capacity: 512,
            window: 3,
            reap: true,
            cycles: 25,
            seed: 0xFEED_BEEF,
        },
        &mut t,
    );
    // A capacity so small that the worker spends much of its life inside
    // the compaction swap, so the kills land there too.
    generation(
        Gen {
            tag: "g4",
            jobs: 150,
            capacity: 48,
            window: 3,
            reap: true,
            cycles: 30,
            seed: 0x0BAD_F00D,
        },
        &mut t,
    );

    eprintln!("crash tally: {t:#?}");
    assert!(t.killed >= 90, "not enough kills actually landed: {t:?}");
    assert!(
        t.opens_with_orphan_rows >= 5,
        "kills never landed inside a dabqlite commit (no orphan rows seen \
         at any restart): {t:?}"
    );
    assert!(
        t.interrupted_compactions >= 1,
        "no compaction was ever interrupted by a kill; the swap protocol \
         was never exercised under crash: {t:?}"
    );
    assert_eq!(t.jobs_committed, 820);
}

/// The same torture without reaping: completed jobs are archived to a
/// different id instead of deleted, so the database grows and compaction
/// has real work to move.
#[test]
fn sigkill_torture_without_reaping() {
    let mut t = Tally::default();
    generation(
        Gen {
            tag: "n0",
            jobs: 120,
            capacity: 512,
            window: 5,
            reap: false,
            cycles: 20,
            seed: 0x1234_5678,
        },
        &mut t,
    );
    eprintln!("no-reap tally: {t:#?}");
    assert!(t.killed >= 8, "{t:?}");
}
