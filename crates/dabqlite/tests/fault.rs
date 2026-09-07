//! Crash-testing the PUBLIC API.
//!
//! Everything else in this repository injects faults at the engine's
//! protocol seam: `SimHost` drives `Engine` against `SimDisk` and sweeps
//! every I/O boundary. That covers the commit protocol thoroughly and
//! covers `Db` not at all — and `Db` is the code applications actually
//! run. Its multi-step operations in particular (a windowed read, a
//! batch, a rebuild that stages and swaps) are sequences the engine-level
//! suites never see as sequences.
//!
//! So this file injects the same faults one layer up, through the
//! `Storage` seam, and holds the whole public surface to the same rule
//! the engine is held to: **after any crash, at any I/O boundary, the
//! database reads as it did before the interrupted call or as it would
//! after it, and never as anything else.**
//!
//! A "crash" here is what a crash is: the process stops, so unsynced
//! writes may or may not have reached the medium. `FaultStorage` keeps
//! `durable` and `current` images exactly as the simulator's disk does,
//! the failing call unwinds the API instead of the process, and the test
//! then reopens from a settled image rather than from the handle it was
//! holding.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use dabqlite::{Db, Error, FileId, Op, Storage, Value, VALUE_LEN};

// ---------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------

/// The three declared files, in memory, split into what is DURABLE and
/// what is merely written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Images {
    durable: [Vec<u8>; 3],
    current: [Vec<u8>; 3],
    /// Byte ranges written since the last sync of that file, so a crash
    /// can settle each one independently.
    unsynced: Vec<(usize, u64, usize)>,
}

fn slot(file: FileId) -> usize {
    match file {
        FileId::Superblock => 0,
        FileId::Rows => 1,
        FileId::RowsOld => 2,
    }
}

impl Images {
    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) {
        let i = slot(file);
        let end = offset as usize + data.len();
        if end > self.current[i].len() {
            self.current[i].resize(end, 0);
        }
        self.current[i][offset as usize..end].copy_from_slice(data);
        self.unsynced.push((i, offset, data.len()));
    }

    fn sync(&mut self, file: FileId) {
        let i = slot(file);
        self.durable[i] = self.current[i].clone();
        self.unsynced.retain(|&(f, _, _)| f != i);
    }

    fn truncate(&mut self, file: FileId, len: u64) {
        let i = slot(file);
        if (len as usize) < self.current[i].len() {
            self.current[i].truncate(len as usize);
        }
        self.unsynced
            .retain(|&(f, off, n)| f != i || off + n as u64 <= len);
    }

    /// Settle the unsynced writes the way a power cut would: each one
    /// survives whole, vanishes, or tears. `fate` decides, deterministically.
    fn settle(&self, fate: &mut impl FnMut(usize) -> u8) -> [Vec<u8>; 3] {
        let mut out = self.durable.clone();
        for (k, &(i, offset, len)) in self.unsynced.iter().enumerate() {
            let src = &self.current[i];
            let keep = match fate(k) % 3 {
                0 => 0,       // vanished
                1 => len,     // survived whole
                _ => len / 2, // torn
            };
            if keep == 0 {
                continue;
            }
            let end = (offset as usize + keep).min(src.len());
            if end > out[i].len() {
                out[i].resize(end, 0);
            }
            out[i][offset as usize..end].copy_from_slice(&src[offset as usize..end]);
        }
        out
    }
}

/// A backend that fails on demand, and remembers what was durable when
/// it did.
#[derive(Clone)]
struct FaultStorage {
    images: Rc<RefCell<Images>>,
    /// I/O operations performed. Reads, writes, syncs and truncates all
    /// count, so a boundary sweep covers every one of them.
    ops: Rc<RefCell<u64>>,
    /// Fail the operation with this index, and every one after it: a
    /// crashed process does not come back for the next call.
    fail_from: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Crashed;

impl FaultStorage {
    fn new(images: Rc<RefCell<Images>>) -> Self {
        FaultStorage {
            images,
            ops: Rc::new(RefCell::new(0)),
            fail_from: None,
        }
    }

    fn failing_from(images: Rc<RefCell<Images>>, at: u64) -> Self {
        FaultStorage {
            fail_from: Some(at),
            ..FaultStorage::new(images)
        }
    }

    fn count(&self) -> u64 {
        *self.ops.borrow()
    }

