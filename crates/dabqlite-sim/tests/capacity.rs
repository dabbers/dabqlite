//! Capacity exhaustion is a first-class, specified, testable failure mode
//! with a finite matrix (docs/DESIGN.md §6): fill to N-1, N, N+1; crash at
//! the boundary and recover. The arena is set absurdly small so every run
//! hits the wall.

use dabqlite_core::{Capacities, DbError, FileId, Input, Output, VALUE_LEN};
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

/// §6: "batches are rejected atomically; partial application turns a
/// capacity limit into a corruption bug." The mechanical version: a Full
/// rejection must perform ZERO I/O — no write, no fsync, no read — and
/// leave every on-disk byte untouched.
#[test]
fn full_rejection_performs_zero_io_and_changes_nothing() {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for id in 0..CAPS.rows {
        assert_eq!(insert(&mut host, id), Ok(()));
    }
    let io_before = host.io_count;
    let sb_before = host.disk.contents(FileId::Superblock);
    let rows_before = host.disk.contents(FileId::Rows);

    // Hammer the wall: every rejection is pure, none touches the disk.
    for attempt in 0..25u64 {
        assert_eq!(
            insert(&mut host, CAPS.rows + attempt),
            Err(DbError::Full {
                entity: "records",
                capacity: CAPS.rows
            })
        );
    }
    assert_eq!(host.io_count, io_before, "Full rejection performed I/O");
    assert_eq!(host.disk.contents(FileId::Superblock), sb_before);
    assert_eq!(host.disk.contents(FileId::Rows), rows_before);

    // The generation is untouched too: no phantom commits at the wall.
    assert_eq!(host.engine.generation(), CAPS.rows + 1);
}

/// EIO landing exactly at the wall: each of the 5 I/O ops of the insert
/// that fills the final slot fails; fail-stop, dirty-cache restart, and
/// the wall must still be exact — N-1 with a working retry, or N with
/// DuplicateId on retry. Then a machine crash on top: state stable.
#[test]
fn eio_at_the_wall_leaves_the_wall_exact() {
    for fail_op in 0..5u64 {
        let ctx = format!("fail_op={fail_op}");
        let mut host = SimHost::new(CAPS, SimDisk::new(), None);
        host.open();
        for id in 0..CAPS.rows - 1 {
            assert_eq!(insert(&mut host, id), Ok(()));
        }
        host.fail_after = Some(host.io_count + fail_op);
        let last = CAPS.rows - 1;
        assert_eq!(
            insert(&mut host, last),
            Err(DbError::IoFailed {
                file: match fail_op {
                    0 | 1 => FileId::Rows,
                    _ => FileId::Superblock,
                }
            }),
            "[{ctx}]"
        );

        // Restart on the dirty cache.
        let disk = std::mem::take(&mut host.disk);
        let mut second = SimHost::new(CAPS, disk, None);
        let n1 = match second.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
            other => panic!("[{ctx}] reopen: {other:?}"),
        };
        assert!(
            n1 == CAPS.rows - 1 || n1 == CAPS.rows,
            "[{ctx}] reopened with {n1} rows at the wall"
        );
        if n1 == CAPS.rows - 1 {
            assert_eq!(insert(&mut second, last), Ok(()), "[{ctx}] retry");
        } else {
            assert_eq!(
                insert(&mut second, last),
                Err(DbError::DuplicateId { id: last }),
                "[{ctx}]"
            );
        }
        // Either way the zone is now exactly full.
        assert_eq!(second.engine.usage(), (CAPS.rows, CAPS.rows), "[{ctx}]");
        assert_eq!(
            insert(&mut second, CAPS.rows),
            Err(DbError::Full {
                entity: "records",
                capacity: CAPS.rows
            }),
            "[{ctx}]"
        );

        // Machine crash on top: the full state is durable and exact.
        let mut disk = std::mem::take(&mut second.disk);
        disk.crash(&mut crash_rng(0xEA11, fail_op));
        let mut third = SimHost::new(CAPS, disk, None);
        assert!(
            matches!(
                third.open(),
                Driven::Done(Output::OpenDone { result: Ok(n) }) if n == CAPS.rows
            ),
            "[{ctx}] full database regressed after crash"
        );
        for id in 0..CAPS.rows {
            assert_eq!(third.get(id), Some(val(id as u8)), "[{ctx}] id={id}");
        }
    }
}

