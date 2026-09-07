//! The inspector vs the engine: two independent implementations of the
//! recovery rules, cross-checked (docs/DESIGN.md §7.4 pair philosophy —
//! same pattern as the reference codec vs the generated one).
//!
//! For every disk this suite can manufacture — clean, crash-settled at
//! every boundary, superblock-wiped, forged-foreign, legacy-v1, row-
//! corrupted, orphan-bearing — the pure `inspect()` verdict must predict
//! the engine's actual open outcome exactly, and its forensics counts
//! must match the engine's recovery report.

use dabqlite_core::generated::records_v1;
use dabqlite_core::inspect::{inspect, SlotState, Verdict};
use dabqlite_core::migration::{V1_ROW_SIZE, V1_SCHEMA_HASH};
use dabqlite_core::{
    crc32::crc32, BatchOp, Capacities, DbError, FileId, Output, ROW_SIZE, SB_COPY_SIZE,
    SCHEMA_HASH, VALUE_LEN,
};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};
use rand::{Rng, RngCore};

const CAPS: Capacities = Capacities { rows: 16 };

fn build_db(seed: u64, n: u64) -> SimDisk {
    let ops = gen_workload(seed, n as usize);
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for &(id, value) in &ops {
        host.run(ClientOp::Insert { id, value });
    }
    std::mem::take(&mut host.disk)
}

/// The agreement oracle: inspect the disk, open it with a real engine,
/// and demand the verdict predicted the outcome — including the orphan
/// and rollback-evidence accounting when the open succeeds.
fn assert_agreement(disk: SimDisk, ctx: &str) {
    assert_agreement_caps(CAPS, disk, ctx);
}

fn assert_agreement_caps(caps: Capacities, disk: SimDisk, ctx: &str) {
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    let mut host = SimHost::new(caps, disk, None);
    let actual = match host.open() {
        Driven::Done(Output::OpenDone { result }) => result,
        other => panic!("[{ctx}] open did not finish: {other:?}"),
    };
    let predicted = match report.verdict {
        Verdict::FreshInit => Ok(0),
        Verdict::Recovers { rows } => Ok(rows),
        Verdict::SchemaMismatch { file_schema, .. } => Err(DbError::SchemaMismatch {
            file_schema,
            binary_schema: SCHEMA_HASH,
        }),
        Verdict::Corrupt { what } => Err(DbError::Corrupt { what }),
    };
    // The verdict's row count is the MANIFEST — how many slots recovery
    // adopts — while `OpenDone` reports how many rows are LIVE
    // afterwards. Those are the same number only until something is
    // deleted, updated, or written across several slots, so check both
    // against their own counterpart rather than conflating them.
    match (predicted, actual) {
        (Ok(rows), Ok(live)) => {
            assert_eq!(
                rows,
                host.engine.usage().0,
                "[{ctx}] recovered slot count diverged: {report:?}"
            );
            assert_eq!(
                report.rows.live_records, live,
                "[{ctx}] inspector and engine disagree about what is LIVE: {report:?}"
            );
        }
        (predicted, actual) => assert_eq!(
            predicted, actual,
            "[{ctx}] inspector and engine disagree: {report:?}"
        ),
    }
    if matches!(report.verdict, Verdict::Recovers { .. }) {
        let rr = host.engine.recovery_report();
        assert_eq!(
            report.rows.orphan_valid, rr.orphan_valid_rows,
            "[{ctx}] orphan accounting diverged"
        );
        assert_eq!(
            report.rollback_evidence, rr.rollback_evidence,
            "[{ctx}] rollback-evidence flag diverged"
        );
        assert_eq!(report.rows.committed_corrupt, 0, "[{ctx}]");
        assert_eq!(report.rows.duplicate_ids, 0, "[{ctx}]");
    }
}

