//! The inspector CLI (docs/DESIGN.md §9 step 8): `sqlite3 file.db` for
//! dabqlite. Read-only forensics over a database directory.
//!
//! Usage: `dabqlite-inspect <dir> [--verify | --repair-to <newdir> | --gc]`
//!
//! - Opens every file READ-ONLY and takes NO lock: forensics must work
//!   beside a live (or wedged) writer, and a mid-commit view is itself
//!   valid evidence — the superblock protocol is what makes torn states
//!   interpretable.
//! - Never writes a byte anywhere. All analysis is `dabqlite_core::inspect`,
//!   which is pure (bytes in, report out).
//! - `--verify` sets the exit code for scripting: 0 when the directory
//!   would open cleanly with no rollback evidence, 2 otherwise. It is a
//!   full scrub: every committed row is re-verified against its checksum,
//!   so it detects at-rest rot that a running database has not re-read.
//! - `--repair-to <newdir>` rebuilds a clean database from the rows that
//!   still verify, into a NEW directory. The source is opened read-only
//!   and never written — repair-by-rebuild, never repair-in-place, so the
//!   only copy of the truth is never overwritten and a failed repair
//!   costs nothing. Rows that cannot be verified are DROPPED, and exactly
//!   how many is reported: this is the one operation in the system that
//!   knowingly discards data, so it says so plainly.
//! - `--gc` reclaims dead space in place. Today that is exactly one
//!   thing: the legacy rows file a completed migration left behind
//!   (docs/DESIGN.md §4.4 — inert, but not free). It runs ONLY when the
//!   superblock proves the migration finished, and it takes the
//!   single-writer lock, because unlike every other mode here it mutates.
//! - Output prints file NAMES, never paths, so it is byte-deterministic
//!   for a given database — and golden-tested as such.

use std::path::Path;
use std::process::ExitCode;

use dabqlite_core::inspect::{inspect, InspectReport, SlotState, Verdict};
use dabqlite_core::migration::V1_SCHEMA_HASH;
use dabqlite_core::SCHEMA_HASH;
use dabqlite_core::{Capacities, Output};
use dabqlite_host::posix::{rows_file_name, SUPERBLOCK_FILE};
use dabqlite_host::{Host, PosixStorage, ReadOnlyDir};

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

/// Rebuild a clean database in `dest` from every row of `src` that still
/// verifies. Returns (recovered, dropped).
///
/// Safety comes from the shape, not from care: the source is opened
/// through a read-only handle that cannot write, and the destination is a
/// new directory. Nothing is overwritten, so an interrupted repair leaves
/// both the damaged original and a partial copy — never a damaged
/// original made worse.
fn repair_to(src: &Path, dest: &Path) -> Result<(u64, u64), String> {
    if dest.exists() && std::fs::read_dir(dest).map(|d| d.count()).unwrap_or(1) != 0 {
        return Err(format!(
            "{} already exists and is not empty; repair writes a NEW database \
             and will not overwrite one",
            dest.display()
        ));
    }

    // Capacity must cover what the file already holds; the rows file's own
    // length is the honest upper bound.
    let rows_bytes = std::fs::metadata(src.join(rows_file_name(SCHEMA_HASH)))
        .map(|m| m.len())
        .unwrap_or(0);
    let caps = Capacities {
        rows: (rows_bytes / dabqlite_core::ROW_SIZE as u64).max(1),
    };

    let ro = ReadOnlyDir::open_dir(src).map_err(|e| format!("opening {}: {e}", src.display()))?;
    let mut source = Host::new(caps, ro);
    match source
        .open_salvage()
        .map_err(|e| format!("salvage open: {e}"))?
    {
        Output::OpenDone { result: Ok(_) } => {}
        Output::OpenDone { result: Err(e) } => {
            return Err(format!("salvage open refused this directory: {e:?}"))
        }
        other => return Err(format!("unexpected open result: {other:?}")),
    }
    let dropped = source.engine.quarantined();
    let rows: Vec<(u64, [u8; dabqlite_core::VALUE_LEN])> = source.engine.live_rows().collect();
    drop(source);

    let out =
        PosixStorage::open_dir(dest).map_err(|e| format!("creating {}: {e}", dest.display()))?;
    let mut target = Host::new(caps, out);
    match target
        .open()
        .map_err(|e| format!("opening destination: {e}"))?
    {
        Output::OpenDone { result: Ok(0) } => {}
        other => return Err(format!("destination is not empty: {other:?}")),
    }
    for &(id, value) in &rows {
        match target.insert(id, value) {
            Output::InsertDone { result: Ok(()), .. } => {}
            other => return Err(format!("writing row {id}: {other:?}")),
        }
    }
    Ok((rows.len() as u64, dropped))
}

