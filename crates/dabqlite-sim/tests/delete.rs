//! Deletion, held to the same standard as every other commit.
//!
//! A delete is not a special case in this engine: it APPENDS a tombstone
//! and flips the superblock, which is exactly what an insert does. That is
//! the whole design argument — the rows file stays append-only, so a crash
//! mid-delete resolves all-or-nothing by the same mechanism that makes a
//! crash mid-insert safe, and the file's order IS the commit order, so
//! replaying it replays history (insert, delete, insert again — the last
//! word wins because it is last).
//!
//! What this suite proves:
//! - a delete is durable, and survives restart;
//! - crashing or failing I/O at EVERY boundary of a delete leaves the row
//!   either fully present or fully gone, never in between, and never
//!   damages a neighbour;
//! - an id can be reused after deletion, and the replay order is exact;
//! - deleting something absent is refused with ZERO I/O;
//! - every read path (get, range, find) stops returning a deleted row at
//!   the same instant, checked against an oracle;
//! - a long interleaved workload matches a BTreeMap exactly, across
//!   crashes.

use std::collections::BTreeMap;

use dabqlite_core::{Capacities, DbError, FileId, Input, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 64 };

fn build(seed: u64, n: usize) -> (SimDisk, Vec<(u64, [u8; VALUE_LEN])>) {
    let ops = gen_workload(seed, n);
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for &(id, value) in &ops {
        host.run(ClientOp::Insert { id, value });
    }
    (std::mem::take(&mut host.disk), ops)
}

fn delete(host: &mut SimHost, id: u64) -> Result<(), DbError> {
    match host.run(ClientOp::Delete { id }) {
        Driven::Done(Output::DeleteDone { result, .. }) => result,
        other => panic!("delete({id}): {other:?}"),
    }
}

fn open(disk: SimDisk) -> (SimHost, u64) {
    let mut host = SimHost::new(CAPS, disk, None);
    let n = match host.open() {
        Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
        other => panic!("open: {other:?}"),
    };
    (host, n)
}

fn open_strict(disk: SimDisk) -> (SimHost, Result<u64, DbError>) {
    let mut host = SimHost::new(CAPS, disk, None);
    let r = match host.open() {
        Driven::Done(Output::OpenDone { result }) => result,
        other => panic!("open: {other:?}"),
    };
    (host, r)
}

fn open_salvage(disk: SimDisk) -> (SimHost, u64) {
    let mut host = SimHost::new(CAPS, disk, None);
    let n = match host.open_salvage() {
        Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
        other => panic!("salvage open: {other:?}"),
    };
    (host, n)
}

fn get_result(host: &mut SimHost, id: u64) -> Result<Option<[u8; VALUE_LEN]>, DbError> {
    match host.run_input(dabqlite_core::Input::Get { id }) {
        // These suites write full-width values, so one window is whole.
        Driven::Done(Output::GetDone { result, .. }) => result.map(|v| v.map(|w| w.bytes)),
        other => panic!("get: {other:?}"),
    }
}

#[test]
fn a_deleted_row_is_gone_and_stays_gone_across_restarts() {
    for seed in 0..6u64 {
        let (disk, ops) = build(seed, 8);
        let (mut host, n) = open(disk);
        assert_eq!(n, ops.len() as u64);

        let (victim, victim_value) = ops[3];
        assert_eq!(host.get(victim), Some(victim_value));
        assert_eq!(delete(&mut host, victim), Ok(()));
        assert_eq!(host.get(victim), None, "seed={seed}: still readable");
        assert_eq!(host.engine.live_count(), ops.len() as u64 - 1);

        // Restart: the deletion is durable, and nothing else moved.
        let disk = std::mem::take(&mut host.disk);
        let (mut host, n) = open(disk);
        assert_eq!(n, ops.len() as u64 - 1, "seed={seed}");
        assert_eq!(host.get(victim), None, "seed={seed}: resurrected");
        for &(id, value) in &ops {
            if id == victim {
                continue;
            }
            assert_eq!(host.get(id), Some(value), "seed={seed} id={id}");
        }
    }
}

