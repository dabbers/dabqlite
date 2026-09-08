//! Values longer than one row slot, held to the standard everything else
//! is held to.
//!
//! A long value is a run of slots — a head plus continuations — written
//! inside ONE commit. That is the whole design: it makes a long value as
//! atomic as a short one, because it IS one commit, and it means a crash
//! can never leave half a value behind.
//!
//! What this suite proves:
//!
//! - a value round-trips byte for byte at every length, including the
//!   lengths that straddle a slot boundary, and including values that end
//!   in zeros;
//! - crashing or failing I/O at EVERY boundary of a long-value write
//!   leaves the value entirely present or entirely absent — never a
//!   prefix;
//! - a damaged continuation costs its own value and NOTHING else: strict
//!   open refuses by name, salvage quarantines exactly that value's rows,
//!   and every other row is still served;
//! - a head is never served without its continuations, which is the
//!   silent-truncation failure this format's CONTINUES bit exists to make
//!   impossible;
//! - substring search still finds matches that straddle a slot seam;
//! - updating a long value with a short one (and back) is exact, and the
//!   slots it leaves behind are accounted for.

use dabqlite_core::layout::{encode_row, RowKind};
use dabqlite_core::{
    BatchOp, Capacities, DbError, FileId, Input, Output, MAX_COMMIT_ROWS, MAX_VALUE_LEN, ROW_SIZE,
    VALUE_LEN,
};
use dabqlite_sim::workload::crash_rng;
use dabqlite_sim::{Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 512 };

/// A deterministic value of exactly `len` bytes. Leaked so batch ops can
/// borrow it inline; a test binary exits and never needs it back.
fn payload(seed: u64, len: usize) -> &'static [u8] {
    let mut v = vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (seed.wrapping_mul(0x9E37_79B9).wrapping_add(i as u64 * 31) & 0xFF) as u8;
    }
    Box::leak(v.into_boxed_slice())
}

