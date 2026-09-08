#![cfg(target_arch = "wasm32")]
//! Real OPFS, real Chromium, real dedicated worker.
//!
//! `tests/contract.rs` proves the backend obeys the storage contract
//! against a *model* of the sync-access-handle API. This suite proves the
//! model is not a fiction — the same argument structure the project uses
//! for the simulator (`dabqlite-host/tests/equivalence.rs`).
//!
//! The central test does not need golden files or a checked-in byte
//! blob. It runs the identical workload twice inside one browser tab:
//! once against REAL OPFS, once against the in-memory model, and demands
//! byte-for-byte agreement. Chained with the native suite — where the
//! model is proven byte-identical to POSIX files and to the simulator —
//! that gives the property the design actually needs:
//!
//! ```text
//! real OPFS  ==  model  ==  POSIX files  ==  simulator
//! ```
//!
//! so a database written in a browser is byte-identical to one written
//! on a server, and every simulated fault result transfers to both.
//!
//! Run with:
//! ```text
//! CHROMEDRIVER=... cargo test -p dabqlite-web --target wasm32-unknown-unknown
//! ```

use dabqlite_core::{Capacities, FileId, Output, ROW_SIZE, VALUE_LEN};
use dabqlite_host::{rows_file_name, Host, Storage, SUPERBLOCK_FILE};
use dabqlite_web::fake::FakeSet;
use dabqlite_web::opfs;
use dabqlite_web::SyncHandle;
use wasm_bindgen_test::*;

// OPFS sync access handles exist ONLY in dedicated workers
// (docs/DESIGN.md §8.1). This is the harness honoring that.
wasm_bindgen_test_configure!(run_in_dedicated_worker);

const CAPS: Capacities = Capacities { rows: 32 };
const INSERTS: u64 = 12;

/// A deterministic workload — no RNG crate needed, and identical for
/// every backend it is run against.
fn workload() -> Vec<(u64, [u8; VALUE_LEN])> {
    (0..INSERTS)
        .map(|i| {
            let id = i.wrapping_mul(2_654_435_761) % 997;
            (id, [(i as u8).wrapping_mul(37) ^ 0x5A; VALUE_LEN])
        })
        .collect()
}

fn run_workload<S: Storage>(host: &mut Host<S>, ops: &[(u64, [u8; VALUE_LEN])]) {
    match host.open().expect("size probe") {
        Output::OpenDone { result: Ok(0) } => {}
        other => panic!("open: {other:?}"),
    }
    for &(id, value) in ops {
        match host.insert(id, value) {
            Output::InsertDone { result: Ok(()), .. } => {}
            other => panic!("insert: {other:?}"),
        }
    }
}

/// Read a whole file back through the storage seam.
fn whole_file<S: Storage>(storage: &mut S, file: FileId) -> Vec<u8> {
    let len = storage.len(file).expect("len");
    storage.read(file, 0, len).expect("read")
}

/// THE test: real OPFS and the model, same workload, same bytes.
#[wasm_bindgen_test]
async fn real_opfs_bytes_match_the_model_exactly() {
    let dir = "dabqlite-equiv";
    let _ = opfs::remove_dir(dir).await;

    let ops = workload();

    // Real OPFS.
    let storage = opfs::open_dir(dir).await.expect("open real OPFS");
    let mut real = Host::new(CAPS, storage);
    run_workload(&mut real, &ops);
    let real_sb = whole_file(&mut real.storage, FileId::Superblock);
    let real_rows = whole_file(&mut real.storage, FileId::Rows);

    // The model, in the same tab, same workload.
    let (model_storage, files) = FakeSet::new();
    let mut model = Host::new(CAPS, model_storage);
    run_workload(&mut model, &ops);

    assert_eq!(
        real_sb,
        files.superblock.contents(),
        "{SUPERBLOCK_FILE} differs between real OPFS and the model"
    );
    assert_eq!(
        real_rows,
        files.rows.contents(),
        "{} differs between real OPFS and the model",
        rows_file_name(dabqlite_core::SCHEMA_HASH)
    );

    // A genuine reopen: close the handles, acquire fresh ones, recover.
    real.storage.close();
    drop(real);
    let storage = opfs::open_dir(dir).await.expect("reopen real OPFS");
    let mut reopened = Host::new(CAPS, storage);
    match reopened.open().expect("size probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, ops.len() as u64),
        other => panic!("recovery: {other:?}"),
    }
    for &(id, value) in &ops {
        match reopened.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v.payload(), value, "id={id}"),
            other => panic!("get {id}: {other:?}"),
        }
    }
    assert!(
        !reopened.engine.recovery_report().rollback_evidence,
        "a clean browser reopen flagged rollback"
    );
    reopened.storage.close();
}