/// The property that makes deletion safe: crash anywhere inside it and the
/// row is either entirely present or entirely gone.
#[test]
fn a_crash_at_every_boundary_of_a_delete_is_all_or_nothing() {
    for seed in 0..4u64 {
        let (base, ops) = build(seed, 6);
        let (victim, victim_value) = ops[2];

        for boundary in 0..6u64 {
            for settle in 0..3u64 {
                let ctx = format!("seed={seed} boundary={boundary} settle={settle}");
                let mut host = SimHost::new(CAPS, base.clone(), None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                let _ = host.run(ClientOp::Delete { id: victim });

                let mut disk = std::mem::take(&mut host.disk);
                let mut rng = crash_rng(0xDE1E7E, settle);
                disk.crash(&mut rng);

                let (mut host, n) = open(disk);
                let deleted = match host.get(victim) {
                    None => true,
                    Some(v) => {
                        assert_eq!(v, victim_value, "[{ctx}] value changed under a delete");
                        false
                    }
                };
                assert_eq!(
                    n,
                    ops.len() as u64 - u64::from(deleted),
                    "[{ctx}] live count disagrees with what is readable"
                );
                // Every other row is untouched either way.
                for &(id, value) in &ops {
                    if id == victim {
                        continue;
                    }
                    assert_eq!(host.get(id), Some(value), "[{ctx}] neighbour id={id}");
                }
                // And the database is still writable afterwards.
                assert!(matches!(
                    host.run(ClientOp::Insert {
                        id: 900_001,
                        value: [1; VALUE_LEN]
                    }),
                    Driven::Done(Output::InsertDone { result: Ok(()), .. })
                ));
            }
        }
    }
}

/// I/O failure at every boundary of a delete: fail-stop, then the restart
/// resolves all-or-nothing, exactly like the crash case.
#[test]
fn an_io_failure_at_every_boundary_of_a_delete_fail_stops_cleanly() {
    let (base, ops) = build(9, 6);
    let (victim, victim_value) = ops[4];

    for fail_at in 0..6u64 {
        let ctx = format!("fail_at={fail_at}");
        let mut host = SimHost::new(CAPS, base.clone(), None);
        host.open();
        host.fail_after = Some(host.io_count + fail_at);
        match host.run(ClientOp::Delete { id: victim }) {
            Driven::Done(Output::DeleteDone {
                result: Err(DbError::IoFailed { .. }),
                ..
            }) => {}
            Driven::Done(Output::DeleteDone { result: Ok(()), .. }) => {}
            other => panic!("[{ctx}] delete must fail-stop or commit: {other:?}"),
        }
        // After a fail-stop everything is refused; if the delete slipped
        // through before the injected failure, reads still work.
        match host.run(ClientOp::Get { id: ops[0].0 }) {
            Driven::Done(Output::GetDone {
                result: Err(DbError::IoFailed { .. }),
                ..
            }) => {}
            Driven::Done(Output::GetDone { result: Ok(_), .. }) => {}
            other => panic!("[{ctx}] unexpected read after a failed delete: {other:?}"),
        }

        let disk = std::mem::take(&mut host.disk);
        let (mut host, n) = open(disk);
        let deleted = host.get(victim).is_none();
        if !deleted {
            assert_eq!(host.get(victim), Some(victim_value), "[{ctx}]");
        }
        assert_eq!(n, ops.len() as u64 - u64::from(deleted), "[{ctx}]");
        for &(id, value) in &ops {
            if id == victim {
                continue;
            }
            assert_eq!(host.get(id), Some(value), "[{ctx}] neighbour id={id}");
        }
    }
}

/// An id may be used again after deletion, and the replay order decides.
/// This is the property the append-only tombstone design buys: the file IS
/// the history, so "insert, delete, insert" reconstructs to the LAST one.
#[test]
fn an_id_can_be_reused_after_deletion_and_replays_in_order() {
    let (disk, ops) = build(21, 4);
    let (mut host, _) = open(disk);
    let id = ops[1].0;
    let second = [0xB1; VALUE_LEN];
    let third = [0xC2; VALUE_LEN];

    assert_eq!(delete(&mut host, id), Ok(()));
    assert!(matches!(
        host.run(ClientOp::Insert { id, value: second }),
        Driven::Done(Output::InsertDone { result: Ok(()), .. })
    ));
    assert_eq!(host.get(id), Some(second));
    // Re-inserting while live is still a duplicate.
    assert!(matches!(
        host.run(ClientOp::Insert { id, value: third }),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::DuplicateId { .. }),
            ..
        })
    ));
    assert_eq!(delete(&mut host, id), Ok(()));
    assert!(matches!(
        host.run(ClientOp::Insert { id, value: third }),
        Driven::Done(Output::InsertDone { result: Ok(()), .. })
    ));

    // Replay from disk must reach the same final answer.
    let disk = std::mem::take(&mut host.disk);
    let (mut host, n) = open(disk);
    assert_eq!(n, ops.len() as u64, "live count after reuse");
    assert_eq!(host.get(id), Some(third), "replay picked the wrong version");
    for &(other, value) in &ops {
        if other == id {
            continue;
        }
        assert_eq!(host.get(other), Some(value));
    }
    // The ordered scan sees the reused id exactly once, with the new value.
    let rows = host.range_all(0, u64::MAX);
    assert_eq!(rows.len(), ops.len());
    assert_eq!(
        rows.iter().filter(|(k, _)| *k == id).count(),
        1,
        "a reused id appeared more than once in an ordered scan"
    );
    assert!(rows.contains(&(id, third)));
}

