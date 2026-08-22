//! The migration path under fire (docs/DESIGN.md §4.8, build step 7).
//!
//! The claims, each pinned below:
//! - the legacy rows file is READ, NEVER WRITTEN — byte-identical through
//!   success, crash, EIO, and retry;
//! - a crash at every I/O boundary resolves to exactly one of two worlds:
//!   still-legacy (superblock names v1; migration re-runs from scratch)
//!   or fully-migrated (superblock names v2 over a complete, durable
//!   rows file) — never mixed, never lost, never a third state;
//! - EIO fail-stops harmlessly and the migration is re-runnable;
//! - migrating an already-current file is an idempotent no-op with zero
//!   writes;
//! - corrupt or impossible legacy state is refused loudly, zero harm;
//! - the whole thing is deterministic.

use dabqlite_core::generated::records_v1;
use dabqlite_core::migration::{V1_ROW_SIZE, V1_SCHEMA_HASH, V1_VALUE_LEN};
use dabqlite_core::{crc32::crc32, Capacities, DbError, FileId, Output, SB_COPY_SIZE, VALUE_LEN};
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, Misdirect, SimDisk, SimHost, WriteFate};

const CAPS: Capacities = Capacities { rows: 16 };

fn v1_value(seed: u64, i: u64) -> [u8; V1_VALUE_LEN] {
    (seed ^ (i.wrapping_mul(0x9E37_79B9_7F4A_7C15))).to_le_bytes()
}

/// The value the migrated row must hold: old bytes left-aligned, zero tail.
fn v2_value(seed: u64, i: u64) -> [u8; VALUE_LEN] {
    let mut v = [0u8; VALUE_LEN];
    v[..V1_VALUE_LEN].copy_from_slice(&v1_value(seed, i));
    v
}

/// A structurally-valid v1 superblock copy, forged byte-by-byte (a v1
/// binary is not available to this test — its on-disk artifacts are).
fn forge_v1_sb(generation: u64, row_count: u64) -> [u8; SB_COPY_SIZE] {
    let mut out = [0u8; SB_COPY_SIZE];
    out[0..8].copy_from_slice(b"DABQSB01");
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    out[16..24].copy_from_slice(&row_count.to_le_bytes());
    out[24..32].copy_from_slice(&V1_SCHEMA_HASH.to_le_bytes());
    let crc = crc32(&out[0..32]);
    out[32..36].copy_from_slice(&crc.to_le_bytes());
    out
}

/// Build the disk a v1 binary would leave behind after `n` committed
/// inserts: v1 rows in the legacy rows file, a v1 superblock at a
/// generation whose pair matches the commit protocol (gen = n inserts + 1
/// init), everything fsynced.
fn build_v1_disk(seed: u64, n: u64) -> SimDisk {
    let mut disk = SimDisk::new();
    for i in 0..n {
        let row = records_v1::RecordsRow {
            // Interleaved ids so the migrated btree splits both ways.
            id: if i % 2 == 0 { i } else { 2 * n - i },
            value: v1_value(seed, i),
        };
        let mut slot = [0u8; V1_ROW_SIZE];
        records_v1::encode_records_row(&row, &mut slot);
        disk.write(FileId::RowsOld, i * V1_ROW_SIZE as u64, &slot);
    }
    disk.fsync(FileId::RowsOld);
    let generation = n + 1;
    let pair = (generation % 2) * 2;
    for slot in [pair, pair + 1] {
        disk.write(
            FileId::Superblock,
            slot * SB_COPY_SIZE as u64,
            &forge_v1_sb(generation, n),
        );
    }
    disk.fsync(FileId::Superblock);
    disk
}

/// Every id/value the v1 db holds, for verification after migration.
fn expected_rows(seed: u64, n: u64) -> Vec<(u64, [u8; VALUE_LEN])> {
    (0..n)
        .map(|i| {
            let id = if i % 2 == 0 { i } else { 2 * n - i };
            (id, v2_value(seed, i))
        })
        .collect()
}