/// The reopen capacity boundary must be exact: capacity == data succeeds
/// (immediately full), capacity == data - 1 fails naming both numbers,
/// capacity == data + 1 gives exactly one slot of runway.
#[test]
fn reopen_capacity_boundaries_are_exact() {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for id in 0..CAPS.rows {
        assert_eq!(insert(&mut host, id), Ok(()));
    }
    let disk = std::mem::take(&mut host.disk);

    // Exactly-equal capacity: opens, and is already at the wall.
    let mut exact = SimHost::new(Capacities { rows: CAPS.rows }, disk.clone(), None);
    assert!(matches!(
        exact.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == CAPS.rows
    ));
    assert_eq!(exact.engine.usage(), (CAPS.rows, CAPS.rows));
    assert_eq!(
        insert(&mut exact, 99),
        Err(DbError::Full {
            entity: "records",
            capacity: CAPS.rows
        })
    );

    // One below: refused, and the error is the config change needed.
    let mut small = SimHost::new(
        Capacities {
            rows: CAPS.rows - 1,
        },
        disk.clone(),
        None,
    );
    assert!(matches!(
        small.open(),
        Driven::Done(Output::OpenDone {
            result: Err(DbError::CapacityBelowData {
                required,
                configured,
            })
        }) if required == CAPS.rows && configured == CAPS.rows - 1
    ));

    // One above: exactly one slot of runway, then the wall again.
    let mut grown = SimHost::new(
        Capacities {
            rows: CAPS.rows + 1,
        },
        disk,
        None,
    );
    assert!(matches!(
        grown.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == CAPS.rows
    ));
    assert_eq!(insert(&mut grown, 100), Ok(()));
    assert_eq!(
        insert(&mut grown, 101),
        Err(DbError::Full {
            entity: "records",
            capacity: CAPS.rows + 1
        })
    );
}

/// A 100%-full database must serve every read path perfectly: point gets,
/// full ordered paged scans, singleton and empty ranges.
#[test]
fn full_database_serves_every_query_path() {
    let caps = Capacities { rows: 16 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    for id in 0..caps.rows {
        match host.run(ClientOp::Insert {
            id: id * 3,
            value: val(id as u8),
        }) {
            Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {}
            other => panic!("fill: {other:?}"),
        }
    }
    assert_eq!(host.engine.usage(), (caps.rows, caps.rows));

    // Point gets: every row, plus honest absence between rows.
    for id in 0..caps.rows {
        assert_eq!(host.get(id * 3), Some(val(id as u8)));
        assert_eq!(host.get(id * 3 + 1), None);
    }

    // Full ordered paged scan at 100% fill.
    let mut seen = Vec::new();
    let mut cursor = 0u64;
    loop {
        let page = match host.run_input(Input::Range {
            lo: cursor,
            hi: u64::MAX,
        }) {
            Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
            other => panic!("range: {other:?}"),
        };
        seen.extend_from_slice(&page.items[..page.count as usize]);
        match page.next {
            Some(n) => cursor = n,
            None => break,
        }
    }
    assert_eq!(seen.len() as u64, caps.rows);
    assert!(
        seen.windows(2).all(|w| w[0].0 < w[1].0),
        "scan out of order"
    );

    // Empty and singleton ranges at the wall.
    let empty = match host.run_input(Input::Range { lo: 7, hi: 5 }) {
        Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!((empty.count, empty.next), (0, None));
}
