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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::layout::{
    decode_row, decode_sb_any, RowKind, ROW_SIZE, SB_COPIES, SB_COPY_SIZE, SCHEMA_HASH,
};
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
    /// Committed rows that are deletions rather than records.
    pub tombstones: u64,
    /// Committed rows that supersede an earlier value for the same id.
    pub superseded: u64,
    /// Rows still LIVE after replaying the whole commit order — the
    /// inspector's own answer to "how many rows would open serve", worked
    /// out independently of the engine.
    pub live_records: u64,
    /// Updates referring to an id that was not live at that point in the
    /// commit order — impossible for the engine to write.
    pub orphan_updates: u64,
    /// Deletions referring to an id that was not live at that point in the
    /// commit order — impossible for the engine to write, so evidence of
    /// damage or of a file we did not produce.
    pub orphan_tombstones: u64,
    /// Value continuations with nothing in front of them to continue —
    /// impossible for the engine to write, since a chunk is only ever
    /// appended directly after the row it belongs to.
    pub orphan_chunks: u64,
    /// Continuation slots consumed by values that DID run to their
    /// promised end — the slots a multi-row value occupies beyond its
    /// head.
    pub chunks: u64,
    /// Of those, the ones held by a value that was later superseded or
    /// deleted. Without this, a retired eight-slot value would report one
    /// slot of reclaimable space instead of eight.
    pub dead_chunks: u64,
    /// Values whose continuations do not run to the end the head
    /// promised: the run is missing, short, damaged, or belongs to
    /// another id. The whole value is unreadable, not just its tail.
    pub truncated_values: u64,
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

