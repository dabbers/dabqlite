//! Media-fault (bit rot) testing. Crash faults lose *unsynced* writes; media
//! faults damage *durable* data. The guarantees differ (docs/DESIGN.md §5):
//!
//! - Superblock: every generation is written to two slots, so any single
//!   corrupted copy is survivable with zero data loss.
//! - Rows: corruption of a committed row is *detected* end-to-end (checksums)
//!   and reported as `Corrupt` — never returned as silently wrong data.

use dabqlite_core::{Capacities, DbError, Output, SB_COPY_SIZE, SB_ZONE_SIZE, VALUE_LEN};
use dabqlite_core::{FileId, ROW_SIZE};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 16 };
const INSERTS: usize = 8;

/// Build a database with INSERTS committed rows; return its disk + workload.
fn build_db(seed: u64) -> (SimDisk, Vec<(u64, [u8; VALUE_LEN])>) {
    let ops = gen_workload(seed, INSERTS);
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(0) })
    ));
    for &(id, value) in &ops {
        let out = host.run(ClientOp::Insert { id, value });
        assert!(matches!(
            out,
            Driven::Done(Output::InsertDone { result: Ok(()), .. })
        ));
    }
    (std::mem::take(&mut host.disk), ops)
}

/// The two superblock slots holding generation `g` (pair rotation).
fn slots_for(generation: u64) -> [u64; 2] {
    let pair = generation % 2;
    [pair * 2, pair * 2 + 1]
}

fn verify_all(host: &mut SimHost, ops: &[(u64, [u8; VALUE_LEN])]) {
    for &(id, value) in ops {
        assert_eq!(host.get(id), Some(value), "committed id={id} lost");
    }
}

#[test]
fn single_superblock_copy_corruption_loses_nothing() {
    for seed in 0..8u64 {
        let (disk, ops) = build_db(seed);
        // INSERTS commits after gen 1 => latest generation is INSERTS + 1.
        let latest_gen = INSERTS as u64 + 1;

        // Corrupt each byte-position class of each of the two live copies,
        // one at a time: recovery must still find the other copy.
        for slot in slots_for(latest_gen) {
            for byte_in_slot in [0u64, 9, 17, 33] {
                // magic, generation, row_count, crc regions
                let mut damaged = disk.clone();
                damaged.corrupt(
                    FileId::Superblock,
                    slot * SB_COPY_SIZE as u64 + byte_in_slot,
                    0x40,
                );
                let mut host = SimHost::new(CAPS, damaged, None);
                match host.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        assert_eq!(
                            n, INSERTS as u64,
                            "seed={seed} slot={slot} byte={byte_in_slot}: lost rows"
                        );
                    }
                    other => panic!(
                        "seed={seed} slot={slot} byte={byte_in_slot}: open failed: {other:?}"
                    ),
                }
                verify_all(&mut host, &ops);
            }
        }
    }
}

#[test]
fn corrupted_committed_row_is_detected_never_silent() {
    for seed in 0..8u64 {
        let (disk, _ops) = build_db(seed);
        // Corrupt one byte in each committed row, one at a time. Recovery
        // must fail loudly with Corrupt — the alternative (serving a wrong
        // value with a straight face) is the one unforgivable outcome.
        for row in 0..INSERTS as u64 {
            for byte_in_row in [0u64, 8, 20] {
                // id, value, value tail
                let mut damaged = disk.clone();
                damaged.corrupt(FileId::Rows, row * ROW_SIZE as u64 + byte_in_row, 0x04);
                let mut host = SimHost::new(CAPS, damaged, None);
                match host.open() {
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. }),
                    }) => {}
                    other => panic!(
                        "seed={seed} row={row} byte={byte_in_row}: \
                         corruption not detected: {other:?}"
                    ),
                }
            }
        }
    }
}

#[test]
fn double_fault_on_latest_pair_falls_back_one_generation() {
    // Corrupting BOTH copies of the latest generation exceeds the declared
    // single-fault tolerance. The documented behavior: recovery falls back
    // to the previous generation (losing exactly the last commit), because
    // "highest generation with a valid checksum" is the rule (§4.4). This
    // test pins that boundary so a change to it is a conscious decision.
    for seed in 0..8u64 {
        let (mut disk, ops) = build_db(seed);
        let latest_gen = INSERTS as u64 + 1;
        for slot in slots_for(latest_gen) {
            disk.corrupt(FileId::Superblock, slot * SB_COPY_SIZE as u64 + 9, 0x01);
        }
        let mut host = SimHost::new(CAPS, disk, None);
        match host.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                assert_eq!(n, INSERTS as u64 - 1, "seed={seed}: expected fallback");
            }
            other => panic!("seed={seed}: fallback recovery failed: {other:?}"),
        }
        // All but the last commit present; the last one gone entirely.
        for &(id, value) in &ops[..INSERTS - 1] {
            assert_eq!(host.get(id), Some(value));
        }
        assert_eq!(host.get(ops[INSERTS - 1].0), None);
    }
}

#[test]
fn corruption_outside_live_data_is_inert() {
    // Stale superblock slots and row slots beyond the committed count are
    // not referenced by the manifest; damage there must change nothing
    // ("orphan files are inert by construction", §4.4 — same for orphan
    // bytes).
    for seed in 0..8u64 {
        let (disk, ops) = build_db(seed);
        let latest_gen = INSERTS as u64 + 1;
        let stale = slots_for(latest_gen - 1);
        for slot in stale {
            let mut damaged = disk.clone();
            damaged.corrupt(FileId::Superblock, slot * SB_COPY_SIZE as u64 + 9, 0xFF);
            let mut host = SimHost::new(CAPS, damaged, None);
            assert!(
                matches!(
                    host.open(),
                    Driven::Done(Output::OpenDone { result: Ok(n) }) if n == INSERTS as u64
                ),
                "seed={seed} stale slot={slot}: stale corruption must be inert"
            );
            verify_all(&mut host, &ops);
        }
    }
    // Sanity: the superblock zone is exactly the four slots we reason about.
    assert_eq!(SB_ZONE_SIZE, 4 * SB_COPY_SIZE);
}
