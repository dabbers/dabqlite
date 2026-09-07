//! Catching a reader up, incrementally.
//!
//! A reader sees the generation it opened on and nothing after it. That
//! is what makes a read lock-free and a scan self-consistent — and it is
//! also why a live view meant closing and reopening the database in a
//! loop, paying a full replay of every committed row to learn about three
//! new ones.
//!
//! The oracle throughout is the strongest one available and the only one
//! worth using: **a refreshed reader must be indistinguishable from a
//! reader opened fresh at that moment.** Not "has the right rows" —
//! indistinguishable, across every read the library offers, because the
//! incremental replay and the full one are the same code and any
//! difference between them is a bug in the sharing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dabqlite::{Db, Error, FileId, MemoryStorage, Op, Storage, Value, VALUE_LEN};

type Mem = Db<Shared>;

/// One set of file images, shared by several handles — two processes, or
/// two browser workers, looking at the same database.
#[derive(Clone, Default)]
struct Shared(Rc<RefCell<[Vec<u8>; 3]>>);

fn slot(file: FileId) -> usize {
    match file {
        FileId::Superblock => 0,
        FileId::Rows => 1,
        FileId::RowsOld => 2,
    }
}

impl Storage for Shared {
    type Error = std::convert::Infallible;

    fn len(&mut self, file: FileId) -> Result<u64, Self::Error> {
        Ok(self.0.borrow()[slot(file)].len() as u64)
    }
    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Self::Error> {
        let images = self.0.borrow();
        let bytes = &images[slot(file)];
        let start = (offset as usize).min(bytes.len());
        let end = (offset.saturating_add(len) as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }
    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Self::Error> {
        let mut images = self.0.borrow_mut();
        let bytes = &mut images[slot(file)];
        let end = offset as usize + data.len();
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[offset as usize..end].copy_from_slice(data);
        Ok(())
    }
    fn sync(&mut self, _file: FileId) -> Result<(), Self::Error> {
        Ok(())
    }
    fn truncate(&mut self, file: FileId, len: u64) -> Result<(), Self::Error> {
        let mut images = self.0.borrow_mut();
        let bytes = &mut images[slot(file)];
        if (len as usize) < bytes.len() {
            bytes.truncate(len as usize);
        }
        Ok(())
    }
}

/// Two handles over ONE set of file images: a writer and a reader, the
/// way two processes or two workers see the same database.
fn pair(rows: u64) -> (Mem, Mem) {
    let images = Shared::default();
    let writer = Db::with_storage(images.clone(), rows).expect("writer");
    let reader = Db::with_storage(images, rows).expect("reader");
    (writer, reader)
}

/// Everything a reader can be asked, in one value, so two readers can be
/// compared rather than spot-checked.
fn view(db: &Mem) -> Vec<u8> {
    let mut out = Vec::new();
    for (id, v) in db.range(0, u64::MAX).expect("range") {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(v.as_bytes());
        out.push(0xff);
    }
    out.push(0xfe);
    for (id, v) in db.range_rev(0, u64::MAX).expect("range_rev") {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out.push(0xfd);
    for (id, v) in db.range_by_value(b"", b"").expect("by value") {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out.push(0xfc);
    for needle in [&b"a"[..], b"zz", b"row", b""] {
        for (id, v) in db.find(needle).expect("find") {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(v.as_bytes());
        }
        out.push(0xfb);
    }
    let s = db.stats();
    out.extend_from_slice(&s.live.to_le_bytes());
    out.extend_from_slice(&s.slots.to_le_bytes());
    out.extend_from_slice(&s.dead.to_le_bytes());
    out
}

fn val(n: u64) -> Value {
    Value::from_vec(format!("row-{n:04}-{}", "z".repeat((n % 40) as usize)).into_bytes()).unwrap()
}

/// The headline, held to the strongest oracle there is.
#[test]
fn a_refreshed_reader_is_indistinguishable_from_a_fresh_one() {
    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 4096).expect("writer");
    let mut reader = Db::with_storage(images.clone(), 4096).expect("reader");

    let mut rng = 0x1234_5678u64;
    let mut next = |n: u64| {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        (rng >> 33) % n
    };
    for round in 0..60u64 {
        // A round of traffic the reader knows nothing about yet.
        match next(4) {
            0 if round > 4 => {
                let id = next(30);
                let _ = writer.remove(id);
            }
            1 => {
                let ops: Vec<Op> = (0..1 + next(5))
                    .map(|k| Op::put(next(30) + k, val(round * 10 + k)))
                    .collect();
                let _ = writer.batch(&ops);
            }
            _ => {
                writer.put(next(30), val(round)).expect("put");
            }
        }

        let gained = reader.refresh().expect("refresh");
        let fresh = Db::with_storage(images.clone(), 4096).expect("fresh reader");
        assert_eq!(
            view(&reader),
            view(&fresh),
            "round {round}: a refreshed reader diverged from a fresh one \
             (gained {gained} slots)"
        );
        assert_eq!(reader.stats().slots, fresh.stats().slots);
        // And refreshing again, with nothing new, changes nothing.
        assert_eq!(reader.refresh().expect("refresh"), 0);
        assert_eq!(view(&reader), view(&fresh));
    }
}

/// Before a refresh, a reader sees the view it opened on — which is the
/// property that makes the refresh worth having rather than a no-op.
#[test]
fn a_reader_is_frozen_until_it_is_refreshed() {
    let (mut writer, mut reader) = pair(256);
    writer.put(1, Value::from_bytes(b"one").unwrap()).unwrap();
    assert_eq!(reader.refresh().unwrap(), 1);
    assert_eq!(reader.get(1).unwrap().unwrap().as_bytes(), b"one");

    writer.put(2, Value::from_bytes(b"two").unwrap()).unwrap();
    writer.put(1, Value::from_bytes(b"ONE").unwrap()).unwrap();
    assert_eq!(reader.get(2).unwrap(), None, "still on the old generation");
    assert_eq!(reader.get(1).unwrap().unwrap().as_bytes(), b"one");

    // Two rows appended: the new record and the one that superseded row 1.
    assert_eq!(reader.refresh().unwrap(), 2);
    assert_eq!(reader.get(2).unwrap().unwrap().as_bytes(), b"two");
    assert_eq!(reader.get(1).unwrap().unwrap().as_bytes(), b"ONE");
    assert_eq!(reader.stats().dead, 1, "the superseded slot came across");
}

/// The cost claim, which is the whole reason this exists: a refresh reads
/// the rows appended since, not the database. Measured as bytes read
/// through a counting backend, because a timing test would only prove the
/// machine was busy.
#[test]
fn a_refresh_reads_the_new_rows_and_not_the_database() {
    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 8192).expect("writer");
    for id in 0..2000u64 {
        writer.put(id, Value::from_bytes(b"v").unwrap()).unwrap();
    }
    let counting = CountingStorage::new(images.clone());
    let reads = Rc::clone(&counting.bytes);
    let mut reader = Db::with_storage(counting, 8192).expect("reader");
    let after_open = reads.get();
    assert!(
        after_open >= 2000 * 32,
        "an open reads the whole database: {after_open} bytes"
    );

    writer
        .put(9999, Value::from_bytes(b"new").unwrap())
        .unwrap();
    reads.set(0);
    assert_eq!(reader.refresh().unwrap(), 1);
    let refreshed = reads.get();
    assert_eq!(reader.get(9999).unwrap().unwrap().as_bytes(), b"new");
    assert!(
        refreshed < after_open / 20,
        "a refresh read {refreshed} bytes where an open read {after_open}; \
         it is supposed to read the new rows, not the database"
    );
}

/// A writer that grew past the reader's arenas cannot be followed until
/// the reader grows too — and the refusal says which, rather than
/// silently serving a stale view or panicking on a row that will not fit.
#[test]
fn a_reader_too_small_for_the_writer_says_so_and_can_grow_into_it() {
    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 16).expect("writer");
    let mut reader = Db::with_storage(images.clone(), 16).expect("reader");
    for id in 0..16u64 {
        writer.put(id, Value::from_bytes(b"v").unwrap()).unwrap();
    }
    assert_eq!(reader.refresh().unwrap(), 16);

