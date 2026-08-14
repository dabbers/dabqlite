#![cfg(unix)]
//! Scale on REAL files: 100k rows through actual POSIX I/O with real
//! fsyncs, byte-identical to the simulator at the same scale, recovered
//! through the real read path. The sim scale suite proves the engine at
//! 1M rows; this proves the storage seam doesn't bend at volume.

use std::convert::Infallible;
use std::path::PathBuf;

use dabqlite_core::{Capacities, FileId, Output, VALUE_LEN};
use dabqlite_host::{Host, PosixStorage, Storage};
use dabqlite_sim::SimDisk;

const N: u64 = 100_000;

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

fn value_for(id: u64) -> [u8; VALUE_LEN] {
    let mut v = [0u8; VALUE_LEN];
    v[..8].copy_from_slice(&id.to_le_bytes());
    v[8..].copy_from_slice(&(!id).to_le_bytes());
    v
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-scale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "scale suite runs in release (assertions stay on); CI runs it explicitly"
)]
fn hundred_thousand_rows_on_real_files() {
    let caps = Capacities { rows: N };

    let mut sim = Host::new(caps, SimStorage(SimDisk::new()));
    let dir = scratch_dir();
    let mut posix = Host::new(caps, PosixStorage::open_dir(&dir).expect("open dir"));
    assert!(matches!(
        sim.open().expect("sim"),
        Output::OpenDone { result: Ok(0) }
    ));
    assert!(matches!(
        posix.open().expect("posix"),
        Output::OpenDone { result: Ok(0) }
    ));

    for id in 0..N {
        let v = value_for(id);
        assert!(matches!(
            sim.insert(id, v),
            Output::InsertDone { result: Ok(()), .. }
        ));
        assert!(matches!(
            posix.insert(id, v),
            Output::InsertDone { result: Ok(()), .. }
        ));
    }
    drop(posix); // close handles + release the lock: real reopen below

    // Byte-for-byte at volume: 3.2 MB of rows + superblock identical.
    for (file, name) in [
        (
            FileId::Superblock,
            dabqlite_host::posix::SUPERBLOCK_FILE.to_string(),
        ),
        (
            FileId::Rows,
            dabqlite_host::posix::rows_file_name(dabqlite_core::SCHEMA_HASH),
        ),
    ] {
        let sim_bytes = sim.storage.0.contents(file);
        let real_bytes = std::fs::read(dir.join(&name)).expect("read back");
        assert_eq!(sim_bytes, real_bytes, "{name} diverged at scale");
    }

    // Real recovery of 100k rows through actual file reads and fsyncs.
    let mut reopened = Host::new(caps, PosixStorage::open_dir(&dir).expect("reopen"));
    assert!(matches!(
        reopened.open().expect("reopen"),
        Output::OpenDone { result: Ok(n) } if n == N
    ));
    assert!(!reopened.engine.recovery_report().rollback_evidence);
    for id in (0..N).step_by(997) {
        assert!(matches!(
            reopened.get(id),
            Output::GetDone { result: Ok(Some(v)), .. } if v == value_for(id)
        ));
    }
    std::fs::remove_dir_all(&dir).ok();
}