/// A database written the way the newest paths write it: atomic batches
/// carrying values that span several row slots, with updates and
/// deletions mixed in so the commit replay has every row kind in it.
///
/// The inspector reaches its verdict from the bytes alone, so a value's
/// RUN — head plus continuations, ending where the head said it would —
/// is a rule it has to know independently. It did not, once: it counted
/// every continuation as a stranded chunk and called every long-value
/// database corrupt, and nothing caught it until the soak started
/// writing them.
fn build_varlen_db(seed: u64, caps: Capacities) -> SimDisk {
    let mut rng = crash_rng(0x5641_524C, seed); // "VARL"
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    let mut values: Vec<(u64, Vec<u8>)> = Vec::new();
    for i in 0..3u64 {
        let batch: Vec<(u64, Vec<u8>)> = (0..2u64)
            .map(|k| {
                let len = rng.gen_range(0..=3 * VALUE_LEN);
                let mut v = vec![0u8; len];
                rng.fill_bytes(&mut v);
                (i * 2 + k, v)
            })
            .collect();
        let ops: Vec<BatchOp> = batch
            .iter()
            .map(|(id, v)| BatchOp::Insert { id: *id, value: v })
            .collect();
        assert!(
            matches!(
                host.batch(&ops),
                Driven::Done(Output::BatchDone { result: Ok(()), .. })
            ),
            "seed={seed} batch {i} refused"
        );
        values.extend(batch);
    }
    // One batch that updates a long value and deletes another, so the
    // replay contains an Update run and a Tombstone as well as Records.
    let mut fresh = vec![0u8; 2 * VALUE_LEN + 5];
    rng.fill_bytes(&mut fresh);
    assert!(matches!(
        host.batch(&[
            BatchOp::Update {
                id: values[0].0,
                value: &fresh,
            },
            BatchOp::Delete { id: values[1].0 },
        ]),
        Driven::Done(Output::BatchDone { result: Ok(()), .. })
    ));
    std::mem::take(&mut host.disk)
}

#[test]
fn agreement_on_long_values_and_batches() {
    let caps = Capacities { rows: 64 };
    for seed in 0..6u64 {
        assert_agreement_caps(
            caps,
            build_varlen_db(seed, caps),
            &format!("varlen seed={seed}"),
        );
    }
}

