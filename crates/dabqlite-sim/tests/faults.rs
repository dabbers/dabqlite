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
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost, WriteFate};

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
fn exhaustive_bitrot_superblock_every_byte_is_survivable() {
    // EVERY byte of the superblock zone, two bit positions each. A flip in
    // a live copy falls back to its twin; a flip in a stale slot is inert;
    // a flip in padding is caught by full-slot validation. In all cases:
    // zero loss.
    for seed in 0..4u64 {
        let (disk, ops) = build_db(seed);
        let zone_len = disk.len(FileId::Superblock);
        assert_eq!(zone_len, SB_ZONE_SIZE as u64, "zone size drifted");
        for offset in 0..zone_len {
            for mask in [0x01u8, 0x80] {
                let mut damaged = disk.clone();
                damaged.corrupt(FileId::Superblock, offset, mask);
                let mut host = SimHost::new(CAPS, damaged, None);
                match host.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        assert_eq!(
                            n, INSERTS as u64,
                            "seed={seed} offset={offset} mask={mask:#x}: lost rows"
                        );
                    }
                    other => {
                        panic!("seed={seed} offset={offset} mask={mask:#x}: open failed: {other:?}")
                    }
                }
                verify_all(&mut host, &ops);
            }
        }
    }
}

#[test]
fn exhaustive_bitrot_rows_every_byte_is_detected() {
    // EVERY byte of every committed row, two bit positions each. With
    // padding validated, there are no dead zones: any flip in live row data
    // must be detected at open — serving a wrong value with a straight face
    // is the one unforgivable outcome.
    for seed in 0..4u64 {
        let (disk, _ops) = build_db(seed);
        let live_len = INSERTS as u64 * ROW_SIZE as u64;
        for offset in 0..live_len {
            for mask in [0x01u8, 0x80] {
                let mut damaged = disk.clone();
                damaged.corrupt(FileId::Rows, offset, mask);
                let mut host = SimHost::new(CAPS, damaged, None);
                match host.open() {
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. }),
                    }) => {}
                    other => panic!(
                        "seed={seed} offset={offset} mask={mask:#x}: \
                         corruption not detected: {other:?}"
                    ),
                }
            }
        }
    }
}

#[test]
fn truncation_at_rest_is_never_silently_wrong() {
    // The tail of a file vanishes at rest (lost extent, fs repair, sloppy
    // backup). Sweep every 8-byte truncation point of both files. Outcome
    // must be: full recovery, an older-but-correct state, or a loud error.
    // Never wrong data.
    for seed in 0..4u64 {
        let (disk, ops) = build_db(seed);
        let oracle: std::collections::BTreeMap<u64, [u8; VALUE_LEN]> =
            ops.iter().copied().collect();

        // Superblock zone truncation.
        for cut in (0..=SB_ZONE_SIZE as u64).step_by(8) {
            let ctx = format!("seed={seed} sb truncated to {cut}");
            let mut damaged = disk.clone();
            damaged.truncate_at_rest(FileId::Superblock, cut);
            let mut host = SimHost::new(CAPS, damaged, None);
            match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                    assert!(n <= INSERTS as u64, "[{ctx}] impossible row count {n}");
                    for (&id, &value) in &oracle {
                        match host.get(id) {
                            None => {}
                            Some(got) => assert_eq!(got, value, "[{ctx}] wrong bytes served"),
                        }
                    }
                }
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::Corrupt { .. }),
                }) => {}
                other => panic!("[{ctx}] unexpected outcome: {other:?}"),
            }
        }

        // Rows-file truncation: the superblock still references the full
        // set, so any cut below the live region must be detected.
        let live_len = INSERTS as u64 * ROW_SIZE as u64;
        for cut in (0..live_len).step_by(8) {
            let ctx = format!("seed={seed} rows truncated to {cut}");
            let mut damaged = disk.clone();
            damaged.truncate_at_rest(FileId::Rows, cut);
            let mut host = SimHost::new(CAPS, damaged, None);
            assert!(
                matches!(
                    host.open(),
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. })
                    })
                ),
                "[{ctx}] truncated rows file not detected"
            );
        }
    }
}