    assert_eq!(writer.grow(64).unwrap(), 64);
    for id in 16..40u64 {
        writer.put(id, Value::from_bytes(b"v").unwrap()).unwrap();
    }
    match reader.refresh() {
        Err(Error::CapacityTooSmall { required, asked }) => {
            assert_eq!(required, 40, "the writer's row count");
            assert_eq!(asked, 16, "this reader's arenas");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The reader still answers what it had.
    assert_eq!(reader.stats().slots, 16);
    assert_eq!(reader.get(0).unwrap().unwrap().as_bytes(), b"v");

    assert_eq!(reader.grow(64).unwrap(), 64);
    assert_eq!(reader.stats().slots, 40, "growing already caught it up");
    assert_eq!(reader.refresh().unwrap(), 0);
    assert_eq!(reader.get(39).unwrap().unwrap().as_bytes(), b"v");
}

/// Multi-slot values are the case the incremental replay can get wrong:
/// the tail it reads must begin at a value's HEAD, never in the middle of
/// a run. It always does, because a commit is atomic and the manifest's
/// row count is therefore always a commit boundary — but that is an
/// argument, and this is the test.
#[test]
fn refreshing_across_long_values_never_lands_inside_a_run() {
    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 4096).expect("writer");
    let mut reader = Db::with_storage(images.clone(), 4096).expect("reader");
    for round in 0..12u64 {
        // Every round appends a value of a different slot count, so the
        // boundary the reader resumes at falls at a different place in
        // each one.
        let len = VALUE_LEN * (round as usize % 7 + 1) + (round as usize % 3);
        let v = Value::from_vec(vec![b'a' + round as u8; len]).unwrap();
        writer.put(round, v.clone()).unwrap();
        // Sometimes several in one commit, so the tail spans a batch.
        if round % 3 == 0 {
            writer
                .batch(&[
                    Op::put(100 + round, v.clone()),
                    Op::put(200 + round, Value::from_bytes(b"short").unwrap()),
                ])
                .unwrap();
        }
        reader.refresh().expect("refresh");
        let fresh = Db::with_storage(images.clone(), 4096).expect("fresh");
        assert_eq!(view(&reader), view(&fresh), "round {round}");
        assert_eq!(
            reader.get(round).unwrap().unwrap().as_bytes(),
            v.as_bytes(),
            "round {round}: a long value came back changed"
        );
    }
}

/// A refresh writes nothing. A reader takes no lock and modifies no byte,
/// and that has to stay true of the path that reads new state.
#[test]
fn refreshing_modifies_nothing() {
    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 256).expect("writer");
    let mut reader = Db::with_storage(images.clone(), 256).expect("reader");
    for id in 0..8u64 {
        writer.put(id, val(id)).unwrap();
    }
    let before = writer.snapshot().expect("snapshot").to_bytes();
    assert_eq!(reader.refresh().unwrap(), 8);
    let after = writer.snapshot().expect("snapshot").to_bytes();
    assert_eq!(before, after, "a refresh changed the database");
    assert_eq!(reader.refresh().unwrap(), 0);
    assert_eq!(writer.snapshot().expect("snapshot").to_bytes(), after);
}