#[test]
fn agreement_on_long_values_at_every_crash_boundary() {
    // A batch of multi-slot values is ONE commit spanning many I/O ops,
    // so its crash boundaries are exactly where a naive reader would
    // mistake a partial run for a whole one. Both implementations have
    // to reach the same verdict at every one of them.
    let caps = Capacities { rows: 64 };
    for seed in 0..3u64 {
        for boundary in 0..14u64 {
            for settle in 0..2u64 {
                let disk = build_varlen_db(seed, caps);
                let mut host = SimHost::new(caps, disk, None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                let value = vec![0xA7; 2 * VALUE_LEN + 3];
                let driven = host.batch(&[
                    BatchOp::Insert {
                        id: 900 + seed,
                        value: &value,
                    },
                    BatchOp::Insert {
                        id: 950 + seed,
                        value: &value,
                    },
                ]);
                let mut disk = std::mem::take(&mut host.disk);
                if matches!(driven, Driven::Crashed) {
                    disk.crash(&mut crash_rng(
                        0x4C4F_4E47,
                        seed * 100 + boundary * 4 + settle,
                    ));
                }
                assert_agreement_caps(caps, disk, &format!("seed={seed} b={boundary} s={settle}"));
            }
        }
    }
}

#[test]
fn agreement_when_a_continuation_is_damaged_or_stranded() {
    let caps = Capacities { rows: 64 };
    // Find a continuation slot: a chunk row is one that follows a head
    // which said the value continues. Row 1 of a database whose first
    // value is long is the simplest such slot, so build until one is.
    let (disk, chunk_row) = (0..8u64)
        .find_map(|seed| {
            let disk = build_varlen_db(seed, caps);
            let rows = disk.contents(FileId::Rows);
            (1..rows.len() / ROW_SIZE)
                .find(|&r| {
                    dabqlite_core::layout::decode_row(&rows[r * ROW_SIZE..(r + 1) * ROW_SIZE])
                        .is_some_and(|s| s.kind == dabqlite_core::layout::RowKind::Chunk)
                })
                .map(|r| (disk, r as u64))
        })
        .expect("some seed writes a multi-slot value");

    // Damaged: the head promised a continuation and what follows is not
    // a readable one, so the WHOLE value is unreadable — not just its
    // tail, and not the file.
    let mut damaged = disk.clone();
    damaged.corrupt(FileId::Rows, chunk_row * ROW_SIZE as u64 + 3, 0x40);
    let report = inspect(
        &damaged.contents(FileId::Superblock),
        &damaged.contents(FileId::Rows),
    );
    assert_eq!(
        report.verdict,
        Verdict::Corrupt {
            what: dabqlite_core::defect::TRUNCATED_VALUE
        },
        "a value whose continuation is unreadable is not a readable value"
    );
    assert_eq!(report.rows.truncated_values, 1);
    assert_agreement_caps(caps, damaged, "damaged continuation");

    // Stranded: the same bytes copied verbatim over a LATER row (a
    // checksum covers the row, not its position, so this is exactly what
    // a misdirected write produces). Nothing in front of it claims it.
    let rows = disk.contents(FileId::Rows);
    let bytes = rows[(chunk_row as usize) * ROW_SIZE..(chunk_row as usize + 1) * ROW_SIZE].to_vec();
    let last = (rows.len() / ROW_SIZE - 1) as u64;
    assert!(last > chunk_row, "need a later row to strand the copy in");
    let mut stranded = disk.clone();
    stranded.write(FileId::Rows, last * ROW_SIZE as u64, &bytes);
    stranded.fsync(FileId::Rows);
    let report = inspect(
        &stranded.contents(FileId::Superblock),
        &stranded.contents(FileId::Rows),
    );
    assert_eq!(
        report.verdict,
        Verdict::Corrupt {
            what: dabqlite_core::defect::ORPHAN_CHUNK
        },
        "a continuation with nothing to continue is damage, not a row"
    );
    assert_eq!(report.rows.orphan_chunks, 1);
    assert_agreement_caps(caps, stranded, "stranded continuation");
}

#[test]
fn agreement_on_clean_databases() {
    for seed in 0..4u64 {
        for n in [0u64, 1, 7, 16] {
            assert_agreement(build_db(seed, n), &format!("clean seed={seed} n={n}"));
        }
    }
    assert_agreement(SimDisk::new(), "empty directory");
}

#[test]
fn agreement_at_every_crash_boundary() {
    for seed in 0..3u64 {
        let base = build_db(seed, 4);
        for boundary in 0..5u64 {
            for settle in 0..3u64 {
                let mut host = SimHost::new(CAPS, base.clone(), None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                assert!(matches!(
                    host.run(ClientOp::Insert {
                        id: u64::MAX - seed, // guaranteed fresh
                        value: [0xAB; VALUE_LEN]
                    }),
                    Driven::Crashed
                ));
                let mut disk = std::mem::take(&mut host.disk);
                disk.crash(&mut crash_rng(
                    0x1259EC7,
                    seed * 1000 + boundary * 10 + settle,
                ));
                assert_agreement(disk, &format!("crash s={seed} b={boundary} st={settle}"));
            }
        }
    }
}

#[test]
fn agreement_on_wiped_and_corrupted_superblocks() {
    // All superblock slots zeroed over live rows: Corrupt, and the
    // inspector sees four Empty slots.
    let mut disk = build_db(1, 5);
    let sb_len = disk.contents(FileId::Superblock).len();
    disk.write(FileId::Superblock, 0, &vec![0u8; sb_len]);
    disk.fsync(FileId::Superblock);
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert!(report
        .slots
        .iter()
        .all(|s| matches!(s, SlotState::Empty | SlotState::Missing)));
    assert_agreement(disk, "wiped superblock");

    // A single flipped bit in one live copy: that slot reads Invalid, the
    // twin still carries the generation, recovery succeeds.
    let mut disk = build_db(1, 5);
    let live_gen = 6u64; // 5 inserts + init
    let first_slot = (live_gen % 2) * 2;
    disk.corrupt(
        FileId::Superblock,
        first_slot * SB_COPY_SIZE as u64 + 9,
        0x10,
    );
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert_eq!(report.slots[first_slot as usize], SlotState::Invalid);
    assert!(matches!(report.verdict, Verdict::Recovers { rows: 5 }));
    assert_agreement(disk, "single-copy bit rot");
}

#[test]
fn agreement_on_foreign_and_legacy_schemas() {
    // Forge a foreign schema over live pair slots.
    let forge = |generation: u64, row_count: u64, schema: u64| {
        let mut out = [0u8; SB_COPY_SIZE];
        out[0..8].copy_from_slice(b"DABQSB01");
        out[8..16].copy_from_slice(&generation.to_le_bytes());
        out[16..24].copy_from_slice(&row_count.to_le_bytes());
        out[24..32].copy_from_slice(&schema.to_le_bytes());
        let crc = crc32(&out[0..32]);
        out[32..36].copy_from_slice(&crc.to_le_bytes());
        out
    };
    let foreign_schema = 0xABAD_1DEA_0000_0001u64;
    let mut disk = build_db(2, 4);
    let generation = 5u64;
    for pair_base in [0u64, 2] {
        for slot in [pair_base, pair_base + 1] {
            let g = if pair_base == (generation % 2) * 2 {
                generation
            } else {
                generation - 1
            };
            disk.write(
                FileId::Superblock,
                slot * SB_COPY_SIZE as u64,
                &forge(g, 4, foreign_schema),
            );
        }
    }
    disk.fsync(FileId::Superblock);
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert_eq!(
        report.verdict,
        Verdict::SchemaMismatch {
            file_schema: foreign_schema,
            migratable: false
        },
        "unknown schema must not claim migratability"
    );
    assert_agreement(disk, "foreign schema");

    // A genuine legacy v1 database: mismatch, and MIGRATABLE.
    let mut disk = SimDisk::new();
    let row = records_v1::RecordsRow {
        id: 3,
        value: [7; 8],
    };
    let mut slot = [0u8; V1_ROW_SIZE];
    records_v1::encode_records_row(&row, &mut slot);
    disk.write(FileId::RowsOld, 0, &slot);
    disk.fsync(FileId::RowsOld);
    for s in [0u64, 1] {
        // generation 2 lives in pair 0
        disk.write(
            FileId::Superblock,
            s * SB_COPY_SIZE as u64,
            &forge(2, 1, V1_SCHEMA_HASH),
        );
    }
    disk.fsync(FileId::Superblock);
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert_eq!(
        report.verdict,
        Verdict::SchemaMismatch {
            file_schema: V1_SCHEMA_HASH,
            migratable: true
        },
        "the compiled-in legacy schema is exactly what migration exists for"
    );
    assert_agreement(disk, "legacy v1");
}

#[test]
fn agreement_on_row_corruption_and_orphans() {
    // Flip a bit inside a committed row: Corrupt, with the offset named.
    let mut disk = build_db(3, 6);
    disk.corrupt(FileId::Rows, 2 * 32 + 5, 0x01);
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert_eq!(report.rows.committed_corrupt, 1);
    assert_eq!(report.rows.corrupt_offsets, vec![2 * 32]);
    assert_agreement(disk, "corrupt committed row");

    // A valid orphan beyond the manifest (the in-flight-insert artifact):
    // recovery succeeds, orphan counted, no rollback evidence.
    let mut host = SimHost::new(CAPS, build_db(4, 3), None);
    host.open();
    host.crash_after = Some(host.io_count + 2); // die after row write+fsync
    let ops = gen_workload(999, 1);
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: ops[0].0,
            value: ops[0].1
        }),
        Driven::Crashed
    ));
    let disk = std::mem::take(&mut host.disk);
    // No machine crash: the page cache survived (process restart).
    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert_eq!(report.rows.orphan_valid, 1, "the in-flight artifact");
    assert!(!report.rollback_evidence);
    assert_agreement(disk, "single orphan");
}

