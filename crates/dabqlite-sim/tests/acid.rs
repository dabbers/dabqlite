//! ACID, verified property by property (docs/DESIGN.md §5 states the
//! position per-target rather than claiming uniform ACID; this suite pins
//! each claim with a named test). Much of the evidence lives in the fault
//! suites — this file consolidates the four letters and adds the checks
//! that only make sense stated *as* ACID:
//!
//! - **A**tomicity: the superblock generation flip is the sole commit
//!   point. No observer — point get or range scan — ever sees a partial
//!   write, and a crashed-then-retried insert applies exactly once.
//! - **C**onsistency: schema enforcement (PK uniqueness, fixed widths) and
//!   structural invariants hold at every observable state, including after
//!   arbitrary crash/retry churn.
//! - **I**solation: serializable by construction — single writer, all
//!   access serialized. Verified at EVERY intermediate I/O state of a
//!   commit, for every client operation.
//! - **D**urability: acknowledged implies durable, at every acknowledgment
//!   point, under crash-and-settle.

use std::collections::BTreeMap;

use dabqlite_core::generated::queries::{get_record, insert_record, list_records};
use dabqlite_core::{Capacities, DbError, Input, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 32 };

/// Full paged scan; panics on any disorder.
fn scan_all(host: &mut SimHost) -> Vec<(u64, [u8; VALUE_LEN])> {
    let mut out: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
    let mut cursor = 0u64;
    loop {
        let page = match host.run_input(Input::Range {
            lo: cursor,
            hi: u64::MAX,
        }) {
            Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
            other => panic!("scan failed: {other:?}"),
        };
        for &(k, v) in &page.items[..page.count as usize] {
            if let Some(&(pk, _)) = out.last() {
                assert!(k > pk, "scan out of order");
            }
            out.push((k, v));
        }
        match page.next {
            Some(n) => cursor = n,
            None => return out,
        }
    }
}

/// ATOMICITY: crash at every I/O boundary of a commit; after recovery both
/// observers (point get and range scan) see the in-flight write entirely
/// or not at all — never a fragment, never a disagreement between
/// observers.
#[test]
fn atomicity_no_observer_ever_sees_a_partial_write() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, 5);
        let total_io = {
            let mut h = SimHost::new(CAPS, SimDisk::new(), None);
            h.open();
            for &(id, value) in &ops {
                h.run(ClientOp::Insert { id, value });
            }
            h.io_count
        };
        for boundary in 0..total_io {
            let ctx = format!("seed={seed} boundary={boundary}");
            let mut h = SimHost::new(CAPS, SimDisk::new(), Some(boundary));
            let mut acked = BTreeMap::new();
            let mut in_flight = None;
            let mut crashed = matches!(h.open(), Driven::Crashed);
            if !crashed {
                for &(id, value) in &ops {
                    match h.run_input(insert_record(id, value)) {
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
            let mut d = std::mem::take(&mut h.disk);
            d.crash(&mut crash_rng(seed ^ 0xAC1D, boundary));
            let mut rec = SimHost::new(CAPS, d, None);
            let n = match rec.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] recovery: {other:?}"),
            };

            // Both observers, one truth.
            let scanned = scan_all(&mut rec);
            assert_eq!(scanned.len() as u64, n, "[{ctx}] scan vs count");
            if let Some((id, value)) = in_flight {
                let via_get = match rec.run_input(get_record(id)) {
                    Driven::Done(Output::GetDone { result: Ok(v), .. }) => v,
                    other => panic!("[{ctx}] {other:?}"),
                };
                let via_scan = scanned.iter().find(|&&(k, _)| k == id).map(|&(_, v)| v);
                assert_eq!(via_get, via_scan, "[{ctx}] observers disagree");
                match via_get {
                    Some(v) => assert_eq!(v, value, "[{ctx}] partial in-flight value"),
                    None => {}
                }
            }
            for (&id, &value) in &acked {
                assert_eq!(
                    scanned.iter().find(|&&(k, _)| k == id).map(|&(_, v)| v),
                    Some(value),
                    "[{ctx}] acked row missing from scan"
                );
            }
        }
    }
}