/// A salvaged handle holds rows it could not verify, so there is no
/// honest incremental answer over it. Refused by name, with the handle
/// still answering.
#[test]
fn a_salvaged_handle_refuses_to_refresh() {
    let mut db = Db::<MemoryStorage>::in_memory_with(64).expect("open");
    for id in 0..6u64 {
        db.put(id, val(id)).unwrap();
    }
    let snap = db.snapshot().expect("snapshot");
    let mut damaged = snap.to_bytes();
    let rows_start = damaged.len() - (db.stats().slots as usize * 32);
    damaged[rows_start + 32 * 2 + 4] ^= 0xff;
    let snap = dabqlite::Snapshot::from_bytes(&damaged).expect("still a snapshot");

    let mut salvaged = Db::<MemoryStorage>::load_salvaged(&snap).expect("salvage");
    assert!(salvaged.is_degraded());
    match salvaged.refresh() {
        Err(Error::Degraded { quarantined }) => assert!(quarantined > 0),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(salvaged.get(0).is_ok(), "and it still answers");
}

// ---------------------------------------------------------------------
// A backend that counts the bytes it hands back.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct CountingStorage {
    inner: Shared,
    bytes: Rc<Cell<u64>>,
}

impl CountingStorage {
    fn new(inner: Shared) -> Self {
        CountingStorage {
            inner,
            bytes: Rc::new(Cell::new(0)),
        }
    }
}