fn verify_migrated(host: &mut SimHost, seed: u64, n: u64) {
    for &(id, value) in &expected_rows(seed, n) {
        assert_eq!(host.get(id), Some(value), "id={id}");
    }
    // The rebuilt ordered index agrees: strictly ascending full scan.
    let mut ids: Vec<u64> = expected_rows(seed, n).iter().map(|&(id, _)| id).collect();
    ids.sort_unstable();
    let scanned = host.range_all(0, u64::MAX);
    assert_eq!(scanned.iter().map(|&(id, _)| id).collect::<Vec<_>>(), ids);
}

#[test]
fn the_upgrade_path_end_to_end() {
    for seed in 0..4u64 {
        for n in [0u64, 1, 5, 16] {
            let disk = build_v1_disk(seed, n);
            let legacy_bytes = disk.contents(FileId::RowsOld);

            // Step 1: the version gate refuses the legacy file, naming it.
            let mut host = SimHost::new(CAPS, disk, None);
            match host.open() {
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::SchemaMismatch { file_schema, .. }),
                }) => assert_eq!(file_schema, V1_SCHEMA_HASH, "seed={seed} n={n}"),
                other => panic!("seed={seed} n={n}: gate must refuse, got {other:?}"),
            }

            // Step 2: migrate. (A fresh host, as a real app would restart
            // after the refusal.)
            let disk = std::mem::take(&mut host.disk);
            let mut host = SimHost::new(CAPS, disk, None);
            match host.run_migration() {
                Driven::Done(Output::MigrateDone { result: Ok(rows) }) => {
                    assert_eq!(rows, n, "seed={seed} n={n}")
                }
                other => panic!("seed={seed} n={n}: migration failed: {other:?}"),
            }

            // Step 3: the same binary now opens the file cleanly.
            let disk = std::mem::take(&mut host.disk);
            let mut host = SimHost::new(CAPS, disk, None);
            match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(rows) }) => {
                    assert_eq!(rows, n, "seed={seed} n={n}")
                }
                other => panic!("seed={seed} n={n}: post-migration open: {other:?}"),
            }
            assert!(!host.engine.recovery_report().rollback_evidence);
            verify_migrated(&mut host, seed, n);

            // The legacy file: read, never written. Byte-identical.
            assert_eq!(
                host.disk.contents(FileId::RowsOld),
                legacy_bytes,
                "seed={seed} n={n}: migration wrote the legacy file"
            );
        }
    }
}

#[test]
fn migrating_an_already_current_file_is_a_durable_noop() {
    let disk = build_v1_disk(7, 6);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.run_migration(),
        Driven::Done(Output::MigrateDone { result: Ok(6) })
    ));
    let after_first = (
        host.disk.contents(FileId::Superblock),
        host.disk.contents(FileId::Rows),
        host.disk.contents(FileId::RowsOld),
    );

    // Second run: zero writes (fsyncs allowed — they make visible state
    // durable, they change no bytes), identical disk.
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.run_migration(),
        Driven::Done(Output::MigrateDone { result: Ok(6) })
    ));
    assert_eq!(host.n_writes, 0, "noop migration performed writes");
    assert_eq!(
        (
            host.disk.contents(FileId::Superblock),
            host.disk.contents(FileId::Rows),
            host.disk.contents(FileId::RowsOld),
        ),
        after_first,
        "noop migration changed bytes"
    );
}