    /// Advance the counter and say whether this operation dies.
    fn step(&mut self) -> bool {
        let mut n = self.ops.borrow_mut();
        let at = *n;
        *n += 1;
        self.fail_from.is_some_and(|from| at >= from)
    }
}

impl Storage for FaultStorage {
    type Error = Crashed;

    fn len(&mut self, file: FileId) -> Result<u64, Crashed> {
        if self.step() {
            return Err(Crashed);
        }
        Ok(self.images.borrow().current[slot(file)].len() as u64)
    }

    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Crashed> {
        if self.step() {
            return Err(Crashed);
        }
        let images = self.images.borrow();
        let bytes = &images.current[slot(file)];
        let start = (offset as usize).min(bytes.len());
        let end = (offset.saturating_add(len) as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Crashed> {
        // A crashed write may still have reached the page cache, so the
        // bytes land before the failure — the worst case, and the one the
        // engine must survive.
        let dies = self.step();
        self.images.borrow_mut().write(file, offset, data);
        if dies {
            return Err(Crashed);
        }
        Ok(())
    }

    fn sync(&mut self, file: FileId) -> Result<(), Crashed> {
        if self.step() {
            // A crashed fsync syncs nothing.
            return Err(Crashed);
        }
        self.images.borrow_mut().sync(file);
        Ok(())
    }

    fn truncate(&mut self, file: FileId, len: u64) -> Result<(), Crashed> {
        if self.step() {
            return Err(Crashed);
        }
        self.images.borrow_mut().truncate(file, len);
        Ok(())
    }
}

// ---------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------

const ROWS: u64 = 512;

fn fresh() -> Rc<RefCell<Images>> {
    Rc::new(RefCell::new(Images::default()))
}

fn open(images: &Rc<RefCell<Images>>) -> Db<FaultStorage> {
    Db::with_storage(FaultStorage::new(Rc::clone(images)), ROWS).expect("open")
}

/// Reopen from a SETTLED image: the durable bytes plus whichever unsynced
/// writes survived the crash.
fn reopen_after_crash(images: &Rc<RefCell<Images>>, seed: u64) -> Db<FaultStorage> {
    let settled = {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut fate = move |_k: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 3) as u8
        };
        images.borrow().settle(&mut fate)
    };
    let landed = Rc::new(RefCell::new(Images {
        durable: settled.clone(),
        current: settled,
        unsynced: Vec::new(),
    }));
    Db::with_storage(FaultStorage::new(landed), ROWS).expect("recovery must always succeed")
}

fn model(db: &Db<FaultStorage>) -> BTreeMap<u64, Vec<u8>> {
    db.all()
        .expect("scan")
        .into_iter()
        .map(|(id, v)| (id, v.into_bytes()))
        .collect()
}

fn v(n: u64, len: usize) -> Value {
    let mut bytes = vec![0u8; len];
    for (k, b) in bytes.iter_mut().enumerate() {
        *b = (n as u8).wrapping_mul(31).wrapping_add(k as u8);
    }
    Value::from_vec(bytes).expect("in range")
}

// ---------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------

/// Every write the public API offers, interrupted at EVERY I/O boundary,
/// with the unsynced writes settled three ways: the database always reads
/// as it did before the call or as it would after it.
#[test]
fn every_public_write_is_all_or_nothing_at_every_io_boundary() {
    // One case per write shape the API offers, including the ones that
    // span several slots and the ones that are several writes.
    /// One write the API offers, applied to a database.
    type Case = Box<dyn Fn(&mut Db<FaultStorage>) -> Result<(), Error>>;
    let cases: Vec<(&str, Case)> = vec![
        (
            "insert",
            Box::new(|db: &mut Db<FaultStorage>| db.insert(50, v(50, 8))),
        ),
        (
            "insert long",
            Box::new(|db: &mut Db<FaultStorage>| db.insert(50, v(50, 7 * VALUE_LEN + 3))),
        ),
        (
            "put over",
            Box::new(|db: &mut Db<FaultStorage>| db.put(1, v(99, 40))),
        ),
        (
            "update",
            Box::new(|db: &mut Db<FaultStorage>| db.update(2, v(98, 5))),
        ),
        (
            "delete",
            Box::new(|db: &mut Db<FaultStorage>| db.remove(3).map(|_| ())),
        ),
        (
            "batch",
            Box::new(|db: &mut Db<FaultStorage>| {
                db.batch(&[
                    Op::put(4, v(70, 33)),
                    Op::delete(0),
                    Op::insert(60, v(60, 3 * VALUE_LEN)),
                ])
            }),
        ),
        (
            "guarded batch",
            Box::new(|db: &mut Db<FaultStorage>| {
                db.batch(&[Op::expect(1, v(1, 20)), Op::put(1, v(1, 200))])
            }),
        ),
    ];

    for (name, apply) in &cases {
        // Build the same starting database every time, fault-free.
        let seed_images = fresh();
        {
            let mut db = open(&seed_images);
            for i in 0..5u64 {
                db.insert(i, v(i, 20)).expect("seed");
            }
        }
        let before = {
            let db = open(&seed_images);
            model(&db)
        };
        let after = {
            let images = Rc::new(RefCell::new(seed_images.borrow().clone()));
            let mut db = open(&images);
            apply(&mut db).expect("the operation must succeed without faults");
            model(&db)
        };
        assert_ne!(before, after, "[{name}] the case must change something");

        // How many I/O operations the clean run takes, so the sweep
        // covers every boundary of it and a couple past the end.
        let boundaries = {
            let images = Rc::new(RefCell::new(seed_images.borrow().clone()));
            let mut db = open(&images);
            let base = db.storage().count();
            let _ = apply(&mut db);
            db.storage().count() - base + 2
        };

        let (mut landed_before, mut landed_after) = (0u32, 0u32);
        for boundary in 0..boundaries {
            for settle in 0..3u64 {
                let images = Rc::new(RefCell::new(seed_images.borrow().clone()));
                let ctx = format!("{name} boundary={boundary} settle={settle}");
                {
                    let db = open(&images);
                    let at = db.storage().count() + boundary;
                    // Re-open the same images with the fault armed, so
                    // the boundary counts from the operation itself.
                    let armed = FaultStorage::failing_from(Rc::clone(&images), at);
                    let mut db = Db::with_storage(armed, ROWS).expect("open");
                    // The open itself may be what dies; that is a boundary
                    // worth sweeping too, and it must not panic.
                    let _ = apply(&mut db);
                }
                let recovered = reopen_after_crash(&images, settle * 31 + boundary);
                let got = model(&recovered);
                if got == before {
                    landed_before += 1;
                } else if got == after {
                    landed_after += 1;
                } else {
                    panic!("[{ctx}] the database landed in neither state: {got:?}");
                }
                // And whatever it landed in, it is a working database.
                assert!(!recovered.is_degraded(), "[{ctx}] recovered degraded");
                assert!(
                    !recovered.recovery_report().rollback_evidence,
                    "[{ctx}] an interrupted call was reported as lost data"
                );
            }
        }
        // A crash sweep that never interrupts anything proves nothing.
        // Every case must be caught BOTH before it took effect and after
        // it did, or the boundaries are landing outside the operation.
        assert!(
            landed_before > 0 && landed_after > 0,
            "[{name}] {landed_before} crashes landed before the write and \
             {landed_after} after it — the sweep is not straddling the \
             commit point"
        );
    }
}

/// A long interleaved workload through the PUBLIC API, crashed at a
/// random point in every round, reconciled against a `BTreeMap` after
/// each one.
///
/// The sweep above interrupts one call at a time from a clean start. This
/// is the other half: hundreds of rounds on one database, where every
/// round begins on whatever the last crash left behind. Sequences are
/// where the facade has state the engine does not — a windowed read, a
/// batch built from a projection, a rebuild — and a database that drifts
/// by one row every hundred crashes would pass the sweep and fail here.
///
/// Everything derives from the seed, so a failure reproduces exactly.
#[test]
fn a_long_crashing_workload_never_diverges_from_the_oracle() {
    for seed in 0..12u64 {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut rng = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };

        let images = fresh();
        let mut oracle: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut crashes = 0u32;
        let mut resolved_committed = 0u32;
        let mut resolved_lost = 0u32;

        for round in 0..60u32 {
            let ctx = format!("seed={seed} round={round}");

            // Plan one operation against the oracle, and know what it
            // would leave behind.
            let id = rng() % 40;
            let len = (rng() % 5) as usize * VALUE_LEN + (rng() % 7) as usize;
            let value = v(id, len);
            let (op, mut after) = match rng() % 100 {
                0..=14 if !oracle.is_empty() => {
                    let victim = *oracle
                        .keys()
                        .nth((rng() % oracle.len() as u64) as usize)
                        .expect("non-empty");
                    let mut after = oracle.clone();
                    after.remove(&victim);
                    (vec![Op::remove(victim)], after)
                }
                15..=39 => {
                    // A batch, planned against the projection so it is
                    // legal by construction.
                    let other = rng() % 40;
                    let mut after = oracle.clone();
                    after.insert(id, value.as_bytes().to_vec());
                    after.remove(&other);
                    (vec![Op::put(id, value.clone()), Op::remove(other)], after)
                }
                40..=54 if oracle.contains_key(&id) => {
                    // Guarded: the assertion holds, so the batch commits.
                    let mut after = oracle.clone();
                    after.insert(id, value.as_bytes().to_vec());
                    (
                        vec![
                            Op::expect(id, Value::from_bytes(&oracle[&id]).expect("in range")),
                            Op::put(id, value.clone()),
                        ],
                        after,
                    )
                }
                _ => {
                    let mut after = oracle.clone();
                    after.insert(id, value.as_bytes().to_vec());
                    (vec![Op::put(id, value.clone())], after)
                }
            };
            // A batch of `remove`s that hit nothing writes nothing.
            if after == oracle {
                after = oracle.clone();
            }

            // Sometimes crash inside it, at a boundary drawn from the
            // operation's own I/O length.
            let crash_at = if rng() % 3 == 0 {
                let span = {
                    let probe_images = Rc::new(RefCell::new(images.borrow().clone()));
                    let mut probe = open(&probe_images);
                    let base = probe.storage().count();
                    let _ = probe.batch(&op);
                    probe.storage().count() - base
                };
                (span > 0).then(|| rng() % span)
            } else {
                None
            };

            match crash_at {
                None => {
                    let mut db = open(&images);
                    db.batch(&op).unwrap_or_else(|e| panic!("[{ctx}] {e:?}"));
                    oracle = after;
                    // The images are already durable-consistent: the last
                    // call synced.
                    let db = open(&images);
                    assert_eq!(model(&db), oracle, "[{ctx}] clean round diverged");
                }
                Some(at) => {
                    crashes += 1;
                    let before = oracle.clone();
                    {
                        let base = open(&images).storage().count();
                        let armed = FaultStorage::failing_from(Rc::clone(&images), base + at);
                        if let Ok(mut db) = Db::with_storage(armed, ROWS) {
                            let _ = db.batch(&op);
                        }
                    }
                    let recovered = reopen_after_crash(&images, rng());
                    let got = model(&recovered);
                    if got == after {
                        resolved_committed += 1;
                        oracle = after;
                    } else if got == before {
                        resolved_lost += 1;
                    } else {
                        panic!("[{ctx}] neither state after a crash: {got:?}");
                    }
                    assert!(!recovered.is_degraded(), "[{ctx}] recovered degraded");
                    assert!(
                        !recovered.recovery_report().rollback_evidence,
                        "[{ctx}] an interrupted call was reported as lost data"
                    );
                    // Carry the settled bytes forward: the next round
                    // starts on what the crash actually left.
                    let settled = recovered.storage().images.borrow().clone();
                    *images.borrow_mut() = settled;
                }
            }

            // The whole read surface, against the oracle, every round.
            let db = open(&images);
            assert_eq!(model(&db), oracle, "[{ctx}] scan diverged");
            for (&k, want) in &oracle {
                assert_eq!(
                    db.get(k).unwrap().map(Value::into_bytes).as_deref(),
                    Some(want.as_slice()),
                    "[{ctx}] get({k})"
                );
            }
            let mut descending: Vec<u64> = db
                .range_rev(0, u64::MAX)
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            let mut ascending: Vec<u64> = oracle.keys().copied().collect();
            ascending.reverse();
            assert_eq!(descending, ascending, "[{ctx}] descending scan");
            descending.truncate(3);
            assert_eq!(
                db.last(3)
                    .unwrap()
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect::<Vec<_>>(),
                descending,
                "[{ctx}] last(3)"
            );
            // Search, in every mode, against the same oracle.
            if let Some(needle) = oracle.values().find(|v| v.len() >= 3).cloned() {
                for (mode, want) in [
                    (
                        dabqlite::Match::Contains,
                        oracle
                            .iter()
                            .filter(|(_, v)| v.windows(needle.len()).any(|w| w == needle))
                            .count(),
                    ),
                    (
                        dabqlite::Match::Exact,
                        oracle.values().filter(|v| **v == needle).count(),
                    ),
                ] {
                    assert_eq!(
                        db.find_matching(&needle, mode).unwrap().len(),
                        want,
                        "[{ctx}] {mode:?}"
                    );
                }
            }
            // And a rebuild of whatever survived loses nothing.
            if round % 17 == 0 {
                let mut db = open(&images);
                let rebuilt = db.compact_to_memory().expect("rebuild");
                assert_eq!(
                    rebuilt
                        .all()
                        .unwrap()
                        .into_iter()
                        .map(|(k, v)| (k, v.into_bytes()))
                        .collect::<BTreeMap<_, _>>(),
                    oracle,
                    "[{ctx}] a rebuild lost something"
                );
            }
        }

        assert!(crashes > 5, "seed={seed}: only {crashes} crashes");
        assert!(
            resolved_committed > 0 && resolved_lost > 0,
            "seed={seed}: {resolved_committed} crashed calls committed and \
             {resolved_lost} did not — the crashes are not straddling the \
             commit point"
        );
    }
}