fn fresh() -> SimHost {
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    host
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

fn open_salvage(disk: SimDisk) -> (SimHost, Result<u64, DbError>) {
    let mut host = SimHost::new(CAPS, disk, None);
    let r = match host.open_salvage() {
        Driven::Done(Output::OpenDone { result }) => result,
        other => panic!("salvage open: {other:?}"),
    };
    (host, r)
}

fn put(host: &mut SimHost, id: u64, value: &[u8]) -> Result<u64, DbError> {
    match host.batch(&[BatchOp::Put { id, value }]) {
        Driven::Done(Output::BatchDone {
            rows,
            result: Ok(()),
        }) => Ok(rows),
        Driven::Done(Output::BatchDone {
            result: Err(reject),
            ..
        }) => Err(reject.error),
        other => panic!("put: {other:?}"),
    }
}

/// Slots a value of `len` bytes needs: one per row-width, and at least one
/// even when empty — an empty value is still a value.
fn slots(len: usize) -> u64 {
    len.div_ceil(VALUE_LEN).max(1) as u64
}

// ---------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------

#[test]
fn a_value_of_any_length_round_trips_byte_for_byte() {
    // Every boundary that matters: empty, inside a slot, exactly a slot,
    // one over, several, and the ceiling itself.
    let lengths = [
        0usize,
        1,
        VALUE_LEN - 1,
        VALUE_LEN,
        VALUE_LEN + 1,
        VALUE_LEN * 2,
        VALUE_LEN * 2 + 1,
        100,
        MAX_VALUE_LEN - 1,
        MAX_VALUE_LEN,
    ];
    for (i, &len) in lengths.iter().enumerate() {
        let mut host = fresh();
        let value = payload(i as u64, len);
        assert_eq!(put(&mut host, 1, value), Ok(slots(len)), "len {len}");
        assert_eq!(
            host.get_bytes(1).as_deref(),
            Some(value),
            "len {len} did not round-trip in memory"
        );

        // And from the file alone.
        let disk = std::mem::take(&mut host.disk);
        let (mut host, live) = open(disk);
        assert_eq!(live, 1, "len {len}: one value is one live record");
        assert_eq!(
            host.get_bytes(1).as_deref(),
            Some(value),
            "len {len} did not survive a restart"
        );
    }
}

/// Trailing zeros are data. A store that trims them is a store that
/// silently shortens binary payloads.
#[test]
fn a_value_that_ends_in_zeros_keeps_them() {
    let mut host = fresh();
    for (id, case) in [
        &[0u8][..],
        &[0u8; VALUE_LEN][..],
        &[0u8; VALUE_LEN * 3][..],
        b"tail\0\0\0",
    ]
    .iter()
    .enumerate()
    {
        assert!(put(&mut host, id as u64, case).is_ok());
        assert_eq!(
            host.get_bytes(id as u64).as_deref(),
            Some(*case),
            "a value ending in zeros came back short"
        );
    }
}

#[test]
fn a_value_longer_than_a_commit_can_carry_is_refused_before_any_io() {
    let mut host = fresh();
    let io_before = host.io_count;
    let too_long = payload(1, MAX_VALUE_LEN + 1);
    assert_eq!(
        put(&mut host, 1, too_long),
        Err(DbError::ValueTooLong {
            len: (MAX_VALUE_LEN + 1) as u32,
            max: MAX_VALUE_LEN as u32,
        })
    );
    assert_eq!(
        host.io_count, io_before,
        "a refused value performed I/O anyway"
    );
    assert_eq!(host.get_bytes(1), None);
    // And the ceiling itself is accepted, so the refusal is a boundary
    // rather than a blanket.
    assert!(put(&mut host, 1, payload(2, MAX_VALUE_LEN)).is_ok());
}

#[test]
fn replacing_a_long_value_with_a_short_one_and_back_is_exact() {
    let mut host = fresh();
    let long = payload(1, 200);
    let short = payload(2, 3);
    assert_eq!(put(&mut host, 7, long), Ok(slots(200)));
    assert_eq!(put(&mut host, 7, short), Ok(1));
    assert_eq!(host.get_bytes(7).as_deref(), Some(short));
    assert_eq!(put(&mut host, 7, long), Ok(slots(200)));
    assert_eq!(host.get_bytes(7).as_deref(), Some(long));

    // Every slot is accounted for: one live value, and the rest dead
    // weight a rebuild would compact.
    assert_eq!(host.engine.live_count(), 1);
    let (slots_used, _) = host.engine.usage();
    assert_eq!(slots_used, slots(200) + 1 + slots(200));

    let disk = std::mem::take(&mut host.disk);
    let (mut host, _) = open(disk);
    assert_eq!(
        host.get_bytes(7).as_deref(),
        Some(long),
        "replay disagreed with the live engine"
    );
}

#[test]
fn deleting_a_long_value_removes_all_of_it() {
    let mut host = fresh();
    let long = payload(1, 200);
    put(&mut host, 7, long).unwrap();
    match host.batch(&[dabqlite_core::BatchOp::Delete { id: 7 }]) {
        Driven::Done(Output::BatchDone {
            result: Ok(()),
            rows: 1,
        }) => {}
        other => panic!("delete: {other:?}"),
    }
    assert_eq!(host.get_bytes(7), None);
    let disk = std::mem::take(&mut host.disk);
    let (mut host, live) = open(disk);
    assert_eq!(live, 0);
    assert_eq!(
        host.get_bytes(7),
        None,
        "a deleted long value came back after a restart"
    );
}

// ---------------------------------------------------------------------
// Crash and I/O failure
// ---------------------------------------------------------------------

/// THE property for long values: a crash anywhere inside writing one
/// leaves it entirely present or entirely absent. A prefix would be a
/// value silently cut short, which is the failure this whole design
/// exists to make impossible.
#[test]
fn a_crash_at_every_boundary_of_a_long_write_is_all_or_nothing() {
    for &len in &[VALUE_LEN + 1, 100, VALUE_LEN * 8] {
        let mut base_host = fresh();
        let neighbour = payload(99, 40);
        put(&mut base_host, 1, neighbour).unwrap();
        let base = std::mem::take(&mut base_host.disk);
        let value = payload(7, len);
        let rows = slots(len);

        for boundary in 0..(rows + 4) {
            for settle in 0..3u64 {
                let ctx = format!("len={len} boundary={boundary} settle={settle}");
                let mut host = SimHost::new(CAPS, base.clone(), None);
                host.open();
                host.crash_after = Some(host.io_count + boundary);
                let _ = host.batch(&[BatchOp::Put { id: 2, value }]);

                let mut disk = std::mem::take(&mut host.disk);
                let mut rng = crash_rng(0x10_4EA7, settle);
                disk.crash(&mut rng);

                let (mut host, live) = open(disk);
                match host.get_bytes(2) {
                    None => assert_eq!(live, 1, "[{ctx}] absent value still counted"),
                    Some(got) => {
                        assert_eq!(
                            got, value,
                            "[{ctx}] a long value came back changed or CUT SHORT"
                        );
                        assert_eq!(live, 2, "[{ctx}]");
                    }
                }
                // The neighbour is untouched either way.
                assert_eq!(
                    host.get_bytes(1).as_deref(),
                    Some(neighbour),
                    "[{ctx}] neighbour damaged"
                );
            }
        }
    }
}

#[test]
fn an_io_failure_at_every_boundary_of_a_long_write_fail_stops_cleanly() {
    let len = 100usize;
    let rows = slots(len);
    let mut base_host = fresh();
    put(&mut base_host, 1, payload(99, 40)).unwrap();
    let base = std::mem::take(&mut base_host.disk);
    let value = payload(7, len);

    for fail_at in 0..(rows + 4) {
        let ctx = format!("fail_at={fail_at}");
        let mut host = SimHost::new(CAPS, base.clone(), None);
        host.open();
        host.fail_after = Some(host.io_count + fail_at);
        match host.batch(&[BatchOp::Put { id: 2, value }]) {
            Driven::Done(Output::BatchDone { result: Err(r), .. }) => {
                assert!(matches!(r.error, DbError::IoFailed { .. }), "[{ctx}] {r:?}")
            }
            Driven::Done(Output::BatchDone { result: Ok(()), .. }) => {}
            other => panic!("[{ctx}] {other:?}"),
        }
        let disk = std::mem::take(&mut host.disk);
        let (mut host, _) = open(disk);
        if let Some(got) = host.get_bytes(2) {
            assert_eq!(got, value, "[{ctx}] a long value came back CUT SHORT");
        }
    }
}

// ---------------------------------------------------------------------
// Damage containment
// ---------------------------------------------------------------------

/// The reason the format carries a CONTINUES bit. Damage a value's
/// continuation and the value must go — head included — rather than
/// coming back silently short. And the damage must stop there: every
/// other value is still exact.
#[test]
fn a_damaged_continuation_costs_its_own_value_and_nothing_else() {
    // Three values: a short one, a long one, another short one.
    let mut host = fresh();
    let a = payload(1, 10);
    let long = payload(2, 100);
    let c = payload(3, 12);
    put(&mut host, 1, a).unwrap();
    put(&mut host, 2, long).unwrap();
    put(&mut host, 3, c).unwrap();
    let long_rows = slots(100);
    let base = std::mem::take(&mut host.disk);

    // Damage each continuation in turn (row 1 is the long value's head).
    for k in 1..long_rows {
        let ctx = format!("chunk {k}");
        let mut disk = base.clone();
        disk.corrupt(FileId::Rows, ((1 + k) as usize * ROW_SIZE + 5) as u64, 0x40);

        let (_, strict) = open_strict(disk.clone());
        assert!(
            matches!(strict, Err(DbError::Corrupt { .. })),
            "[{ctx}] strict open must refuse a broken value: {strict:?}"
        );

        let (mut host, salvaged) = open_salvage(disk);
        assert_eq!(salvaged, Ok(2), "[{ctx}] the two short values must survive");
        assert_eq!(
            host.engine.quarantined(),
            long_rows,
            "[{ctx}] the whole broken value should be quarantined, head included"
        );
        assert_eq!(
            host.get_bytes(1).as_deref(),
            Some(a),
            "[{ctx}] a neighbour was lost"
        );
        assert_eq!(
            host.get_bytes(3).as_deref(),
            Some(c),
            "[{ctx}] a neighbour was lost"
        );
        // The critical one: the damaged value must NOT come back as a
        // prefix. Its head is intact on disk, and serving it alone would
        // be serving a value silently cut short.
        assert_eq!(
            get_result(&mut host, 2),
            Err(DbError::Degraded {
                quarantined: long_rows
            }),
            "[{ctx}] a head was served without its continuations"
        );
    }
}

fn get_result(host: &mut SimHost, id: u64) -> Result<Option<Vec<u8>>, DbError> {
    match host.run_input(Input::Get { id }) {
        Driven::Done(Output::GetDone { result, .. }) => {
            result.map(|v| v.map(|w| w.payload().to_vec()))
        }
        other => panic!("get: {other:?}"),
    }
}

/// The complement, and the reason the CONTINUES bit is worth a byte: a
/// damaged row that is NOT a continuation costs exactly one row. Without
/// an explicit end marker, recovery would have to assume the row before
/// it might have owned it, and every corrupt row would take its
/// predecessor down too.
#[test]
fn a_damaged_row_after_a_short_value_costs_only_itself() {
    let mut host = fresh();
    for i in 0..5u64 {
        put(&mut host, i, payload(i, 10)).unwrap();
    }
    let mut disk = std::mem::take(&mut host.disk);
    disk.corrupt(FileId::Rows, (3 * ROW_SIZE + 5) as u64, 0x40);

    let (mut host, salvaged) = open_salvage(disk);
    assert_eq!(
        host.engine.quarantined(),
        1,
        "one damaged row must cost exactly one row"
    );
    assert_eq!(salvaged, Ok(4));
    for i in [0u64, 1, 2, 4] {
        assert_eq!(
            host.get_bytes(i).as_deref(),
            Some(payload(i, 10)),
            "row {i}"
        );
    }
}

/// A value whose head promises a continuation the file does not have —
/// truncation, or a manifest naming fewer rows than the value needs —
/// is refused by name rather than served short.
#[test]
fn a_value_promising_a_continuation_that_is_not_there_is_refused_by_name() {
    let mut host = fresh();
    put(&mut host, 1, payload(1, 10)).unwrap();
    let mut disk = std::mem::take(&mut host.disk);

    // Rewrite the single committed row as a head that claims to continue.
    let mut planted = [0u8; ROW_SIZE];
    let mut value = [0u8; VALUE_LEN];
    value[..10].copy_from_slice(&payload(1, 10)[..10]);
    encode_row(RowKind::Record, 0, 10, true, 1, &value, &mut planted);
    disk.write(FileId::Rows, 0, &planted);

    let (_, strict) = open_strict(disk.clone());
    assert_eq!(
        strict,
        Err(DbError::Corrupt {
            what: dabqlite_core::defect::TRUNCATED_VALUE
        }),
        "a value cut off by the end of the file must be refused, and named"
    );

    let (mut host, salvaged) = open_salvage(disk);
    assert_eq!(salvaged, Ok(0));
    assert_eq!(host.engine.quarantined(), 1);
    assert_eq!(
        get_result(&mut host, 1),
        Err(DbError::Degraded { quarantined: 1 }),
        "a truncated value must not be served as a prefix"
    );
}

// ---------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------

/// A substring that straddles a slot boundary is still found. Without
/// this, a long value would be quietly unsearchable across its own seams —
/// a scan that silently misses matches, which is indistinguishable from
/// data loss to whoever is reading it.
#[test]
fn substring_search_crosses_slot_seams_and_stays_exact() {
    let mut host = fresh();
    // Place the needle so it spans the boundary between slot 0 and 1.
    let mut spanning = vec![b'.'; 80];
    spanning[VALUE_LEN - 3..VALUE_LEN + 3].copy_from_slice(b"needle");
    put(&mut host, 1, &spanning).unwrap();
    // A decoy that contains the needle nowhere, and a short value that
    // does contain it.
    put(&mut host, 2, &[b'x'; 80]).unwrap();
    put(&mut host, 3, b"a needle here").unwrap();

    let hits: Vec<u64> = find_ids(&mut host, b"needle");
    assert_eq!(
        hits,
        vec![1, 3],
        "a match across a slot seam was missed, or a non-match was returned"
    );

    // The same after a restart, so it is the rebuilt index being tested
    // and not the one built at write time.
    let disk = std::mem::take(&mut host.disk);
    let (mut host, _) = open(disk);
    assert_eq!(find_ids(&mut host, b"needle"), vec![1, 3]);
}

/// Reading a value is LINEAR in its length, not quadratic.
///
/// A value is read one slot at a time, and every window has to carry the
/// whole value's length — so measuring the run once per WINDOW meant a
/// k-slot read walked the run k times and decoded k²/2 rows. A key/value
/// store measured the consequence: 0.5 us for a 16-byte value against
/// 720 us for 2 KiB, a 1,400x jump for 128x the bytes, with the cost per
/// slot itself growing linearly.
///
/// Counting run measurements is the honest way to test it: it is the work
/// that was being repeated, and it does not depend on how fast this
/// machine happens to be.
#[test]
fn reading_a_value_measures_its_run_once_not_once_per_slot() {
    let caps = Capacities { rows: 1024 };
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();

    // Values from one slot to the longest the format allows.
    let sizes = [
        1usize,
        VALUE_LEN,
        4 * VALUE_LEN,
        64 * VALUE_LEN,
        MAX_VALUE_LEN,
    ];
    for (i, &len) in sizes.iter().enumerate() {
        let value: Vec<u8> = (0..len).map(|k| (k as u8).wrapping_mul(31)).collect();
        assert!(matches!(
            host.batch(&[BatchOp::Insert {
                id: i as u64,
                value: &value
            }]),
            Driven::Done(Output::BatchDone { result: Ok(()), .. })
        ));

        let before = host.engine.extent_walks();
        let got = host.get_bytes(i as u64).expect("readable");
        let walks = host.engine.extent_walks() - before;
        assert_eq!(got, value, "len={len}");
        assert!(
            walks <= 1,
            "reading a {len}-byte value measured its run {walks} times; it \
             should be measured once however many slots it spans"
        );
    }

    // And a read is a handful of round trips, not one per slot: a window
    // carries sixteen slots, so 2 KiB is eight calls rather than 128.
    for (i, &len) in sizes.iter().enumerate() {
        let mut windows = 0usize;
        let first = match host.run_input(Input::Get { id: i as u64 }) {
            Driven::Done(Output::GetDone {
                result: Ok(Some(w)),
                ..
            }) => w,
            other => panic!("{other:?}"),
        };
        windows += 1;
        let mut next = first.next_offset();
        while let Some(offset) = next {
            let w = match host.run_input(Input::GetFrom {
                id: i as u64,
                offset,
            }) {
                Driven::Done(Output::GetDone {
                    result: Ok(Some(w)),
                    ..
                }) => w,
                other => panic!("{other:?}"),
            };
            windows += 1;
            next = w.next_offset();
        }
        let slots = len.div_ceil(VALUE_LEN).max(1);
        assert_eq!(
            windows,
            slots.div_ceil(dabqlite_core::WINDOW_LEN / VALUE_LEN),
            "a {len}-byte value ({slots} slots) took {windows} windows"
        );
    }

    // The property that was actually broken: cost per SLOT must not grow
    // with the number of slots.
    let short = {
        let before = host.engine.extent_walks();
        host.get_bytes(1).unwrap();
        host.engine.extent_walks() - before
    };
    let longest = {
        let before = host.engine.extent_walks();
        host.get_bytes(4).unwrap();
        host.engine.extent_walks() - before
    };
    assert_eq!(
        short, longest,
        "a 128-slot value must cost the same number of run measurements as \
         a one-slot one"
    );
}

/// Every match, in ascending id order. Pages arrive newest-first.
fn find_ids(host: &mut SimHost, needle: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = match host.run_input(Input::Find {
            needle,
            mode: dabqlite_core::Match::Contains,
            after,
        }) {
            Driven::Done(Output::FindDone { result: Ok(p) }) => p,
            other => panic!("find: {other:?}"),
        };
        out.extend(page.items[..page.count as usize].iter().map(|r| r.id));
        match page.next {
            Some(n) => after = Some(n),
            None => {
                out.reverse();
                return out;
            }
        }
    }
}

