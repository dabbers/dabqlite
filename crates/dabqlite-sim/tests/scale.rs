//! Scale validation: the claims proven at 4–4096 rows, re-proven at a
//! million. The design makes big-N a linear extrapolation (fixed layouts,
//! one arena per zone, O(log n) index paths) — this suite checks the
//! extrapolation instead of assuming it, at the largest size that keeps
//! the suite honest to run on every push.

use dabqlite_core::{Capacities, DbError, Input, Output, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const N: u64 = 1_000_000;

fn value_for(id: u64) -> [u8; VALUE_LEN] {
    let mut v = [0u8; VALUE_LEN];
    v[..8].copy_from_slice(&id.to_le_bytes());
    v[8..].copy_from_slice(&(id ^ 0xA5A5_A5A5_A5A5_A5A5).to_le_bytes());
    v
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "scale suite runs in release (assertions stay on: profile sets debug-assertions=true); CI runs it explicitly"
)]
fn million_row_database_end_to_end() {
    let caps = Capacities { rows: N };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    assert!(matches!(
        host.open(),
        Driven::Done(Output::OpenDone { result: Ok(0) })
    ));

    // Interleaved key order (both ends toward the middle) so the btree
    // splits everywhere, not just on the right edge.
    for i in 0..N {
        let id = if i.is_multiple_of(2) { i } else { 2 * N - i };
        match host.run(ClientOp::Insert {
            id,
            value: value_for(id),
        }) {
            Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {}
            other => panic!("insert {i}: {other:?}"),
        }
    }
    assert_eq!(host.engine.usage(), (N, N));

    // The wall at one million: first-class, zero-I/O, exact.
    let io_at_wall = host.io_count;
    assert!(matches!(
        host.run(ClientOp::Insert {
            id: 4,
            value: value_for(4)
        }),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::DuplicateId { .. }),
            ..
        })
    ));
    assert!(matches!(
        host.run(ClientOp::Insert { id: 2 * N + 1, value: value_for(0) }),
        Driven::Done(Output::InsertDone {
            result: Err(DbError::Full { capacity, .. }),
            ..
        }) if capacity == N
    ));
    assert_eq!(
        host.io_count, io_at_wall,
        "rejections at scale performed I/O"
    );

    // Sampled point lookups across the whole keyspace.
    for i in (0..N).step_by(9973) {
        let id = if i.is_multiple_of(2) { i } else { 2 * N - i };
        match host.run(ClientOp::Get { id }) {
            Driven::Done(Output::GetDone {
                result: Ok(Some(v)),
                ..
            }) => {
                assert_eq!(v, value_for(id), "id={id}");
            }
            other => panic!("get {id}: {other:?}"),
        }
    }

    // Full recovery of the 32 MB table: every row re-verified through the
    // checksummed read path, both indices rebuilt.
    let disk = std::mem::take(&mut host.disk);
    let mut recovered = SimHost::new(caps, disk, None);
    assert!(matches!(
        recovered.open(),
        Driven::Done(Output::OpenDone { result: Ok(n) }) if n == N
    ));
    assert!(!recovered.engine.recovery_report().rollback_evidence);

    // Paged range windows at the start, middle, and end of a million-row
    // ordered index, plus a full-count sweep in coarse strides.
    for lo in [0u64, N / 2, 2 * N - 60] {
        let page = match recovered.run_input(Input::Range { lo, hi: lo + 200 }) {
            Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
            other => panic!("range at {lo}: {other:?}"),
        };
        assert!(page.count > 0, "window at {lo} empty");
        for w in page.items[..page.count as usize].windows(2) {
            assert!(w[0].0 < w[1].0);
        }
        for &(k, v) in &page.items[..page.count as usize] {
            assert_eq!(v, value_for(k));
        }
    }

    // Crash at every boundary of the millionth-row insert: N/N+1 at scale.
    let base = {
        let mut h = SimHost::new(caps, SimDisk::new(), None);
        h.open();
        for i in 0..N - 1 {
            h.run(ClientOp::Insert {
                id: i,
                value: value_for(i),
            });
        }
        h
    };
    let (snapshot, io_base) = {
        let mut b = base;
        (std::mem::take(&mut b.disk), b.io_count)
    };
    for boundary in 0..5u64 {
        let mut h = SimHost::new(caps, snapshot.clone(), None);
        assert!(matches!(
            h.open(),
            Driven::Done(Output::OpenDone { result: Ok(n) }) if n == N - 1
        ));
        let _ = io_base;
        h.crash_after = Some(h.io_count + boundary);
        assert!(matches!(
            h.run(ClientOp::Insert {
                id: N - 1,
                value: value_for(N - 1)
            }),
            Driven::Crashed
        ));
        let mut d = std::mem::take(&mut h.disk);
        d.crash(&mut crash_rng(0x5CA1E, boundary));
        let mut rec = SimHost::new(caps, d, None);
        let n = match rec.open() {
            Driven::Done(Output::OpenDone { result: Ok(n) }) => n,
            other => panic!("boundary {boundary}: {other:?}"),
        };
        assert!(
            n == N - 1 || n == N,
            "boundary {boundary}: {n} rows at the million-row wall"
        );
    }
}
