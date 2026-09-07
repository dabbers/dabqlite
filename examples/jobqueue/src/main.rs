//! The worker binary the crash test kills.
//!
//! Usage:
//!   jobqueue run    --root DIR --journal FILE --jobs N [opts]
//!   jobqueue stat   --root DIR --journal FILE --jobs N [opts]
//!   jobqueue audit  --journal FILE
//!   jobqueue churn  --root DIR --journal FILE [--rounds N] [--batch SLOTS]
//!                   [--max-cell BYTES] [--cells N] [--vary]
//!   jobqueue verify --root DIR --journal FILE [--capacity C]
//!
//! Exit codes:
//!   0  clean, drained
//!   1  error
//!   2  usage
//!   3  ROLLBACK EVIDENCE (acknowledged data lost)
//!   4  audit found a duplicate
//!   5  HALF-APPLIED BATCH
//!   6  SHORT VALUE (a multi-slot value came back truncated)
//!  10  ran, did not drain

use std::path::PathBuf;
use std::process::ExitCode;

use jobqueue::{
    audit, churn, churn_verify, inspect, run, ChurnConfig, Config, Journal, QueueError,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: jobqueue <run|stat|audit|churn|verify> [--root DIR] [--journal FILE] ..."
        );
        return ExitCode::from(2);
    }
    match dispatch(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("jobqueue: {e}");
            match e {
                // 3 = the alarm the library asks hosts to raise.
                QueueError::RollbackEvidence(_) => ExitCode::from(3),
                // 5 = the alarm THIS crate raises: a batch was not atomic.
                QueueError::HalfBatch(_) => ExitCode::from(5),
                // 6 = the alarm this REVISION raises: a value that spans
                // several row slots came back short.
                QueueError::ShortValue { .. } => ExitCode::from(6),
                _ => ExitCode::from(1),
            }
        }
    }
}

fn dispatch(args: &[String]) -> Result<ExitCode, QueueError> {
    let cmd = args[0].as_str();
    let mut cfg = Config::new(PathBuf::from("queue"), PathBuf::from("queue.journal"), 100);
    let mut churn_cfg = ChurnConfig::new(PathBuf::from("queue"), PathBuf::from("queue.journal"));
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut val = || -> String {
            i += 1;
            args.get(i).cloned().unwrap_or_default()
        };
        match flag {
            "--root" => {
                let v = val();
                cfg.root = PathBuf::from(&v);
                churn_cfg.root = PathBuf::from(&v);
            }
            "--journal" => {
                let v = val();
                cfg.journal = PathBuf::from(&v);
                churn_cfg.journal = PathBuf::from(&v);
            }
            "--jobs" => cfg.jobs = val().parse().unwrap_or(cfg.jobs),
            "--capacity" => {
                let v = val().parse().unwrap_or(cfg.capacity);
                cfg.capacity = v;
                churn_cfg.capacity = v;
            }
            "--window" => cfg.window = val().parse().unwrap_or(cfg.window),
            "--enqueue-chunk" => cfg.enqueue_chunk = val().parse().unwrap_or(cfg.enqueue_chunk),
            "--compact-at" => {
                let v = val().parse().unwrap_or(cfg.compact_at);
                cfg.compact_at = v;
                churn_cfg.compact_at = v;
            }
            "--delay-us" => cfg.delay_us = val().parse().unwrap_or(cfg.delay_us),
            "--max-steps" => cfg.max_steps = val().parse().unwrap_or(cfg.max_steps),
            "--no-reap" => cfg.reap = false,
            "--rounds" => churn_cfg.rounds = val().parse().unwrap_or(churn_cfg.rounds),
            "--batch" => churn_cfg.batch = val().parse().unwrap_or(churn_cfg.batch),
            "--max-cell" => churn_cfg.max_cell = val().parse().unwrap_or(churn_cfg.max_cell),
            "--cells" => churn_cfg.cells = val().parse().unwrap_or(churn_cfg.cells),
            "--vary" => churn_cfg.vary = true,
            "--seed" => churn_cfg.seed = val().parse().unwrap_or(churn_cfg.seed),
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
                "{} steps={} enq={} claim={} work={} commit={} batches={} compactions={} \
                 payload_bytes={} committed_to={} checksum={:#018x} fill={:.2}",
                if r.drained { "DRAINED" } else { "PARTIAL" },
                r.steps,
                r.enqueued,
                r.claimed,
                r.work_performed,
                r.committed,
                r.batches,
                r.compactions,
                r.payload_bytes,
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
                 live={} slots={} dead={} capacity={} rollback_evidence={} orphans={} \
                 half_batch={:?} short_payloads={:?}",
                s.enqueue_next,
                s.committed,
                s.checksum,
                s.outstanding.len(),
                s.live,
                s.slots,
                s.dead,
                s.capacity,
                s.rollback_evidence,
                s.orphan_valid_rows,
                s.half_batch,
                s.short_payloads
            );
            for (id, job) in &s.outstanding {
                println!(
                    "  job {id} state={} attempts={} payload={}B",
                    job.state,
                    job.attempts,
                    job.payload.len()
                );
            }
            Ok(if !s.short_payloads.is_empty() {
                ExitCode::from(6)
            } else if s.half_batch.is_some() {
                ExitCode::from(5)
            } else {
                ExitCode::SUCCESS
            })
        }
        "audit" => {
            let a = audit(&cfg.journal)?;
            println!(
                "runs={} compactions={}/{} enqueued={} committed={} worked={} \
                 orphan_opens={} orphan_batch_opens={} widest_orphan_batch={} \
                 churn={}/{} dup_enqueue={:?} dup_commit={:?}",
                a.runs,
                a.compactions_finished,
                a.compactions_started,
                a.enqueued.len(),
                a.committed.len(),
                a.worked.len(),
                a.opens_with_orphan_rows,
                a.opens_with_orphan_batches,
                a.widest_orphan_batch,
                a.churn_acks,
                a.churn_intents,
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
        "churn" => {
            let mut journal = Journal::open(&churn_cfg.journal)?;
            let r = churn(&churn_cfg, &mut journal)?;
            println!(
                "CHURN rounds={} ops={} rows={} compactions={}",
                r.rounds, r.ops, r.rows, r.compactions
            );
            Ok(ExitCode::SUCCESS)
        }
        "verify" => {
            let v = churn_verify(&churn_cfg)?;
            println!(
                "cells={} digest={:#018x} computed={:#018x} intents={} orphans={} \
                 rollback_evidence={} widest_cell={} short_values={:?} half_batch={:?}",
                v.cells,
                v.digest,
                v.computed,
                v.rounds_logged,
                v.orphan_valid_rows,
                v.rollback_evidence,
                v.widest_cell,
                v.short_values,
                v.half_batch
            );
            Ok(if !v.short_values.is_empty() {
                ExitCode::from(6)
            } else if v.half_batch.is_some() {
                ExitCode::from(5)
            } else {
                ExitCode::SUCCESS
            })
        }
        other => {
            eprintln!("jobqueue: unknown command {other}");
            Ok(ExitCode::from(2))
        }
    }
}
