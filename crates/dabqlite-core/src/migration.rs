//! The migration path (docs/DESIGN.md §4.8): pure `OldRow -> NewRow`
//! functions over generated types.
//!
//! A migration runs inside the NEW binary, offline, single-writer. The
//! function here is deliberately trivial to state and total by
//! construction: every value the old type can hold maps to exactly one
//! value of the new type, no `Option`, no `Result`, no panic path. That
//! totality is what makes "the migration cannot fail halfway through a
//! row" a type-level fact instead of a hope — and the property tests
//! below re-verify it over the old type's entire value-space structure
//! (every byte pattern of the fixed-width payload, boundary ids).
//!
//! Widening policy (§4.8 field discipline: append, never reorder, never
//! remove): v1's 8-byte `value` keeps its offset and its bytes; the new
//! tail is zero-filled. A v2 reader sees old payloads left-aligned with a
//! zeroed suffix — deterministic, order-preserving, reversible up to the
//! (zero) tail.

use crate::generated::{records, records_v1};

/// v1 row width on disk (24 bytes: id, 8-byte value, crc, padding).
pub const V1_ROW_SIZE: usize = records_v1::RECORDS_ROW_SIZE;
/// The legacy schema's hash, as it appears in a v1 file's superblock.
pub const V1_SCHEMA_HASH: u64 = records_v1::RECORDS_SCHEMA_HASH;
/// v1 payload width.
pub const V1_VALUE_LEN: usize = 8;

const _: () = assert!(V1_ROW_SIZE == 24);
// The whole point of the gate: the two schemas must never collide.
const _: () = assert!(V1_SCHEMA_HASH != records::RECORDS_SCHEMA_HASH);

/// The pure migration: total over every v1 row, by construction.
pub fn migrate_row(old: records_v1::RecordsRow) -> records::RecordsRow {
    let mut value = [0u8; 16];
    value[..V1_VALUE_LEN].copy_from_slice(&old.value);
    records::RecordsRow { id: old.id, value }
}

use alloc::vec;
use alloc::vec::Vec;

use crate::engine::{Capacities, DbError, Engine, FileId, Input, Output, WriteBuf};
use crate::layout::{
    decode_row, decode_sb_any, encode_row, SbDecodeError, ROW_SIZE, SB_COPY_SIZE, SB_ZONE_SIZE,
    SCHEMA_HASH,
};