/// A scan page carries a long value's LENGTH even though it cannot carry
/// its bytes, so a caller can never mistake the head slot for the whole
/// value.
#[test]
fn a_scan_page_reports_a_long_values_length_rather_than_a_prefix() {
    let mut host = fresh();
    let short = payload(1, 5);
    let long = payload(2, 100);
    put(&mut host, 1, short).unwrap();
    put(&mut host, 2, long).unwrap();

    let page = match host.run_input(Input::Range { lo: 0, hi: 100 }) {
        Driven::Done(Output::RangeDone { result: Ok(p) }) => p,
        other => panic!("range: {other:?}"),
    };
    assert_eq!(page.count, 2);
    assert_eq!(page.items[0].id, 1);
    assert_eq!(page.items[0].len, 5);
    assert_eq!(
        page.items[0].value(),
        Some(short),
        "a value that fits should travel in the page"
    );
    assert_eq!(page.items[1].id, 2);
    assert_eq!(
        page.items[1].len, 100,
        "the page must report the real length"
    );
    assert_eq!(
        page.items[1].value(),
        None,
        "a value that does not fit must be refused, not truncated"
    );
}

// ---------------------------------------------------------------------
// Capacity
// ---------------------------------------------------------------------

#[test]
fn a_long_value_that_would_not_fit_is_refused_before_any_io() {
    const SMALL: Capacities = Capacities { rows: 8 };
    let mut host = SimHost::new(SMALL, SimDisk::new(), None);
    host.open();
    put(&mut host, 1, payload(1, 5)).unwrap(); // one slot; seven left
    let io_before = host.io_count;
    // Nine slots' worth: more than the whole database, let alone what is
    // left of it.
    assert_eq!(
        put(&mut host, 2, payload(2, VALUE_LEN * 9)),
        Err(DbError::Full {
            entity: "records",
            capacity: 8,
            dead: 0,
        })
    );
    assert_eq!(host.io_count, io_before, "a refused value performed I/O");
    assert_eq!(host.get_bytes(2), None);
    // Exactly filling the remainder is accepted.
    assert_eq!(put(&mut host, 2, payload(3, VALUE_LEN * 7)), Ok(7));
    assert_eq!(host.engine.usage(), (8, 8));
}

