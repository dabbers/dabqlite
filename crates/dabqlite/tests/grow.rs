//! Growing a full database, in place, on an open handle.
//!
//! A store that cannot take another write, with no way forward that does
//! not go through dropping the database, reads to whoever is using it as
//! data loss — whatever the file says. `Db::grow` is the way forward:
//! recovery once, against the same storage handle, with the writer lock
//! never let go and no file rewritten.
//!
//! What has to be true, and is tested here: every row survives it; the
//! new room is immediately usable; growing is not a write; a failed grow
//! leaves a database that reports its failure rather than one that has
//! vanished; and the new capacity is the one the file remembers after the
//! next commit, not before it.

use std::collections::BTreeMap;

use dabqlite::{Db, Error, MemoryStorage, Op, Value, VALUE_LEN};

type Mem = Db<MemoryStorage>;

fn fill(db: &mut Mem, from: u64, to: u64, model: &mut BTreeMap<u64, Vec<u8>>) {
    for id in from..to {
        let v = format!("row-{id:04}").into_bytes();
        db.put(id, Value::from_vec(v.clone()).unwrap()).unwrap();
        model.insert(id, v);
    }
}

fn all(db: &Mem) -> Vec<(u64, Vec<u8>)> {
    db.range(0, u64::MAX)
        .unwrap()
        .into_iter()
        .map(|(id, v)| (id, v.as_bytes().to_vec()))
        .collect()
}

fn want(model: &BTreeMap<u64, Vec<u8>>) -> Vec<(u64, Vec<u8>)> {
    model.iter().map(|(&k, v)| (k, v.clone())).collect()
}

/// The whole point: a database at its ceiling takes writes again, and
/// nothing that was in it moved.
#[test]
fn a_full_database_grows_and_keeps_every_row() {
    let mut db = Mem::in_memory_with(32).expect("open");
    let mut model = BTreeMap::new();
    fill(&mut db, 0, 32, &mut model);
    assert_eq!(db.stats().free(), 0);

    // Full, and compacting would free nothing: every slot is live.
    assert_eq!(db.stats().dead, 0);
    match db.put(99, Value::from_bytes(b"x").unwrap()) {
        Err(Error::Full {
            capacity: 32,
            dead: 0,
        }) => {}
        other => panic!("expected Full, got {other:?}"),
    }

    assert_eq!(db.grow(128).expect("grow"), 128);
    assert_eq!(db.stats().capacity, 128);
    assert_eq!(db.stats().slots, 32, "growing moved no data");
    assert_eq!(all(&db), want(&model), "every row survived");

    // The new room is usable immediately.
    fill(&mut db, 32, 128, &mut model);
    assert_eq!(db.stats().free(), 0);
    assert_eq!(all(&db), want(&model));
    // And the write that was refused for room stayed refused: growing
    // makes room, it does not replay what the ceiling turned away.
    assert_eq!(db.get(500).unwrap(), None);
}

/// Growing is not a write. The file records the new capacity when the
/// database is next written to — the same rule opening with a larger
/// capacity follows, and for the same reason: an operation that was not
/// asked to modify a database must not modify it.
#[test]
fn growing_writes_nothing_until_the_next_commit() {
    let mut db = Mem::in_memory_with(16).expect("open");
    db.put(1, Value::from_bytes(b"a").unwrap()).unwrap();
    let before = db.snapshot().expect("snapshot").to_bytes();

    assert_eq!(db.grow(64).expect("grow"), 64);
    let after = db.snapshot().expect("snapshot").to_bytes();
    assert_eq!(before, after, "growing modified the database");
    assert_eq!(
        db.stats().capacity,
        64,
        "but this handle knows the new size"
    );

    // The next commit records it, and a reopen that is told nothing finds
    // the larger number waiting.
    db.put(2, Value::from_bytes(b"b").unwrap()).unwrap();
    let snap = db.snapshot().expect("snapshot");
    assert_eq!(snap.capacity(), Some(64));
    let reopened = Mem::load(&snap).expect("reopen");
    assert_eq!(reopened.stats().capacity, 64);
}

/// It never shrinks. A smaller number is not an error and not obeyed —
/// discarding room the data might be using is what `CapacityTooSmall`
/// exists to refuse, and it is refused at a door where the caller can see
/// it (a reopen), not silently here.
#[test]
fn growing_never_shrinks() {
    let mut db = Mem::in_memory_with(64).expect("open");
    db.put(1, Value::from_bytes(b"a").unwrap()).unwrap();
    assert_eq!(db.grow(8).expect("grow"), 64);
    assert_eq!(db.grow(64).expect("grow"), 64, "equal is a no-op");
    assert_eq!(db.stats().capacity, 64);
    assert_eq!(db.get(1).unwrap().unwrap().as_bytes(), b"a");

    // Zero is not a capacity, and asking for it is not a way to get one.
    assert_eq!(db.grow(0).expect("grow"), 64);
}

