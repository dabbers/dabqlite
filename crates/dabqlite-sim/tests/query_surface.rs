//! The compiled query surface under stress. Plans are fixed at build time
//! (no runtime planner, by design — docs/DESIGN.md §4.3), so what must be
//! proven at runtime is the full RESULT matrix of the compiled operations,
//! through the generated functions only:
//!
//! - **Empty results**: get on absent keys, at every lifecycle stage.
//! - **Present results**: get on every inserted key, exact bytes.
//! - **Error results**: DuplicateId, Full, NotOpen, fail-stop IoFailed.
//! - **Large results with faults halfway through**: the one multi-row
//!   result path in v1 is the full-table read at recovery. It is stressed
//!   at thousands of rows with corruption, EIO, and crashes landing in the
//!   MIDDLE of the result — the "error halfway through" case.
//! - **Equivalence**: the generated surface is byte-identical to raw
//!   engine inputs — same disks, same outputs, always.

use std::collections::BTreeMap;

use dabqlite_core::generated::queries::{get_record, insert_record, OPERATIONS};
use dabqlite_core::{Capacities, DbError, FileId, Input, Output, ROW_SIZE, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

fn insert_ok(host: &mut SimHost, id: u64, value: [u8; VALUE_LEN]) {
    match host.run_input(insert_record(id, value)) {
        Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {}
        other => panic!("insert_record({id}) failed: {other:?}"),
    }
}

fn get_result(host: &mut SimHost, id: u64) -> Result<Option<[u8; VALUE_LEN]>, DbError> {
    match host.run_input(get_record(id)) {
        Driven::Done(Output::GetDone { result, .. }) => result,
        other => panic!("get_record({id}) did not complete: {other:?}"),
    }
}

#[test]
fn operation_space_is_closed_and_maps_to_engine_inputs() {
    // The manifest is the whole surface…
    assert_eq!(OPERATIONS, &["get_record", "insert_record"]);
    // …and each operation maps to exactly the engine input it claims.
    assert_eq!(
        insert_record(7, [3; VALUE_LEN]),
        Input::Insert {
            id: 7,
            value: [3; VALUE_LEN]
        }
    );
    assert_eq!(get_record(7), Input::Get { id: 7 });
}

#[test]
fn generated_surface_is_byte_identical_to_raw_inputs() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, 10);
        let caps = Capacities { rows: 32 };

        let mut via_generated = SimHost::new(caps, SimDisk::new(), None);
        via_generated.open();
        let mut via_raw = SimHost::new(caps, SimDisk::new(), None);
        via_raw.open();

        for &(id, value) in &ops {
            let a = via_generated.run_input(insert_record(id, value));
            let b = via_raw.run(ClientOp::Insert { id, value });
            assert_eq!(a, b, "seed={seed}: outputs diverged");
            let a = via_generated.run_input(get_record(id));
            let b = via_raw.run(ClientOp::Get { id });
            assert_eq!(a, b, "seed={seed}: get outputs diverged");
        }
        for file in [FileId::Superblock, FileId::Rows] {
            assert_eq!(
                via_generated.disk.contents(file),
                via_raw.disk.contents(file),
                "seed={seed}: {file:?} bytes diverged between surfaces"
            );
        }
    }
}

#[test]
fn result_matrix_empty_present_and_error_results() {
    let caps = Capacities { rows: 4 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);

    // NotOpen: querying before open is an error result, not a panic.
    assert_eq!(get_result(&mut host, 1), Err(DbError::NotOpen));
    host.open();

    // Empty result on a fresh database.
    assert_eq!(get_result(&mut host, 1), Ok(None));

    // Present results: exact bytes back for every key, at every fill level.
    for id in 0..4u64 {
        insert_ok(&mut host, id, [id as u8; VALUE_LEN]);
        for probe in 0..=id {
            assert_eq!(
                get_result(&mut host, probe),
                Ok(Some([probe as u8; VALUE_LEN]))
            );
        }
        // Empty results interleaved: keys not yet inserted stay absent.
        assert_eq!(get_result(&mut host, id + 1), Ok(None));
    }

    // Error result: duplicate key, database unharmed.
    assert!(matches!(
        host.run_input(insert_record(2, [9; VALUE_LEN])),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::DuplicateId { id: 2 }),
            ..
        })
    ));
    assert_eq!(get_result(&mut host, 2), Ok(Some([2; VALUE_LEN])));

    // Error result: capacity, database unharmed and still readable.
    assert!(matches!(
        host.run_input(insert_record(99, [9; VALUE_LEN])),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::Full { capacity: 4, .. }),
            ..
        })
    ));
    assert_eq!(get_result(&mut host, 99), Ok(None));

    // Error result: fail-stop. After an EIO the whole surface returns
    // IoFailed until restart — queries never limp.
    let mut failing = SimHost::new(Capacities { rows: 8 }, SimDisk::new(), None);
    failing.open();
    failing.fail_after = Some(failing.io_count); // very next I/O op fails
    assert!(matches!(
        failing.run_input(insert_record(1, [1; VALUE_LEN])),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::IoFailed { .. }),
            ..
        })
    ));
    assert!(matches!(
        get_result(&mut failing, 1),
        Err(DbError::IoFailed { .. })
    ));
}

