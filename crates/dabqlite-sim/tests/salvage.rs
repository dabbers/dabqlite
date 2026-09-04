//! Corruption containment: one bad row costs one row, not the database.
//!
//! Before salvage mode, a single failed row checksum made the whole
//! database unopenable — correct ("never serve wrong data") but a brutal
//! operational cliff: one rotted bit in row 3 of 100,000 put all 100,000
//! out of reach. Salvage mode keeps the correctness and removes the
//! cliff, by being precise about what it does and does not know:
//!
//! - a row that VERIFIES is served, exactly, as always;
//! - a row that does not is QUARANTINED — never served, never guessed at;
//! - a `Get` HIT is therefore still perfect; a `Get` MISS becomes
//!   `Degraded`, because "absent" and "was in a quarantined slot" are
//!   indistinguishable and answering `None` would be a confident lie;
//! - scans return verified rows with `incomplete: true`, since a silently
//!   short scan is indistinguishable from data loss;
//! - writes are refused, and salvage touches not one byte of the file.
//!
//! The strict open is unchanged and still the default: a database that
//! silently degrades is worse than one that says it is damaged.

use dabqlite_core::{Capacities, DbError, FileId, Input, Output, ROW_SIZE, VALUE_LEN};
use dabqlite_sim::host::ClientOp;
use dabqlite_sim::{gen_workload, Driven, SimDisk, SimHost};

const CAPS: Capacities = Capacities { rows: 32 };