/// ATOMICITY (exactly-once): crash a commit at every boundary, recover,
/// retry the same insert, and prove via the range observer that the row
/// exists EXACTLY once — double-apply cannot hide behind a point lookup.
#[test]
fn crashed_then_retried_inserts_apply_exactly_once() {
    for boundary in 0..5u64 {
        for seed in 0..6u64 {
            let ctx = format!("seed={seed} boundary={boundary}");
            let mut h = SimHost::new(CAPS, SimDisk::new(), None);
            h.open();
            for id in 0..3u64 {
                h.run(ClientOp::Insert {
                    id,
                    value: [id as u8; VALUE_LEN],
                });
            }
            let target = 100u64;
            h.crash_after = Some(h.io_count + boundary);
            assert!(matches!(
                h.run_input(insert_record(target, [7; VALUE_LEN])),
                Driven::Crashed
            ));
            let mut d = std::mem::take(&mut h.disk);
            d.crash(&mut crash_rng(seed ^ 0x1CE, boundary));
            let mut rec = SimHost::new(CAPS, d, None);
            assert!(matches!(
                rec.open(),
                Driven::Done(Output::OpenDone { result: Ok(_) })
            ));

            // Retry until it lands (either it was committed — DuplicateId —
            // or it succeeds now).
            match rec.run_input(insert_record(target, [7; VALUE_LEN])) {
                Driven::Done(Output::InsertDone { result: Ok(()), .. })
                | Driven::Done(Output::InsertDone {
                    result: Err(DbError::DuplicateId { .. }),
                    ..
                }) => {}
                other => panic!("[{ctx}] retry: {other:?}"),
            }
            let hits: Vec<_> = scan_all(&mut rec)
                .into_iter()
                .filter(|&(k, _)| k == target)
                .collect();
            assert_eq!(
                hits,
                vec![(target, [7; VALUE_LEN])],
                "[{ctx}] retried insert must exist exactly once"
            );
        }
    }
}

/// ISOLATION: at EVERY intermediate I/O state of a commit (row write, rows
/// fsync, both superblock copy writes, superblock fsync), every client
/// operation is refused with Busy. No dirty reads, no phantom pages, no
/// interleaving — serializable because serial.
#[test]
fn isolation_every_operation_busy_at_every_commit_stage() {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    host.run(ClientOp::Insert {
        id: 1,
        value: [1; VALUE_LEN],
    });

    // Drive the next insert by hand, probing at every stage.
    let mut out = host.engine.tick(insert_record(2, [2; VALUE_LEN]));
    let mut stages = 0;
    loop {
        match out {
            Output::Write { file, offset, data } => {
                probe_all_busy(&mut host.engine, stages);
                host.disk.write(file, offset, data.as_slice());
                out = host.engine.tick(Input::WriteDone { file });
                stages += 1;
            }
            Output::Fsync { file } => {
                probe_all_busy(&mut host.engine, stages);
                host.disk.fsync(file);
                out = host.engine.tick(Input::FsyncDone { file });
                stages += 1;
            }
            Output::InsertDone { result: Ok(()), .. } => break,
            other => panic!("unexpected: {other:?}"),
        }
    }
    // The commit protocol is exactly 5 I/O stages; if it grows, isolation
    // coverage must grow with it — this assert makes that impossible to
    // forget.
    assert_eq!(
        stages, 5,
        "commit protocol changed; extend isolation probes"
    );

    // After commit, everything is visible and consistent again.
    assert!(matches!(
        host.engine.tick(get_record(2)),
        Output::GetDone { result: Ok(Some(v)), .. } if v == [2; VALUE_LEN]
    ));
}

fn probe_all_busy(engine: &mut dabqlite_core::Engine, stage: usize) {
    assert!(
        matches!(
            engine.tick(insert_record(99, [9; VALUE_LEN])),
            Output::InsertDone {
                result: Err(DbError::Busy),
                ..
            }
        ),
        "insert not Busy at commit stage {stage}"
    );
    assert!(
        matches!(
            engine.tick(get_record(1)),
            Output::GetDone {
                result: Err(DbError::Busy),
                ..
            }
        ),
        "get not Busy at commit stage {stage}"
    );
    assert!(
        matches!(
            engine.tick(list_records(0, u64::MAX)),
            Output::RangeDone {
                result: Err(DbError::Busy)
            }
        ),
        "range not Busy at commit stage {stage}"
    );
}