#[test]
fn a_batch_of_long_values_is_bounded_by_the_commit_not_the_op_count() {
    // The bound is the COMMIT's width in slots, and nothing else. It is not
    // the number of operations, and — since v6 widened the span field — it
    // is no longer the width of a single value either: a maximum-length
    // value costs `MAX_VALUE_LEN / VALUE_LEN` slots out of a commit that
    // can carry `MAX_COMMIT_ROWS`, so several of them compose atomically.
    const PER_OP: u64 = (MAX_VALUE_LEN / VALUE_LEN) as u64;
    const FITS: u64 = MAX_COMMIT_ROWS as u64 / PER_OP;
    const _: () = assert!(
        FITS > 1,
        "the largest value must compose with at least one other"
    );
    assert_eq!(
        FITS * PER_OP,
        MAX_COMMIT_ROWS as u64,
        "this test reads the boundary exactly, so it must land on one"
    );

    let big = payload(1, MAX_VALUE_LEN);
    // The longest commit the format allows needs a database that can hold
    // it, which is more than the rest of this suite works in.
    let caps = Capacities {
        rows: (MAX_COMMIT_ROWS as u64 + 8).max(CAPS.rows),
    };

    // Exactly a commit's worth of maximum-length values: accepted, in one
    // commit, and every one of them readable afterwards.
    let mut host = SimHost::new(caps, SimDisk::new(), None);
    host.open();
    let ops: Vec<BatchOp> = (0..FITS)
        .map(|i| BatchOp::Put { id: i, value: big })
        .collect();
    match host.batch(&ops) {
        Driven::Done(Output::BatchDone {
            rows,
            result: Ok(()),
        }) => assert_eq!(rows, MAX_COMMIT_ROWS as u64),
        other => panic!("a commit-sized batch of long values: {other:?}"),
    }
    for i in 0..FITS {
        assert_eq!(host.get_bytes(i).as_deref(), Some(big), "value {i}");
    }

    // One more of them does not fit, and says so by name, before any I/O
    // — in a database with room to spare, so the refusal is about the
    // COMMIT's width and not about the database being full.
    let roomy = Capacities {
        rows: (MAX_COMMIT_ROWS as u64 * 2 + 8).max(CAPS.rows),
    };
    let mut host = SimHost::new(roomy, SimDisk::new(), None);
    host.open();
    let ops: Vec<BatchOp> = (0..=FITS)
        .map(|i| BatchOp::Put { id: i, value: big })
        .collect();
    let io_before = host.io_count;
    match host.batch(&ops) {
        Driven::Done(Output::BatchDone {
            rows: 0,
            result: Err(reject),
        }) => {
            assert_eq!(
                u64::from(reject.at),
                FITS,
                "the operation that crosses the boundary is the one blamed"
            );
            assert_eq!(
                reject.error,
                DbError::BatchTooLong {
                    rows: (FITS + 1) * PER_OP,
                    max: MAX_COMMIT_ROWS as u64
                }
            );
        }
        other => panic!("expected a refusal: {other:?}"),
    }
    assert_eq!(host.io_count, io_before);
    assert_eq!(host.get_bytes(0), None, "nothing may be written");
}

