#![cfg(unix)]
//! The OPFS backend against the storage contract — natively, exhaustively,
//! with no browser in sight.
//!
//! The whole fault harness rests on every backend implementing the same
//! contract (`dabqlite-host`'s `Storage`). The POSIX backend has an
//! equivalence suite proving it does; this is the OPFS backend's, and it
//! is a THREE-way comparison: simulator, POSIX files, and the OPFS
//! backend driven through a faithful model of the sync-access-handle API.
//! If all three produce identical bytes and identical outcomes under
//! identical damage, then every simulated fault result transfers to the
//! browser as it already transfers to disk.
//!
//! On top of equivalence, the OPFS-specific hazards get their own teeth:
//! short I/O (a conforming handle may legally move fewer bytes than
//! asked), zero-progress I/O (which must fail loudly rather than hand the
//! engine a half-filled buffer), handle failures (`DOMException` →
//! fail-stop), and gap zero-filling on writes past EOF.
//!
//! What this suite CANNOT prove is that real OPFS behaves as modeled —
//! that is `tests/opfs_browser.rs`, which runs these same assumptions
//! against actual Chromium on actual OPFS.

use std::convert::Infallible;
use std::path::PathBuf;

use dabqlite_core::{Capacities, DbError, FileId, Output, VALUE_LEN};
use dabqlite_host::{rows_file_name, Host, PosixStorage, Storage, SUPERBLOCK_FILE};
use dabqlite_sim::{gen_workload, SimDisk};
use dabqlite_web::fake::FakeSet;
use dabqlite_web::OpfsError;

const CAPS: Capacities = Capacities { rows: 32 };
const INSERTS: usize = 12;

/// The simulated disk behind the same `Storage` seam, so all three runs
/// share every line of driver code.
struct SimStorage(SimDisk);

impl Storage for SimStorage {
    type Error = Infallible;

    fn len(&mut self, file: FileId) -> Result<u64, Infallible> {
        Ok(self.0.len(file))
    }
    fn read(&mut self, file: FileId, offset: u64, len: u64) -> Result<Vec<u8>, Infallible> {
        Ok(self.0.read(file, offset, len))
    }
    fn write(&mut self, file: FileId, offset: u64, data: &[u8]) -> Result<(), Infallible> {
        self.0.write(file, offset, data);
        Ok(())
    }
    fn sync(&mut self, file: FileId) -> Result<(), Infallible> {
        self.0.fsync(file);
        Ok(())
    }
}

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-opfs-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open_ok<S: Storage>(host: &mut Host<S>) -> Result<u64, DbError> {
    match host.open().expect("size probe") {
        Output::OpenDone { result } => result,
        other => panic!("open returned {other:?}"),
    }
}

fn get_ok<S: Storage>(host: &mut Host<S>, id: u64) -> Option<[u8; VALUE_LEN]> {
    match host.get(id) {
        Output::GetDone { result: Ok(v), .. } => v,
        other => panic!("get returned {other:?}"),
    }
}

fn run_workload<S: Storage>(host: &mut Host<S>, ops: &[(u64, [u8; VALUE_LEN])]) {
    assert_eq!(open_ok(host), Ok(0));
    for &(id, value) in ops {
        match host.insert(id, value) {
            Output::InsertDone { result: Ok(()), .. } => {}
            other => panic!("insert returned {other:?}"),
        }
    }
}