/// The heart of it: crash at EVERY I/O boundary of the migration, settle
/// the page cache randomly (several seeds per boundary), and demand the
/// two-world invariant. Then re-run the migration from whichever world
/// resulted and demand full recovery of every row.
#[test]
fn crash_at_every_boundary_leaves_two_worlds_only() {
    let seed = 11u64;
    let n = 6u64;
    let pristine = build_v1_disk(seed, n);
    let legacy_bytes = pristine.contents(FileId::RowsOld);

    // Learn the fault-free I/O count first.
    let total_io = {
        let mut host = SimHost::new(CAPS, pristine.clone(), None);
        assert!(matches!(
            host.run_migration(),
            Driven::Done(Output::MigrateDone { result: Ok(rows) }) if rows == n
        ));
        host.io_count
    };
    // 1 sb read + 1 legacy read + n row writes + 1 rows fsync
    // + 2 sb writes + 1 sb fsync.
    assert_eq!(total_io, 6 + n, "migration I/O shape changed");

    let mut migrated_worlds = 0u64;
    let mut legacy_worlds = 0u64;
    for boundary in 0..total_io {
        for settle_seed in 0..4u64 {
            let mut host = SimHost::new(CAPS, pristine.clone(), Some(boundary));
            assert!(
                matches!(host.run_migration(), Driven::Crashed),
                "boundary {boundary} did not crash"
            );
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(0x316, boundary * 101 + settle_seed));

            // The legacy file survived the crash byte-identical: it had no
            // unsynced writes because migration never writes it.
            assert_eq!(
                disk.contents(FileId::RowsOld),
                legacy_bytes,
                "boundary {boundary} settle {settle_seed}: legacy file changed"
            );

            // Two worlds only.
            let mut host = SimHost::new(CAPS, disk, None);
            match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(rows) }) => {
                    assert_eq!(rows, n, "boundary {boundary}: partial commit");
                    migrated_worlds += 1;
                    verify_migrated(&mut host, seed, n);
                }
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::SchemaMismatch { file_schema, .. }),
                }) => {
                    assert_eq!(file_schema, V1_SCHEMA_HASH);
                    legacy_worlds += 1;
                    // Still legacy: the migration re-runs to completion.
                    let disk = std::mem::take(&mut host.disk);
                    let mut host = SimHost::new(CAPS, disk, None);
                    assert!(matches!(
                        host.run_migration(),
                        Driven::Done(Output::MigrateDone { result: Ok(rows) }) if rows == n
                    ));
                    let disk = std::mem::take(&mut host.disk);
                    let mut host = SimHost::new(CAPS, disk, None);
                    assert!(matches!(
                        host.open(),
                        Driven::Done(Output::OpenDone { result: Ok(rows) }) if rows == n
                    ));
                    verify_migrated(&mut host, seed, n);
                }
                other => panic!("boundary {boundary} settle {settle_seed}: THIRD world: {other:?}"),
            }
        }
    }
    // Coverage floor: both worlds must actually occur across the sweep.
    assert!(legacy_worlds > 0, "sweep never saw the still-legacy world");
    assert!(migrated_worlds > 0, "sweep never saw the migrated world");
}

/// EIO at every boundary: fail-stop with the file named, legacy file
/// untouched, and the migration re-runnable to full success.
#[test]
fn eio_at_every_boundary_fails_stop_and_recovers() {
    let seed = 13u64;
    let n = 6u64;
    let pristine = build_v1_disk(seed, n);
    let legacy_bytes = pristine.contents(FileId::RowsOld);
    let total_io = 6 + n;

    for boundary in 0..total_io {
        let mut host = SimHost::new(CAPS, pristine.clone(), None);
        host.fail_after = Some(boundary);
        match host.run_migration() {
            Driven::Done(Output::MigrateDone {
                result: Err(DbError::IoFailed { .. }),
            }) => {}
            other => panic!("boundary {boundary}: expected fail-stop, got {other:?}"),
        }
        assert_eq!(
            host.disk.contents(FileId::RowsOld),
            legacy_bytes,
            "boundary {boundary}: EIO path wrote the legacy file"
        );
        // Retry on the same disk (page cache intact, like a process
        // restart after EIO): completes, and everything is there.
        let disk = std::mem::take(&mut host.disk);
        let mut host = SimHost::new(CAPS, disk, None);
        assert!(matches!(
            host.run_migration(),
            Driven::Done(Output::MigrateDone { result: Ok(rows) }) if rows == n
        ));
        let disk = std::mem::take(&mut host.disk);
        let mut host = SimHost::new(CAPS, disk, None);
        assert!(matches!(
            host.open(),
            Driven::Done(Output::OpenDone { result: Ok(rows) }) if rows == n
        ));
        verify_migrated(&mut host, seed, n);
    }
}