/// **A long value costs ONE commit's fsyncs, whatever its length.**
///
/// The job-queue sample asserts this by timing, which is the wrong
/// instrument: wall-clock mixes the fsyncs with the row writes and with
/// the index work, so the number moves whenever any of those move and
/// nobody can tell which. Here it is counted instead, which is what the
/// claim actually says.
#[test]
fn a_value_of_any_length_costs_the_same_two_fsyncs() {
    let caps = Capacities {
        rows: (MAX_COMMIT_ROWS as u64 * 2 + 16).max(CAPS.rows),
    };
    let mut baseline = None;
    for slots in [1usize, 2, 8, 32, 64, MAX_VALUE_LEN / VALUE_LEN] {
        let mut host = SimHost::new(caps, SimDisk::new(), None);
        host.open();
        let before = (host.n_fsyncs, host.n_writes);
        let value = payload(slots as u64, slots * VALUE_LEN);
        assert_eq!(put(&mut host, 1, value), Ok(slots as u64));
        let fsyncs = host.n_fsyncs - before.0;
        let writes = host.n_writes - before.1;

        // Two: the rows, then the superblock that names them.
        assert_eq!(fsyncs, 2, "{slots} slots cost {fsyncs} fsyncs");
        match baseline {
            None => baseline = Some(fsyncs),
            Some(b) => assert_eq!(fsyncs, b, "{slots} slots changed the fsync count"),
        }
        // Writes DO scale with slots — one per row plus the superblock
        // pair — and saying so is the other half of an honest claim. It
        // is the fsyncs that are shared, not the I/O.
        assert_eq!(
            writes,
            slots as u64 + 2,
            "{slots} slots: one write per row plus the superblock pair"
        );
    }
}
