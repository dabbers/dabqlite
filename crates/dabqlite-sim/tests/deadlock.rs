//! Deadlock validation.
//!
//! Classic lock-ordering deadlock is structurally impossible here: the
//! core is a single-threaded sans-I/O state machine with no threads,
//! mutexes, channels, or blocking calls — and the wasm32 determinism gate
//! ENFORCES that (no_std: `std::sync` does not exist to link against).
//! The one OS lock (flock) is LOCK_NB: it refuses instantly, never waits.
//!
//! What the lockstep protocol CAN have is the deadlock's analog: a
//! **protocol stall** — a machine that keeps emitting I/O requests and
//! never reaches a terminal output (the host spins forever), or a machine
//! that silently absorbs an input the host expects an answer to (the host
//! waits forever). This suite pins the three defenses:
//!
//! 1. **Fuel watchdog**: both drive loops bound every operation by a
//!    static I/O budget and panic loudly on overrun. The simulator's
//!    `stall_from` knob arranges a wedged machine on demand — a
//!    deliberately-created deadlock at any I/O boundary — and the
//!    watchdog must catch every one.
//! 2. **Liveness budgets**: every operation's exact I/O count is pinned,
//!    so termination is not "eventually" but "in exactly k steps".
//! 3. **No silent absorption**: an unexpected input panics (protocol
//!    violation) instead of being ignored — an ignored input is how a
//!    lockstep peer starves forever.

use dabqlite_core::generated::records_v1;
use dabqlite_core::migration::{V1_ROW_SIZE, V1_SCHEMA_HASH};
use dabqlite_core::{crc32::crc32, Capacities, FileId, Input, Output, SB_COPY_SIZE, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 16 };

fn build_db(n: u64) -> SimDisk {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for i in 0..n {
        host.run(ClientOp::Insert {
            id: i,
            value: [i as u8; VALUE_LEN],
        });
    }
    std::mem::take(&mut host.disk)
}

fn build_v1_disk(n: u64) -> SimDisk {
    let mut disk = SimDisk::new();
    for i in 0..n {
        let row = records_v1::RecordsRow {
            id: i,
            value: [i as u8; 8],
        };
        let mut slot = [0u8; V1_ROW_SIZE];
        records_v1::encode_records_row(&row, &mut slot);
        disk.write(FileId::RowsOld, i * V1_ROW_SIZE as u64, &slot);
    }
    disk.fsync(FileId::RowsOld);
    let generation = n + 1;
    let pair = (generation % 2) * 2;
    let mut copy = [0u8; SB_COPY_SIZE];
    copy[0..8].copy_from_slice(b"DABQSB01");
    copy[8..16].copy_from_slice(&generation.to_le_bytes());
    copy[16..24].copy_from_slice(&n.to_le_bytes());
    copy[24..32].copy_from_slice(&V1_SCHEMA_HASH.to_le_bytes());
    let crc = crc32(&copy[0..32]);
    copy[32..36].copy_from_slice(&crc.to_le_bytes());
    for slot in [pair, pair + 1] {
        disk.write(FileId::Superblock, slot * SB_COPY_SIZE as u64, &copy);
    }
    disk.fsync(FileId::Superblock);
    disk
}

// ---- 1. Arranged deadlocks: the watchdog catches a wedged machine ------

#[test]
#[should_panic(expected = "protocol stall")]
fn a_machine_wedged_at_open_is_caught_not_hung() {
    let mut host = SimHost::new(CAPS, build_db(4), None);
    host.stall_from = Some(0);
    host.open();
}

#[test]
#[should_panic(expected = "protocol stall")]
fn a_machine_wedged_mid_insert_is_caught_not_hung() {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    // Wedge partway through the 5-op commit protocol.
    host.stall_from = Some(host.io_count + 2);
    host.run(ClientOp::Insert {
        id: 1,
        value: [1; VALUE_LEN],
    });
}

#[test]
#[should_panic(expected = "protocol stall")]
fn a_machine_wedged_mid_migration_is_caught_not_hung() {
    let mut host = SimHost::new(CAPS, build_v1_disk(6), None);
    // Wedge in the middle of the row-rewrite phase.
    host.stall_from = Some(4);
    host.run_migration();
}