#[test]
fn corrupt_legacy_rows_are_refused_loudly() {
    for byte in [0usize, 7, 8, 16, 20, 23, 24 * 3 + 5] {
        let mut disk = build_v1_disk(3, 6);
        disk.corrupt(FileId::RowsOld, byte as u64, 0x40);
        let legacy_bytes = disk.contents(FileId::RowsOld);
        let sb_bytes = disk.contents(FileId::Superblock);

        let mut host = SimHost::new(CAPS, disk, None);
        match host.run_migration() {
            Driven::Done(Output::MigrateDone {
                result: Err(DbError::Corrupt { what }),
            }) => assert!(
                what.contains("checksum"),
                "byte {byte}: error must name the cause: {what}"
            ),
            other => panic!("byte {byte}: corrupt row must refuse, got {other:?}"),
        }
        // Refusal wrote nothing the operator would need for forensics:
        // legacy file and superblock exactly as found (the staging file
        // may hold partial rows — inert, nothing names it).
        assert_eq!(host.disk.contents(FileId::RowsOld), legacy_bytes);
        assert_eq!(host.disk.contents(FileId::Superblock), sb_bytes);
    }
}

#[test]
fn foreign_schema_and_capacity_are_refused() {
    // A schema this binary has no migration for: refused, both hashes named.
    let mut disk = build_v1_disk(0, 4);
    let foreign = 0xFEED_FACE_CAFE_0002u64;
    let generation = 5u64; // matches build_v1_disk(_, 4)
    let pair = (generation % 2) * 2;
    for slot in [pair, pair + 1] {
        let mut copy = forge_v1_sb(generation, 4);
        copy[24..32].copy_from_slice(&foreign.to_le_bytes());
        let crc = crc32(&copy[0..32]);
        copy[32..36].copy_from_slice(&crc.to_le_bytes());
        disk.write(FileId::Superblock, slot * SB_COPY_SIZE as u64, &copy);
    }
    disk.fsync(FileId::Superblock);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.run_migration(),
        Driven::Done(Output::MigrateDone {
            result: Err(DbError::SchemaMismatch { file_schema, .. })
        }) if file_schema == foreign
    ));

    // More legacy rows than the configured capacity: refused with numbers.
    let disk = build_v1_disk(0, 6);
    let mut host = SimHost::new(Capacities { rows: 5 }, disk, None);
    assert!(matches!(
        host.run_migration(),
        Driven::Done(Output::MigrateDone {
            result: Err(DbError::CapacityBelowData {
                required: 6,
                configured: 5
            })
        })
    ));
}

#[test]
fn migration_is_deterministic() {
    let disk = build_v1_disk(9, 6);
    let run = |d: SimDisk| {
        let mut host = SimHost::new(CAPS, d, None);
        assert!(matches!(
            host.run_migration(),
            Driven::Done(Output::MigrateDone { result: Ok(6) })
        ));
        (
            host.disk.contents(FileId::Superblock),
            host.disk.contents(FileId::Rows),
            host.disk.contents(FileId::RowsOld),
            host.io_count,
        )
    };
    assert_eq!(run(disk.clone()), run(disk));
}

/// The migrated world's data, or proof we are still (recoverably) in the
/// legacy world — with `allow_loud` (out-of-budget faults only), a LOUD
/// Corrupt open is also acceptable, PROVIDED the migration's self-healing
/// redo then converges to the full migrated world with zero loss.
fn assert_two_worlds_and_converge(
    mut disk: SimDisk,
    seed: u64,
    n: u64,
    allow_loud: bool,
    ctx: &str,
) {
    let rerun = |disk: SimDisk, ctx: &str| {
        let mut host = SimHost::new(CAPS, disk, None);
        assert!(
            matches!(
                host.run_migration(),
                Driven::Done(Output::MigrateDone { result: Ok(rows) }) if rows == n
            ),
            "{ctx}: retry did not converge"
        );
        let disk = std::mem::take(&mut host.disk);
        let mut host = SimHost::new(CAPS, disk, None);
        assert!(matches!(
            host.open(),
            Driven::Done(Output::OpenDone { result: Ok(rows) }) if rows == n
        ));
        verify_migrated(&mut host, seed, n);
    };
    let mut host = SimHost::new(CAPS, disk, None);
    match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(rows) }) => {
            assert_eq!(rows, n, "{ctx}: partial commit");
            verify_migrated(&mut host, seed, n);
        }
        Driven::Done(Output::OpenDone {
            result: Err(DbError::SchemaMismatch { file_schema, .. }),
        }) => {
            assert_eq!(file_schema, V1_SCHEMA_HASH, "{ctx}");
            disk = std::mem::take(&mut host.disk);
            rerun(disk, ctx);
        }
        Driven::Done(Output::OpenDone {
            result: Err(DbError::Corrupt { .. }),
        }) if allow_loud => {
            // Out-of-budget faults may leave the current world incoherent
            // (loud, never wrong). The migration's verify-then-redo path
            // must rebuild it from the untouched legacy source.
            disk = std::mem::take(&mut host.disk);
            rerun(disk, ctx);
        }
        other => panic!("{ctx}: third world: {other:?}"),
    }
}