/// The superblock layout writes slots out of order, so a write past the
/// end MUST zero-fill the gap. The model assumes it; here the real
/// platform is asked directly.
#[wasm_bindgen_test]
async fn writing_past_the_end_zero_fills_on_real_opfs() {
    let dir = "dabqlite-gap";
    let _ = opfs::remove_dir(dir).await;

    let mut storage = opfs::open_dir(dir).await.expect("open");
    storage
        .write(FileId::Rows, 100, &[0xAB; 4])
        .expect("gap write");
    let bytes = whole_file(&mut storage, FileId::Rows);

    assert_eq!(bytes.len(), 104, "real OPFS did not extend the file");
    assert!(
        bytes[..100].iter().all(|&b| b == 0),
        "real OPFS left the gap unzeroed — the model is wrong: {:?}",
        &bytes[..16]
    );
    assert_eq!(&bytes[100..], &[0xAB; 4]);
    storage.close();
}

/// Reads clamp at EOF instead of erroring — the contract every backend
/// shares, checked against the platform rather than assumed.
#[wasm_bindgen_test]
async fn reads_clamp_at_eof_on_real_opfs() {
    let dir = "dabqlite-eof";
    let _ = opfs::remove_dir(dir).await;

    let mut storage = opfs::open_dir(dir).await.expect("open");
    storage.write(FileId::Rows, 0, &[7u8; 64]).expect("write");

    assert_eq!(
        storage.read(FileId::Rows, 200, 16).expect("past EOF").len(),
        0
    );
    assert_eq!(
        storage
            .read(FileId::Rows, 56, 64)
            .expect("straddling")
            .len(),
        8
    );
    assert_eq!(storage.read(FileId::Rows, 0, 0).expect("empty").len(), 0);
    assert_eq!(
        storage.read(FileId::Rows, 0, 64).expect("whole"),
        vec![7u8; 64]
    );
    storage.close();
}

/// Single-writer, enforced by the platform (docs/DESIGN.md §2, §8.1): a
/// sync access handle is exclusive, so a second acquisition of the same
/// file is refused while the first is open — and succeeds once it is
/// closed. This is the browser's `flock`, and it needs no election
/// protocol to be correct for the single-tab case.
#[wasm_bindgen_test]
async fn a_second_sync_handle_on_the_same_file_is_refused() {
    let dir = "dabqlite-lock";
    let _ = opfs::remove_dir(dir).await;

    let first = opfs::acquire(dir, SUPERBLOCK_FILE)
        .await
        .expect("first handle");

    let second = opfs::acquire(dir, SUPERBLOCK_FILE).await;
    assert!(
        second.is_err(),
        "a second sync access handle was granted — the browser's \
         single-writer guarantee does not hold as designed"
    );

    // Releasing the first hands the lock over, exactly like flock.
    first.raw().close();
    let third = opfs::acquire(dir, SUPERBLOCK_FILE).await;
    assert!(third.is_ok(), "the lock was not released on close");
    third.expect("third").raw().close();
}

