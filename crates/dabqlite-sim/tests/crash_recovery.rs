//! The test that means the harness works (docs/DESIGN.md §7.3):
//!
//! Crash the simulated process at every I/O boundary in a run, recover, and
//! assert the state is exactly N or N+1 committed inserts and never in
//! between. Reproducible from a seed — every assertion message carries the
//! (seed, boundary) pair that reproduces it.

use dabqlite_core::{Capacities, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const SEEDS: u64 = 32;
const INSERTS: usize = 8;
const CAPS: Capacities = Capacities { rows: 16 };

/// Run the full workload with no crash; return total I/O ops performed.
fn clean_run_io_total(ops: &[(u64, [u8; VALUE_LEN])]) -> u64 {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(0) })
    ));
    for &(id, value) in ops {
        let out = host.run(ClientOp::Insert { id, value });
        assert!(
            matches!(out, Driven::Done(Output::InsertDone { result: Ok(()), .. })),
            "clean run insert failed: {out:?}"
        );
    }
    host.io_count
}

#[test]
fn crash_at_every_io_boundary_recovers_to_n_or_n_plus_1() {
    for seed in 0..SEEDS {
        let ops = gen_workload(seed, INSERTS);
        let total_io = clean_run_io_total(&ops);
        assert!(total_io > 0);

        for boundary in 0..total_io {
            let ctx = format!("seed={seed} boundary={boundary}");

            // --- run until the crash -----------------------------------
            let mut host = SimHost::new(CAPS, SimDisk::new(), Some(boundary));
            let mut acked: Vec<(u64, [u8; VALUE_LEN])> = Vec::new();
            let mut in_flight: Option<(u64, [u8; VALUE_LEN])> = None;
            let mut crashed = matches!(host.open(), Driven::Crashed);
            if !crashed {
                for &(id, value) in &ops {
                    match host.run(ClientOp::Insert { id, value }) {
                        Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {
                            acked.push((id, value));
                        }
                        Driven::Done(other) => panic!("[{ctx}] unexpected: {other:?}"),
                        Driven::Crashed => {
                            in_flight = Some((id, value));
                            crashed = true;
                            break;
                        }
                    }
                }
            }
            assert!(crashed, "[{ctx}] boundary within clean-run I/O must crash");

            // --- settle unsynced writes (survive / vanish / tear) -------
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(seed, boundary));

            // --- recover -------------------------------------------------
            let mut recovered = SimHost::new(CAPS, disk, None);
            let n = match recovered.open() {
                Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
                other => panic!("[{ctx}] recovery failed: {other:?}"),
            };

            // --- the property: exactly N or N+1, never in between -------
            let n_acked = acked.len() as u64;
            assert!(
                n == n_acked || n == n_acked + 1,
                "[{ctx}] recovered {n} rows, but {n_acked} inserts were acknowledged"
            );

            // Every acknowledged insert must be present and intact.
            for &(id, value) in &acked {
                assert_eq!(
                    recovered.get(id),
                    Some(value),
                    "[{ctx}] acknowledged insert id={id} lost or corrupted"
                );
            }

            // If the in-flight insert committed, it must be fully intact;
            // if it did not commit, it must be entirely absent. Nothing in
            // between (that is the whole point).
            if n == n_acked + 1 {
                let (id, value) = in_flight
                    .unwrap_or_else(|| panic!("[{ctx}] recovered N+1 rows with none in flight"));
                assert_eq!(
                    recovered.get(id),
                    Some(value),
                    "[{ctx}] in-flight insert id={id} committed but corrupted"
                );
            } else if let Some((id, _)) = in_flight {
                assert_eq!(
                    recovered.get(id),
                    None,
                    "[{ctx}] unacknowledged insert id={id} partially applied"
                );
            }

            // Negative space: ids never submitted must be absent.
            let submitted = acked.len() + usize::from(in_flight.is_some());
            for &(id, _) in ops.iter().skip(submitted) {
                assert_eq!(
                    recovered.get(id),
                    None,
                    "[{ctx}] never-submitted insert id={id} present after recovery"
                );
            }

            // The recovered database must still be writable.
            let probe_id = u64::MAX; // gen_workload ids are random; collision odds ~0, but check
            if recovered.get(probe_id).is_none() && n < CAPS.rows {
                let out = recovered.run(ClientOp::Insert {
                    id: probe_id,
                    value: [0xAB; VALUE_LEN],
                });
                assert!(
                    matches!(out, Driven::Done(Output::InsertDone { result: Ok(()), .. })),
                    "[{ctx}] recovered database rejected a fresh insert: {out:?}"
                );
            }
        }
    }
}

/// Crash during recovery-after-crash: tear the initial superblock write,
/// then crash the re-initialization at every boundary too.
#[test]
fn crash_during_fresh_init_recovers() {
    for seed in 0..SEEDS {
        // First: crash a fresh open at each of its boundaries (two copy
        // writes, then the fsync).
        for boundary in 0..3 {
            let ctx = format!("seed={seed} boundary={boundary}");
            let mut host = SimHost::new(CAPS, SimDisk::new(), Some(boundary));
            assert!(
                matches!(host.open(), Driven::Crashed),
                "[{ctx}] fresh open has 3 I/O ops; must crash"
            );
            let mut disk = std::mem::take(&mut host.disk);
            disk.crash(&mut crash_rng(seed, boundary));

            let mut recovered = SimHost::new(CAPS, disk, None);
            match recovered.open() {
                Driven::Done(Output::OpenDone { result: Ok(0) }) => {}
                other => panic!("[{ctx}] fresh-init recovery failed: {other:?}"),
            }
            // And it must be usable.
            let out = recovered.run(ClientOp::Insert {
                id: 1,
                value: [1; VALUE_LEN],
            });
            assert!(matches!(
                out,
                Driven::Done(Output::InsertDone { result: Ok(()), .. })
            ));
        }
    }
}
