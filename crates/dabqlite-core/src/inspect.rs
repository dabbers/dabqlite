//! The inspector (docs/DESIGN.md §9 step 8): read-only forensics over the
//! raw file bytes. `sqlite3 file.db` for dabqlite — what is actually in
//! this directory, slot by slot and row by row, and what would a binary
//! of this schema conclude from it?
//!
//! This is a deliberate SECOND IMPLEMENTATION of the recovery rules,
//! written from the spec, not by calling into the engine — the same
//! pattern as the reference codec. The agreement property test in
//! `dabqlite-sim/tests/inspect.rs` drives both implementations over
//! fault-generated disks and demands identical verdicts, so a divergence
//! in either one fails loudly instead of hiding.
//!
//! Everything here is pure: bytes in, report out. No I/O, no clock, no
//! allocation beyond the bounded report itself (sample lists are capped;
//! counts are exact). The CLI in `dabqlite-host` is a thin shell over it.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::layout::{decode_row, decode_sb_any, ROW_SIZE, SB_COPIES, SB_COPY_SIZE, SCHEMA_HASH};
use crate::migration::V1_SCHEMA_HASH;

/// At most this many example offsets/ids are collected per defect class;
/// counts are always exact (bounded buffers, docs/DESIGN.md §4.5).
pub const SAMPLE_CAP: usize = 16;

/// One superblock slot, as found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// The slot's bytes are absent (short file) — never written.
    Missing,
    /// All-zero bytes: allocated but never written.
    Empty,
    /// Present but fails structural verification (magic/checksum/padding):
    /// a torn write, bit rot, or garbage. Recovery skips it.
    Invalid,
    /// Structurally valid.
    Valid {
        generation: u64,
        row_count: u64,
        schema: u64,
        /// A generation only ever lives in pair `g % 2`. A valid copy
        /// sitting outside its home pair is the product of a misdirected
        /// write; recovery distrusts it.
        in_home_pair: bool,
    },
}

/// The copy recovery would trust, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveCopy {
    pub slot: u8,
    pub generation: u64,
    pub row_count: u64,
    pub schema: u64,
}

/// Row-zone accounting from a full scan of the rows file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowScan {
    /// Committed rows (within the manifest) that verify.
    pub committed_valid: u64,
    /// Committed rows that fail checksum/padding — recovery refuses the
    /// file if this is nonzero.
    pub committed_corrupt: u64,
    /// Sample offsets of corrupt committed rows.
    pub corrupt_offsets: Vec<u64>,
    /// Distinct ids seen more than once among committed rows — recovery
    /// refuses the file if nonzero.
    pub duplicate_ids: u64,
    pub duplicate_samples: Vec<u64>,
    /// Checksum-valid rows BEYOND the manifest. One is the normal artifact
    /// of an in-flight, unacknowledged insert; two or more is rollback
    /// evidence (see `rollback_evidence`).
    pub orphan_valid: u64,
    /// Slots beyond the manifest that hold garbage (torn/zero) — inert.
    pub orphan_invalid: u64,
}