/// A checksum-valid CURRENT-schema copy sitting OUTSIDE its home pair —
/// the product of a misdirected write — must be distrusted by both
/// implementations, even when it advertises a tempting higher generation.
#[test]
fn agreement_on_misplaced_valid_copies() {
    let mut disk = build_db(6, 4);
    let generation = 5u64; // 4 inserts + init; home pair 1 (slots 2,3)
                           // Forge a valid current-schema copy at generation g+3 (home pair 0)
                           // and plant it in slot 2 — inside gen g's pair, outside its own.
    let mut forged = [0u8; SB_COPY_SIZE];
    forged[0..8].copy_from_slice(b"DABQSB01");
    forged[8..16].copy_from_slice(&(generation + 3).to_le_bytes());
    forged[16..24].copy_from_slice(&1u64.to_le_bytes());
    forged[24..32].copy_from_slice(&SCHEMA_HASH.to_le_bytes());
    let crc = crc32(&forged[0..32]);
    forged[32..36].copy_from_slice(&crc.to_le_bytes());
    disk.write(FileId::Superblock, 2 * SB_COPY_SIZE as u64, &forged);
    disk.fsync(FileId::Superblock);

    let report = inspect(
        &disk.contents(FileId::Superblock),
        &disk.contents(FileId::Rows),
    );
    assert_eq!(
        report.slots[2],
        SlotState::Valid {
            generation: generation + 3,
            row_count: 1,
            schema: SCHEMA_HASH,
            in_home_pair: false,
        },
        "the misplaced copy must be reported, flagged, and distrusted"
    );
    assert!(matches!(report.verdict, Verdict::Recovers { rows: 4 }));
    assert_agreement(disk, "misplaced valid copy");
}

/// Inspection is PURE: it borrows bytes, so mutating anything is a type
/// error — but pin the observable version too: inspecting a disk twice
/// yields identical reports and identical disk contents.
#[test]
fn inspection_is_readonly_and_deterministic() {
    let disk = build_db(5, 6);
    let sb = disk.contents(FileId::Superblock);
    let rows = disk.contents(FileId::Rows);
    let a = inspect(&sb, &rows);
    let b = inspect(&sb, &rows);
    assert_eq!(a, b, "inspection must be deterministic");
    assert_eq!(disk.contents(FileId::Superblock), sb);
    assert_eq!(disk.contents(FileId::Rows), rows);
}