/// Reclaim dead space in place. Returns bytes freed.
///
/// The only dead weight a v1 database accumulates is the legacy rows file
/// of a completed migration. The rows zone itself cannot accumulate
/// garbage: it is append-only under a manifest, and the bytes past the
/// manifest (in-flight and rollback residue) are overwritten by the next
/// insert rather than piling up. There are no deletes yet, so there are
/// no tombstones and no free lists to compact.
///
/// The safety condition is the whole feature: the legacy file is only
/// inert once the superblock names the CURRENT schema. Before that it is
/// the only copy of the data, and deleting it would be the worst bug in
/// the system — so this refuses unless recovery would succeed AND the
/// live superblock copy is on this binary's schema.
fn gc(dir: &Path) -> Result<u64, String> {
    let superblock = read_optional(&dir.join(SUPERBLOCK_FILE));
    let rows = read_optional(&dir.join(rows_file_name(SCHEMA_HASH)));
    let report = inspect(&superblock, &rows);
    let live = match (report.verdict, report.live) {
        (Verdict::Recovers { .. }, Some(live)) => live,
        (verdict, _) => {
            return Err(format!(
                "refusing: this directory does not recover cleanly ({verdict:?}), \
                 so the legacy file may still be the only copy of the data"
            ))
        }
    };
    if live.schema != SCHEMA_HASH {
        return Err(format!(
            "refusing: the live superblock is on schema 0x{:016X}, not this \
             binary's 0x{SCHEMA_HASH:016X} — migrate first; the legacy file \
             is still live data",
            live.schema
        ));
    }
    let legacy = dir.join(rows_file_name(V1_SCHEMA_HASH));
    let bytes = std::fs::metadata(&legacy).map(|m| m.len()).unwrap_or(0);
    if bytes == 0 {
        return Ok(0);
    }
    // This is the one mutating mode, so unlike inspection it takes the
    // single-writer lock: no reclaiming space under a live writer.
    let _lock = PosixStorage::open_dir(dir)
        .map_err(|e| format!("cannot take the single-writer lock: {e}"))?;
    std::fs::remove_file(&legacy).map_err(|e| format!("removing the legacy file: {e}"))?;
    Ok(bytes)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: dabqlite-inspect <dir> [--verify]");
        return ExitCode::FAILURE;
    };
    let mut verify = false;
    let mut collect = false;
    let mut repair: Option<String> = None;
    match args.next().as_deref() {
        None => {}
        Some("--verify") => verify = true,
        Some("--gc") => collect = true,
        Some("--repair-to") => match args.next() {
            Some(dest) => repair = Some(dest),
            None => {
                eprintln!("--repair-to needs a destination directory");
                return ExitCode::FAILURE;
            }
        },
        Some(other) => {
            eprintln!("unknown argument: {other}");
            return ExitCode::FAILURE;
        }
    }
    let dir = Path::new(&dir);

    let superblock = read_optional(&dir.join(SUPERBLOCK_FILE));
    let rows = read_optional(&dir.join(rows_file_name(SCHEMA_HASH)));
    let report = inspect(&superblock, &rows);
    print_report(dir, &report);

    if collect {
        println!();
        match gc(dir) {
            Ok(0) => println!("gc: nothing to reclaim"),
            Ok(bytes) => println!("gc: reclaimed {bytes} bytes (legacy rows file)"),
            Err(e) => {
                eprintln!("gc: {e}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    if let Some(dest) = repair {
        println!();
        match repair_to(dir, Path::new(&dest)) {
            Ok((recovered, dropped)) => {
                println!("repair: wrote {recovered} rows to {dest}");
                if dropped > 0 {
                    println!(
                        "repair: DROPPED {dropped} unverifiable row(s) — they are gone, \
                         and the original in {} is untouched if you want another look",
                        dir.display()
                    );
                } else {
                    println!("repair: nothing was dropped; every row verified");
                }
            }
            Err(e) => {
                eprintln!("repair failed: {e}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

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
