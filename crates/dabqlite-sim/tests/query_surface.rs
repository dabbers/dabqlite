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

use dabqlite_core::generated::queries::{get_record, insert_record, list_records, OPERATIONS};
use dabqlite_core::{Capacities, DbError, FileId, Input, Output, RangePage, ROW_SIZE, VALUE_LEN};
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
    assert_eq!(OPERATIONS, &["get_record", "insert_record", "list_records"]);
    // …and each operation maps to exactly the engine input it claims.
    assert_eq!(
        insert_record(7, [3; VALUE_LEN]),
        Input::Insert {
            id: 7,
            value: [3; VALUE_LEN]
        }
    );
    assert_eq!(get_record(7), Input::Get { id: 7 });
    assert_eq!(list_records(3, 9), Input::Range { lo: 3, hi: 9 });
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

fn range_page(host: &mut SimHost, lo: u64, hi: u64) -> RangePage {
    match host.run_input(list_records(lo, hi)) {
        Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
        other => panic!("list_records({lo},{hi}) failed: {other:?}"),
    }
}

/// Full paged scan through the generated surface, asserting strict
/// ascending order across page boundaries.
fn scan_all(host: &mut SimHost, lo: u64, hi: u64) -> Vec<(u64, [u8; VALUE_LEN])> {
    let mut out = Vec::new();
    let mut cursor = lo;
    let mut pages = 0u64;
    loop {
        pages += 1;
        assert!(pages <= 1 << 20, "paging did not terminate");
        let page = range_page(host, cursor, hi);
        for &(k, v) in &page.items[..page.count as usize] {
            if let Some(&(pk, _)) = out.last() {
                assert!(k > pk, "scan not strictly ascending: {pk} then {k}");
            }
            assert!(k >= lo && k <= hi, "key {k} outside [{lo},{hi}]");
            out.push((k, v));
        }
        match page.next {
            Some(n) => cursor = n,
            None => return out,
        }
    }
}

#[test]
fn multi_row_result_matrix_against_oracle() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, 40);
        let caps = Capacities { rows: 64 };
        let mut host = SimHost::new(caps, SimDisk::new(), None);
        host.open();
        let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
        for &(id, value) in &ops {
            insert_ok(&mut host, id, value);
            oracle.insert(id, value);
        }

        // Full-table scan == oracle, in order.
        let want: Vec<_> = oracle.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(scan_all(&mut host, 0, u64::MAX), want, "seed={seed}");

        // Empty results: inverted bounds and vacant ranges.
        assert_eq!(scan_all(&mut host, 10, 5), vec![], "seed={seed} inverted");
        let page = range_page(&mut host, 10, 5);
        assert_eq!((page.count, page.next), (0, None));

        // Narrow results: singleton ranges on and off keys.
        for (&k, &v) in oracle.iter().take(10) {
            assert_eq!(scan_all(&mut host, k, k), vec![(k, v)], "seed={seed}");
        }

        // Arbitrary sub-ranges vs oracle, including bounds landing between
        // keys and at the extremes.
        let keys: Vec<u64> = oracle.keys().copied().collect();
        for i in (0..keys.len()).step_by(5) {
            for j in (i..keys.len()).step_by(7) {
                let (lo, hi) = (keys[i].saturating_sub(1), keys[j].saturating_add(1));
                let want: Vec<_> = oracle.range(lo..=hi).map(|(&k, &v)| (k, v)).collect();
                assert_eq!(scan_all(&mut host, lo, hi), want, "seed={seed} [{lo},{hi}]");
            }
        }
    }
}

#[test]
fn large_paged_scan_with_interruptions_between_pages() {
    const N: u64 = 4096;
    let caps = Capacities { rows: N };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for id in 0..N {
        insert_ok(&mut host, id, [(id % 251) as u8; VALUE_LEN]);
    }

    // Large result: the full 4096-row scan is 512 pages, exact and ordered.
    let all = scan_all(&mut host, 0, u64::MAX);
    assert_eq!(all.len(), N as usize);
    assert!(all.iter().enumerate().all(|(i, &(k, _))| k == i as u64));

    // Page-boundary edges: ranges sized exactly at, one under, and one
    // over the page size, at the start, middle, and end of the table.
    for base in [0u64, N / 2, N - 9] {
        for width in [7u64, 8, 9] {
            let got = scan_all(&mut host, base, base + width - 1);
            assert_eq!(
                got.len() as u64,
                width.min(N - base),
                "base={base} width={width}"
            );
        }
    }

    // PROCESS RESTART halfway through a paged result: fetch half the
    // pages, restart (recover), continue from the cursor — the remainder
    // must be exactly the missing rows. The cursor is a plain key, so it
    // survives restarts by construction.
    let mut first_half = Vec::new();
    let mut cursor = 0u64;
    for _ in 0..256 {
        let page = range_page(&mut host, cursor, u64::MAX);
        first_half.extend_from_slice(&page.items[..page.count as usize]);
        cursor = page.next.expect("mid-table page must have a continuation");
    }
    let disk = std::mem::take(&mut host.disk);
    let mut restarted = SimHost::new(caps, disk, None);
    assert!(matches!(
        restarted.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == N
    ));
    let second_half = scan_all(&mut restarted, cursor, u64::MAX);
    let mut combined = first_half;
    combined.extend(second_half);
    assert_eq!(
        combined.len(),
        N as usize,
        "restart mid-scan lost or duplicated rows"
    );
    assert!(combined
        .iter()
        .enumerate()
        .all(|(i, &(k, _))| k == i as u64));

    // WRITES BETWEEN PAGES: a row inserted ahead of the cursor appears in
    // the remainder; the scan is over live committed state per page.
    let caps2 = Capacities { rows: 32 };
    let mut h2 = SimHost::new(caps2, SimDisk::new(), None);
    h2.open();
    for id in (0..20u64).map(|i| i * 2) {
        insert_ok(&mut h2, id, [1; VALUE_LEN]);
    }
    let page = range_page(&mut h2, 0, u64::MAX);
    let cursor = page.next.expect("more pages");
    insert_ok(&mut h2, cursor + 1, [7; VALUE_LEN]); // lands ahead of cursor
    let rest = scan_all(&mut h2, cursor, u64::MAX);
    assert!(
        rest.iter()
            .any(|&(k, v)| k == cursor + 1 && v == [7; VALUE_LEN]),
        "row committed ahead of the cursor must appear in the continuation"
    );

    // BUSY mid-insert: range during in-flight insert I/O is refused, not
    // interleaved (v1 serializes everything).
    let mut h3 = SimHost::new(caps2, SimDisk::new(), None);
    h3.open();
    let out = h3.engine.tick(insert_record(1, [1; VALUE_LEN]));
    assert!(matches!(out, Output::Write { .. }));
    assert!(matches!(
        h3.engine.tick(list_records(0, 10)),
        Output::RangeDone {
            result: Err(DbError::Busy)
        }
    ));
}
