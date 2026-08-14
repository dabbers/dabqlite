//! The schema version gate (docs/DESIGN.md §4.8): until the migration path
//! lands (build step 7), a schema mismatch has exactly one safe behavior —
//! the file becomes a **safe brick**: refused loudly, named precisely,
//! harmed not at all. "Silent corruption becomes a startup error."
//!
//! This suite forges structurally-valid superblock copies written under a
//! different schema hash (i.e., what an incompatible binary would leave
//! behind) and pins the whole matrix.

use dabqlite_core::crc32::crc32;
use dabqlite_core::{Capacities, DbError, FileId, Output, SB_COPY_SIZE, SCHEMA_HASH, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 16 };
const FOREIGN_SCHEMA: u64 = 0xFEED_FACE_CAFE_0001;

/// Build a structurally-valid superblock copy under an arbitrary schema
/// hash — byte-for-byte what a binary compiled against a different schema
/// would write. Mirrors the on-disk layout independently of the engine's
/// encoder (a second implementation, like the codec reference oracle).
fn forge_sb_copy(generation: u64, row_count: u64, schema: u64) -> [u8; SB_COPY_SIZE] {
    let mut out = [0u8; SB_COPY_SIZE];
    out[0..8].copy_from_slice(b"DABQSB01");
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    out[16..24].copy_from_slice(&row_count.to_le_bytes());
    out[24..32].copy_from_slice(&schema.to_le_bytes());
    let crc = crc32(&out[0..32]);
    out[32..36].copy_from_slice(&crc.to_le_bytes());
    out
}

/// Slots for generation g (pair rotation, mirrored from the engine).
fn slots_for(generation: u64) -> [u64; 2] {
    let pair = generation % 2;
    [pair * 2, pair * 2 + 1]
}

fn build_db(seed: u64) -> (SimDisk, Vec<(u64, [u8; VALUE_LEN])>, u64) {
    let ops = gen_workload(seed, 6);
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for &(id, value) in &ops {
        host.run(ClientOp::Insert { id, value });
    }
    let generation = host.engine.generation();
    (std::mem::take(&mut host.disk), ops, generation)
}

/// Overwrite both live superblock slots with foreign-schema copies: the
/// file now claims to have been written by an incompatible binary.
fn make_foreign(disk: &mut SimDisk, generation: u64, row_count: u64, schema: u64) {
    for slot in slots_for(generation) {
        let copy = forge_sb_copy(generation, row_count, schema);
        disk.write(FileId::Superblock, slot * SB_COPY_SIZE as u64, &copy);
    }
    // And the stale pair too, so no same-schema copy survives anywhere.
    for slot in slots_for(generation - 1) {
        let copy = forge_sb_copy(generation - 1, row_count.saturating_sub(1), schema);
        disk.write(FileId::Superblock, slot * SB_COPY_SIZE as u64, &copy);
    }
    disk.fsync(FileId::Superblock);
}