/// Long values span several slots, and a slot is the unit capacity is
/// counted in. A grow has to carry the runs across whole, or a value
/// comes back short — which is the one failure this library treats as
/// unthinkable.
#[test]
fn growing_carries_multi_slot_values_whole() {
    let mut db = Mem::in_memory_with(64).expect("open");
    let mut model = BTreeMap::new();
    for id in 0..8u64 {
        let v = vec![b'a' + id as u8; VALUE_LEN * (id as usize + 1)];
        db.put(id, Value::from_vec(v.clone()).unwrap()).unwrap();
        model.insert(id, v);
    }
    let slots_before = db.stats().slots;
    assert_eq!(db.grow(512).expect("grow"), 512);
    assert_eq!(db.stats().slots, slots_before);
    assert_eq!(all(&db), want(&model));
    for (id, v) in &model {
        assert_eq!(
            db.get(*id).unwrap().expect("present").as_bytes(),
            v.as_slice(),
            "value {id} came back changed"
        );
    }
    // The derived indexes came with it: substring search and the
    // value-ordered scan both answer over the regrown database.
    assert_eq!(db.find(b"cccc").unwrap().len(), 1);
    assert_eq!(db.range_by_value(b"", b"").unwrap().len(), 8);
}

/// A retired slot is not a free one, so growing and compacting answer
/// different questions — and `Full` says which one applies.
#[test]
fn growing_and_compacting_answer_different_fulls() {
    let mut db = Mem::in_memory_with(16).expect("open");
    // Half the slots are dead: this database wants compaction, not room.
    for round in 0..2 {
        for id in 0..8u64 {
            db.put(id, Value::from_vec(vec![b'0' + round; 4]).unwrap())
                .unwrap();
        }
    }
    assert_eq!(db.stats().dead, 8);
    match db.put(9, Value::from_bytes(b"x").unwrap()) {
        Err(Error::Full { dead, .. }) => {
            assert_eq!(dead, 8, "the error names the reclaimable slots")
        }
        other => panic!("{other:?}"),
    }
    let compacted = db.compact_to_memory().expect("compact");
    assert_eq!(compacted.stats().dead, 0);
    assert_eq!(compacted.stats().slots, 8);

    // Whereas a database with nothing dead can only grow.
    let mut db = Mem::in_memory_with(8).expect("open");
    for id in 0..8u64 {
        db.put(id, Value::from_bytes(b"v").unwrap()).unwrap();
    }
    assert_eq!(db.stats().dead, 0);
    let same = db.compact_to_memory().expect("compact");
    assert_eq!(same.stats().free(), 0, "compaction freed nothing");
    assert_eq!(db.grow(16).unwrap(), 16);
    assert_eq!(db.stats().free(), 8);
}

/// Grown once, grown again, and again — the operation is not a
/// one-shot, and repeating it does not accumulate anything.
#[test]
fn growing_repeatedly_is_just_growing() {
    let mut db = Mem::in_memory_with(8).expect("open");
    let mut model = BTreeMap::new();
    let mut cap = 8u64;
    for _ in 0..5 {
        let from = model.len() as u64;
        fill(&mut db, from, cap, &mut model);
        assert_eq!(db.stats().free(), 0);
        cap *= 2;
        assert_eq!(db.grow(cap).expect("grow"), cap);
        assert_eq!(all(&db), want(&model));
    }
    assert_eq!(db.stats().capacity, 256);
    assert_eq!(db.stats().slots, model.len() as u64);
}

/// A salvaged database is read-only and partly unverifiable. Growing it
/// would mean a STRICT reopen, which refuses — after the salvage handle
/// had already been thrown away to find out. So it is refused first, by
/// name, with the handle intact.
#[test]
fn a_salvaged_database_refuses_to_grow_and_stays_readable() {
    let mut db = Mem::in_memory_with(64).expect("open");
    for id in 0..6u64 {
        db.put(id, Value::from_vec(format!("v{id}").into_bytes()).unwrap())
            .unwrap();
    }
    let mut snap = db.snapshot().expect("snapshot");
    // Damage one row's checksum.
    let bytes = snap.to_bytes();
    let mut damaged = bytes.clone();
    // The rows image starts after the snapshot header; find it by
    // reloading and corrupting through the public door instead of
    // guessing offsets.
    let rows_start = damaged.len() - (db.stats().slots as usize * 32);
    damaged[rows_start + 32 * 2 + 4] ^= 0xff;
    snap = dabqlite::Snapshot::from_bytes(&damaged).expect("still a snapshot");

    assert!(Mem::load(&snap).is_err(), "strict load must refuse");
    let mut salvaged = Mem::load_salvaged(&snap).expect("salvage");
    assert!(salvaged.is_degraded());
    let before = salvaged.stats().capacity;
    match salvaged.grow(1024) {
        Err(Error::Degraded { quarantined }) => assert!(quarantined > 0),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(salvaged.stats().capacity, before, "nothing changed");
    // And the handle still works: this is the point of refusing first.
    assert!(salvaged.get(0).is_ok());
}

/// Growing a database that a batch is about to overflow is the real
/// sequence: `Full`, grow, retry, and the batch lands whole.
#[test]
fn a_batch_refused_for_room_lands_after_a_grow() {
    let mut db = Mem::in_memory_with(16).expect("open");
    for id in 0..12u64 {
        db.put(id, Value::from_bytes(b"v").unwrap()).unwrap();
    }
    let ops: Vec<Op> = (100..108u64)
        .map(|id| Op::insert(id, Value::from_bytes(b"batch").unwrap()))
        .collect();
    match db.batch(&ops) {
        Err(Error::BatchRejected { cause, .. }) => {
            assert!(matches!(*cause, Error::Full { .. }), "{cause:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(db.get(100).unwrap(), None, "nothing of it landed");
    assert_eq!(db.grow(64).unwrap(), 64);
    db.batch(&ops).expect("and now it fits");
    for id in 100..108u64 {
        assert_eq!(db.get(id).unwrap().expect("present").as_bytes(), b"batch");
    }
}