/// DURABILITY: after EVERY acknowledgment, a machine crash with adversarial
/// settle loses nothing acknowledged. (The fsync before the ack is the
/// point; this proves it at every single ack point of a workload.)
#[test]
fn durability_every_acknowledgment_survives_a_crash() {
    for seed in 0..6u64 {
        let ops = gen_workload(seed, 8);
        let mut h = SimHost::new(CAPS, SimDisk::new(), None);
        h.open();
        let mut acked = BTreeMap::new();
        for (i, &(id, value)) in ops.iter().enumerate() {
            match h.run_input(insert_record(id, value)) {
                Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                    acked.insert(id, value);
                }
                other => panic!("insert: {other:?}"),
            }
            // Crash RIGHT NOW, on a clone, and verify every ack so far.
            let mut d = h.disk.clone();
            d.crash(&mut crash_rng(seed ^ 0xD00D, i as u64));
            let mut rec = SimHost::new(CAPS, d, None);
            let n = match rec.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("seed={seed} ack={i}: {other:?}"),
            };
            assert_eq!(n, acked.len() as u64, "seed={seed} ack={i}: loss");
            for (&id, &value) in &acked {
                assert_eq!(
                    rec.get(id),
                    Some(value),
                    "seed={seed} ack={i}: acked id={id} not durable"
                );
            }
        }
    }
}

/// CONSISTENCY: after arbitrary crash/retry churn, the database satisfies
/// every structural invariant an observer can check: unique keys, strictly
/// ascending scans, exact values, count agreement between observers, and
/// PK-uniqueness enforcement still live.
#[test]
fn consistency_invariants_hold_after_churn() {
    for seed in 0..6u64 {
        let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
        let mut disk = SimDisk::new();
        // Churn: repeated partial workloads ended by crashes.
        for round in 0..6u64 {
            let mut h = SimHost::new(CAPS, disk, None);
            let opened = match h.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("seed={seed} round={round}: {other:?}"),
            };
            assert!(
                opened == oracle.len() as u64 || opened == oracle.len() as u64 + 1,
                "seed={seed} round={round}"
            );
            // Resolve possible in-flight commit from last round.
            if opened == oracle.len() as u64 + 1 {
                let scanned = scan_all(&mut h);
                for (k, v) in scanned {
                    oracle.entry(k).or_insert(v);
                }
            }
            let ops = gen_workload(seed ^ (round << 32), 4);
            let mut crashed = false;
            h.crash_after = Some(h.io_count + (round % 19) + 1);
            for &(id, value) in &ops {
                match h.run_input(insert_record(id, value)) {
                    Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                        oracle.insert(id, value);
                    }
                    Driven::Crashed => {
                        crashed = true;
                        break;
                    }
                    other => panic!("seed={seed} round={round}: {other:?}"),
                }
            }
            disk = std::mem::take(&mut h.disk);
            if crashed {
                disk.crash(&mut crash_rng(seed ^ 0xC0C0, round));
            }
        }

        // Final verification: every invariant, both observers.
        let mut h = SimHost::new(CAPS, disk, None);
        let n = match h.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
            other => panic!("seed={seed} final: {other:?}"),
        };
        let scanned = scan_all(&mut h); // asserts strict ascending => unique
        assert_eq!(scanned.len() as u64, n, "seed={seed}: observer count split");
        for &(k, v) in &scanned {
            assert_eq!(h.get(k), Some(v), "seed={seed}: observers disagree on {k}");
            match oracle.get(&k) {
                Some(&ov) => assert_eq!(v, ov, "seed={seed}: value drift on {k}"),
                None => panic!("seed={seed}: phantom key {k}"),
            }
            // PK uniqueness is still enforced on the live database.
            assert!(matches!(
                h.run_input(insert_record(k, [0xEE; VALUE_LEN])),
                Driven::Done(Output::InsertDone {
                    result: Err(DbError::DuplicateId { .. }),
                    ..
                })
            ));
            // ...and the duplicate attempt changed nothing.
            assert_eq!(
                h.get(k),
                Some(v),
                "seed={seed}: duplicate attempt mutated {k}"
            );
        }
    }
}