/// **A handle that goes out of scope releases the lock.**
///
/// The browser's single-writer guarantee is a per-file exclusive sync
/// access handle, which is `flock` with one difference that matters: a
/// process losing a file descriptor releases `flock`, and a worker
/// losing an `OpfsHandle` used to release NOTHING. The lock stayed held
/// by an object nobody could reach, for the life of the worker, and
/// every retry came back `NoModificationAllowedError` — which to an
/// application is indistinguishable from "the database is gone".
///
/// A database is dropped rather than closed by any error path in any
/// caller, so this is the ordinary case, not the exotic one.
#[wasm_bindgen_test]
async fn dropping_a_handle_releases_the_browsers_single_writer_lock() {
    let dir = "dabqlite-drop-lock";
    let _ = opfs::remove_dir(dir).await;

    {
        let _held = opfs::acquire(dir, SUPERBLOCK_FILE).await.expect("first");
        assert!(
            opfs::acquire(dir, SUPERBLOCK_FILE).await.is_err(),
            "the lock is not exclusive while held"
        );
    }
    let again = opfs::acquire(dir, SUPERBLOCK_FILE).await;
    assert!(
        again.is_ok(),
        "a dropped handle kept the lock — the file is unopenable for the          life of this worker, with nothing left to close it"
    );
}

/// The same property at the level a caller actually uses: a whole
/// database dropped without `close()` must be openable again.
#[wasm_bindgen_test]
async fn a_database_dropped_without_closing_can_be_opened_again() {
    let dir = "dabqlite-drop-open";
    let _ = opfs::remove_dir(dir).await;

    {
        let _storage = opfs::open_dir(dir).await.expect("first open");
    }
    let second = opfs::open_dir(dir).await;
    assert!(
        second.is_ok(),
        "dropping a database wedged its own files; close() cannot be the          only way out, because an error path never reaches it"
    );
    second.expect("second").close();
}

/// **A half-acquired database releases what it took.**
///
/// `open_dir` takes three handles in sequence. If the second one is
/// refused — another worker holds it, or the platform says no — the
/// first was already acquired, and leaking it wedged a file that the
/// failed open never even used. The retry then failed for a DIFFERENT
/// reason than the original, on a file the caller had no idea was
/// involved.
///
/// This reproduces it exactly: hold the rows file, watch `open_dir`
/// fail, release it, and demand that the retry succeed.
#[wasm_bindgen_test]
async fn a_half_acquired_database_does_not_wedge_the_files_it_took() {
    let dir = "dabqlite-partial";
    let _ = opfs::remove_dir(dir).await;
    let rows_name = rows_file_name(dabqlite_core::SCHEMA_HASH);

    // Somebody else holds the rows file. `open_dir` reaches it second,
    // by which time it is already holding the superblock.
    let blocker = opfs::acquire(dir, &rows_name).await.expect("blocker");
    assert!(
        opfs::open_dir(dir).await.is_err(),
        "open_dir should be refused while another handle holds a file"
    );

    // The blocker goes away. Nothing else should still be held.
    drop(blocker);
    let recovered = opfs::open_dir(dir).await;
    assert!(
        recovered.is_ok(),
        "the failed open kept the superblock, so the database can never be          opened again in this worker — a refusal turned into a brick"
    );
    recovered.expect("recovered").close();
}

/// Flush is the commit point's durability primitive. At minimum, flushed
/// bytes must be visible to a freshly acquired handle — the property
/// recovery depends on. (Whether they survive a killed tab is the
/// browser's business, and is documented as best-effort in
/// docs/FAULTS.md.)
#[wasm_bindgen_test]
async fn flushed_bytes_are_visible_to_a_fresh_handle() {
    let dir = "dabqlite-flush";
    let _ = opfs::remove_dir(dir).await;

    let handle = opfs::acquire(dir, SUPERBLOCK_FILE).await.expect("handle");
    let payload: Vec<u8> = (0..=255u8).collect();
    let n = handle.write_at(&payload, 0).expect("write");
    assert_eq!(n, payload.len(), "real OPFS short-wrote a 256-byte buffer");
    handle.flush().expect("flush");
    assert_eq!(handle.size().expect("size"), payload.len() as u64);
    handle.raw().close();

    let fresh = opfs::acquire(dir, SUPERBLOCK_FILE)
        .await
        .expect("fresh handle");
    let mut buf = vec![0u8; payload.len()];
    let n = fresh.read_at(&mut buf, 0).expect("read");
    assert_eq!(n, payload.len(), "short read of flushed bytes");
    assert_eq!(buf, payload, "flushed bytes came back different");
    fresh.raw().close();
}