/// The offline migration state machine (docs/DESIGN.md §4.8). Sans-I/O,
/// same lockstep protocol as [`Engine`]: one [`Input`] in, one [`Output`]
/// out, driven by the host under the single-writer lock.
///
/// ## Protocol (and why a crash at any boundary is safe)
///
/// ```text
/// ReadSb ─▶ ReadOldRows ─▶ WriteNewRows (×n) ─▶ FsyncNewRows
///                                                    │
///        MigrateDone ◀── FsyncSb ◀── WriteSb{0,1} ◀──┘
/// ```
///
/// - The legacy rows file is **read, never written**. Byte-identical
///   before, during, after, crash or no crash.
/// - The new rows file is fully written and fsynced *before* the
///   superblock flips — the same visible-implies-durable ordering as the
///   commit protocol.
/// - The flip writes generation `g+1`, which lives in the OTHER slot
///   pair, so the legacy generation's copies are never touched. Crash
///   before the sb fsync: the superblock still names the legacy schema,
///   the legacy binary still works, and migration re-runs from scratch
///   (the partial new file is inert — nothing names it). Crash after:
///   the new schema is live over a complete, durable rows file.
/// - Running against an already-migrated file VERIFIES it (reads and
///   checksums every row it names) and acknowledges with **zero writes**
///   when coherent. If the current world is incoherent — possible only
///   through out-of-budget faults such as a lying fsync retracted by a
///   crash — and the legacy world survives (it always does in-protocol:
///   the legacy file is never written), the migration REDOES itself from
///   the legacy source, flipping above both generations. Fsyncgate over
///   a migration therefore loses NOTHING, ever.
pub struct MigrationEngine {
    state: MState,
    caps: Capacities,
    /// Arena for the legacy rows file: the one allocation, made at
    /// construction (docs/DESIGN.md §4.2 applies here too).
    old: Vec<u8>,
    old_addr: usize,
    rows_len: u64,
    rows_old_len: u64,
    /// The legacy world found while reading the superblock, kept in case
    /// the CURRENT world turns out incoherent and must be rebuilt from it.
    legacy: Option<(u64, u64)>,
    /// The current-schema generation seen in the superblock, if any.
    current: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MState {
    New,
    ReadSb,
    ReadOldRows {
        generation: u64,
        row_count: u64,
    },
    WriteNewRows {
        generation: u64,
        row_count: u64,
        next: u64,
    },
    FsyncNewRows {
        generation: u64,
        row_count: u64,
    },
    WriteSb {
        generation: u64,
        row_count: u64,
        copy: u8,
    },
    FsyncSb {
        row_count: u64,
    },
    /// Already-current file found: VERIFY its rows before acknowledging.
    /// Out-of-budget faults (a lying fsync retracted by a crash) can
    /// leave the superblock naming the current schema over a rows file
    /// that never became durable; acknowledging that unread would wedge
    /// the database behind a Corrupt open. If verification fails and the
    /// legacy world still exists, the migration REDOES itself from it —
    /// the legacy file is never written, so it is always a valid source.
    NoopVerifyRows {
        row_count: u64,
    },
    /// Already-current file found: fsync rows then superblock before
    /// acknowledging, in case the current-schema superblock is sitting
    /// unsynced in the page cache from a previous attempt that died after
    /// its writes but before its fsync. Visible must imply durable here
    /// exactly as it does in recovery.
    NoopFsyncRows {
        row_count: u64,
    },
    NoopFsyncSb {
        row_count: u64,
    },
    Done,
    Failed(DbError),
}

impl MigrationEngine {
    pub fn new(caps: Capacities) -> Self {
        assert!(caps.rows > 0, "capacity must be positive");
        let bytes = (caps.rows as usize)
            .checked_mul(V1_ROW_SIZE)
            .expect("rows capacity overflows legacy arena size");
        let old = vec![0u8; bytes];
        let old_addr = old.as_ptr() as usize;
        MigrationEngine {
            state: MState::New,
            caps,
            old,
            old_addr,
            rows_len: 0,
            rows_old_len: 0,
            legacy: None,
            current: None,
        }
    }

    /// Begin. The host reports existing file sizes (the same contract as
    /// `Input::Open`): `superblock_len` for the shared superblock,
    /// `rows_len` for the CURRENT-schema rows file (so an already-current
    /// world can be verified, not assumed), `rows_old_len` for the legacy
    /// rows file.
    pub fn start(&mut self, superblock_len: u64, rows_len: u64, rows_old_len: u64) -> Output {
        assert!(
            matches!(self.state, MState::New),
            "protocol violation: migration already started"
        );
        self.rows_len = rows_len;
        self.rows_old_len = rows_old_len;
        if superblock_len == 0 {
            if rows_old_len == 0 {
                // Nothing exists: nothing to migrate. The engine's open
                // will initialize fresh.
                return self.finish(Ok(0));
            }
            // Negative space, same rule as recovery: committed rows imply
            // a superblock. Refuse rather than guess.
            return self.finish(Err(DbError::Corrupt {
                what: "no superblock but legacy rows file is non-empty",
            }));
        }
        self.state = MState::ReadSb;
        Output::Read {
            file: FileId::Superblock,
            offset: 0,
            len: SB_ZONE_SIZE as u64,
        }
    }

