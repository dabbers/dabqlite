//! Disk exhaustion (ENOSPC): a fault REGIME, not a fault event. Once the
//! disk is full, every write and fsync fails until space frees — while
//! reads keep working. That persistence is what the one-shot EIO sweeps
//! cannot express, and it reaches a place they never did: RECOVERY
//! ITSELF running on a full disk, over and over, until an operator frees
//! space.
//!
//! The claims:
//! - hitting the wall mid-commit fail-stops; nothing is ever partially
//!   applied to the acknowledged state;
//! - recovery on a still-full disk fails LOUDLY (`IoFailed`) any number
//!   of times, corrupting nothing;
//! - the moment space frees, one recovery converges and every
//!   acknowledged row is present, exact — zero loss across the episode;
//! - reads (gets, ranges, finds) on an already-open engine keep working
//!   through the regime — they perform no I/O at all;
//! - migration on a full disk fails cleanly, legacy intact, and
//!   converges after space frees.
//!
//! The same episode runs against REAL files with genuine ENOSPC (a
//! size-capped tmpfs) in `dabqlite-host/tests/enospc.rs`.

use dabqlite_core::{Capacities, DbError, FileId, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::{build_v1_disk, crash_rng};
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};
use rand_chacha::rand_core::SeedableRng;

const CAPS: Capacities = Capacities { rows: 32 };

fn build_db(seed: u64, n: usize) -> (SimDisk, Vec<(u64, [u8; VALUE_LEN])>) {
    let ops = gen_workload(seed, n);
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for &(id, value) in &ops {
        host.run(ClientOp::Insert { id, value });
    }
    (std::mem::take(&mut host.disk), ops)
}

/// The full episode, with the wall arriving at EVERY boundary of an
/// insert: fail-stop, N recoveries refused on the full disk, then space
/// frees and everything is there.
#[test]
fn the_full_disk_episode_at_every_insert_boundary() {
    for seed in 0..4u64 {
        let (base, ops) = build_db(seed, 5);
        for boundary in 0..5u64 {
            let ctx = format!("seed={seed} boundary={boundary}");
            let mut host = SimHost::new(CAPS, base.clone(), None);
            host.open();
            host.disk_full_from = Some(host.io_count + boundary);
            let extra = (u64::MAX - seed, [0xD7; VALUE_LEN]);
            match host.run(ClientOp::Insert {
                id: extra.0,
                value: extra.1,
            }) {
                Driven::Done(Output::InsertDone {
                    result: Err(DbError::IoFailed { .. }),
                    ..
                }) => {}
                other => panic!("[{ctx}] wall must fail-stop: {other:?}"),
            }
            // Fail-stopped: everything is refused with the same error.
            assert!(matches!(
                host.run(ClientOp::Get { id: ops[0].0 }),
                Driven::Done(Output::GetDone {
                    result: Err(DbError::IoFailed { .. }),
                    ..
                })
            ));

            // Restart on the STILL-FULL disk, repeatedly: recovery is
            // refused loudly every time (its fsyncs cannot complete), and
            // refusing changes nothing — three refusals, zero cumulative
            // harm.
            let mut disk = std::mem::take(&mut host.disk);
            for attempt in 0..3 {
                let mut host = SimHost::new(CAPS, disk, None);
                host.disk_full_from = Some(0);
                match host.open() {
                    Driven::Done(Output::OpenDone {
                        result: Err(DbError::IoFailed { .. }),
                    }) => {}
                    other => panic!("[{ctx}] attempt {attempt}: full-disk recovery: {other:?}"),
                }
                disk = std::mem::take(&mut host.disk);
            }

            // Space freed: one recovery converges; the in-flight insert
            // resolved all-or-nothing; every acked row byte-exact.
            let mut host = SimHost::new(CAPS, disk, None);
            let n = match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] post-episode recovery: {other:?}"),
            };
            assert!(
                n == ops.len() as u64 || n == ops.len() as u64 + 1,
                "[{ctx}] {n} rows after the episode"
            );
            for &(id, value) in &ops {
                assert_eq!(host.get(id), Some(value), "[{ctx}] id={id}");
            }
            if n == ops.len() as u64 + 1 {
                assert_eq!(host.get(extra.0), Some(extra.1), "[{ctx}] in-flight");
            }
        }
    }
}