#[test]
fn foreign_schema_file_is_a_safe_brick() {
    for seed in 0..6u64 {
        let (mut disk, _ops, generation) = build_db(seed);
        make_foreign(&mut disk, generation, 6, FOREIGN_SCHEMA);

        let before_sb = disk.contents(FileId::Superblock);
        let before_rows = disk.contents(FileId::Rows);

        let mut host = SimHost::new(CAPS, disk, None);
        // Refused, and the error names BOTH hashes — the operator can see
        // exactly which binary wrote the file and which one is running.
        match host.open() {
            Driven::Done(Output::OpenDone {
                result:
                    Err(DbError::SchemaMismatch {
                        file_schema,
                        binary_schema,
                    }),
            }) => {
                assert_eq!(file_schema, FOREIGN_SCHEMA, "seed={seed}");
                assert_eq!(binary_schema, SCHEMA_HASH, "seed={seed}");
            }
            other => panic!("seed={seed}: expected SchemaMismatch, got {other:?}"),
        }

        // SAFE brick: the rejected open changed nothing — not one byte.
        // A correct binary coming along later must find the file pristine.
        assert_eq!(
            host.disk.contents(FileId::Superblock),
            before_sb,
            "seed={seed}: rejected open mutated the superblock"
        );
        assert_eq!(
            host.disk.contents(FileId::Rows),
            before_rows,
            "seed={seed}: rejected open mutated the rows file"
        );

        // The engine is fail-stopped on the mismatch: every operation is
        // refused with the same error, none silently proceeds.
        let err = DbError::SchemaMismatch {
            file_schema: FOREIGN_SCHEMA,
            binary_schema: SCHEMA_HASH,
        };
        assert!(matches!(
            host.run(ClientOp::Insert { id: 1, value: [1; VALUE_LEN] }),
            Driven::Done(Output::InsertDone { result: Err(e), .. }) if e == err
        ));
        assert!(matches!(
            host.run(ClientOp::Get { id: 1 }),
            Driven::Done(Output::GetDone { result: Err(e), .. }) if e == err
        ));
    }
}

#[test]
fn stray_foreign_copy_in_a_stale_slot_is_ignored() {
    // One foreign-schema copy sitting in a STALE slot (e.g. debris from a
    // briefly-run wrong binary that never committed) must not poison an
    // otherwise-healthy file: the live same-schema copies win.
    for seed in 0..6u64 {
        let (mut disk, ops, generation) = build_db(seed);
        // Plant a foreign copy with a HIGHER generation in a stale slot —
        // the most tempting possible bait.
        let stale = slots_for(generation - 1)[0];
        let bait = forge_sb_copy(generation + 7, 1, FOREIGN_SCHEMA);
        disk.write(FileId::Superblock, stale * SB_COPY_SIZE as u64, &bait);
        disk.fsync(FileId::Superblock);

        let mut host = SimHost::new(CAPS, disk, None);
        match host.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                assert_eq!(n, 6, "seed={seed}: healthy copies must win");
            }
            other => panic!("seed={seed}: {other:?}"),
        }
        for &(id, value) in &ops {
            assert_eq!(host.get(id), Some(value), "seed={seed} id={id}");
        }
    }
}

#[test]
fn the_gate_is_bidirectional() {
    // "New binary, old file" and "old binary, new file" are the same test
    // from the engine's perspective — any hash difference refuses, in
    // either direction, whether the foreign hash is smaller or larger.
    for foreign in [1u64, SCHEMA_HASH - 1, SCHEMA_HASH + 1, u64::MAX] {
        let (mut disk, _ops, generation) = build_db(0);
        make_foreign(&mut disk, generation, 6, foreign);
        let mut host = SimHost::new(CAPS, disk, None);
        assert!(
            matches!(
                host.open(),
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::SchemaMismatch { file_schema, .. })
                }) if file_schema == foreign
            ),
            "foreign=0x{foreign:016X} must be refused"
        );
    }
}

#[test]
fn forged_gate_matrix_is_deterministic_and_repeatable() {
    // Open the same foreign file many times: identical refusal every time,
    // zero cumulative harm — an operator retrying in confusion loses
    // nothing.
    let (mut disk, _ops, generation) = build_db(3);
    make_foreign(&mut disk, generation, 6, FOREIGN_SCHEMA);
    let pristine_sb = disk.contents(FileId::Superblock);
    let pristine_rows = disk.contents(FileId::Rows);

    for attempt in 0..10 {
        let mut host = SimHost::new(CAPS, disk, None);
        assert!(
            matches!(
                host.open(),
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::SchemaMismatch { .. })
                })
            ),
            "attempt {attempt}"
        );
        disk = std::mem::take(&mut host.disk);
        assert_eq!(
            disk.contents(FileId::Superblock),
            pristine_sb,
            "attempt {attempt}"
        );
        assert_eq!(
            disk.contents(FileId::Rows),
            pristine_rows,
            "attempt {attempt}"
        );
    }
}