/// Exhaustive settle of the FLIP window: crash right before the final
/// superblock fsync leaves exactly the two flip writes unsynced —
/// enumerate every persistence combination of both (Keep/Drop/prefixes/
/// sector subsets), and demand the two-worlds invariant for each.
#[test]
fn every_persistence_combination_of_the_flip_window() {
    let seed = 17u64;
    let n = 4u64;
    let pristine = build_v1_disk(seed, n);
    let total_io = 6 + n;

    // Crash immediately before the final fsync: ops 0..total_io-1 done.
    let mut host = SimHost::new(CAPS, pristine.clone(), Some(total_io - 1));
    assert!(matches!(host.run_migration(), Driven::Crashed));
    let disk = std::mem::take(&mut host.disk);
    let window = disk.unsynced_writes();
    assert_eq!(window.len(), 2, "flip window is exactly the two sb writes");

    fn fates_for(len: usize) -> Vec<WriteFate> {
        let mut fates = vec![WriteFate::Drop, WriteFate::Keep];
        for n in (8..len).step_by(8) {
            fates.push(WriteFate::Prefix(n));
        }
        fates.push(WriteFate::Subset(0b10));
        fates.push(WriteFate::Subset(0b0101_0101));
        fates.push(WriteFate::SubsetGarbage {
            mask: 0b1111_0000,
            garbage_sector: 1,
            garbage: [0x5A; dabqlite_sim::SECTOR],
        });
        fates
    }
    let mut scenarios = 0u64;
    for f0 in fates_for(64) {
        for f1 in fates_for(64) {
            let mut d = disk.clone();
            d.settle_with(&[f0, f1]);
            assert_eq!(
                d.contents(FileId::RowsOld),
                pristine.contents(FileId::RowsOld),
                "legacy file changed"
            );
            assert_two_worlds_and_converge(d, seed, n, false, &format!("fates {f0:?}/{f1:?}"));
            scenarios += 1;
        }
    }
    assert!(scenarios >= 100, "only {scenarios} flip-window scenarios");
}

/// Misdirected writes during migration (firmware lies about WHERE): the
/// outcome may be the migrated world, the legacy world, or a LOUD
/// refusal — never silently wrong data, and the legacy file only changes
/// if the misdirect physically lands there (same-file shifts; the
/// engine never targets it).
#[test]
fn misdirected_writes_during_migration_are_never_silent() {
    let seed = 19u64;
    let n = 4u64;
    let pristine = build_v1_disk(seed, n);
    let total_io = 6 + n;
    let mut hits = 0u64;
    for idx in 0..total_io {
        for kind in [
            Misdirect::Shift(64),
            Misdirect::Shift(-64),
            Misdirect::CrossFile,
        ] {
            let mut host = SimHost::new(CAPS, pristine.clone(), None);
            host.misdirect_at = Some((idx, kind));
            let outcome = host.run_migration();
            hits += host.misdirected;
            let ctx = format!("idx={idx} kind={kind:?}");
            match outcome {
                Driven::Done(Output::MigrateDone { result: Ok(rows) }) => {
                    assert_eq!(rows, n, "{ctx}");
                    // The machine believes it migrated. Open and demand
                    // NEVER-WRONG: either every row is exact, or the
                    // damage is detected loudly.
                    let disk = std::mem::take(&mut host.disk);
                    let mut host = SimHost::new(CAPS, disk, None);
                    match host.open() {
                        Driven::Done(Output::OpenDone { result: Ok(rows) }) if rows == n => {
                            verify_migrated(&mut host, seed, n);
                        }
                        Driven::Done(Output::OpenDone {
                            result: Err(DbError::Corrupt { .. }),
                        }) => {} // loud: acceptable for an out-of-budget fault
                        Driven::Done(Output::OpenDone { result: Ok(rows) }) => {
                            panic!("{ctx}: silent wrong count {rows}");
                        }
                        other => panic!("{ctx}: {other:?}"),
                    }
                }
                Driven::Done(Output::MigrateDone {
                    result: Err(DbError::Corrupt { .. } | DbError::SchemaMismatch { .. }),
                }) => {} // refused loudly mid-flight: fine
                other => panic!("{ctx}: {other:?}"),
            }
        }
    }
    assert!(hits > 0, "no misdirect ever fired");
}