/// What a binary compiled against THIS schema would conclude at open,
/// mirroring `Engine` recovery exactly (capacity aside — the inspector
/// has no runtime capacity; parity holds for any capacity that admits
/// the file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing committed: open would initialize fresh.
    FreshInit,
    /// Open would recover this many rows.
    Recovers { rows: u64 },
    /// Open would refuse: the file belongs to a different schema. If the
    /// hash is the compiled-in legacy schema, the migration path applies.
    SchemaMismatch { file_schema: u64, migratable: bool },
    /// Open would refuse: on-disk state violates a protocol invariant.
    Corrupt { what: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReport {
    pub slots: [SlotState; SB_COPIES],
    pub live: Option<LiveCopy>,
    pub rows: RowScan,
    /// True when 2+ valid orphans survive — physical evidence that
    /// acknowledged commits were rolled back by an out-of-budget fault.
    pub rollback_evidence: bool,
    pub verdict: Verdict,
}

fn inspect_slot(sb: &[u8], slot: usize) -> SlotState {
    let Some(chunk) = sb.get(slot * SB_COPY_SIZE..(slot + 1) * SB_COPY_SIZE) else {
        return SlotState::Missing;
    };
    if chunk.iter().all(|&b| b == 0) {
        return SlotState::Empty;
    }
    match decode_sb_any(chunk) {
        Ok((copy, schema)) => SlotState::Valid {
            generation: copy.generation,
            row_count: copy.row_count,
            schema,
            in_home_pair: home_pair_slots(copy.generation).contains(&(slot as u8)),
        },
        Err(_) => SlotState::Invalid,
    }
}

/// The pair rotation rule, restated independently of the engine.
fn home_pair_slots(generation: u64) -> [u8; 2] {
    let pair = (generation % 2) as u8;
    [pair * 2, pair * 2 + 1]
}

/// Inspect a database from its raw file bytes. Pure and total: any input
/// produces a report, never a panic — garbage files are the expected
/// case for a forensics tool.
pub fn inspect(superblock: &[u8], rows: &[u8]) -> InspectReport {
    let mut slots = [SlotState::Missing; SB_COPIES];
    for (slot, out) in slots.iter_mut().enumerate() {
        *out = inspect_slot(superblock, slot);
    }

    // Live-copy election, mirroring recovery: highest generation among
    // structurally-valid CURRENT-schema copies in their home pair; ties
    // resolve to the first slot (strict `>` while scanning in slot order).
    let mut live: Option<LiveCopy> = None;
    let mut foreign: Option<u64> = None;
    for (slot, state) in slots.iter().enumerate() {
        if let SlotState::Valid {
            generation,
            row_count,
            schema,
            in_home_pair,
        } = *state
        {
            if schema != SCHEMA_HASH {
                // The engine records a mismatch before the home-pair
                // check, so the inspector must too (it only matters when
                // no current-schema copy exists at all).
                foreign = Some(schema);
                continue;
            }
            if !in_home_pair {
                continue;
            }
            if live.is_none_or(|l| generation > l.generation) {
                live = Some(LiveCopy {
                    slot: slot as u8,
                    generation,
                    row_count,
                    schema,
                });
            }
        }
    }

    // Row scan: committed range per the live manifest, orphan scan beyond
    // it over the whole file (the inspector is capacity-free; it reports
    // the raw truth).
    let committed = live.map_or(0, |l| l.row_count);
    let mut scan = RowScan::default();
    let mut seen = BTreeSet::new();
    // Recovery stops at the FIRST defective committed row, in row order;
    // the verdict must name the same defect the engine would, even when
    // several kinds are present. The scan itself still counts everything —
    // that is the whole point of a forensics tool.
    let mut first_defect: Option<&'static str> = None;
    let live_bytes = (committed as usize).saturating_mul(ROW_SIZE);
    for row in 0..committed {
        let off = (row as usize) * ROW_SIZE;
        match rows.get(off..off + ROW_SIZE).and_then(decode_row) {
            Some((id, _)) => {
                if seen.insert(id) {
                    scan.committed_valid += 1;
                } else {
                    scan.duplicate_ids += 1;
                    if scan.duplicate_samples.len() < SAMPLE_CAP {
                        scan.duplicate_samples.push(id);
                    }
                    first_defect.get_or_insert(crate::defect::DUPLICATE_ID);
                }
            }
            None => {
                scan.committed_corrupt += 1;
                if scan.corrupt_offsets.len() < SAMPLE_CAP {
                    scan.corrupt_offsets.push(off as u64);
                }
                first_defect.get_or_insert(crate::defect::ROW_CHECKSUM);
            }
        }
    }
    let mut off = live_bytes;
    while off + ROW_SIZE <= rows.len() {
        if decode_row(&rows[off..off + ROW_SIZE]).is_some() {
            scan.orphan_valid += 1;
        } else if rows[off..off + ROW_SIZE].iter().any(|&b| b != 0) {
            scan.orphan_invalid += 1;
        }
        off += ROW_SIZE;
    }
    let rollback_evidence = scan.orphan_valid >= 2;

    // The verdict, in the engine's exact decision order.
    let verdict = match live {
        None => {
            if let Some(file_schema) = foreign {
                Verdict::SchemaMismatch {
                    file_schema,
                    migratable: file_schema == V1_SCHEMA_HASH,
                }
            } else if !rows.is_empty() {
                Verdict::Corrupt {
                    what: "no valid superblock copy but rows file is non-empty",
                }
            } else {
                Verdict::FreshInit
            }
        }
        Some(l) => {
            if (l.row_count as usize).saturating_mul(ROW_SIZE) > rows.len() {
                Verdict::Corrupt {
                    what: "superblock references rows beyond the rows file",
                }
            } else if let Some(what) = first_defect {
                Verdict::Corrupt { what }
            } else {
                Verdict::Recovers { rows: l.row_count }
            }
        }
    };

    InspectReport {
        slots,
        live,
        rows: scan,
        rollback_evidence,
        verdict,
    }
}