/// The browser's in-memory store, in the browser — and interchangeable
/// with OPFS in both directions.
///
/// This is the workflow a browser without OPFS (Safari private mode) has
/// to live on, and the one a browser WITH OPFS wants anyway: run entirely
/// in RAM at memory speed, then snapshot the image to durable storage,
/// and load it back later. The images are the same bytes either way, so
/// "in-memory" and "persistent" are the same database in two places.
#[wasm_bindgen_test]
async fn the_in_memory_store_and_opfs_are_interchangeable() {
    use dabqlite_host::MemoryStorage;

    let dir = "dabqlite-mem-bridge";
    let _ = opfs::remove_dir(dir).await;
    let ops = workload();

    // Run the whole database in RAM — no OPFS, no handles, no locks.
    let mut ram = Host::new(CAPS, MemoryStorage::new());
    run_workload(&mut ram, &ops);
    for &(id, value) in &ops {
        match ram.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v.payload(), value),
            other => panic!("in-memory get {id}: {other:?}"),
        }
    }
    let (sb_image, rows_image, _) = ram.storage.snapshot();

    // Snapshot it into OPFS by writing the images through the seam.
    let mut opfs_storage = opfs::open_dir(dir).await.expect("open OPFS");
    opfs_storage
        .write(FileId::Superblock, 0, &sb_image)
        .expect("persist superblock");
    opfs_storage
        .write(FileId::Rows, 0, &rows_image)
        .expect("persist rows");
    opfs_storage.sync(FileId::Superblock).expect("flush sb");
    opfs_storage.sync(FileId::Rows).expect("flush rows");
    opfs_storage.close();

    // A later session opens it from OPFS and finds the same database.
    let storage = opfs::open_dir(dir).await.expect("reopen OPFS");
    let mut persisted = Host::new(CAPS, storage);
    match persisted.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, ops.len() as u64),
        other => panic!("opening the snapshot from OPFS: {other:?}"),
    }
    for &(id, value) in &ops {
        match persisted.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v.payload(), value, "id={id}"),
            other => panic!("persisted get {id}: {other:?}"),
        }
    }
    // Keep writing on the OPFS-backed database...
    match persisted.insert(31_337, [0xA5; VALUE_LEN]) {
        Output::InsertDone { result: Ok(()), .. } => {}
        other => panic!("insert on the persisted database: {other:?}"),
    }
    let sb_back = whole_file(&mut persisted.storage, FileId::Superblock);
    let rows_back = whole_file(&mut persisted.storage, FileId::Rows);
    persisted.storage.close();

    // ...and carry it back into RAM, where it is the same database again.
    let mut ram_again = Host::new(
        CAPS,
        MemoryStorage::from_images(sb_back, rows_back, Vec::new()),
    );
    match ram_again.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, ops.len() as u64 + 1),
        other => panic!("loading OPFS bytes back into RAM: {other:?}"),
    }
    for &(id, value) in &ops {
        match ram_again.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v.payload(), value, "id={id}"),
            other => panic!("round-tripped get {id}: {other:?}"),
        }
    }
    match ram_again.get(31_337) {
        Output::GetDone {
            result: Ok(Some(v)),
            ..
        } => assert_eq!(v.payload(), [0xA5; VALUE_LEN]),
        other => panic!("the OPFS-side commit did not survive the trip: {other:?}"),
    }
}