/// Deleting something that is not there is a caller mistake, refused
/// before any I/O — nothing is written, nothing is consumed.
#[test]
fn deleting_an_absent_row_is_refused_with_zero_io() {
    let (disk, ops) = build(5, 4);
    let (mut host, _) = open(disk);
    let before_io = host.io_count;
    let before_rows = host.disk.contents(FileId::Rows);
    let before_sb = host.disk.contents(FileId::Superblock);

    assert_eq!(
        delete(&mut host, 12_345_678),
        Err(DbError::NotFound { id: 12_345_678 })
    );
    // ...and deleting the same row twice is the same mistake.
    let id = ops[0].0;
    assert_eq!(delete(&mut host, id), Ok(()));
    let io_after_real_delete = host.io_count;
    assert_eq!(delete(&mut host, id), Err(DbError::NotFound { id }));

    assert_eq!(
        host.io_count, io_after_real_delete,
        "a refused delete performed I/O"
    );
    assert_eq!(
        before_io + 5,
        io_after_real_delete,
        "a delete costs 5 I/Os, like an insert"
    );
    // The refusals changed nothing on disk beyond the one real delete.
    assert_ne!(host.disk.contents(FileId::Rows), before_rows);
    assert_ne!(host.disk.contents(FileId::Superblock), before_sb);
}

/// Deleting everything leaves an empty, healthy, writable database.
#[test]
fn deleting_every_row_leaves_an_empty_readable_database() {
    let (disk, ops) = build(33, 10);
    let (mut host, _) = open(disk);
    for &(id, _) in &ops {
        assert_eq!(delete(&mut host, id), Ok(()));
    }
    assert_eq!(host.engine.live_count(), 0);
    assert!(host.range_all(0, u64::MAX).is_empty());
    assert!(
        host.find_all(&[]).is_empty(),
        "find still returns deleted rows"
    );

    let disk = std::mem::take(&mut host.disk);
    let (mut host, n) = open(disk);
    assert_eq!(n, 0, "an emptied database should recover as empty");
    for &(id, _) in &ops {
        assert_eq!(host.get(id), None);
    }
    assert!(host.range_all(0, u64::MAX).is_empty());
    // And it still works.
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: 7,
            value: [9; VALUE_LEN]
        }),
        Driven::Done(Output::InsertDone { result: Ok(()), .. })
    ));
    assert_eq!(host.get(7), Some([9; VALUE_LEN]));
}

