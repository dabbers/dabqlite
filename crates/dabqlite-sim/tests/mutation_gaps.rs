//! Closures for gaps the first full cargo-mutants run exposed (462 mutants
//! against the whole workspace suite): each test here kills a mutant that
//! every other suite missed. The two below are the ENGINE-level survivors —
//! not checker-teeth or golden pins, but genuinely untested recovery
//! behavior.

use dabqlite_core::crc32::crc32;
use dabqlite_core::{Capacities, DbError, FileId, Output, SB_COPY_SIZE, SCHEMA_HASH, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 16 };

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

/// A structurally-valid superblock copy under OUR schema, mirroring the
/// on-disk layout independently of the engine's encoder.
fn forge_sb_copy(generation: u64, row_count: u64) -> [u8; SB_COPY_SIZE] {
    let mut out = [0u8; SB_COPY_SIZE];
    out[0..8].copy_from_slice(b"DABQSB01");
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    out[16..24].copy_from_slice(&row_count.to_le_bytes());
    out[24..32].copy_from_slice(&SCHEMA_HASH.to_le_bytes());
    let crc = crc32(&out[0..32]);
    out[32..36].copy_from_slice(&crc.to_le_bytes());
    out
}

/// Committed rows with NO valid superblock copy anywhere is a negative-space
/// impossibility under the commit protocol — the only explanations are
/// out-of-model corruption. The engine must REFUSE, loudly and harmlessly:
/// the mutant this kills flipped the emptiness check so the engine silently
/// re-initialized, orphaning every committed row behind a fresh gen-1
/// superblock. That is the exact shape of silent data loss this project
/// exists to make impossible.
#[test]
fn rows_without_any_valid_superblock_refuse_instead_of_reinit() {
    for seed in 0..6u64 {
        let (mut disk, _ops, _generation) = build_db(seed);
        let sb_len = disk.contents(FileId::Superblock).len();
        disk.write(FileId::Superblock, 0, &vec![0u8; sb_len]);
        disk.fsync(FileId::Superblock);

        let before_sb = disk.contents(FileId::Superblock);
        let before_rows = disk.contents(FileId::Rows);

        let mut host = SimHost::new(CAPS, disk, None);
        match host.open() {
            Driven::Done(Output::OpenDone {
                result: Err(DbError::Corrupt { what }),
            }) => {
                assert!(
                    what.contains("no valid superblock"),
                    "seed={seed}: error must name the impossibility: {what}"
                );
            }
            other => panic!("seed={seed}: expected Corrupt refusal, got {other:?}"),
        }

        // The refusal wrote nothing: the evidence is preserved intact for
        // forensics, and a recovery tool gets the file exactly as found.
        assert_eq!(
            host.disk.contents(FileId::Superblock),
            before_sb,
            "seed={seed}"
        );
        assert_eq!(host.disk.contents(FileId::Rows), before_rows, "seed={seed}");
    }
}

/// Two checksum-valid copies of the SAME generation that disagree cannot be
/// produced by the commit protocol (a generation's two slots are written
/// with identical bytes) — only by out-of-model corruption that survives
/// CRC. The tie-break must be deterministic and pinned: the first slot of
/// the pair wins. Kills the `>` → `>=` mutant in recover_from_sb, under
/// which the LAST copy would win and recovery would silently shrink by a
/// row.
#[test]
fn same_generation_conflict_resolves_to_the_first_slot() {
    let (mut disk, ops, generation) = build_db(3);
    // The engine's own copy in the pair's first slot says row_count=6;
    // forge the second slot to claim 5.
    let second_slot = (generation % 2) * 2 + 1;
    let forged = forge_sb_copy(generation, 5);
    disk.write(
        FileId::Superblock,
        second_slot * SB_COPY_SIZE as u64,
        &forged,
    );
    disk.fsync(FileId::Superblock);

    let mut host = SimHost::new(CAPS, disk, None);
    match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(n) }) => {
            assert_eq!(n, 6, "first slot of the pair must win the tie");
        }
        other => panic!("expected clean open, got {other:?}"),
    }
    assert_eq!(host.engine.recovery_report().orphan_valid_rows, 0);
    for &(id, value) in &ops {
        assert_eq!(host.get(id), Some(value), "id={id}");
    }
}
