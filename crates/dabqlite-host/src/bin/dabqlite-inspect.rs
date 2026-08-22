//! The inspector CLI (docs/DESIGN.md §9 step 8): `sqlite3 file.db` for
//! dabqlite. Read-only forensics over a database directory.
//!
//! Usage: `dabqlite-inspect <dir> [--verify]`
//!
//! - Opens every file READ-ONLY and takes NO lock: forensics must work
//!   beside a live (or wedged) writer, and a mid-commit view is itself
//!   valid evidence — the superblock protocol is what makes torn states
//!   interpretable.
//! - Never writes a byte anywhere. All analysis is `dabqlite_core::inspect`,
//!   which is pure (bytes in, report out).
//! - `--verify` sets the exit code for scripting: 0 when the directory
//!   would open cleanly with no rollback evidence, 2 otherwise.
//! - Output prints file NAMES, never paths, so it is byte-deterministic
//!   for a given database — and golden-tested as such.

use std::path::Path;
use std::process::ExitCode;

use dabqlite_core::inspect::{inspect, InspectReport, SlotState, Verdict};
use dabqlite_core::migration::V1_SCHEMA_HASH;
use dabqlite_core::SCHEMA_HASH;
use dabqlite_host::posix::{rows_file_name, SUPERBLOCK_FILE};

fn read_optional(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn print_report(dir: &Path, report: &InspectReport) {
    println!("dabqlite inspector");
    println!("binary schema   0x{SCHEMA_HASH:016X}");
    println!("legacy schema   0x{V1_SCHEMA_HASH:016X} (migratable)");
    println!();

    println!("files:");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".dabq"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for name in names {
        let size = std::fs::metadata(dir.join(&name))
            .map(|m| m.len())
            .unwrap_or(0);
        let role = if name == SUPERBLOCK_FILE {
            "superblock"
        } else if name == rows_file_name(SCHEMA_HASH) {
            "rows (current schema)"
        } else if name == rows_file_name(V1_SCHEMA_HASH) {
            "rows (legacy schema; inert once migrated)"
        } else if name == "lock.dabq" {
            "single-writer lock (flock; empty by design)"
        } else {
            "UNRECOGNIZED (orphan? not part of the declared file set)"
        };
        println!("  {name:<28} {size:>8} B  {role}");
    }
    println!();

    println!("superblock slots:");
    for (i, slot) in report.slots.iter().enumerate() {
        match slot {
            SlotState::Missing => println!("  slot {i}: missing (file too short)"),
            SlotState::Empty => println!("  slot {i}: empty (never written)"),
            SlotState::Invalid => {
                println!("  slot {i}: INVALID (torn write, bit rot, or garbage)")
            }
            SlotState::Valid {
                generation,
                row_count,
                schema,
                in_home_pair,
            } => {
                let live = report.live.is_some_and(|l| l.slot as usize == i);
                let mut notes = String::new();
                if !in_home_pair {
                    notes.push_str("  MISPLACED (misdirected write; distrusted)");
                }
                if *schema != SCHEMA_HASH {
                    notes.push_str("  foreign schema");
                }
                if live {
                    notes.push_str("  <- LIVE");
                }
                println!(
                    "  slot {i}: gen {generation}, rows {row_count}, schema 0x{schema:016X}{notes}"
                );
            }
        }
    }
    println!();

    println!("row zone:");
    println!("  committed valid    {}", report.rows.committed_valid);
    println!("  committed corrupt  {}", report.rows.committed_corrupt);
    for off in &report.rows.corrupt_offsets {
        println!("    corrupt slot at byte offset {off}");
    }
    println!("  duplicate ids      {}", report.rows.duplicate_ids);
    for id in &report.rows.duplicate_samples {
        println!("    duplicate id {id}");
    }
    println!("  orphans (valid)    {}", report.rows.orphan_valid);
    println!("  orphans (garbage)  {}", report.rows.orphan_invalid);
    println!(
        "  rollback evidence  {}",
        if report.rollback_evidence {
            "YES - 2+ valid rows beyond the manifest: acknowledged commits \
             were rolled back by an out-of-budget fault"
        } else {
            "none"
        }
    );
    println!();

    match report.verdict {
        Verdict::FreshInit => println!("verdict: empty - open would initialize fresh"),
        Verdict::Recovers { rows } => println!("verdict: healthy - open recovers {rows} rows"),
        Verdict::SchemaMismatch {
            file_schema,
            migratable,
        } => {
            println!(
                "verdict: schema mismatch - file 0x{file_schema:016X} vs binary 0x{SCHEMA_HASH:016X}"
            );
            if migratable {
                println!("         this is the compiled-in legacy schema: run the migration");
            } else {
                println!(
                    "         no migration path in this binary: safe brick, zero bytes touched"
                );
            }
        }
        Verdict::Corrupt { what } => println!("verdict: CORRUPT - {what}"),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: dabqlite-inspect <dir> [--verify]");
        return ExitCode::FAILURE;
    };
    let verify = match args.next().as_deref() {
        None => false,
        Some("--verify") => true,
        Some(other) => {
            eprintln!("unknown argument: {other}");
            return ExitCode::FAILURE;
        }
    };
    let dir = Path::new(&dir);

    let superblock = read_optional(&dir.join(SUPERBLOCK_FILE));
    let rows = read_optional(&dir.join(rows_file_name(SCHEMA_HASH)));
    let report = inspect(&superblock, &rows);
    print_report(dir, &report);

    if verify {
        let healthy = matches!(
            report.verdict,
            Verdict::FreshInit | Verdict::Recovers { .. }
        ) && !report.rollback_evidence;
        if !healthy {
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