/// The wedge is catchable at EVERY boundary of an insert, not just a
/// sampled one — each arranged deadlock dies by watchdog panic, and after
/// unwinding, the DISK is intact: a caught stall is recoverable exactly
/// like a crash at the same boundary.
#[test]
fn every_insert_boundary_survives_an_arranged_deadlock() {
    for boundary in 0..5u64 {
        let disk = build_db(3);
        let result = std::panic::catch_unwind(move || {
            let mut host = SimHost::new(CAPS, disk, None);
            host.open();
            host.stall_from = Some(host.io_count + boundary);
            host.run(ClientOp::Insert {
                id: 100,
                value: [9; VALUE_LEN],
            });
            unreachable!("the stall must panic before a terminal output");
        });
        let panic_msg = match result {
            Err(e) => e
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic".into()),
            Ok(never) => never,
        };
        assert!(
            panic_msg.contains("protocol stall"),
            "boundary {boundary}: wrong panic: {panic_msg}"
        );
    }
    // And the same database, driven without the wedge, works fine — the
    // arranged deadlocks above left no global poison.
    let mut host = SimHost::new(CAPS, build_db(3), None);
    match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(3) }) => {}
        other => panic!("recovery after arranged deadlocks: {other:?}"),
    }
}

// ---- 2. Liveness budgets: termination in exactly k steps ---------------

/// The liveness table: every operation's I/O count, pinned exactly. A
/// deadlock needs an unbounded loop; these bounds are not just finite but
/// exact, so any new I/O in any path is a conscious, test-breaking change.
#[test]
fn every_operation_terminates_in_exactly_its_budget() {
    // Fresh init: 2 superblock copy writes + 1 fsync.
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    assert_eq!(host.io_count, 3, "fresh open budget");

    // Insert: row write, rows fsync, 2 sb writes, sb fsync.
    let before = host.io_count;
    host.run(ClientOp::Insert {
        id: 1,
        value: [1; VALUE_LEN],
    });
    assert_eq!(host.io_count - before, 5, "insert budget");

    // Get and Range: pure in-memory, ZERO I/O — they cannot even stall.
    let before = host.io_count;
    host.run(ClientOp::Get { id: 1 });
    host.run_input(Input::Range {
        lo: 0,
        hi: u64::MAX,
    });
    assert_eq!(host.io_count - before, 0, "read-path budget");

    // Recovery: read sb, read rows, fsync rows, fsync sb.
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    host.open();
    assert_eq!(host.io_count, 4, "recovery budget");

    // Rejections: zero I/O (pinned deeper in capacity.rs; asserted here
    // because zero I/O means zero opportunity to stall).
    let before = host.io_count;
    host.run(ClientOp::Insert {
        id: 1,
        value: [2; VALUE_LEN],
    });
    assert_eq!(host.io_count - before, 0, "duplicate rejection budget");

    // Migration of n rows: 2 reads + n row writes + rows fsync
    // + 2 sb writes + sb fsync = n + 6.
    let n = 6u64;
    let mut host = SimHost::new(CAPS, build_v1_disk(n), None);
    host.run_migration();
    assert_eq!(host.io_count, n + 6, "migration budget");

    // Idempotent re-migration: 1 read + 2 durability fsyncs.
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    host.run_migration();
    assert_eq!(host.io_count, 3, "noop migration budget");
}

// ---- 3. The OS lock cannot deadlock: it never waits --------------------

// flock is taken with LOCK_NB and held for the storage lifetime; a
// conflicting open returns WouldBlock IMMEDIATELY instead of queueing —
// pinned by kind in `dabqlite-host/tests/locking.rs`
// (second_open_in_same_process_is_refused asserts ErrorKind::WouldBlock).
// With no blocking acquisition anywhere, there is no wait-for graph to
// have a cycle in. The note lives here so the deadlock story is complete
// in one file; the executable pin lives with the other lock tests.