/// Large results, and faults landing in the middle of them. The full-table
/// read at recovery is v1's one multi-row result path: at 4096 rows it is a
/// 128 KiB result, and every fault class is aimed at its midsection.
#[test]
fn large_result_faults_halfway_through() {
    const N: u64 = 4096;
    let caps = Capacities { rows: N };

    // Build the large table through the generated surface.
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for id in 0..N {
        insert_ok(&mut host, id, [(id % 251) as u8; VALUE_LEN]);
    }
    let disk = std::mem::take(&mut host.disk);
    let verify_all = |host: &mut SimHost| {
        for id in (0..N).step_by(97) {
            assert_eq!(
                get_result(host, id),
                Ok(Some([(id % 251) as u8; VALUE_LEN]))
            );
        }
        assert_eq!(get_result(host, N + 1), Ok(None));
    };

    // Clean large-result recovery: all 4096 rows, exact.
    let mut clean = SimHost::new(caps, disk.clone(), None);
    assert!(matches!(
        clean.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == N
    ));
    verify_all(&mut clean);

    // Corruption halfway through the result: byte flips at 1/4, 1/2, 3/4
    // of the big read — detected, never a partial or wrong result.
    let total = N * ROW_SIZE as u64;
    for mid in [total / 4, total / 2, total * 3 / 4] {
        let mut damaged = disk.clone();
        damaged.corrupt(FileId::Rows, mid, 0x10);
        let mut h = SimHost::new(caps, damaged, None);
        assert!(
            matches!(
                h.open(),
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::Corrupt { .. })
                })
            ),
            "corruption at byte {mid} of the large result not detected"
        );
    }

    // Transient in-flight corruption in the middle of the big read: same
    // detection guarantee (read index 1 is the rows read).
    let mut h = SimHost::new(caps, disk.clone(), None);
    h.read_corrupt_at = Some((1, (N as usize * ROW_SIZE) / 2, 0x40));
    assert!(matches!(
        h.open(),
        Driven::Done(Output::OpenDone {
            result: Err(DbError::Corrupt { .. })
        })
    ));

    // EIO halfway through the result: fail-stop, then a clean retry sees
    // every row (the failure left nothing behind — reads are pure).
    let mut h = SimHost::new(caps, disk.clone(), None);
    h.fail_after = Some(1); // the rows read
    assert!(matches!(
        h.open(),
        Driven::Done(Output::OpenDone {
            result: Err(DbError::IoFailed { file: FileId::Rows })
        })
    ));
    let retry_disk = std::mem::take(&mut h.disk);
    let mut retry = SimHost::new(caps, retry_disk, None);
    assert!(matches!(
        retry.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == N
    ));
    verify_all(&mut retry);

    // Crashes at every boundary of the large-result recovery (there are
    // only 4: sb read, rows read, 2 recovery fsyncs — pinned here so a
    // protocol change is noticed), then settle and recover: idempotent,
    // zero loss, at scale.
    for boundary in 0..4u64 {
        let mut h = SimHost::new(caps, disk.clone(), Some(boundary));
        assert!(matches!(h.open(), Driven::Crashed), "boundary {boundary}");
        let mut d = std::mem::take(&mut h.disk);
        d.crash(&mut crash_rng(0xB16, boundary));
        let mut recovered = SimHost::new(caps, d, None);
        assert!(
            matches!(
                recovered.open(),
                Driven::Done(Output::OpenDone { result: Ok(n) }) if n == N
            ),
            "crash at boundary {boundary} of large-result recovery lost rows"
        );
        verify_all(&mut recovered);
    }
}

/// The generated surface under the crash schedule: every I/O boundary of a
/// workload driven purely through compiled operations, N/N+1 recovery.
#[test]
fn crash_sweep_through_generated_surface_only() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, 6);
        let caps = Capacities { rows: 16 };
        let total_io = {
            let mut host = SimHost::new(caps, SimDisk::new(), None);
            host.open();
            for &(id, value) in &ops {
                insert_ok(&mut host, id, value);
            }
            host.io_count
        };
        for boundary in 0..total_io {
            let ctx = format!("seed={seed} boundary={boundary}");
            let mut host = SimHost::new(caps, SimDisk::new(), Some(boundary));
            let mut acked: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
            let mut in_flight = None;
            let mut crashed = matches!(host.open(), Driven::Crashed);
            if !crashed {
                for &(id, value) in &ops {
                    match host.run_input(insert_record(id, value)) {
                        Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                            acked.insert(id, value);
                        }
                        Driven::Crashed => {
                            in_flight = Some((id, value));
                            crashed = true;
                            break;
                        }
                        other => panic!("[{ctx}] {other:?}"),
                    }
                }
            }
            assert!(crashed);
            let mut d = std::mem::take(&mut host.disk);
            d.crash(&mut crash_rng(seed ^ 0x9E37, boundary));
            let mut rec = SimHost::new(caps, d, None);
            let n = match rec.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] recovery failed: {other:?}"),
            };
            assert!(
                n == acked.len() as u64 || n == acked.len() as u64 + 1,
                "[{ctx}] {n} recovered, {} acked",
                acked.len()
            );
            for (&id, &value) in &acked {
                assert_eq!(get_result(&mut rec, id), Ok(Some(value)), "[{ctx}]");
            }
            if n == acked.len() as u64 + 1 {
                let (id, value) = in_flight.expect("N+1 without in-flight");
                assert_eq!(get_result(&mut rec, id), Ok(Some(value)), "[{ctx}]");
            }
        }
    }
}