fn build(seed: u64, n: usize) -> (SimDisk, Vec<(u64, [u8; VALUE_LEN])>) {
    let ops = gen_workload(seed, n);
    let mut host = SimHost::new(CAPS, SimDisk::new(), None);
    host.open();
    for &(id, value) in &ops {
        host.run(ClientOp::Insert { id, value });
    }
    (std::mem::take(&mut host.disk), ops)
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

fn get_result(host: &mut SimHost, id: u64) -> Result<Option<[u8; VALUE_LEN]>, DbError> {
    match host.run_input(Input::Get { id }) {
        Driven::Done(Output::GetDone { result, .. }) => result,
        other => panic!("get: {other:?}"),
    }
}

/// The headline property, swept over EVERY row: damaging any one row
/// leaves every other row readable and exact.
#[test]
fn corrupting_any_single_row_costs_exactly_that_row() {
    let n = 8usize;
    for seed in 0..4u64 {
        let (pristine, ops) = build(seed, n);
        for victim in 0..n {
            let ctx = format!("seed={seed} victim={victim}");
            let mut disk = pristine.clone();
            // Flip a bit inside the victim's id field.
            disk.corrupt(FileId::Rows, (victim * ROW_SIZE) as u64, 0x40);

            // Strict open still refuses — the default does not change.
            let (_, strict) = open_strict(disk.clone());
            assert!(
                matches!(strict, Err(DbError::Corrupt { .. })),
                "[{ctx}] strict open must refuse damage: {strict:?}"
            );

            // Salvage open contains it.
            let (mut host, salvaged) = open_salvage(disk);
            assert_eq!(salvaged, Ok(n as u64), "[{ctx}] salvage open");
            assert!(host.engine.is_degraded(), "[{ctx}] should report degraded");
            assert_eq!(host.engine.quarantined(), 1, "[{ctx}] quarantine count");
            assert_eq!(
                host.engine.recovery_report().quarantined_rows,
                1,
                "[{ctx}] report"
            );

            // Every OTHER row is present and byte-exact.
            for (row, &(id, value)) in ops.iter().enumerate() {
                if row == victim {
                    continue;
                }
                assert_eq!(
                    get_result(&mut host, id),
                    Ok(Some(value)),
                    "[{ctx}] surviving row {row} (id={id})"
                );
            }

            // The victim's id is a MISS, and a miss is refused, not denied:
            // we cannot tell "never existed" from "was in the bad slot".
            let victim_id = ops[victim].0;
            assert_eq!(
                get_result(&mut host, victim_id),
                Err(DbError::Degraded { quarantined: 1 }),
                "[{ctx}] a miss in salvage mode must refuse, not answer None"
            );
        }
    }
}

/// Every byte of every row, exhaustively: containment is never a panic,
/// never a wrong answer, and never a lost neighbour.
#[test]
fn every_byte_of_every_row_is_contained() {
    let n = 5usize;
    let (pristine, ops) = build(21, n);
    for victim in 0..n {
        for byte in 0..ROW_SIZE {
            let ctx = format!("victim={victim} byte={byte}");
            let mut disk = pristine.clone();
            let off = (victim * ROW_SIZE + byte) as u64;
            disk.corrupt(FileId::Rows, off, 0x80);

            let (mut host, salvaged) = open_salvage(disk);
            assert_eq!(salvaged, Ok(n as u64), "[{ctx}]");
            let quarantined = host.engine.quarantined();
            // A flip in the padding or checksum still fails verification;
            // there are no dead bytes (layout.rs pins that separately).
            assert_eq!(quarantined, 1, "[{ctx}] exactly one row damaged");

            for (row, &(id, value)) in ops.iter().enumerate() {
                if row == victim {
                    continue;
                }
                assert_eq!(
                    get_result(&mut host, id),
                    Ok(Some(value)),
                    "[{ctx}] neighbour row {row}"
                );
            }
        }
    }
}

/// Many damaged rows: containment scales — the cost is exactly the rows
/// that are damaged, never more.
#[test]
fn many_corrupt_rows_cost_exactly_themselves() {
    let n = 10usize;
    let (pristine, ops) = build(5, n);
    let victims = [1usize, 4, 7, 8];
    let mut disk = pristine.clone();
    for &v in &victims {
        disk.corrupt(FileId::Rows, (v * ROW_SIZE + 3) as u64, 0x11);
    }
    let (mut host, salvaged) = open_salvage(disk);
    assert_eq!(salvaged, Ok(n as u64));
    assert_eq!(host.engine.quarantined(), victims.len() as u64);
    for (row, &(id, value)) in ops.iter().enumerate() {
        if victims.contains(&row) {
            continue;
        }
        assert_eq!(get_result(&mut host, id), Ok(Some(value)), "row {row}");
    }
    // A full range scan returns exactly the survivors, and says it is short.
    let survivors = host.range_all(0, u64::MAX);
    assert_eq!(survivors.len(), n - victims.len());
    for (id, value) in &survivors {
        let expect = ops.iter().find(|(i, _)| i == id).expect("known id");
        assert_eq!(*value, expect.1);
    }
}

/// Scans must announce their own incompleteness: a silently short scan is
/// indistinguishable from data loss.
#[test]
fn scans_in_salvage_mode_declare_themselves_incomplete() {
    let (pristine, ops) = build(3, 6);
    let mut disk = pristine.clone();
    disk.corrupt(FileId::Rows, (2 * ROW_SIZE) as u64, 0x08);
    let (mut host, _) = open_salvage(disk);

    match host.run_input(Input::Range {
        lo: 0,
        hi: u64::MAX,
    }) {
        Driven::Done(Output::RangeDone { result: Ok(page) }) => {
            assert!(page.incomplete, "range page must flag incompleteness");
        }
        other => panic!("range: {other:?}"),
    }
    let mut needle = [0u8; VALUE_LEN];
    needle[..3].copy_from_slice(&ops[0].1[..3]);
    match host.run_input(Input::Find {
        needle,
        needle_len: 3,
        after: None,
    }) {
        Driven::Done(Output::FindDone { result: Ok(page) }) => {
            assert!(page.incomplete, "find page must flag incompleteness");
        }
        other => panic!("find: {other:?}"),
    }
}

/// Salvage is read-only, and provably inert: not one byte of the file
/// changes, and not one write or fsync is issued.
#[test]
fn salvage_writes_not_one_byte() {
    let (pristine, ops) = build(8, 6);
    let mut disk = pristine.clone();
    disk.corrupt(FileId::Rows, (ROW_SIZE + 9) as u64, 0x04);
    let before_sb = disk.contents(FileId::Superblock);
    let before_rows = disk.contents(FileId::Rows);

    let (mut host, _) = open_salvage(disk);
    assert_eq!(host.n_writes, 0, "salvage issued a write");
    assert_eq!(host.n_fsyncs, 0, "salvage issued an fsync");

    // Reads do not change that.
    for &(id, _) in &ops {
        let _ = get_result(&mut host, id);
    }
    let _ = host.range_all(0, u64::MAX);

    assert_eq!(host.n_writes, 0, "a read path wrote something");
    assert_eq!(host.disk.contents(FileId::Superblock), before_sb);
    assert_eq!(host.disk.contents(FileId::Rows), before_rows);

    // Writes are refused outright.
    match host.run_input(Input::Insert {
        id: 987_654,
        value: [3; VALUE_LEN],
    }) {
        Driven::Done(Output::InsertDone {
            result: Err(DbError::Degraded { quarantined }),
            ..
        }) => assert_eq!(quarantined, 1),
        other => panic!("insert in salvage mode must be refused: {other:?}"),
    }
    assert_eq!(host.disk.contents(FileId::Rows), before_rows);
}

/// A salvage open of an UNDAMAGED database is an ordinary open: full
/// read/write, no degradation, and the usual durability work happens.
/// Salvage is a fallback, not a downgrade.
#[test]
fn salvage_of_a_healthy_database_is_an_ordinary_open() {
    for seed in 0..4u64 {
        let (disk, ops) = build(seed, 6);
        let (mut host, salvaged) = open_salvage(disk);
        assert_eq!(salvaged, Ok(ops.len() as u64), "seed={seed}");
        assert!(!host.engine.is_degraded(), "seed={seed}");
        assert_eq!(host.engine.quarantined(), 0, "seed={seed}");
        // Recovery still fsynced (visible implies durable).
        assert!(host.n_fsyncs > 0, "seed={seed}: healthy salvage must fsync");
        // And it is writable.
        match host.run_input(Input::Insert {
            id: 4242,
            value: [9; VALUE_LEN],
        }) {
            Driven::Done(Output::InsertDone { result: Ok(()), .. }) => {}
            other => panic!("seed={seed}: healthy salvage must accept writes: {other:?}"),
        }
        for &(id, value) in &ops {
            assert_eq!(host.get(id), Some(value), "seed={seed}");
        }
        // Pages from a healthy database are never flagged incomplete.
        match host.run_input(Input::Range {
            lo: 0,
            hi: u64::MAX,
        }) {
            Driven::Done(Output::RangeDone { result: Ok(page) }) => {
                assert!(!page.incomplete, "seed={seed}")
            }
            other => panic!("range: {other:?}"),
        }
    }
}

/// A duplicate id (two committed rows claiming one key) is damage too,
/// and is quarantined rather than fatal — the first occurrence wins.
#[test]
fn duplicate_ids_are_quarantined_not_fatal() {
    let n = 6usize;
    let (pristine, ops) = build(15, n);
    // Overwrite row 4 with a byte-perfect copy of row 1: two valid rows,
    // one id. Only a duplicate check can catch this — checksums cannot.
    let mut disk = pristine.clone();
    let rows = disk.contents(FileId::Rows);
    let clone_src = rows[ROW_SIZE..2 * ROW_SIZE].to_vec();
    for (i, &b) in clone_src.iter().enumerate() {
        let off = (4 * ROW_SIZE + i) as u64;
        let existing = rows[4 * ROW_SIZE + i];
        if existing != b {
            disk.corrupt(FileId::Rows, off, existing ^ b);
        }
    }
    assert_eq!(
        disk.contents(FileId::Rows)[4 * ROW_SIZE..5 * ROW_SIZE],
        clone_src[..],
        "the duplicate was not planted"
    );

    let (_, strict) = open_strict(disk.clone());
    assert!(
        matches!(strict, Err(DbError::Corrupt { .. })),
        "strict open must refuse duplicates: {strict:?}"
    );

    let (mut host, salvaged) = open_salvage(disk);
    assert_eq!(salvaged, Ok(n as u64));
    assert_eq!(host.engine.quarantined(), 1, "the later duplicate");
    // The surviving copy still answers, exactly.
    assert_eq!(get_result(&mut host, ops[1].0), Ok(Some(ops[1].1)));
    // Row 4's original id is now unreachable — and refused, not denied.
    assert_eq!(
        get_result(&mut host, ops[4].0),
        Err(DbError::Degraded { quarantined: 1 })
    );
}

/// Damage that destroys the manifest is NOT containable, and salvage does
/// not pretend otherwise: it fails exactly as a strict open does. Salvage
/// widens what can be read, never what can be believed.
#[test]
fn salvage_does_not_paper_over_manifest_damage() {
    let (pristine, _) = build(6, 4);
    // Destroy every superblock copy.
    let sb_len = pristine.contents(FileId::Superblock).len();
    let mut disk = pristine.clone();
    for off in 0..sb_len {
        disk.corrupt(FileId::Superblock, off as u64, 0xFF);
    }
    let (_, strict) = open_strict(disk.clone());
    let (_, salvaged) = open_salvage(disk);
    assert!(matches!(strict, Err(DbError::Corrupt { .. })), "{strict:?}");
    assert_eq!(
        salvaged, strict,
        "salvage must agree with strict open about unreadable manifests"
    );
}