/// Corruption containment works in the browser too, on real OPFS bytes.
#[wasm_bindgen_test]
async fn salvage_contains_damage_on_real_opfs() {
    let dir = "dabqlite-salvage";
    let _ = opfs::remove_dir(dir).await;
    let ops = workload();

    let storage = opfs::open_dir(dir).await.expect("open");
    let mut host = Host::new(CAPS, storage);
    run_workload(&mut host, &ops);

    // Damage one committed row in place, through the real handle.
    let victim = 2usize;
    let mut row = host
        .storage
        .read(FileId::Rows, (victim * ROW_SIZE) as u64, ROW_SIZE as u64)
        .expect("read row");
    row[6] ^= 0x40;
    host.storage
        .write(FileId::Rows, (victim * ROW_SIZE) as u64, &row)
        .expect("damage row");
    host.storage.sync(FileId::Rows).expect("flush");
    host.storage.close();

    // Strict open refuses it.
    let storage = opfs::open_dir(dir).await.expect("reopen strict");
    let mut strict = Host::new(CAPS, storage);
    match strict.open().expect("probe") {
        Output::OpenDone {
            result: Err(dabqlite_core::DbError::Corrupt { .. }),
        } => {}
        other => panic!("strict open must refuse damage: {other:?}"),
    }
    strict.storage.close();

    // Salvage contains it: every other row is exact, in a real browser.
    let storage = opfs::open_dir(dir).await.expect("reopen salvage");
    let mut rescue = Host::new(CAPS, storage);
    match rescue.open_salvage().expect("probe") {
        // One row short: the quarantined one is not served, and salvage
        // reports what it can actually give you rather than what the
        // manifest claims.
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, ops.len() as u64 - 1),
        other => panic!("salvage open: {other:?}"),
    }
    assert_eq!(rescue.engine.quarantined(), 1);
    for (row_ix, &(id, value)) in ops.iter().enumerate() {
        if row_ix == victim {
            continue;
        }
        match rescue.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v.payload(), value, "id={id}"),
            other => panic!("survivor {id}: {other:?}"),
        }
    }
    rescue.storage.close();
}

/// Recovery shortens the rows file to the manifest, and it has to do that
/// through a real `FileSystemSyncAccessHandle.truncate` — the one storage
/// operation the engine performs that is neither a read, a write, nor a
/// flush, and therefore the one most likely to be a fiction of the model.
///
/// Without it, residue from commits that were never acknowledged piles up
/// across restarts until two of different widths look like one
/// acknowledged commit that was rolled back, and a healthy database
/// reports permanent data loss. That is worth proving in the browser, not
/// only in the simulator.
#[wasm_bindgen_test]
async fn recovery_truncates_residue_on_real_opfs() {
    let dir = "dabqlite-truncate";
    let _ = opfs::remove_dir(dir).await;
    let ops = workload();

    let storage = opfs::open_dir(dir).await.expect("open");
    let mut host = Host::new(CAPS, storage);
    run_workload(&mut host, &ops);
    let committed = whole_file(&mut host.storage, FileId::Rows).len();

    // Leave residue past the manifest, exactly as an interrupted commit
    // would: valid-looking bytes the superblock does not reference.
    let junk = [0x7Eu8; ROW_SIZE * 5];
    host.storage
        .write(FileId::Rows, committed as u64, &junk)
        .expect("append residue");
    host.storage.sync(FileId::Rows).expect("flush");
    let grown = whole_file(&mut host.storage, FileId::Rows).len();
    assert_eq!(
        grown,
        committed + junk.len(),
        "the setup did not grow the file"
    );
    host.storage.close();

    // Reopen: recovery must shorten the file back to the manifest.
    let storage = opfs::open_dir(dir).await.expect("reopen");
    let mut reopened = Host::new(CAPS, storage);
    match reopened.open().expect("size probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, ops.len() as u64),
        other => panic!("recovery: {other:?}"),
    }
    assert_eq!(
        whole_file(&mut reopened.storage, FileId::Rows).len(),
        committed,
        "real OPFS truncate did not shorten the rows file"
    );
    // And every row is still exactly right.
    for &(id, value) in &ops {
        match reopened.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v.payload(), value, "id={id}"),
            other => panic!("get {id}: {other:?}"),
        }
    }
    reopened.storage.close();

    // A second reopen finds nothing to clear and stays quiet.
    let storage = opfs::open_dir(dir).await.expect("reopen again");
    let mut again = Host::new(CAPS, storage);
    match again.open().expect("size probe") {
        Output::OpenDone { result: Ok(_) } => {}
        other => panic!("recovery: {other:?}"),
    }
    let report = again.engine.recovery_report();
    assert_eq!(report.orphan_valid_rows, 0);
    assert!(!report.rollback_evidence);
    again.storage.close();
}
