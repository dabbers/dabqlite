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

use dabqlite_core::{Capacities, FileId, Output, VALUE_LEN};
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
            } => assert_eq!(v, value, "id={id}"),
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