/// **Growing performs no write, so a fault during it cannot cost a byte.**
///
/// `Db::grow` re-runs recovery against the same storage handle at a larger
/// capacity. Recovery reads, and can also truncate and fsync when it has
/// residue to clean up — so "growing writes nothing" is a claim about a
/// path that touches storage, not an obvious one. This sweeps a failure
/// over every I/O boundary of the grow and holds two things at each: the
/// call either succeeds or reports its failure, never both and never
/// neither; and whatever it did, the DURABLE bytes are what they were
/// before it was called, so the database a later process opens is
/// untouched.
#[test]
fn a_fault_at_every_boundary_of_a_grow_costs_nothing() {
    let mut swept = 0usize;
    let mut failures = 0usize;
    let mut successes = 0usize;
    for boundary in 0..24u64 {
        // A database with something in it, including a multi-slot value,
        // so the replay a grow performs has real work to do.
        let images = fresh();
        let mut db = open(&images);
        for id in 0..8u64 {
            db.put(id, v(id, 8 + (id as usize % 3) * VALUE_LEN))
                .expect("put");
        }
        db.remove(3).expect("remove");
        let before = model(&db);
        let durable_before = images.borrow().durable.clone();
        drop(db);

        // Reopen with a storage armed to fail from `boundary`. The open
        // itself may be what fails; that is not this test's subject, so
        // it is skipped rather than asserted about.
        let armed = FaultStorage::failing_from(Rc::clone(&images), boundary);
        let Ok(mut db) = Db::with_storage(armed, ROWS) else {
            continue;
        };
        if model(&db) != before {
            // The open recovered to a different state; the grow below
            // would be growing a different database.
            continue;
        }
        swept += 1;
        match db.grow(ROWS * 4) {
            Ok(cap) => {
                successes += 1;
                assert_eq!(cap, ROWS * 4);
                assert_eq!(db.stats().capacity, ROWS * 4);
                assert_eq!(model(&db), before, "a successful grow lost rows");
            }
            Err(_) => {
                failures += 1;
                // The handle is still there and still answers — it
                // reports its failure rather than having vanished.
                assert!(db.stats().capacity >= ROWS);
            }
        }
        assert_eq!(
            images.borrow().durable,
            durable_before,
            "boundary {boundary}: growing made a durable change"
        );
        // And the database a later process opens is the one that was
        // there before the grow was attempted.
        drop(db);
        let recovered = Db::with_storage(FaultStorage::new(Rc::clone(&images)), ROWS)
            .expect("recovery must always succeed");
        assert_eq!(
            model(&recovered),
            before,
            "boundary {boundary}: the database changed under a failed grow"
        );
    }
    assert!(swept >= 8, "only {swept} boundaries reached the grow");
    assert!(
        failures > 0 && successes > 0,
        "the sweep must see both outcomes, saw {successes} ok and {failures} failed"
    );
}
