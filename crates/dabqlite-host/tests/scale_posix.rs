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

/// Tests here both SPAWN processes and hold the single-writer lock, and
/// those two things interact badly in parallel: `Command::spawn` forks,
/// and between fork and exec the child holds duplicates of every parent
/// fd — including a flock'd lock file another test in this binary is
/// using. flock is held by the open file description, so the lock appears
/// taken until the child execs and its O_CLOEXEC copies close, and a
/// concurrent `open_dir` sees a phantom `WouldBlock`.
///
/// The same guard `locking.rs` carries, for the same reason. (Found as a
/// flaky failure at roughly one run in three — and a flaky suite makes
/// every mutation-testing kill meaningless, which is why it is worth
/// fixing rather than retrying.)
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    let _serial = serial();
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

/// Salvage and rebuild at volume. Containment that only works on toy
/// databases is not containment: a rescue is needed precisely when the
/// database is large enough that losing it matters.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "scale suite runs in release (assertions stay on); CI runs it explicitly"
)]
fn salvage_and_repair_at_a_hundred_thousand_rows() {
    let _serial = serial();
    use dabqlite_host::rows_file_name;
    let caps = Capacities { rows: N };
    // Its own directory: these tests run in parallel in one process, so a
    // shared scratch path would have them clobber each other.
    let dir = std::env::temp_dir().join(format!("dabqlite-scale-salvage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    {
        let mut host = Host::new(caps, PosixStorage::open_dir(&dir).expect("open"));
        assert!(matches!(
            host.open().expect("probe"),
            Output::OpenDone { result: Ok(0) }
        ));
        for id in 0..N {
            match host.insert(id, value_for(id)) {
                Output::InsertDone { result: Ok(()), .. } => {}
                other => panic!("insert {id}: {other:?}"),
            }
        }
    }

    // Damage rows scattered across the whole file, including both ends.
    let victims = [0u64, 1, N / 3, N / 2, N - 2, N - 1];
    let rows_path = dir.join(rows_file_name(dabqlite_core::SCHEMA_HASH));
    let mut bytes = std::fs::read(&rows_path).expect("read rows");
    for &v in &victims {
        bytes[v as usize * dabqlite_core::ROW_SIZE + 11] ^= 0x80;
    }
    std::fs::write(&rows_path, bytes).expect("write rows");

    // Strict open refuses; salvage contains exactly the damage.
    {
        let mut host = Host::new(caps, PosixStorage::open_dir(&dir).expect("open"));
        assert!(matches!(
            host.open().expect("probe"),
            Output::OpenDone {
                result: Err(dabqlite_core::DbError::Corrupt { .. })
            }
        ));
    }
    let mut host = Host::new(caps, PosixStorage::open_dir(&dir).expect("open"));
    match host.open_salvage().expect("probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, N),
        other => panic!("salvage at scale: {other:?}"),
    }
    assert_eq!(host.engine.quarantined(), victims.len() as u64);
    // Every undamaged row — all ~100k of them — is still exact.
    for id in 0..N {
        if victims.contains(&id) {
            continue;
        }
        match host.get(id) {
            Output::GetDone {
                result: Ok(Some(v)),
                ..
            } => assert_eq!(v, value_for(id)),
            other => panic!("survivor {id}: {other:?}"),
        }
    }
    drop(host);

    // Rebuild at volume, then verify the rebuilt database exhaustively.
    let dest = std::env::temp_dir().join(format!("dabqlite-scale-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dabqlite-inspect"))
        .arg(&dir)
        .arg("--repair-to")
        .arg(&dest)
        .output()
        .expect("run inspector");
    assert!(
        out.status.success(),
        "repair at scale failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut host = Host::new(caps, PosixStorage::open_dir(&dest).expect("open rebuilt"));
    match host.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => assert_eq!(n, N - victims.len() as u64),
        other => panic!("rebuilt database: {other:?}"),
    }
    assert!(!host.engine.is_degraded());
    for id in 0..N {
        let got = match host.get(id) {
            Output::GetDone { result: Ok(v), .. } => v,
            other => panic!("rebuilt get {id}: {other:?}"),
        };
        if victims.contains(&id) {
            assert_eq!(got, None, "row {id} was resurrected");
        } else {
            assert_eq!(got, Some(value_for(id)), "rebuilt row {id}");
        }
    }

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dest).ok();
}
