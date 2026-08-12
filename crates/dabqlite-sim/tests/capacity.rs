//! Capacity exhaustion is a first-class, specified, testable failure mode
//! with a finite matrix (docs/DESIGN.md §6): fill to N-1, N, N+1; crash at
//! the boundary and recover. The arena is set absurdly small so every run
//! hits the wall.

use dabqlite_core::{Capacities, DbError, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 4 };

fn val(b: u8) -> [u8; VALUE_LEN] {
    [b; VALUE_LEN]
}

fn insert(host: &mut SimHost, id: u64) -> Result<(), DbError> {
    match host.run(ClientOp::Insert {
        id,
        value: val(id as u8),
    }) {
        Driven::Done(Output::InsertDone { result, .. }) => result,
        other => panic!("insert did not complete: {other:?}"),
    }
}

#[test]
fn fill_to_the_wall_and_over_it() {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();

    // Fill to N-1: plenty of runway.
    for id in 0..CAPS.rows - 1 {
        assert_eq!(insert(&mut host, id), Ok(()));
    }
    assert_eq!(host.engine.usage(), (CAPS.rows - 1, CAPS.rows));

    // Fill to exactly N: succeeds, zone now full.
    assert_eq!(insert(&mut host, CAPS.rows - 1), Ok(()));
    assert_eq!(host.engine.usage(), (CAPS.rows, CAPS.rows));

    // N+1: a first-class error that names the entity and the configured
    // capacity — it should read like documentation.
    assert_eq!(
        insert(&mut host, CAPS.rows),
        Err(DbError::Full {
            entity: "records",
            capacity: CAPS.rows
        })
    );

    // The failure must not have damaged anything: everything is readable
    // and the rejected id is absent.
    for id in 0..CAPS.rows {
        assert_eq!(host.get(id), Some(val(id as u8)));
    }
    assert_eq!(host.get(CAPS.rows), None);

    // Full is stable across restart.
    let disk = std::mem::take(&mut host.disk);
    let mut reopened = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        reopened.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == CAPS.rows
    ));
    assert_eq!(
        insert(&mut reopened, CAPS.rows),
        Err(DbError::Full {
            entity: "records",
            capacity: CAPS.rows
        })
    );
}

#[test]
fn crash_at_every_boundary_of_the_last_slot() {
    // Crash while inserting the row that exactly fills the zone, at every
    // I/O boundary of that insert (5 ops: write row, fsync rows, write both
    // superblock copies, fsync superblock).
    for boundary_in_insert in 0..5u64 {
        for seed in 0..8u64 {
            let ctx = format!("seed={seed} boundary_in_insert={boundary_in_insert}");

            // Fill to N-1 cleanly, counting I/O so we can place the crash.
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.open();
            for id in 0..CAPS.rows - 1 {
                assert_eq!(insert(&mut host, id), Ok(()));
            }
            let crash_at = host.io_count + boundary_in_insert;
            host.crash_after = Some(crash_at);

            let last_id = CAPS.rows - 1;
            let crashed = matches!(
                host.run(ClientOp::Insert {
                    id: last_id,
                    value: val(last_id as u8)
                }),
                Driven::Crashed
            );
            assert!(crashed, "[{ctx}] must crash inside the final insert");

            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(seed, crash_at));

            let mut recovered = SimHost::new(CAPS, disk, None);
            let n = match recovered.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] recovery failed: {other:?}"),
            };
            assert!(
                n == CAPS.rows - 1 || n == CAPS.rows,
                "[{ctx}] recovered {n} rows at the capacity boundary"
            );
            if n == CAPS.rows - 1 {
                // The slot is still free: the insert must succeed now.
                assert_eq!(insert(&mut recovered, last_id), Ok(()));
            } else {
                // The insert committed: retrying must report duplicate, and
                // one past it must report Full.
                assert_eq!(recovered.get(last_id), Some(val(last_id as u8)));
                assert_eq!(
                    insert(&mut recovered, last_id),
                    Err(DbError::DuplicateId { id: last_id })
                );
            }
            assert_eq!(
                insert(&mut recovered, CAPS.rows),
                Err(DbError::Full {
                    entity: "records",
                    capacity: CAPS.rows
                })
            );
        }
    }
}
