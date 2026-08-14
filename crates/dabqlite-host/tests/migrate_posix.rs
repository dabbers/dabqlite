#![cfg(unix)]
//! The migration path on REAL files (docs/DESIGN.md §4.8): a directory a
//! v1 binary would leave behind — hash-named legacy rows file plus its
//! superblock — migrated through `PosixStorage` with genuine writes and
//! fsyncs, then opened by the ordinary engine. The migrated bytes must be
//! IDENTICAL to what the simulator produces for the same input: the
//! sim/real equivalence contract extends to migration.

use std::path::PathBuf;

use dabqlite_core::generated::records_v1;
use dabqlite_core::migration::{V1_ROW_SIZE, V1_SCHEMA_HASH, V1_VALUE_LEN};
use dabqlite_core::{crc32::crc32, Capacities, FileId, Output, SB_COPY_SIZE, SCHEMA_HASH};
use dabqlite_host::posix::{rows_file_name, SUPERBLOCK_FILE};
use dabqlite_host::{Host, PosixStorage};
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 16 };
const N: u64 = 6;
const SEED: u64 = 21;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-migrate-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn v1_value(i: u64) -> [u8; V1_VALUE_LEN] {
    (SEED ^ (i.wrapping_mul(0x9E37_79B9_7F4A_7C15))).to_le_bytes()
}

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

/// The raw v1 artifacts: (legacy rows bytes, superblock bytes).
fn v1_artifacts() -> (Vec<u8>, Vec<u8>) {
    let mut rows = Vec::new();
    for i in 0..N {
        let row = records_v1::RecordsRow {
            id: if i % 2 == 0 { i } else { 2 * N - i },
            value: v1_value(i),
        };
        let mut slot = [0u8; V1_ROW_SIZE];
        records_v1::encode_records_row(&row, &mut slot);
        rows.extend_from_slice(&slot);
    }
    let generation = N + 1;
    let pair = ((generation % 2) * 2) as usize;
    let mut sb = vec![0u8; (pair + 2) * SB_COPY_SIZE];
    for slot in [pair, pair + 1] {
        sb[slot * SB_COPY_SIZE..(slot + 1) * SB_COPY_SIZE]
            .copy_from_slice(&forge_v1_sb(generation, N));
    }
    (rows, sb)
}

#[test]
fn real_files_migrate_and_match_the_simulator_byte_for_byte() {
    let (rows_v1, sb) = v1_artifacts();

    // Lay down the v1 directory as a v1 binary would have left it.
    let dir = scratch_dir("e2e");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(rows_file_name(V1_SCHEMA_HASH)), &rows_v1).expect("rows");
    std::fs::write(dir.join(SUPERBLOCK_FILE), &sb).expect("sb");

    // The gate refuses, naming the legacy schema.
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
    match host.open().expect("probe") {
        Output::OpenDone {
            result: Err(dabqlite_core::DbError::SchemaMismatch { file_schema, .. }),
        } => assert_eq!(file_schema, V1_SCHEMA_HASH),
        other => panic!("gate must refuse the v1 file: {other:?}"),
    }

    // Migrate on real files (fresh handles, as an app would restart).
    drop(host);
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("reopen"));
    match host.migrate().expect("probe") {
        Output::MigrateDone { result: Ok(rows) } => assert_eq!(rows, N),
        other => panic!("migration failed on real files: {other:?}"),
    }

    // The same binary now opens and serves everything.
    drop(host);
    let mut host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("reopen 2"));
    assert!(matches!(
        host.open().expect("probe"),
        Output::OpenDone { result: Ok(n) } if n == N
    ));
    for i in 0..N {
        let id = if i % 2 == 0 { i } else { 2 * N - i };
        let mut expect = [0u8; 16];
        expect[..V1_VALUE_LEN].copy_from_slice(&v1_value(i));
        assert!(matches!(
            host.get(id),
            Output::GetDone { result: Ok(Some(v)), .. } if v == expect
        ));
    }
    drop(host);

    // The legacy file: byte-identical, an inert orphan.
    assert_eq!(
        std::fs::read(dir.join(rows_file_name(V1_SCHEMA_HASH))).expect("legacy"),
        rows_v1,
        "migration modified the legacy file"
    );

    // Sim/real equivalence: the simulator migrating the same artifacts
    // must produce byte-identical superblock and rows files.
    let mut disk = SimDisk::new();
    disk.write(FileId::RowsOld, 0, &rows_v1);
    disk.fsync(FileId::RowsOld);
    disk.write(FileId::Superblock, 0, &sb);
    disk.fsync(FileId::Superblock);
    let mut sim = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        sim.run_migration(),
        Driven::Done(Output::MigrateDone { result: Ok(n) }) if n == N
    ));
    assert_eq!(
        sim.disk.contents(FileId::Rows),
        std::fs::read(dir.join(rows_file_name(SCHEMA_HASH))).expect("migrated rows"),
        "migrated rows diverged between simulator and disk"
    );
    assert_eq!(
        sim.disk.contents(FileId::Superblock),
        std::fs::read(dir.join(SUPERBLOCK_FILE)).expect("migrated sb"),
        "migrated superblock diverged between simulator and disk"
    );

    std::fs::remove_dir_all(&dir).ok();
}