/// Reads never stop working: a full disk cannot touch an open engine's
/// read paths, because they perform zero I/O — pinned, not assumed.
#[test]
fn reads_keep_working_while_the_disk_is_full() {
    let (disk, ops) = build_db(9, 8);
    let mut host = SimHost::new(CAPS, disk, None);
    host.open();
    host.disk_full_from = Some(host.io_count); // the wall arrives NOW
    let io = host.io_count;
    for &(id, value) in &ops {
        assert_eq!(host.get(id), Some(value));
    }
    assert_eq!(host.range_all(0, u64::MAX).len(), ops.len());
    let (_, v) = ops[0];
    assert!(!host.find_all(&v[..3]).is_empty());
    assert_eq!(host.io_count, io, "reads performed I/O on a full disk");
    // The wall only bites when something needs to WRITE.
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: u64::MAX,
            value: [1; VALUE_LEN]
        }),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::IoFailed { .. }),
            ..
        })
    ));
}

/// A crash while the disk is full, settled, then recovery still refused
/// until space frees: the regime composes with the crash model.
#[test]
fn crash_during_the_full_disk_episode() {
    for settle in 0..4u64 {
        let (base, ops) = build_db(11, 4);
        let mut host = SimHost::new(CAPS, base, None);
        host.open();
        host.disk_full_from = Some(host.io_count);
        host.run(ClientOp::Insert {
            id: 999_999,
            value: [3; VALUE_LEN],
        });
        // Machine dies during the episode; unsynced cache settles.
        let mut disk = std::mem::take(&mut host.disk);
        let mut rng = crash_rng(0xD15C, settle);
        disk.crash(&mut rng);

        // Still full at restart: loud refusal.
        let mut host = SimHost::new(CAPS, disk, None);
        host.disk_full_from = Some(0);
        assert!(matches!(
            host.open(),
            Driven::Done(Output::OpenDone {
                result: Err(DbError::IoFailed { .. })
            })
        ));
        // Space freed: exact convergence.
        let disk = std::mem::take(&mut host.disk);
        let mut host = SimHost::new(CAPS, disk, None);
        let n = match host.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
            other => panic!("settle={settle}: {other:?}"),
        };
        assert_eq!(n, ops.len() as u64, "settle={settle}");
        for &(id, value) in &ops {
            assert_eq!(host.get(id), Some(value), "settle={settle} id={id}");
        }
    }
}

/// Single-copy recovery (the storm's find) meets the full disk: the
/// repair write is refused until space frees, then lands — the database
/// never runs repaired-in-name-only.
#[test]
fn superblock_repair_waits_for_space() {
    use dabqlite_sim::WriteFate;
    let (base, ops) = build_db(13, 1);
    let mut host = SimHost::new(CAPS, base, None);
    host.open();
    // Tear the next commit between its two sb copy writes (single-copy
    // survivor), exactly as in faults.rs.
    host.crash_after = Some(host.io_count + 3);
    let extra = (424_242u64, [7; VALUE_LEN]);
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: extra.0,
            value: extra.1
        }),
        Driven::Crashed
    ));
    let mut disk = std::mem::take(&mut host.disk);
    let fates: Vec<WriteFate> = disk
        .unsynced_writes()
        .iter()
        .map(|_| WriteFate::Keep)
        .collect();
    disk.settle_with(&fates);

    // Recovery needs the repair write — refused while full.
    let mut host = SimHost::new(CAPS, disk, None);
    host.disk_full_from = Some(0);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone {
            result: Err(DbError::IoFailed { .. })
        })
    ));
    // Space freed: recovery repairs and serves everything.
    let disk = std::mem::take(&mut host.disk);
    let mut host = SimHost::new(CAPS, disk, None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(2) })
    ));
    assert_eq!(host.get(ops[0].0), Some(ops[0].1));
    assert_eq!(host.get(extra.0), Some(extra.1));
}

/// Migration on a full disk: refused cleanly, legacy byte-identical, and
/// convergent once space frees — at every I/O boundary of the migration.
#[test]
fn migration_on_a_full_disk_converges_when_space_frees() {
    let n = 4u64;
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0xF0011);
    let (pristine, v1_ops) = build_v1_disk(&mut rng, n);
    let legacy_bytes = pristine.contents(FileId::RowsOld);
    let total_io = 6 + n;
    for wall_at in 0..total_io {
        let ctx = format!("wall_at={wall_at}");
        let mut host = SimHost::new(CAPS, pristine.clone(), None);
        host.disk_full_from = Some(wall_at);
        match host.run_migration() {
            Driven::Done(Output::MigrateDone {
                result: Err(DbError::IoFailed { .. }),
            }) => {}
            other => panic!("[{ctx}] full-disk migration must fail-stop: {other:?}"),
        }
        assert_eq!(
            host.disk.contents(FileId::RowsOld),
            legacy_bytes,
            "[{ctx}] legacy file changed"
        );
        // Space freed: the retry converges completely.
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
        for &(id, v1) in &v1_ops {
            let mut want = [0u8; VALUE_LEN];
            want[..8].copy_from_slice(&v1);
            assert_eq!(host.get(id), Some(want), "[{ctx}] id={id}");
        }
    }
}