/// Transient READ faults during migration's two reads: at worst a loud,
/// harmless refusal — and a clean retry always converges.
#[test]
fn corrupt_reads_during_migration_refuse_then_converge() {
    let seed = 23u64;
    let n = 5u64;
    let pristine = build_v1_disk(seed, n);
    for read_idx in [0u64, 1] {
        for byte in [0usize, 40, 70, 100] {
            let mut host = SimHost::new(CAPS, pristine.clone(), None);
            host.read_corrupt_at = Some((read_idx, byte, 0x08));
            let ctx = format!("read={read_idx} byte={byte}");
            match host.run_migration() {
                Driven::Done(Output::MigrateDone { result: Ok(rows) }) => {
                    // The flip survived the transient (e.g. the corrupt
                    // byte hit a stale slot or the twin carried it).
                    assert_eq!(rows, n, "{ctx}");
                }
                Driven::Done(Output::MigrateDone {
                    result: Err(DbError::Corrupt { .. } | DbError::SchemaMismatch { .. }),
                }) => {}
                other => panic!("{ctx}: {other:?}"),
            }
            // Whatever happened, the legacy file is intact and a clean
            // retry converges to the full migrated world.
            let disk = std::mem::take(&mut host.disk);
            assert_eq!(
                disk.contents(FileId::RowsOld),
                pristine.contents(FileId::RowsOld),
                "{ctx}: legacy file changed"
            );
            assert_two_worlds_and_converge(disk, seed, n, false, &ctx);
        }
    }
}

/// A LYING fsync during migration (fsyncgate): the migration may be
/// acknowledged and then retracted by a machine crash — but unlike an
/// insert, migration under fsyncgate loses NOTHING: the legacy file
/// still holds every row, and the retry converges to the full migrated
/// world. Bounded regression, zero data loss.
#[test]
fn lying_fsyncs_during_migration_lose_nothing() {
    let seed = 29u64;
    let n = 4u64;
    let pristine = build_v1_disk(seed, n);
    let total_io = 6 + n;
    for lie_from in 0..total_io {
        for settle_seed in 0..3u64 {
            let mut host = SimHost::new(CAPS, pristine.clone(), None);
            host.lie_fsync_from = Some(lie_from);
            let acked = matches!(
                host.run_migration(),
                Driven::Done(Output::MigrateDone { result: Ok(rows) }) if rows == n
            );
            assert!(acked, "lie_from={lie_from}: migration itself never errors");
            // Machine crash: everything the lying fsyncs never persisted
            // settles arbitrarily.
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(0xF5C, lie_from * 10 + settle_seed));
            assert_eq!(
                disk.contents(FileId::RowsOld),
                pristine.contents(FileId::RowsOld),
                "lie_from={lie_from}: legacy file changed"
            );
            assert_two_worlds_and_converge(
                disk,
                seed,
                n,
                true, // fsyncgate is out-of-budget: loud-then-heal is the contract
                &format!("lie_from={lie_from} settle={settle_seed}"),
            );
        }
    }
}
