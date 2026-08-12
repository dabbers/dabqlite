//! Read-path faults, per TigerBeetle's fault model: disks misdirect *reads*
//! as well as writes, and data can corrupt in flight (bus/DMA/cache) even
//! when the platter is fine.
//!
//! The engine reads only at open, so injecting at reopen covers the entire
//! read path. Two fault kinds:
//!
//! - **Transient corruption**: a bit flips in the returned buffer; the disk
//!   is untouched. Swept over EVERY byte of both recovery reads.
//! - **Misdirected reads**: the buffer contains *valid data from the wrong
//!   offset* — the hardest case, since checksums pass. Position identity
//!   (a superblock copy is only trusted in its own pair slot) and structural
//!   checks are what stand between this and silent corruption.

use dabqlite_core::{Capacities, DbError, Output, ROW_SIZE, SB_ZONE_SIZE, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 16 };
const INSERTS: usize = 8;

/// Recovery I/O layout: index 0 = superblock read, 1 = rows read.
const READ_SB: u64 = 0;
const READ_ROWS: u64 = 1;

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

#[test]
fn transient_corrupt_superblock_read_every_byte_is_survivable() {
    // The 256-byte superblock read contains all four copies; flipping any
    // single byte damages at most one copy, and the live generation's twin
    // is in the same buffer. Zero loss, every byte, every time.
    for seed in 0..4u64 {
        let (disk, ops) = build_db(seed);
        for byte in 0..SB_ZONE_SIZE {
            for mask in [0x01u8, 0x80] {
                let ctx = format!("seed={seed} byte={byte} mask={mask:#x}");
                let mut host = SimHost::new(CAPS, disk.clone(), None);
                host.read_corrupt_at = Some((READ_SB, byte, mask));
                match host.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        assert_eq!(n, INSERTS as u64, "[{ctx}] lost rows");
                    }
                    other => panic!("[{ctx}] open failed: {other:?}"),
                }
                assert_eq!(host.reads_corrupted, 1, "[{ctx}] fault did not fire");
                for &(id, value) in &ops {
                    assert_eq!(host.get(id), Some(value), "[{ctx}] id={id} wrong");
                }
            }
        }
    }
}

#[test]
fn transient_corrupt_rows_read_every_byte_is_detected() {
    // A flipped byte in the rows read must fail the open loudly. (A retry
    // would succeed — the disk is clean — but the engine never serves data
    // it cannot verify. Detection over availability.)
    for seed in 0..4u64 {
        let (disk, _ops) = build_db(seed);
        for byte in 0..INSERTS * ROW_SIZE {
            for mask in [0x01u8, 0x80] {
                let ctx = format!("seed={seed} byte={byte} mask={mask:#x}");
                let mut host = SimHost::new(CAPS, disk.clone(), None);
                host.read_corrupt_at = Some((READ_ROWS, byte, mask));
                match host.open() {
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. }),
                    }) => {}
                    other => panic!("[{ctx}] in-flight corruption not detected: {other:?}"),
                }
                assert_eq!(host.reads_corrupted, 1, "[{ctx}] fault did not fire");
            }
        }
    }
}

#[test]
fn misdirected_reads_are_never_silently_wrong() {
    // Both recovery reads start at offset 0, so only positive shifts can
    // land (the host skips shifts that would go below zero — nothing is
    // there to read). The returned bytes are VALID data from the wrong
    // offset: checksums pass; only positional validation can object.
    let shifts: &[i64] = &[8, 32, 64, 128, 192, 256, 4096];
    let mut full_recoveries = 0u64;
    let mut detections = 0u64;

    for seed in 0..6u64 {
        let (disk, ops) = build_db(seed);
        for &read_idx in &[READ_SB, READ_ROWS] {
            for &shift in shifts {
                let ctx = format!("seed={seed} read_idx={read_idx} shift={shift}");
                let mut host = SimHost::new(CAPS, disk.clone(), None);
                host.read_misdirect_at = Some((read_idx, shift));
                match host.open() {
                    Driven::Done(Output::OpenDone { result: Ok(n) }) => {
                        assert!(n <= INSERTS as u64, "[{ctx}] impossible count {n}");
                        if n == INSERTS as u64 {
                            full_recoveries += 1;
                        }
                        // Served rows must be exactly correct; honest
                        // absence is permitted, wrong bytes never.
                        for &(id, value) in &ops {
                            match host.get(id) {
                                None => {}
                                Some(got) => {
                                    assert_eq!(got, value, "[{ctx}] id={id} wrong bytes")
                                }
                            }
                        }
                        for probe in [u64::MAX, u64::MAX - 1] {
                            if !ops.iter().any(|&(id, _)| id == probe) {
                                assert_eq!(host.get(probe), None, "[{ctx}] phantom row");
                            }
                        }
                    }
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::Corrupt { .. }),
                    }) => detections += 1,
                    other => panic!("[{ctx}] unexpected outcome: {other:?}"),
                }
                // The read at index 0/1 always exists and offset 0 + positive
                // shift always fires.
                assert_eq!(host.reads_misdirected, 1, "[{ctx}] fault did not fire");
            }
        }
    }

    // Both outcome classes must occur, or half the space went untested.
    // (A shift of one superblock slot leaves the live pair's copies at
    // positions that still validate — survivable; large shifts destroy
    // everything — detected.)
    assert!(
        full_recoveries > 0,
        "no survivable misdirected read observed"
    );
    assert!(detections > 0, "no detected misdirected read observed");
}
