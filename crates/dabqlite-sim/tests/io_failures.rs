//! I/O *failure* injection — distinct from crashes. A read, write, or fsync
//! returns an error; the engine fail-stops; the process restarts and
//! re-opens **without a machine crash**, so the page cache (`current`)
//! still holds everything the dead incarnation wrote, synced or not.
//!
//! Swept at every I/O index of a run. Three properties:
//!
//! 1. **Fail-stop**: the in-flight op errors with `IoFailed`; every
//!    subsequent op is rejected until restart.
//! 2. **Atomicity across restart**: the reopened database holds exactly the
//!    acked inserts, plus possibly the failed one — all or nothing.
//! 3. **Visible implies durable**: whatever the reopened database *shows*
//!    must survive a machine crash immediately after. Recovery is required
//!    to fsync what it found before reporting OpenDone — the page cache may
//!    hold state that was never made durable, and serving it un-synced
//!    would let a later power loss erase rows the application already saw.

use dabqlite_core::{Capacities, DbError, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const SEEDS: u64 = 16;
const INSERTS: usize = 6;
const CAPS: Capacities = Capacities { rows: 16 };

#[test]
fn io_failure_at_every_index_fail_stop_and_recovery_durability() {
    for seed in 0..SEEDS {
        let ops = gen_workload(seed, INSERTS);
        let total_io = {
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.open();
            for &(id, value) in &ops {
                host.run(ClientOp::Insert { id, value });
            }
            host.io_count
        };

        for fail_at in 0..total_io {
            let ctx = format!("seed={seed} fail_at={fail_at}");
            let mut host = SimHost::new(CAPS, SimDisk::new(), None);
            host.fail_after = Some(fail_at);

            let mut acked: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
            let mut failed_op: Option<(u64, [u8; VALUE_LEN])> = None;
            let mut open_failed = false;

            match host.open() {
                Driven::Done(Output::OpenDone { result: Ok(0) }) => {}
                Driven::Done(Output::OpenDone {
                    result: Err(DbError::IoFailed { .. }),
                }) => open_failed = true,
                other => panic!("[{ctx}] open: {other:?}"),
            }
            if !open_failed {
                for &(id, value) in &ops {
                    match host.run(ClientOp::Insert { id, value }) {
                        Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                            acked.push((id, value));
                        }
                        Driven::Done(Output::InsertDone {
                            result: Err(DbError::IoFailed { .. }),
                            ..
                        }) => {
                            failed_op = Some((id, value));
                            break;
                        }
                        other => panic!("[{ctx}] insert: {other:?}"),
                    }
                }
            }
            assert!(
                open_failed || failed_op.is_some(),
                "[{ctx}] failure index inside the run must fail something"
            );

            // Property 1: fail-stop. Everything after the failure errors.
            let probe = match host.run(ClientOp::Get { id: 0 }) {
                Driven::Done(Output::GetDone { result, .. }) => result,
                other => panic!("[{ctx}] probe: {other:?}"),
            };
            assert!(
                matches!(probe, Err(DbError::IoFailed { .. })),
                "[{ctx}] engine kept serving after an I/O failure: {probe:?}"
            );

            // Restart the process WITHOUT a machine crash: dirty page cache
            // carries over.
            let disk = std::mem::take(&mut host.disk);
            let mut second = SimHost::new(CAPS, disk, None);
            let n1 = match second.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] reopen after failure: {other:?}"),
            };

            // Property 2: atomicity. Exactly the acked set, plus possibly
            // the failed op, never anything in between.
            let n_acked = acked.len() as u64;
            assert!(
                n1 == n_acked || n1 == n_acked + 1,
                "[{ctx}] reopened with {n1} rows, {n_acked} acked"
            );
            for &(id, value) in &acked {
                assert_eq!(second.get(id), Some(value), "[{ctx}] acked id={id} lost");
            }
            if n1 == n_acked + 1 {
                let (id, value) = failed_op.expect("extra row must be the failed insert");
                assert_eq!(second.get(id), Some(value), "[{ctx}] failed insert torn");
            }

            // Property 3: visible implies durable. Machine-crash NOW; the
            // state the second incarnation showed must not regress.
            let mut disk = std::mem::take(&mut second.disk);
            disk.crash(&mut crash_rng(seed, fail_at));
            let mut third = SimHost::new(CAPS, disk, None);
            let n2 = match third.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] reopen after crash: {other:?}"),
            };
            assert_eq!(
                n2, n1,
                "[{ctx}] state visible after recovery ({n1} rows) regressed to \
                 {n2} after a machine crash: recovery served un-durable data"
            );
            for &(id, value) in &acked {
                assert_eq!(
                    third.get(id),
                    Some(value),
                    "[{ctx}] id={id} lost post-crash"
                );
            }
        }
    }
}
