#![cfg(unix)]
//! Simulator/reality equivalence. The entire fault harness lives on the
//! simulated disk; these tests are the proof that the simulation is not a
//! comfortable fiction:
//!
//! 1. **Byte equivalence**: the same seeded workload, driven through the
//!    same generic `Host`, against the simulator and against real POSIX
//!    files, produces byte-for-byte identical files. The core is
//!    deterministic, so any divergence is a storage-contract mismatch —
//!    exactly the kind of bug that would silently invalidate every
//!    simulated result.
//! 2. **Fault-outcome equivalence**: identical at-rest damage (bit flips,
//!    truncations) to both copies produces identical recovery outcomes,
//!    row for row, error for error.
//! 3. **Real-file recovery**: closing and reopening actual files recovers
//!    everything, through the real fsync path.

use std::convert::Infallible;
use std::path::PathBuf;

use dabqlite_core::{Capacities, DbError, FileId, Output, VALUE_LEN};
use dabqlite_host::{Host, PosixStorage, Storage};
use dabqlite_sim::{gen_workload, SimDisk};

const CAPS: Capacities = Capacities { rows: 32 };
const INSERTS: usize = 12;

/// The simulated disk behind the same `Storage` seam the real backend uses,
/// so both runs share every line of driver code.
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
    let dir = std::env::temp_dir().join(format!("dabqlite-equiv-{}-{tag}", std::process::id()));
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

#[test]
fn same_workload_identical_bytes_on_sim_and_posix() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, INSERTS);

        let mut sim = Host::new(CAPS, SimStorage(SimDisk::new()));
        run_workload(&mut sim, &ops);

        let dir = scratch_dir(&format!("bytes-{seed}"));
        let mut posix = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
        run_workload(&mut posix, &ops);
        drop(posix); // close the handles: the reopen below is a real reopen

        // Byte-for-byte: if these differ, the simulator and reality disagree
        // about the storage contract, and every simulated result is suspect.
        for (file, name) in [
            (FileId::Superblock, dabqlite_host::posix::SUPERBLOCK_FILE),
            (FileId::Rows, dabqlite_host::posix::ROWS_FILE),
        ] {
            let sim_bytes = sim.storage.0.contents(file);
            let real_bytes = std::fs::read(dir.join(name)).expect("read back");
            assert_eq!(
                sim_bytes, real_bytes,
                "seed={seed}: {name} diverged between simulator and disk"
            );
        }

        // Real-file recovery through the real fsync path.
        let mut reopened = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("reopen"));
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

#[test]
fn at_rest_faults_produce_identical_outcomes_on_sim_and_posix() {
    let seed = 7u64;
    let ops = gen_workload(seed, INSERTS);

    // Pristine copies of both worlds.
    let mut sim = Host::new(CAPS, SimStorage(SimDisk::new()));
    run_workload(&mut sim, &ops);
    let pristine = sim.storage.0.clone();

    let master = scratch_dir("faults-master");
    let mut posix = Host::new(CAPS, PosixStorage::open_dir(&master).expect("open dir"));
    run_workload(&mut posix, &ops);
    drop(posix);

    // Fault grid: bit flips and truncations across both files.
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

        // Damage the simulated copy.
        let mut sim_disk = pristine.clone();
        match mask {
            Some(m) => sim_disk.corrupt(file, arg, m),
            None => sim_disk.truncate_at_rest(file, arg),
        }
        let mut sim_host = Host::new(CAPS, SimStorage(sim_disk));
        let sim_outcome = outcome_digest(&mut sim_host, &ops);

        // Damage a fresh copy of the real files identically.
        let dir = scratch_dir(&format!("faults-{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let name = match file {
            FileId::Superblock => dabqlite_host::posix::SUPERBLOCK_FILE,
            FileId::Rows => dabqlite_host::posix::ROWS_FILE,
        };
        for n in [
            dabqlite_host::posix::SUPERBLOCK_FILE,
            dabqlite_host::posix::ROWS_FILE,
        ] {
            std::fs::copy(master.join(n), dir.join(n)).expect("copy");
        }
        match mask {
            Some(m) => {
                let mut bytes = std::fs::read(dir.join(name)).expect("read");
                bytes[arg as usize] ^= m;
                std::fs::write(dir.join(name), bytes).expect("write");
            }
            None => {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(dir.join(name))
                    .expect("open");
                f.set_len(arg).expect("truncate");
            }
        }
        let mut posix_host = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("reopen"));
        let posix_outcome = outcome_digest(&mut posix_host, &ops);

        assert_eq!(
            sim_outcome, posix_outcome,
            "[{ctx}] simulator and reality disagree about the same damage"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::remove_dir_all(&master).ok();
}