/// Every read path must stop seeing a deleted row at the same instant.
#[test]
fn no_read_path_ever_returns_a_deleted_row() {
    let (disk, ops) = build(41, 12);
    let (mut host, _) = open(disk);
    let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = ops.iter().copied().collect();

    for (i, &(id, _)) in ops.iter().enumerate() {
        if i % 3 != 0 {
            continue;
        }
        assert_eq!(delete(&mut host, id), Ok(()));
        oracle.remove(&id);

        // Point lookups.
        for (&k, &v) in &oracle {
            assert_eq!(host.get(k), Some(v), "get diverged after deleting {id}");
        }
        assert_eq!(host.get(id), None);
        // Ordered scan.
        let want: Vec<(u64, [u8; VALUE_LEN])> = oracle.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(host.range_all(0, u64::MAX), want, "range diverged");
        // Substring search over every row's own prefix.
        for (&k, v) in &oracle {
            let hits = host.find_all(&v[..3]);
            assert!(
                hits.iter().any(|(hk, _)| *hk == k),
                "find lost a live row {k} after deleting {id}"
            );
            assert!(
                !hits.iter().any(|(hk, _)| *hk == id),
                "find returned deleted row {id}"
            );
        }
    }
}

/// A long interleaved workload against a BTreeMap, across restarts: the
/// engine's whole visible state must equal the model's, always.
#[test]
fn interleaved_inserts_and_deletes_match_the_oracle_across_restarts() {
    for seed in 0..8u64 {
        let mut rng_state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };

        let mut disk = SimDisk::new();
        let mut oracle: BTreeMap<u64, [u8; VALUE_LEN]> = BTreeMap::new();
        let mut slots = 0u64; // records + tombstones, the real capacity cost

        for round in 0..6 {
            let mut host = SimHost::new(CAPS, disk, None);
            let n = match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("seed={seed} round={round}: open: {other:?}"),
            };
            assert_eq!(n, oracle.len() as u64, "seed={seed} round={round}");

            for _ in 0..6 {
                if slots + 1 >= CAPS.rows {
                    break;
                }
                let delete_it = !oracle.is_empty() && next() % 3 == 0;
                if delete_it {
                    let victim = *oracle
                        .keys()
                        .nth((next() % oracle.len() as u64) as usize)
                        .expect("non-empty");
                    assert_eq!(delete(&mut host, victim), Ok(()));
                    oracle.remove(&victim);
                } else {
                    let id = next() % 50;
                    let value = [(next() % 251) as u8; VALUE_LEN];
                    match host.run(ClientOp::Insert { id, value }) {
                        Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                            assert!(oracle.insert(id, value).is_none());
                        }
                        Driven::Done(Output::InsertDone {
                            result: Err(DbError::DuplicateId { .. }),
                            ..
                        }) => {
                            assert!(oracle.contains_key(&id), "spurious DuplicateId for {id}");
                            continue;
                        }
                        other => panic!("seed={seed}: insert: {other:?}"),
                    }
                }
                slots += 1;
            }

            // Full agreement with the model, every round.
            let want: Vec<(u64, [u8; VALUE_LEN])> = oracle.iter().map(|(&k, &v)| (k, v)).collect();
            assert_eq!(
                host.range_all(0, u64::MAX),
                want,
                "seed={seed} round={round}"
            );
            for (&k, &v) in &oracle {
                assert_eq!(host.get(k), Some(v), "seed={seed} id={k}");
            }
            assert_eq!(host.engine.live_count(), oracle.len() as u64);
            disk = std::mem::take(&mut host.disk);
        }
    }
}