    /// Advance by one input. Panics on protocol violations, exactly like
    /// the engine: a host that completes the wrong I/O has a bug that
    /// must not be absorbed.
    pub fn tick(&mut self, input: Input<'_>) -> Output {
        self.assert_invariants();
        let out = self.tick_inner(input);
        self.assert_invariants();
        out
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(
            self.old.as_ptr() as usize,
            self.old_addr,
            "legacy arena moved: allocation after init is forbidden"
        );
    }

    fn finish(&mut self, result: Result<u64, DbError>) -> Output {
        self.state = match result {
            Ok(_) => MState::Done,
            Err(e) => MState::Failed(e),
        };
        Output::MigrateDone { result }
    }

    fn tick_inner(&mut self, input: Input<'_>) -> Output {
        match (self.state, input) {
            (MState::ReadSb, Input::ReadDone { file, data }) => {
                assert_eq!(file, FileId::Superblock, "protocol violation");
                self.on_sb(data)
            }
            (
                MState::ReadOldRows {
                    generation,
                    row_count,
                },
                Input::ReadDone { file, data },
            ) => {
                assert_eq!(file, FileId::RowsOld, "protocol violation");
                self.on_old_rows(generation, row_count, data)
            }
            (
                MState::WriteNewRows {
                    generation,
                    row_count,
                    next,
                },
                Input::WriteDone { file },
            ) => {
                assert_eq!(file, FileId::Rows, "protocol violation");
                self.stage_row_or_fsync(generation, row_count, next + 1)
            }
            (
                MState::FsyncNewRows {
                    generation,
                    row_count,
                },
                Input::FsyncDone { file },
            ) => {
                assert_eq!(file, FileId::Rows, "protocol violation");
                self.state = MState::WriteSb {
                    generation,
                    row_count,
                    copy: 0,
                };
                Engine::sb_copy_write(generation + 1, row_count, 0)
            }
            (
                MState::WriteSb {
                    generation,
                    row_count,
                    copy: 0,
                },
                Input::WriteDone { file },
            ) => {
                assert_eq!(file, FileId::Superblock, "protocol violation");
                self.state = MState::WriteSb {
                    generation,
                    row_count,
                    copy: 1,
                };
                Engine::sb_copy_write(generation + 1, row_count, 1)
            }
            (
                MState::WriteSb {
                    row_count, copy: 1, ..
                },
                Input::WriteDone { file },
            ) => {
                assert_eq!(file, FileId::Superblock, "protocol violation");
                self.state = MState::FsyncSb { row_count };
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (MState::FsyncSb { row_count }, Input::FsyncDone { file }) => {
                assert_eq!(file, FileId::Superblock, "protocol violation");
                // The commit point: the superblock now names the new
                // schema, durably.
                self.finish(Ok(row_count))
            }
            (MState::NoopVerifyRows { row_count }, Input::ReadDone { file, data }) => {
                assert_eq!(file, FileId::Rows, "protocol violation");
                let need = (row_count as usize) * ROW_SIZE;
                if data.len() < need {
                    return self.redo_or_fail(
                        "current-schema superblock references rows beyond the rows file",
                    );
                }
                for i in 0..row_count as usize {
                    if decode_row(&data[i * ROW_SIZE..(i + 1) * ROW_SIZE]).is_none() {
                        return self
                            .redo_or_fail("current-schema row failed checksum during verify");
                    }
                }
                self.state = MState::NoopFsyncRows { row_count };
                Output::Fsync { file: FileId::Rows }
            }
            (MState::NoopFsyncRows { row_count }, Input::FsyncDone { file }) => {
                assert_eq!(file, FileId::Rows, "protocol violation");
                self.state = MState::NoopFsyncSb { row_count };
                Output::Fsync {
                    file: FileId::Superblock,
                }
            }
            (MState::NoopFsyncSb { row_count }, Input::FsyncDone { file }) => {
                assert_eq!(file, FileId::Superblock, "protocol violation");
                self.finish(Ok(row_count))
            }
            // Fail-stop on any I/O error, exactly like the engine: the
            // legacy file is untouched, so restarting the migration is
            // always safe.
            (
                MState::ReadSb
                | MState::ReadOldRows { .. }
                | MState::WriteNewRows { .. }
                | MState::FsyncNewRows { .. }
                | MState::WriteSb { .. }
                | MState::FsyncSb { .. }
                | MState::NoopVerifyRows { .. }
                | MState::NoopFsyncRows { .. }
                | MState::NoopFsyncSb { .. },
                Input::IoFailed { file },
            ) => self.finish(Err(DbError::IoFailed { file })),
            (state, input) => {
                panic!("protocol violation: migration {state:?} cannot accept {input:?}")
            }
        }
    }

    fn on_sb(&mut self, data: &[u8]) -> Output {
        // Scan all slots structurally; sort valid copies by the schema
        // they were written under. The slot-pair rule applies to every
        // copy regardless of schema: a checksum-valid copy in a foreign
        // slot is a misdirected write and is distrusted (same defense as
        // recovery).
        let mut best_current: Option<(u64, u64)> = None; // (gen, rows)
        let mut best_legacy: Option<(u64, u64)> = None;
        let mut foreign: Option<u64> = None;
        for (slot, chunk) in data.chunks(SB_COPY_SIZE).take(4).enumerate() {
            match decode_sb_any(chunk) {
                Ok((copy, schema)) => {
                    if !Engine::sb_slots_for(copy.generation).contains(&(slot as u8)) {
                        continue;
                    }
                    let best = match schema {
                        s if s == SCHEMA_HASH => &mut best_current,
                        s if s == V1_SCHEMA_HASH => &mut best_legacy,
                        s => {
                            foreign = Some(s);
                            continue;
                        }
                    };
                    if best.is_none_or(|(g, _)| copy.generation > g) {
                        *best = Some((copy.generation, copy.row_count));
                    }
                }
                Err(SbDecodeError::Invalid | SbDecodeError::SchemaMismatch { .. }) => {}
            }
        }

        self.legacy = best_legacy;
        self.current = best_current.map(|(g, _)| g);
        if let Some((_, rows)) = best_current {
            // Already this binary's schema. Not taken on faith: the copy
            // may be the residue of a lying fsync — verify the rows it
            // names actually exist and decode before acknowledging.
            if rows * ROW_SIZE as u64 > self.rows_len {
                return self.redo_or_fail(
                    "current-schema superblock references rows beyond the rows file",
                );
            }
            if rows == 0 {
                self.state = MState::NoopFsyncRows { row_count: 0 };
                return Output::Fsync { file: FileId::Rows };
            }
            self.state = MState::NoopVerifyRows { row_count: rows };
            return Output::Read {
                file: FileId::Rows,
                offset: 0,
                len: rows * ROW_SIZE as u64,
            };
        }
        let Some((generation, row_count)) = best_legacy else {
            if let Some(file_schema) = foreign {
                // A schema this binary has no migration for: safe brick,
                // same as the engine's gate.
                return self.finish(Err(DbError::SchemaMismatch {
                    file_schema,
                    binary_schema: SCHEMA_HASH,
                }));
            }
            if self.rows_old_len > 0 {
                return self.finish(Err(DbError::Corrupt {
                    what: "no valid superblock copy but legacy rows file is non-empty",
                }));
            }
            // Torn first-ever init of a legacy db: nothing was committed,
            // nothing to migrate.
            return self.finish(Ok(0));
        };

        if row_count > self.caps.rows {
            return self.finish(Err(DbError::CapacityBelowData {
                required: row_count,
                configured: self.caps.rows,
            }));
        }
        let need = row_count * V1_ROW_SIZE as u64;
        if need > self.rows_old_len {
            return self.finish(Err(DbError::Corrupt {
                what: "legacy superblock names more rows than the legacy file holds",
            }));
        }
        if row_count == 0 {
            // Empty legacy db: nothing to rewrite, just flip the schema.
            self.state = MState::WriteSb {
                generation,
                row_count: 0,
                copy: 0,
            };
            return Engine::sb_copy_write(generation + 1, 0, 0);
        }
        self.state = MState::ReadOldRows {
            generation,
            row_count,
        };
        Output::Read {
            file: FileId::RowsOld,
            offset: 0,
            len: need,
        }
    }

    /// The current world is incoherent. If the legacy world survives —
    /// and it always survives in-protocol, because migration never writes
    /// the legacy file — rebuild from it, flipping at a generation above
    /// BOTH worlds so recovery elects the rebuilt state. Otherwise refuse
    /// loudly with the named defect.
    fn redo_or_fail(&mut self, what: &'static str) -> Output {
        let Some((legacy_gen, legacy_rows)) = self.legacy else {
            return self.finish(Err(DbError::Corrupt { what }));
        };
        if legacy_rows > self.caps.rows {
            return self.finish(Err(DbError::CapacityBelowData {
                required: legacy_rows,
                configured: self.caps.rows,
            }));
        }
        let need = legacy_rows * V1_ROW_SIZE as u64;
        if need > self.rows_old_len {
            return self.finish(Err(DbError::Corrupt {
                what: "legacy superblock names more rows than the legacy file holds",
            }));
        }
        // Flip above both worlds. The redo's own flip may land in the
        // legacy pair — safe, because the rebuilt rows are fsynced before
        // a single superblock byte is written, so whichever generation a
        // later recovery elects names durable data.
        let base = legacy_gen.max(self.current_gen().unwrap_or(0));
        if legacy_rows == 0 {
            self.state = MState::WriteSb {
                generation: base,
                row_count: 0,
                copy: 0,
            };
            return Engine::sb_copy_write(base + 1, 0, 0);
        }
        self.state = MState::ReadOldRows {
            generation: base,
            row_count: legacy_rows,
        };
        Output::Read {
            file: FileId::RowsOld,
            offset: 0,
            len: need,
        }
    }

    /// The current-schema generation seen in the superblock, if any —
    /// recorded so a redo can flip above it.
    fn current_gen(&self) -> Option<u64> {
        self.current
    }

    fn on_old_rows(&mut self, generation: u64, row_count: u64, data: &[u8]) -> Output {
        let need = (row_count as usize) * V1_ROW_SIZE;
        if data.len() < need {
            return self.finish(Err(DbError::Corrupt {
                what: "legacy rows file shorter than its superblock claims",
            }));
        }
        self.old[..need].copy_from_slice(&data[..need]);
        // Validate EVERY legacy row before writing a single byte: a
        // migration must never invent data, and a checksum failure here
        // means the legacy file needs repair, not a rewrite.
        for i in 0..row_count as usize {
            let slot = &self.old[i * V1_ROW_SIZE..(i + 1) * V1_ROW_SIZE];
            if records_v1::decode_records_row(slot).is_none() {
                return self.finish(Err(DbError::Corrupt {
                    what: "legacy row failed its checksum during migration",
                }));
            }
        }
        self.stage_row_or_fsync(generation, row_count, 0)
    }

    fn stage_row_or_fsync(&mut self, generation: u64, row_count: u64, next: u64) -> Output {
        if next == row_count {
            self.state = MState::FsyncNewRows {
                generation,
                row_count,
            };
            return Output::Fsync { file: FileId::Rows };
        }
        let i = next as usize;
        let slot = &self.old[i * V1_ROW_SIZE..(i + 1) * V1_ROW_SIZE];
        let old = records_v1::decode_records_row(slot).expect("validated during on_old_rows");
        let new = migrate_row(old);
        let mut out = [0u8; ROW_SIZE];
        encode_row(new.id, &new.value, &mut out);
        self.state = MState::WriteNewRows {
            generation,
            row_count,
            next,
        };
        Output::Write {
            file: FileId::Rows,
            offset: next * ROW_SIZE as u64,
            data: WriteBuf::from_slice(&out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structured sweep of the old type's value space: boundary ids ×
    /// per-byte bit patterns. Totality means every one maps, and the map
    /// preserves id, preserves payload bytes at their offsets, and
    /// zero-fills exactly the appended tail.
    #[test]
    fn migration_is_total_and_shape_preserving() {
        let ids = [0u64, 1, u64::MAX, u64::MAX - 1, 0x8000_0000_0000_0000];
        for &id in &ids {
            for pattern in 0..=255u8 {
                for hot in 0..V1_VALUE_LEN {
                    let mut value = [pattern; V1_VALUE_LEN];
                    value[hot] = !pattern;
                    let new = migrate_row(records_v1::RecordsRow { id, value });
                    assert_eq!(new.id, id);
                    assert_eq!(&new.value[..V1_VALUE_LEN], &value);
                    assert_eq!(new.value[V1_VALUE_LEN..], [0u8; 8]);
                }
            }
        }
    }

    /// Every checksum-valid v1 SLOT migrates into a checksum-valid v2
    /// slot: the full disk-to-disk pipeline (decode v1, migrate, encode
    /// v2, decode v2) round-trips, seeded-randomly over the value space.
    #[test]
    fn valid_v1_slots_migrate_to_valid_v2_slots() {
        // Deterministic LCG; no ambient randomness in core tests either.
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        for _ in 0..10_000 {
            let id = next();
            let value = next().to_le_bytes();
            let old = records_v1::RecordsRow { id, value };
            let mut v1_slot = [0u8; V1_ROW_SIZE];
            records_v1::encode_records_row(&old, &mut v1_slot);
            let decoded = records_v1::decode_records_row(&v1_slot).expect("valid v1 slot");
            let new = migrate_row(decoded);
            let mut v2_slot = [0u8; records::RECORDS_ROW_SIZE];
            records::encode_records_row(&new, &mut v2_slot);
            let back = records::decode_records_row(&v2_slot).expect("valid v2 slot");
            assert_eq!(back.id, id);
            assert_eq!(&back.value[..V1_VALUE_LEN], &value);
        }
    }

    /// A client operation aimed at a mid-migration machine must panic
    /// (protocol violation), never be silently absorbed: in a lockstep
    /// protocol, an ignored input leaves the peer waiting forever — the
    /// deadlock seed. Loud beats wedged.
    #[test]
    #[should_panic(expected = "protocol violation")]
    fn client_input_during_migration_is_refused_loudly() {
        let mut m = MigrationEngine::new(Capacities { rows: 8 });
        let out = m.start(256, 0, 24);
        assert!(matches!(out, Output::Read { .. }));
        m.tick(Input::Get { id: 1 });
    }

    /// Same for a completion the machine never requested.
    #[test]
    #[should_panic(expected = "protocol violation")]
    fn unrequested_completion_during_migration_is_refused_loudly() {
        let mut m = MigrationEngine::new(Capacities { rows: 8 });
        let out = m.start(256, 0, 24);
        assert!(matches!(out, Output::Read { .. }));
        // The machine asked for a superblock READ; hand it a rows write
        // completion instead.
        m.tick(Input::WriteDone { file: FileId::Rows });
    }

    /// Order preservation: migration never reorders or renumbers — the
    /// btree rebuild after migration must see the same key sequence.
    #[test]
    fn migration_preserves_key_order() {
        let keys = [0u64, 1, 2, 100, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        let migrated: alloc::vec::Vec<u64> = keys
            .iter()
            .map(|&id| {
                migrate_row(records_v1::RecordsRow {
                    id,
                    value: id.to_le_bytes(),
                })
                .id
            })
            .collect();
        assert_eq!(&migrated[..], &keys[..]);
    }
}