impl Storage for CountingStorage {
    type Error = <Shared as Storage>::Error;

    fn len(&mut self, file: FileId) -> Result<u64, Self::Error> {
        self.inner.len(file)
    }
    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Self::Error> {
        let out = self.inner.read(file, offset, len)?;
        self.bytes.set(self.bytes.get() + out.len() as u64);
        Ok(out)
    }
    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Self::Error> {
        self.inner.write(file, offset, data)
    }
    fn sync(&mut self, file: FileId) -> Result<(), Self::Error> {
        self.inner.sync(file)
    }
    fn truncate(&mut self, file: FileId, len: u64) -> Result<(), Self::Error> {
        self.inner.truncate(file, len)
    }
}

/// A database swapped out from under a reader is not one it can catch up
/// ON — the histories are different, and merging them is not something
/// the library will guess at. Refused by name, with the view it holds
/// intact.
#[test]
fn a_database_that_went_backwards_is_refused_rather_than_merged() {
    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 256).expect("writer");
    let mut reader = Db::with_storage(images.clone(), 256).expect("reader");
    for id in 0..10u64 {
        writer.put(id, val(id)).unwrap();
    }
    let gained = reader.refresh().unwrap();
    assert!(gained >= 10, "ten values, some of them two slots");
    let held = view(&reader);
    drop(writer);

    // A compaction, or a restore, or anything else that puts a DIFFERENT
    // database at the same address: fewer rows than the reader holds.
    {
        let mut images = images.0.borrow_mut();
        images[slot(FileId::Rows)].truncate(4 * 32);
    }
    assert!(matches!(reader.refresh(), Err(Error::Diverged)));
    assert_eq!(view(&reader), held, "the reader kept what it had");
    assert!(reader.get(9).is_ok(), "and still answers");
    // Still refused on the next attempt: this is a state, not a blip.
    assert!(matches!(reader.refresh(), Err(Error::Diverged)));
}

/// The other shape of the same thing: a manifest whose GENERATION went
/// backwards while its row count did not. Forged directly, because no
/// sequence of ordinary operations produces it — which is the point. It
/// is what a restore of an older backup over a live directory looks like,
/// and the reader must refuse it rather than replay nothing and call
/// itself current.
#[test]
fn a_generation_that_went_backwards_is_refused_too() {
    use dabqlite_core::layout::{decode_sb, encode_sb, SB_COPIES, SB_COPY_SIZE};

    let images = Shared::default();
    let mut writer = Db::with_storage(images.clone(), 256).expect("writer");
    let mut reader = Db::with_storage(images.clone(), 256).expect("reader");
    for id in 0..10u64 {
        writer.put(id, val(id)).unwrap();
    }
    let gained = reader.refresh().unwrap();
    assert!(gained >= 10);
    let held = view(&reader);
    let rows_held = reader.stats().slots;
    drop(writer);

    // Same row count, one generation older, checksum valid — every copy,
    // so there is nothing newer left to find.
    {
        let mut imgs = images.0.borrow_mut();
        let sb = &mut imgs[slot(FileId::Superblock)];
        // The copies hold alternating generations (a commit writes the
        // pair for its own parity), so the newest is the one to rewind.
        let current = (0..SB_COPIES)
            .filter_map(|i| decode_sb(&sb[i * SB_COPY_SIZE..(i + 1) * SB_COPY_SIZE]).ok())
            .max_by_key(|c| c.generation)
            .expect("a valid superblock");
        assert_eq!(current.row_count, rows_held);
        let mut copy = [0u8; SB_COPY_SIZE];
        encode_sb(
            current.generation - 1,
            current.row_count,
            current.capacity,
            &mut copy,
        );
        for i in 0..SB_COPIES {
            sb[i * SB_COPY_SIZE..(i + 1) * SB_COPY_SIZE].copy_from_slice(&copy);
        }
    }
    assert!(matches!(reader.refresh(), Err(Error::Diverged)));
    assert_eq!(view(&reader), held, "the reader kept what it had");
}