/// A tombstone costs a slot, so a database at its ceiling cannot record a
/// deletion — refused cleanly, naming the ceiling, with nothing applied.
#[test]
fn a_full_database_refuses_a_delete_without_touching_anything() {
    let caps = Capacities { rows: 4 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for id in 0..4u64 {
        assert!(matches!(
            host.run(ClientOp::Insert {
                id,
                value: [id as u8; VALUE_LEN]
            }),
            Driven::Done(Output::InsertDone { result: Ok(()), .. })
        ));
    }
    let io = host.io_count;
    let rows = host.disk.contents(FileId::Rows);

    match host.run_input(Input::Delete { id: 2 }) {
        Driven::Done(Output::DeleteDone {
            result: Err(DbError::Full { entity, capacity }),
            ..
        }) => {
            assert_eq!(entity, "records");
            assert_eq!(capacity, 4);
        }
        other => panic!("a full database must refuse a delete: {other:?}"),
    }
    assert_eq!(host.io_count, io, "the refusal performed I/O");
    assert_eq!(host.disk.contents(FileId::Rows), rows, "bytes changed");
    // Everything is still readable.
    for id in 0..4u64 {
        assert_eq!(host.get(id), Some([id as u8; VALUE_LEN]));
    }
}

fn update(host: &mut SimHost, id: u64, value: [u8; VALUE_LEN]) -> Result<(), DbError> {
    match host.run(ClientOp::Update { id, value }) {
        Driven::Done(Output::UpdateDone { result, .. }) => result,
        other => panic!("update({id}): {other:?}"),
    }
}

/// An update replaces a value in ONE commit. The alternative —
/// delete-then-insert — is two commits, and a crash between them loses
/// the row entirely; this test crashes at every boundary to show that
/// cannot happen here.
#[test]
fn a_crash_at_every_boundary_of_an_update_keeps_one_value_or_the_other() {
    for seed in 0..3u64 {
        let (base, ops) = build(seed, 6);
        let (target, old_value) = ops[1];
        let new_value = [0xF3; VALUE_LEN];

        for boundary in 0..6u64 {
            for settle in 0..3u64 {
                let ctx = format!("seed={seed} boundary={boundary} settle={settle}");
                let mut host = SimHost::new(CAPS, base.clone(), None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                let _ = host.run(ClientOp::Update {
                    id: target,
                    value: new_value,
                });

                let mut disk = std::mem::take(&mut host.disk);
                let mut rng = crash_rng(0x0FDA7E, settle);
                disk.crash(&mut rng);

                let (mut host, n) = open(disk);
                // The row is NEVER missing: an update is not a delete.
                let got = host
                    .get(target)
                    .unwrap_or_else(|| panic!("[{ctx}] the row vanished under an update"));
                assert!(
                    got == old_value || got == new_value,
                    "[{ctx}] neither the old nor the new value: {got:?}"
                );
                assert_eq!(n, ops.len() as u64, "[{ctx}] live count changed");
                for &(id, value) in &ops {
                    if id == target {
                        continue;
                    }
                    assert_eq!(host.get(id), Some(value), "[{ctx}] neighbour {id}");
                }
            }
        }
    }
}

/// Updates are visible everywhere at once, survive restarts, and are
/// refused for rows that do not exist.
#[test]
fn updates_are_atomic_visible_everywhere_and_durable() {
    let (disk, ops) = build(77, 8);
    let (mut host, _) = open(disk);
    let (id, old) = ops[3];
    let new = *b"UPDATED-VALUE!!!";

    assert_eq!(
        update(&mut host, 999_999, new),
        Err(DbError::NotFound { id: 999_999 })
    );
    assert_eq!(update(&mut host, id, new), Ok(()));
    assert_eq!(host.get(id), Some(new));
    assert_eq!(
        host.engine.live_count(),
        ops.len() as u64,
        "an update is not an insert"
    );

    // Ordered scan and substring search both see the new value only.
    let rows = host.range_all(0, u64::MAX);
    assert_eq!(rows.len(), ops.len());
    assert!(rows.contains(&(id, new)));
    assert!(!rows.contains(&(id, old)));
    assert!(host.find_all(&new[..6]).iter().any(|(k, _)| *k == id));
    assert!(
        !host.find_all(&old[..6]).iter().any(|(k, _)| *k == id),
        "substring search still matches the superseded value"
    );

    // Durable across a restart.
    let disk = std::mem::take(&mut host.disk);
    let (mut host, n) = open(disk);
    assert_eq!(n, ops.len() as u64);
    assert_eq!(host.get(id), Some(new));
    // An updated row can still be deleted, and then re-inserted.
    assert_eq!(delete(&mut host, id), Ok(()));
    assert_eq!(host.get(id), None);
    assert!(matches!(
        host.run(ClientOp::Insert { id, value: old }),
        Driven::Done(Output::InsertDone { result: Ok(()), .. })
    ));
    assert_eq!(host.get(id), Some(old));
}

/// Damage the RECORD that a deletion retired, and the deletion itself
/// becomes an orphan: it refers to a row that is no longer live at that
/// point in the replay. Both slots must be quarantined — the damaged
/// record AND the deletion left dangling by it.
///
/// (Found by mutation testing: `quarantined += 1` in this branch could be
/// removed without a single test noticing, which meant salvage could
/// under-report exactly how much of a churned database it could not read.)
#[test]
fn a_deletion_orphaned_by_a_damaged_record_is_itself_quarantined() {
    use dabqlite_core::ROW_SIZE;
    let n = 6usize;
    let (base, ops) = build(31, n);
    let (victim, _) = ops[2];

    let mut host = SimHost::new(CAPS, base, None);
    host.open();
    assert_eq!(delete(&mut host, victim), Ok(()));
    let mut disk = std::mem::take(&mut host.disk);
    // Row 2 is the victim's record; the tombstone is the last slot.
    disk.corrupt(FileId::Rows, (2 * ROW_SIZE + 6) as u64, 0x20);

    // Strict open refuses, naming the row defect it hits first.
    let (_, strict) = open_strict(disk.clone());
    assert!(
        matches!(strict, Err(DbError::Corrupt { .. })),
        "strict open must refuse: {strict:?}"
    );

    let (mut host, salvaged) = open_salvage(disk);
    assert_eq!(
        host.engine.quarantined(),
        2,
        "the damaged record AND the deletion it orphaned must both be quarantined"
    );
    // Five survivors. The quarantined record IS the row that was deleted,
    // so the orphaned deletion has nothing left to remove: the other five
    // records are live, and the deleted row is absent either way.
    assert_eq!(salvaged, 5);
    for (row, &(id, value)) in ops.iter().enumerate() {
        if row == 2 {
            continue;
        }
        assert_eq!(get_result(&mut host, id), Ok(Some(value)), "row {row}");
    }
}

/// The same for an update: damage the record it superseded, and the
/// update is left referring to a row that was never live.
#[test]
fn an_update_orphaned_by_a_damaged_record_is_itself_quarantined() {
    use dabqlite_core::ROW_SIZE;
    let n = 6usize;
    let (base, ops) = build(43, n);
    let (target, _) = ops[1];

    let mut host = SimHost::new(CAPS, base, None);
    host.open();
    assert_eq!(update(&mut host, target, [0x5E; VALUE_LEN]), Ok(()));
    let mut disk = std::mem::take(&mut host.disk);
    // Row 1 is the superseded record; the update is the last slot.
    disk.corrupt(FileId::Rows, (ROW_SIZE + 9) as u64, 0x11);

    let (_, strict) = open_strict(disk.clone());
    assert!(
        matches!(strict, Err(DbError::Corrupt { .. })),
        "strict open must refuse: {strict:?}"
    );

    let (mut host, salvaged) = open_salvage(disk);
    assert_eq!(
        host.engine.quarantined(),
        2,
        "the damaged record AND the update it orphaned must both be quarantined"
    );
    // The updated row is unreadable; the other five survive.
    assert_eq!(salvaged, 5);
    assert_eq!(
        get_result(&mut host, target),
        Err(DbError::Degraded { quarantined: 2 })
    );
    for (row, &(id, value)) in ops.iter().enumerate() {
        if row == 1 {
            continue;
        }
        assert_eq!(get_result(&mut host, id), Ok(Some(value)), "row {row}");
    }
}