impl RowScan {
    /// Slots a rebuild would reclaim, in SLOTS — the unit capacity is in.
    ///
    /// Every superseding row retires one value and every deletion retires
    /// one and occupies a slot itself, so the dead weight is those heads,
    /// the tombstones, and the continuations the retired values held.
    /// This is the inspector's own answer to `Engine::dead_slots`, worked
    /// out from the bytes.
    pub fn dead_slots(&self) -> u64 {
        self.superseded + 2 * self.tombstones + self.dead_chunks
    }
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

/// Walk a value's run of slots from its head, restating the format rule
/// independently of the engine: the head says whether the value
/// continues, each continuation is a `Chunk` for the same id, and the
/// last one says the value ends there.
///
/// Returns `(slots, intact)` — how many slots the run occupies (so the
/// scan can step over them either way) and whether it reached the end it
/// promised. A broken run reports the head plus the continuations that
/// did arrive, because those are unreadable together with it.
fn walk_run(
    rows: &[u8],
    head_row: u64,
    committed: u64,
    head: &crate::layout::RowSlot,
) -> (u64, bool) {
    let mut slots = 1u64;
    let mut more = head.more;
    while more {
        if slots as usize > crate::layout::MAX_COMMIT_ROWS {
            // Longer than any commit can be, so longer than anything this
            // engine could have written.
            return (slots, false);
        }
        let r = head_row + slots;
        if r >= committed {
            // The value promised a continuation the manifest does not
            // name: truncation, or a superblock that names fewer rows
            // than the value needs.
            return (slots, false);
        }
        let off = (r as usize) * ROW_SIZE;
        match rows.get(off..off + ROW_SIZE).and_then(decode_row) {
            Some(slot) if slot.kind == RowKind::Chunk && slot.id == head.id => {
                more = slot.more;
                slots += 1;
            }
            _ => return (slots, false),
        }
    }
    (slots, true)
}

/// The pair rotation rule, restated independently of the engine.
fn home_pair_slots(generation: u64) -> [u8; 2] {
    let pair = (generation % 2) as u8;
    [pair * 2, pair * 2 + 1]
}

/// Inspect a database from its raw file bytes. Pure and total: any input
/// produces a report, never a panic — garbage files are the expected
/// case for a forensics tool.
/// Is a manifest of `row_count` rows PROVABLY not one the engine wrote?
///
/// The same rule as `Engine::is_ghost_manifest`, and it has to stay the
/// same rule: the last row a real manifest names has span zero, and a
/// manifest that ends mid-commit is a ghost only when the rest of that
/// commit is actually present in the file. Anything else — an unreadable
/// last row, a stranded continuation with nothing after it, a file that
/// ends — is damage, and damage is reported rather than routed around.
fn ghost_manifest(rows: &[u8], row_count: u64) -> bool {
    if row_count == 0 {
        return false;
    }
    let off = ((row_count - 1) as usize) * ROW_SIZE;
    let Some(chunk) = rows.get(off..off + ROW_SIZE) else {
        return false;
    };
    let Some(last) = decode_row(chunk) else {
        return false;
    };
    if last.span == 0 {
        return false;
    }
    (row_count + last.span as u64) as usize * ROW_SIZE <= rows.len()
}

pub fn inspect(superblock: &[u8], rows: &[u8]) -> InspectReport {
    let mut slots = [SlotState::Missing; SB_COPIES];
    for (slot, out) in slots.iter_mut().enumerate() {
        *out = inspect_slot(superblock, slot);
    }

    // Live-copy election, mirroring recovery: highest generation among
    // structurally-valid CURRENT-schema copies in their home pair, and
    // within one generation the copy naming the MOST rows.
    //
    // Two valid copies of one generation are not supposed to exist —
    // they are written from a single encoding — but a re-used generation
    // number plus successive torn writes can reassemble an earlier
    // superblock byte for byte, so they do. Recovery prefers the sibling
    // that the rows agree with; the inspector reaches the same verdict
    // the same way, because a forensics tool that disagrees with recovery
    // about what a file means is worse than no tool.
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
            let better = live.is_none_or(|l| {
                generation > l.generation || (generation == l.generation && row_count > l.row_count)
            });
            if better {
                live = Some(LiveCopy {
                    slot: slot as u8,
                    generation,
                    row_count,
                    schema,
                });
            }
        }
    }
    // Now that a manifest is in hand, ask the ROWS whether it is one this
    // engine could have written. A manifest ending mid-commit — the last
    // row it names still promises more of its own commit — names half a
    // commit, which the protocol cannot produce, so it is a ghost. Fall
    // back to a sibling at the SAME generation, never to an older one:
    // preferring an older manifest over a damaged newer one would be data
    // loss wearing recovery's clothes.
    // A candidate that does not FIT the rows file, or that is a proven
    // ghost, is passed over for the next one down — exactly as recovery
    // does it. Passing over a ghost costs nothing: it was never this
    // file's manifest. Passing over one that does not fit means rows that
    // were fsynced are gone, so it is recorded as rollback evidence
    // rather than swallowed.
    let mut lost_to_short_file = false;
    if let Some(chosen) = live {
        let fits = |c: u64| c as usize * ROW_SIZE <= rows.len();
        if !fits(chosen.row_count) || ghost_manifest(rows, chosen.row_count) {
            let mut best: Option<LiveCopy> = None;
            for (slot, state) in slots.iter().enumerate() {
                if let SlotState::Valid {
                    generation,
                    row_count,
                    schema,
                    in_home_pair,
                } = *state
                {
                    if schema != SCHEMA_HASH
                        || !in_home_pair
                        || !fits(row_count)
                        || ghost_manifest(rows, row_count)
                    {
                        continue;
                    }
                    let better = best.is_none_or(|b| {
                        generation > b.generation
                            || (generation == b.generation && row_count > b.row_count)
                    });
                    if better {
                        best = Some(LiveCopy {
                            slot: slot as u8,
                            generation,
                            row_count,
                            schema,
                        });
                    }
                }
            }
            if let Some(b) = best {
                lost_to_short_file = !fits(chosen.row_count);
                live = Some(b);
            }
        }
    }

    // Row scan: committed range per the live manifest, orphan scan beyond
    // it over the whole file (the inspector is capacity-free; it reports
    // the raw truth).
    let committed = live.map_or(0, |l| l.row_count);
    let mut scan = RowScan::default();
    // id -> how many slots its currently-live value occupies, so that
    // retiring it can account for every slot it held rather than only its
    // head.
    let mut seen: BTreeMap<u64, u64> = BTreeMap::new();
    // Recovery stops at the FIRST defective committed row, in row order;
    // the verdict must name the same defect the engine would, even when
    // several kinds are present. The scan itself still counts everything —
    // that is the whole point of a forensics tool.
    let mut first_defect: Option<&'static str> = None;
    let live_bytes = (committed as usize).saturating_mul(ROW_SIZE);
    // Independently of the engine, replay the commit order: `seen` holds
    // the ids that are LIVE right now, so a record for a live id is a
    // duplicate, a record for a retired one is the id being reused, and a
    // deletion of something not live is damage.
    let mut row = 0u64;
    while row < committed {
        let off = (row as usize) * ROW_SIZE;
        let Some(slot) = rows.get(off..off + ROW_SIZE).and_then(decode_row) else {
            scan.committed_corrupt += 1;
            if scan.corrupt_offsets.len() < SAMPLE_CAP {
                scan.corrupt_offsets.push(off as u64);
            }
            first_defect.get_or_insert(crate::defect::ROW_CHECKSUM);
            row += 1;
            continue;
        };
        let id = slot.id;
        match slot.kind {
            RowKind::Record | RowKind::Update => {
                // A value is written whole, in one commit, as a run of
                // slots: the head plus one continuation per extra
                // row-width of bytes, each carrying the same id and the
                // last one saying the value ends there. A run that does
                // not reach that end is not one this engine wrote, and
                // the whole value goes rather than its head being served
                // cut short.
                let (span, intact) = walk_run(rows, row, committed, &slot);
                if !intact {
                    scan.truncated_values += 1;
                    first_defect.get_or_insert(crate::defect::TRUNCATED_VALUE);
                    row += span;
                    continue;
                }
                if slot.kind == RowKind::Record {
                    if seen.insert(id, span).is_none() {
                        scan.committed_valid += 1;
                    } else {
                        scan.duplicate_ids += 1;
                        if scan.duplicate_samples.len() < SAMPLE_CAP {
                            scan.duplicate_samples.push(id);
                        }
                        first_defect.get_or_insert(crate::defect::DUPLICATE_ID);
                    }
                } else if let Some(previous) = seen.insert(id, span) {
                    // A superseding row is legitimate only for an id
                    // that is live at this point in the commit order.
                    scan.committed_valid += 1;
                    scan.superseded += 1;
                    scan.dead_chunks += previous - 1;
                } else {
                    seen.remove(&id);
                    scan.orphan_updates += 1;
                    first_defect.get_or_insert(crate::defect::ORPHAN_UPDATE);
                }
                scan.chunks += span - 1;
                row += span;
            }
            RowKind::Tombstone => {
                // A deletion that claims to continue is as impossible as
                // one for an id that is not live: a tombstone carries no
                // value to spill into a second slot.
                let retires = if slot.more { None } else { seen.remove(&id) };
                if let Some(previous) = retires {
                    scan.committed_valid += 1;
                    scan.tombstones += 1;
                    scan.dead_chunks += previous - 1;
                } else {
                    scan.orphan_tombstones += 1;
                    first_defect.get_or_insert(crate::defect::ORPHAN_TOMBSTONE);
                }
                row += 1;
            }
            RowKind::Chunk => {
                // Reached only when nothing in front of it claimed it:
                // every well-formed continuation is consumed with its
                // head, above.
                scan.orphan_chunks += 1;
                first_defect.get_or_insert(crate::defect::ORPHAN_CHUNK);
                row += 1;
            }
        }
    }
    scan.live_records = seen.len() as u64;
    // Beyond the manifest lies at most ONE unacknowledged commit, and
    // every slot of it agrees about where that commit ends: a valid row
    // `j` slots past the manifest carrying span `s` claims a commit of
    // `j + s + 1` slots. Orphans that disagree cannot have been written
    // by one commit, so acknowledged work was rolled back. At span 0 this
    // is exactly the old rule — two orphans in a row disagree — which is
    // why replacing it lost nothing.
    let mut claimed: Option<u64> = None;
    let mut disagreed = false;
    let mut off = live_bytes;
    let mut j = 0u64;
    while off + ROW_SIZE <= rows.len() {
        if let Some(slot) = decode_row(&rows[off..off + ROW_SIZE]) {
            scan.orphan_valid += 1;
            let claim = j + slot.span as u64 + 1;
            if claim > crate::layout::MAX_COMMIT_ROWS as u64 {
                // A commit longer than the format can describe is not one
                // the engine wrote, however well these slots agree.
                disagreed = true;
            }
            match claimed {
                None => claimed = Some(claim),
                Some(first) if first == claim => {}
                Some(_) => disagreed = true,
            }
        } else if rows[off..off + ROW_SIZE].iter().any(|&b| b != 0) {
            scan.orphan_invalid += 1;
        }
        off += ROW_SIZE;
        j += 1;
    }
    // Passing over a manifest because the rows it names are not in the
    // file means fsynced rows are gone — the definition of an
    // out-of-budget loss, and the thing `rollback_evidence` exists to
    // say out loud.
    let rollback_evidence = disagreed || lost_to_short_file;

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