#[test]
fn garbage_extension_at_rest_is_inert() {
    // Bytes past the manifest-referenced region are orphans and must be
    // ignored, whatever they contain (§4.4: orphans are inert by
    // construction).
    for seed in 0..4u64 {
        let (disk, ops) = build_db(seed);
        let mut rng = crash_rng(seed, 0xEEEE);
        for extra in [1usize, 7, 64, 4096] {
            for file in [FileId::Rows, FileId::Superblock] {
                let mut damaged = disk.clone();
                damaged.extend_at_rest(file, extra, &mut rng);
                let mut host = SimHost::new(CAPS, damaged, None);
                assert!(
                    matches!(
                        host.open(),
                        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == INSERTS as u64
                    ),
                    "seed={seed} {file:?}+{extra}B garbage tail must be inert"
                );
                verify_all(&mut host, &ops);
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

/// THE STORM'S FIND, pinned deterministically (found at storm seed 2 once
/// find-verification draws shifted its fault schedule): a commit torn
/// between its two superblock copy writes can survive in exactly ONE
/// durable copy; visible-implies-durable recovery acknowledges it — and
/// before self-healing recovery existed, left it with NO redundancy, so
/// one later in-budget fault (a media flip or one transient read
/// corruption) silently rolled back the acknowledged commit. Recovery
/// now REPAIRS the pair: it rewrites both copies of the chosen
/// generation before OpenDone.
#[test]
fn recovery_repairs_superblock_redundancy() {
    // One committed row (generation 2, pair 0).
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    let ops = gen_workload(77, 2);
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: ops[0].0,
            value: ops[0].1
        }),
        Driven::Done(Output::InsertDone { result: Ok(()), .. })
    ));

    // Second insert (generation 3, pair 1 = slots 2,3): crash after the
    // FIRST superblock copy write — row durable, copy 0 in cache, copy 1
    // never written.
    host.crash_after = Some(host.io_count + 3);
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: ops[1].0,
            value: ops[1].1
        }),
        Driven::Crashed
    ));
    let mut disk = std::mem::take(&mut host.disk);
    let fates: Vec<WriteFate> = disk
        .unsynced_writes()
        .iter()
        .map(|_| WriteFate::Keep)
        .collect();
    assert_eq!(fates.len(), 1, "exactly the lone sb copy write is unsynced");
    disk.settle_with(&fates);

    // Recovery sees the single surviving copy of generation 3 and, per
    // visible-implies-durable, acknowledges 2 rows.
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(2) })
    ));

    // THE REPAIR, byte-level: both slots of pair 1 must now hold a valid
    // generation-3 copy. (Before the fix, slot 3 still held stale bytes,
    // and generation 3 existed on disk exactly once.)
    for slot in [2u64, 3] {
        let bytes = host.disk.read(
            FileId::Superblock,
            slot * SB_COPY_SIZE as u64,
            SB_COPY_SIZE as u64,
        );
        let copy = dabqlite_core::layout::decode_sb(&bytes)
            .unwrap_or_else(|e| panic!("slot {slot} not repaired: {e:?}"));
        assert_eq!((copy.generation, copy.row_count), (3, 2), "slot {slot}");
    }

    // And the payoff: a single media fault on the ORIGINALLY-surviving
    // copy is now survivable — the repaired twin carries the generation.
    // (Before the fix: silent fallback to generation 2 — an acknowledged
    // commit lost to one in-budget fault.)
    let mut disk = std::mem::take(&mut host.disk);
    disk.corrupt(FileId::Superblock, 2 * SB_COPY_SIZE as u64 + 9, 0x40);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(2) })
    ));
    for &(id, value) in &ops {
        assert!(matches!(
            host.run(ClientOp::Get { id }),
            Driven::Done(Output::GetDone { result: Ok(Some(v)), .. }) if v == value
        ));
    }

    // Transient variant: a corrupt READ of the repaired pair during the
    // sb scan still recovers exactly — the twin is read clean.
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    host.read_corrupt_at = Some((0, 3 * SB_COPY_SIZE + 9, 0x20));
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(2) })
    ));
    assert_eq!(host.reads_corrupted, 1, "the transient fault must fire");
}
