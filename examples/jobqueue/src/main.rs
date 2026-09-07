//! The worker binary the crash test kills.
//!
//! Usage:
//!   jobqueue run   --root DIR --journal FILE --jobs N [opts]
//!   jobqueue stat  --root DIR --journal FILE --jobs N [opts]
//!   jobqueue audit --journal FILE

use std::path::PathBuf;
use std::process::ExitCode;

use jobqueue::{audit, inspect, run, Config, Journal, QueueError};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: jobqueue <run|stat|audit> [--root DIR] [--journal FILE] ...");
        return ExitCode::from(2);
    }
    match dispatch(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jobqueue: {e}");
            // 3 = the alarm the library asks hosts to raise.
            if matches!(e, QueueError::RollbackEvidence(_)) {
                return ExitCode::from(3);
            }
            ExitCode::from(1)
        }
    }
}

fn dispatch(args: &[String]) -> Result<ExitCode, QueueError> {
    let cmd = args[0].as_str();
    let mut cfg = Config::new(PathBuf::from("queue"), PathBuf::from("queue.journal"), 100);
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut val = || -> String {
            i += 1;
            args.get(i).cloned().unwrap_or_default()
        };
        match flag {
            "--root" => cfg.root = PathBuf::from(val()),
            "--journal" => cfg.journal = PathBuf::from(val()),
            "--jobs" => cfg.jobs = val().parse().unwrap_or(cfg.jobs),
            "--capacity" => cfg.capacity = val().parse().unwrap_or(cfg.capacity),
            "--window" => cfg.window = val().parse().unwrap_or(cfg.window),
            "--compact-at" => cfg.compact_at = val().parse().unwrap_or(cfg.compact_at),
            "--delay-us" => cfg.delay_us = val().parse().unwrap_or(cfg.delay_us),
            "--max-steps" => cfg.max_steps = val().parse().unwrap_or(cfg.max_steps),
            "--no-reap" => cfg.reap = false,
            other => {
                eprintln!("jobqueue: unknown flag {other}");
                return Ok(ExitCode::from(2));
            }
        }
        i += 1;
    }

    match cmd {
        "run" => {
            let mut journal = Journal::open(&cfg.journal)?;
            let r = run(&cfg, &mut journal)?;
            println!(
                "{} steps={} enq={} claim={} work={} commit={} reap={} compactions={} \
                 committed_to={} checksum={:#018x} fill={:.2}",
                if r.drained { "DRAINED" } else { "PARTIAL" },
                r.steps,
                r.enqueued,
                r.claimed,
                r.work_performed,
                r.committed,
                r.reaped,
                r.compactions,
                r.last_committed,
                r.checksum,
                r.stats_fill
            );
            Ok(if r.drained {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(10)
            })
        }
        "stat" => {
            let s = inspect(&cfg)?;
            println!(
                "enqueue_next={} committed={} checksum={:#018x} outstanding={} \
                 live={} slots={} dead={} capacity={} rollback_evidence={} orphans={}",
                s.enqueue_next,
                s.committed,
                s.checksum,
                s.outstanding.len(),
                s.live,
                s.slots,
                s.dead,
                s.capacity,
                s.rollback_evidence,
                s.orphan_valid_rows
            );
            for (id, job) in &s.outstanding {
                println!("  job {id} state={} attempts={}", job.state, job.attempts);
            }
            Ok(ExitCode::SUCCESS)
        }
        "audit" => {
            let a = audit(&cfg.journal)?;
            println!(
                "runs={} compactions={}/{} enqueued={} committed={} worked={} reaped={} \
                 dup_enqueue={:?} dup_commit={:?}",
                a.runs,
                a.compactions_finished,
                a.compactions_started,
                a.enqueued.len(),
                a.committed.len(),
                a.worked.len(),
                a.reaped.len(),
                a.duplicate_enqueues,
                a.duplicate_commits
            );
            Ok(
                if a.duplicate_commits.is_empty() && a.duplicate_enqueues.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(4)
                },
            )
        }
        other => {
            eprintln!("jobqueue: unknown command {other}");
            Ok(ExitCode::from(2))
        }
    }
}