/// The headline claim: the browser backend writes the same bytes as the
/// simulator and as real POSIX files, for the same workload.
#[test]
fn opfs_posix_and_sim_produce_identical_bytes() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, INSERTS);

        let mut sim = Host::new(CAPS, SimStorage(SimDisk::new()));
        run_workload(&mut sim, &ops);

        let (opfs, files) = FakeSet::new();
        let mut opfs = Host::new(CAPS, opfs);
        run_workload(&mut opfs, &ops);

        let dir = scratch_dir(&format!("bytes-{seed}"));
        let mut posix = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
        run_workload(&mut posix, &ops);
        drop(posix);

        for (file, name) in [
            (FileId::Superblock, SUPERBLOCK_FILE.to_string()),
            (FileId::Rows, rows_file_name(dabqlite_core::SCHEMA_HASH)),
        ] {
            let sim_bytes = sim.storage.0.contents(file);
            let posix_bytes = std::fs::read(dir.join(&name)).expect("read back");
            let opfs_bytes = match file {
                FileId::Superblock => files.superblock.contents(),
                FileId::Rows => files.rows.contents(),
                FileId::RowsOld => files.rows_old.contents(),
            };
            assert_eq!(
                sim_bytes, posix_bytes,
                "seed={seed}: {name} diverged between simulator and disk"
            );
            assert_eq!(
                opfs_bytes, posix_bytes,
                "seed={seed}: {name} diverged between OPFS and disk"
            );
        }

        // And a real reopen over the same OPFS files recovers everything.
        let mut reopened = Host::new(CAPS, files.reopen());
        assert_eq!(open_ok(&mut reopened), Ok(INSERTS as u64), "seed={seed}");
        for &(id, value) in &ops {
            assert_eq!(
                get_ok(&mut reopened, id),
                Some(value),
                "seed={seed} id={id}"
            );
        }
        assert!(
            !reopened.engine.recovery_report().rollback_evidence,
            "seed={seed}: clean reopen flagged rollback"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// A comparable digest of "what did opening this damaged database yield".
fn outcome_digest<S: Storage>(host: &mut Host<S>, ops: &[(u64, [u8; VALUE_LEN])]) -> String {
    match host.open().expect("size probe") {
        Output::OpenDone { result: Ok(n) } => {
            let mut digest = format!("ok n={n};");
            for &(id, _) in ops {
                match get_ok(host, id) {
                    Some(v) => digest.push_str(&format!("{id}={v:02x?};")),
                    None => digest.push_str(&format!("{id}=absent;")),
                }
            }
            digest
        }
        Output::OpenDone { result: Err(e) } => format!("err {e:?}"),
        other => panic!("open returned {other:?}"),
    }
}

/// Identical at-rest damage must produce identical recovery outcomes on
/// all three backends — the property that lets the browser inherit the
/// simulator's entire fault matrix.
#[test]
fn at_rest_faults_produce_identical_outcomes_across_backends() {
    let seed = 7u64;
    let ops = gen_workload(seed, INSERTS);

    let mut sim = Host::new(CAPS, SimStorage(SimDisk::new()));
    run_workload(&mut sim, &ops);
    let pristine_sim = sim.storage.0.clone();

    let (opfs, pristine_files) = FakeSet::new();
    let mut opfs = Host::new(CAPS, opfs);
    run_workload(&mut opfs, &ops);
    let pristine_sb = pristine_files.superblock.contents();
    let pristine_rows = pristine_files.rows.contents();

    let master = scratch_dir("faults-master");
    let mut posix = Host::new(CAPS, PosixStorage::open_dir(&master).expect("open dir"));
    run_workload(&mut posix, &ops);
    drop(posix);

    let mut cases: Vec<(FileId, &str, u64, Option<u8>)> = Vec::new();
    for offset in [0u64, 9, 33, 70, 129, 200, 255] {
        cases.push((FileId::Superblock, "flip", offset, Some(0x20)));
    }
    for offset in [0u64, 8, 25, 90, 200, 383] {
        cases.push((FileId::Rows, "flip", offset, Some(0x20)));
    }
    for cut in [0u64, 8, 64, 120, 192] {
        cases.push((FileId::Superblock, "trunc", cut, None));
    }
    for cut in [0u64, 32, 100, 350] {
        cases.push((FileId::Rows, "trunc", cut, None));
    }

    for (i, &(file, kind, arg, mask)) in cases.iter().enumerate() {
        let ctx = format!("case {i}: {file:?} {kind} {arg}");

        // Simulator copy.
        let mut sim_disk = pristine_sim.clone();
        match mask {
            Some(m) => sim_disk.corrupt(file, arg, m),
            None => sim_disk.truncate_at_rest(file, arg),
        }
        let sim_outcome = outcome_digest(&mut Host::new(CAPS, SimStorage(sim_disk)), &ops);

        // OPFS copy: same damage to the same bytes.
        let (opfs_host, files) = FakeSet::new();
        files.superblock.set_contents(pristine_sb.clone());
        files.rows.set_contents(pristine_rows.clone());
        let target = match file {
            FileId::Superblock => &files.superblock,
            FileId::Rows | FileId::RowsOld => &files.rows,
        };
        let mut bytes = target.contents();
        match mask {
            Some(m) => bytes[arg as usize] ^= m,
            None => bytes.truncate(arg as usize),
        }
        target.set_contents(bytes);
        let opfs_outcome = outcome_digest(&mut Host::new(CAPS, opfs_host), &ops);

        // POSIX copy.
        let dir = scratch_dir(&format!("faults-{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let name = match file {
            FileId::Superblock => SUPERBLOCK_FILE.to_string(),
            FileId::Rows | FileId::RowsOld => rows_file_name(dabqlite_core::SCHEMA_HASH),
        };
        for n in [
            SUPERBLOCK_FILE.to_string(),
            rows_file_name(dabqlite_core::SCHEMA_HASH),
        ] {
            std::fs::copy(master.join(&n), dir.join(&n)).expect("copy");
        }
        match mask {
            Some(m) => {
                let mut bytes = std::fs::read(dir.join(&name)).expect("read");
                bytes[arg as usize] ^= m;
                std::fs::write(dir.join(&name), bytes).expect("write");
            }
            None => {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(dir.join(name))
                    .expect("open");
                f.set_len(arg).expect("truncate");
            }
        }
        let posix_outcome = outcome_digest(
            &mut Host::new(CAPS, PosixStorage::open_dir(&dir).expect("reopen")),
            &ops,
        );

        assert_eq!(
            sim_outcome, posix_outcome,
            "[{ctx}] simulator and reality disagree"
        );
        assert_eq!(
            opfs_outcome, posix_outcome,
            "[{ctx}] OPFS and reality disagree about the same damage"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::remove_dir_all(&master).ok();
}

/// A conforming sync access handle may move fewer bytes than asked. The
/// backend must resume until the request is satisfied — byte-identically,
/// even one byte at a time.
#[test]
fn short_io_is_resumed_until_complete() {
    for chunk in [1usize, 3, 7] {
        let ops = gen_workload(3, INSERTS);

        let (reference, ref_files) = FakeSet::new();
        run_workload(&mut Host::new(CAPS, reference), &ops);

        let (choppy, files) = FakeSet::new();
        files.superblock.set_chunk(chunk);
        files.rows.set_chunk(chunk);
        files.rows_old.set_chunk(chunk);
        let mut host = Host::new(CAPS, choppy);
        run_workload(&mut host, &ops);

        assert_eq!(
            files.superblock.contents(),
            ref_files.superblock.contents(),
            "chunk={chunk}: superblock differs under short writes"
        );
        assert_eq!(
            files.rows.contents(),
            ref_files.rows.contents(),
            "chunk={chunk}: rows differ under short writes"
        );

        // And short READS on recovery still reconstruct everything.
        let mut reopened = Host::new(CAPS, files.reopen());
        assert_eq!(open_ok(&mut reopened), Ok(INSERTS as u64), "chunk={chunk}");
        for &(id, value) in &ops {
            assert_eq!(get_ok(&mut reopened, id), Some(value), "chunk={chunk}");
        }
    }
}

/// Zero progress is not a short read — it is a broken handle, and it must
/// be refused loudly. A half-filled buffer returned as data would be
/// exactly the silent corruption this project exists to prevent.
#[test]
fn zero_progress_io_is_refused_not_papered_over() {
    // A stalled READ during recovery.
    let ops = gen_workload(5, 4);
    let (opfs, files) = FakeSet::new();
    run_workload(&mut Host::new(CAPS, opfs), &ops);

    let mut storage = files.reopen();
    files.superblock.stall_once();
    let err = storage
        .read(FileId::Superblock, 0, 64)
        .expect_err("a stalled read must fail");
    assert!(
        matches!(err, OpfsError::ShortRead { got: 0, .. }),
        "wrong error for a stalled read: {err:?}"
    );

    // A stalled WRITE.
    files.rows.stall_once();
    let err = storage
        .write(FileId::Rows, 0, &[1, 2, 3, 4])
        .expect_err("a stalled write must fail");
    assert!(
        matches!(err, OpfsError::ShortWrite { got: 0, .. }),
        "wrong error for a stalled write: {err:?}"
    );
}

/// A handle failure (`DOMException` in the browser) fail-stops the engine
/// through the same path every other backend's I/O errors take.
#[test]
fn handle_failures_fail_stop_the_engine() {
    for fail_at in 0..12u64 {
        let (opfs, files) = FakeSet::new();
        files.superblock.fail_after(fail_at);
        files.rows.fail_after(fail_at);
        let mut host = Host::new(CAPS, opfs);
        // Whatever it is doing when the handle dies, it must be a clean
        // refusal — never a panic, never a silent success.
        match host.open() {
            Ok(Output::OpenDone { result: Ok(_) }) => {
                // Opened before the budget ran out; the next write dies.
                match host.insert(1, [7; VALUE_LEN]) {
                    Output::InsertDone { result: Ok(()), .. } => {}
                    Output::InsertDone {
                        result: Err(DbError::IoFailed { .. }),
                        ..
                    } => {}
                    other => panic!("fail_at={fail_at}: unexpected insert: {other:?}"),
                }
            }
            Ok(Output::OpenDone {
                result: Err(DbError::IoFailed { .. }),
            }) => {}
            Ok(other) => panic!("fail_at={fail_at}: unexpected open: {other:?}"),
            Err(e) => {
                // The size probe itself failed: also a clean error.
                assert!(
                    matches!(e, OpfsError::Handle(_)),
                    "fail_at={fail_at}: {e:?}"
                );
            }
        }
    }
}

/// Writing past the end zero-fills the gap — the assumption the whole
/// superblock layout rests on (slots are written out of order). Asserted
/// here against the model, and against real OPFS in the browser suite.
#[test]
fn writing_past_the_end_zero_fills_the_gap() {
    let (mut storage, files) = FakeSet::new();
    storage
        .write(FileId::Rows, 100, &[0xAB; 4])
        .expect("gap write");
    let bytes = files.rows.contents();
    assert_eq!(bytes.len(), 104, "file did not extend to the write");
    assert!(
        bytes[..100].iter().all(|&b| b == 0),
        "the gap was not zero-filled: {:?}",
        &bytes[..16]
    );
    assert_eq!(&bytes[100..], &[0xAB; 4]);

    // Reads clamp to EOF rather than erroring, exactly like POSIX.
    assert_eq!(
        storage.read(FileId::Rows, 200, 16).expect("past EOF"),
        Vec::<u8>::new()
    );
    assert_eq!(
        storage
            .read(FileId::Rows, 96, 64)
            .expect("straddling")
            .len(),
        8
    );
    assert_eq!(storage.read(FileId::Rows, 0, 0).expect("empty").len(), 0);
}

/// The browser's crash story: only flushed bytes survive a tab that dies.
/// Recovering from exactly the flushed image must still serve every
/// acknowledged row — the same all-or-nothing property the crash sweeps
/// prove on the simulated disk.
#[test]
fn recovery_from_the_flushed_image_loses_no_acknowledged_row() {
    for seed in 0..6u64 {
        let ops = gen_workload(seed, INSERTS);
        let (opfs, files) = FakeSet::new();
        run_workload(&mut Host::new(CAPS, opfs), &ops);

        // The tab dies: unflushed bytes evaporate.
        let (crashed, survivors) = FakeSet::new();
        survivors
            .superblock
            .set_contents(files.superblock.flushed_contents());
        survivors.rows.set_contents(files.rows.flushed_contents());

        let mut host = Host::new(CAPS, crashed);
        assert_eq!(open_ok(&mut host), Ok(INSERTS as u64), "seed={seed}");
        for &(id, value) in &ops {
            assert_eq!(get_ok(&mut host, id), Some(value), "seed={seed} id={id}");
        }
        assert!(files.superblock.flushes() > 0, "nothing was ever flushed");
    }
}
