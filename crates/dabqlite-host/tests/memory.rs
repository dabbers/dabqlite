#![cfg(unix)]
//! The in-memory backend, held to exactly the same bar as the durable ones.
//!
//! An in-memory store is the easy case to get *approximately* right and a
//! tempting place to cut corners, so this suite refuses to treat it as a
//! lesser backend. It is driven through the same `Storage` contract, the
//! same generic host driver, and the same comparisons that validate POSIX
//! files and OPFS:
//!
//! - a workload produces **byte-identical images** to the simulator and to
//!   real files — so an in-memory database is the same database;
//! - identical at-rest damage produces **identical outcomes**, which is
//!   what lets the whole simulated fault matrix mean something here;
//! - corruption containment (salvage) behaves identically;
//! - snapshot/restore round-trips exactly, at every commit boundary —
//!   the property a browser without OPFS actually depends on;
//! - an image moves BOTH WAYS between memory and disk, so the browser and
//!   the server are interchangeable.
//!
//! What is NOT claimed is durability: `sync` has nothing to flush and
//! process death takes everything. That is stated in the module docs and
//! pinned here, rather than left for someone to discover.

use std::convert::Infallible;
use std::path::PathBuf;

use dabqlite_core::{Capacities, DbError, FileId, Output, ROW_SIZE, VALUE_LEN};
use dabqlite_host::{rows_file_name, Host, MemoryStorage, PosixStorage, Storage, SUPERBLOCK_FILE};
use dabqlite_sim::{gen_workload, SimDisk};

const CAPS: Capacities = Capacities { rows: 32 };
const INSERTS: usize = 12;

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

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dabqlite-mem-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open_ok<S: Storage>(host: &mut Host<S>) -> Result<u64, DbError> {
    match host.open().expect("size probe") {
        Output::OpenDone { result } => result,
        other => panic!("open: {other:?}"),
    }
}

fn get_ok<S: Storage>(host: &mut Host<S>, id: u64) -> Option<[u8; VALUE_LEN]> {
    match host.get(id) {
        Output::GetDone { result: Ok(v), .. } => v,
        other => panic!("get: {other:?}"),
    }
}

fn run<S: Storage>(host: &mut Host<S>, ops: &[(u64, [u8; VALUE_LEN])]) {
    assert_eq!(open_ok(host), Ok(0));
    for &(id, value) in ops {
        match host.insert(id, value) {
            Output::InsertDone { result: Ok(()), .. } => {}
            other => panic!("insert: {other:?}"),
        }
    }
}

/// An in-memory database is the SAME database: identical bytes to the
/// simulator and to real files, for the same workload.
#[test]
fn memory_posix_and_sim_produce_identical_bytes() {
    for seed in 0..8u64 {
        let ops = gen_workload(seed, INSERTS);

        let mut mem = Host::new(CAPS, MemoryStorage::new());
        run(&mut mem, &ops);

        let mut sim = Host::new(CAPS, SimStorage(SimDisk::new()));
        run(&mut sim, &ops);

        let dir = scratch(&format!("bytes-{seed}"));
        let mut posix = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
        run(&mut posix, &ops);
        drop(posix);

        for (file, name) in [
            (FileId::Superblock, SUPERBLOCK_FILE.to_string()),
            (FileId::Rows, rows_file_name(dabqlite_core::SCHEMA_HASH)),
        ] {
            let disk = std::fs::read(dir.join(&name)).expect("read back");
            assert_eq!(
                sim.storage.0.contents(file),
                disk,
                "seed={seed}: {name} simulator vs disk"
            );
            assert_eq!(
                mem.storage.image(file),
                &disk[..],
                "seed={seed}: {name} memory vs disk"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Snapshot at EVERY commit boundary, restore, and the database is
/// exactly what it was — the round-trip a browser without OPFS lives on.
#[test]
fn snapshot_and_restore_round_trip_at_every_commit_boundary() {
    let ops = gen_workload(4, INSERTS);
    let mut host = Host::new(CAPS, MemoryStorage::new());
    assert_eq!(open_ok(&mut host), Ok(0));

    for (n, &(id, value)) in ops.iter().enumerate() {
        assert!(matches!(
            host.insert(id, value),
            Output::InsertDone { result: Ok(()), .. }
        ));

        // Snapshot the live database after every commit and reload it.
        let (sb, rows, rows_old) = host.storage.snapshot();
        let restored = MemoryStorage::from_images(sb.clone(), rows.clone(), rows_old.clone());
        let mut reloaded = Host::new(CAPS, restored);
        assert_eq!(
            open_ok(&mut reloaded),
            Ok(n as u64 + 1),
            "restore after {} commits",
            n + 1
        );
        for &(rid, rvalue) in &ops[..=n] {
            assert_eq!(
                get_ok(&mut reloaded, rid),
                Some(rvalue),
                "restored id={rid} after {} commits",
                n + 1
            );
        }
        // The reload is byte-identical, and writable from there.
        assert_eq!(reloaded.storage.image(FileId::Superblock), &sb[..]);
        assert_eq!(reloaded.storage.image(FileId::Rows), &rows[..]);
        assert!(
            !reloaded.engine.recovery_report().rollback_evidence,
            "clean restore flagged rollback"
        );
    }
}

/// An image moves BOTH WAYS between RAM and disk: a database built in a
/// browser opens on a server, and vice versa.
#[test]
fn images_move_between_memory_and_disk_in_both_directions() {
    let ops = gen_workload(11, INSERTS);

    // Memory -> disk.
    let mut mem = Host::new(CAPS, MemoryStorage::new());
    run(&mut mem, &ops);
    let dir = scratch("mem-to-disk");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join(SUPERBLOCK_FILE),
        mem.storage.image(FileId::Superblock),
    )
    .expect("write sb");
    std::fs::write(
        dir.join(rows_file_name(dabqlite_core::SCHEMA_HASH)),
        mem.storage.image(FileId::Rows),
    )
    .expect("write rows");

    let mut on_disk = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open dir"));
    assert_eq!(open_ok(&mut on_disk), Ok(ops.len() as u64));
    for &(id, value) in &ops {
        assert_eq!(get_ok(&mut on_disk, id), Some(value), "disk id={id}");
    }
    // Keep writing on disk, then carry it back to memory.
    assert!(matches!(
        on_disk.insert(777_777, [0xEE; VALUE_LEN]),
        Output::InsertDone { result: Ok(()), .. }
    ));
    drop(on_disk);

    // Disk -> memory.
    let back = MemoryStorage::from_images(
        std::fs::read(dir.join(SUPERBLOCK_FILE)).expect("sb"),
        std::fs::read(dir.join(rows_file_name(dabqlite_core::SCHEMA_HASH))).expect("rows"),
        Vec::new(),
    );
    let mut in_ram = Host::new(CAPS, back);
    assert_eq!(open_ok(&mut in_ram), Ok(ops.len() as u64 + 1));
    for &(id, value) in &ops {
        assert_eq!(get_ok(&mut in_ram, id), Some(value), "back in ram id={id}");
    }
    assert_eq!(get_ok(&mut in_ram, 777_777), Some([0xEE; VALUE_LEN]));
    std::fs::remove_dir_all(&dir).ok();
}

/// Identical damage, identical outcome — the property that lets the
/// simulated fault matrix mean something for an in-memory database too.
#[test]
fn at_rest_damage_produces_identical_outcomes_to_disk() {
    let ops = gen_workload(6, INSERTS);
    let mut pristine = Host::new(CAPS, MemoryStorage::new());
    run(&mut pristine, &ops);
    let (base_sb, base_rows, _) = pristine.storage.snapshot();

    let master = scratch("damage-master");
    let mut posix = Host::new(CAPS, PosixStorage::open_dir(&master).expect("open dir"));
    run(&mut posix, &ops);
    drop(posix);

    let mut cases: Vec<(FileId, u64, Option<u8>)> = Vec::new();
    for off in [0u64, 9, 33, 70, 129, 200, 255] {
        cases.push((FileId::Superblock, off, Some(0x20)));
    }
    for off in [0u64, 8, 25, 90, 200, 383] {
        cases.push((FileId::Rows, off, Some(0x20)));
    }
    for cut in [0u64, 8, 64, 120, 192] {
        cases.push((FileId::Superblock, cut, None));
    }
    for cut in [0u64, 32, 100, 350] {
        cases.push((FileId::Rows, cut, None));
    }

    for (i, &(file, arg, mask)) in cases.iter().enumerate() {
        let ctx = format!("case {i}: {file:?} {arg} {mask:?}");
        let damage = |bytes: &mut Vec<u8>| match mask {
            Some(m) => bytes[arg as usize] ^= m,
            None => bytes.truncate(arg as usize),
        };

        let mut sb = base_sb.clone();
        let mut rows = base_rows.clone();
        match file {
            FileId::Superblock => damage(&mut sb),
            _ => damage(&mut rows),
        }
        let mut mem = Host::new(CAPS, MemoryStorage::from_images(sb, rows, Vec::new()));
        let mem_outcome = digest(&mut mem, &ops);

        let dir = scratch(&format!("damage-{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let names = [
            SUPERBLOCK_FILE.to_string(),
            rows_file_name(dabqlite_core::SCHEMA_HASH),
        ];
        for n in &names {
            std::fs::copy(master.join(n), dir.join(n)).expect("copy");
        }
        let target = dir.join(match file {
            FileId::Superblock => &names[0],
            _ => &names[1],
        });
        let mut bytes = std::fs::read(&target).expect("read");
        damage(&mut bytes);
        std::fs::write(&target, bytes).expect("write");
        let mut disk = Host::new(CAPS, PosixStorage::open_dir(&dir).expect("open"));
        let disk_outcome = digest(&mut disk, &ops);

        assert_eq!(
            mem_outcome, disk_outcome,
            "[{ctx}] memory and disk disagree"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::remove_dir_all(&master).ok();
}

fn digest<S: Storage>(host: &mut Host<S>, ops: &[(u64, [u8; VALUE_LEN])]) -> String {
    match host.open().expect("probe") {
        Output::OpenDone { result: Ok(n) } => {
            let mut d = format!("ok n={n};");
            for &(id, _) in ops {
                match get_ok(host, id) {
                    Some(v) => d.push_str(&format!("{id}={v:02x?};")),
                    None => d.push_str(&format!("{id}=absent;")),
                }
            }
            d
        }
        Output::OpenDone { result: Err(e) } => format!("err {e:?}"),
        other => panic!("open: {other:?}"),
    }
}

/// Corruption containment works identically in RAM: one damaged row costs
/// one row, and salvage still refuses to guess about the rest.
#[test]
fn corruption_containment_works_in_memory_too() {
    let n = 8usize;
    let ops = gen_workload(13, n);
    let mut pristine = Host::new(CAPS, MemoryStorage::new());
    run(&mut pristine, &ops);
    let (sb, rows, _) = pristine.storage.snapshot();

    for victim in 0..n {
        let mut damaged = rows.clone();
        damaged[victim * ROW_SIZE + 4] ^= 0x40;
        let store = MemoryStorage::from_images(sb.clone(), damaged, Vec::new());

        let mut strict = Host::new(CAPS, store.clone());
        assert!(
            matches!(
                strict.open().expect("probe"),
                Output::OpenDone {
                    result: Err(DbError::Corrupt { .. })
                }
            ),
            "victim={victim}: strict open must refuse"
        );

        let mut rescue = Host::new(CAPS, store);
        match rescue.open_salvage().expect("probe") {
            Output::OpenDone { result: Ok(count) } => assert_eq!(count, n as u64),
            other => panic!("victim={victim}: salvage: {other:?}"),
        }
        assert_eq!(rescue.engine.quarantined(), 1, "victim={victim}");
        for (row, &(id, value)) in ops.iter().enumerate() {
            if row == victim {
                continue;
            }
            assert_eq!(
                get_ok(&mut rescue, id),
                Some(value),
                "victim={victim} survivor row {row}"
            );
        }
        // Salvage left the images untouched.
        assert_eq!(rescue.storage.image(FileId::Superblock), &sb[..]);
    }
}

/// The durability story, stated as a test so nobody has to infer it: the
/// commit protocol runs in full (the images change exactly as on disk),
/// but nothing outlives the process — dropping the store is total loss,
/// and that is the whole reason `snapshot` exists.
#[test]
fn memory_keeps_consistency_and_makes_no_durability_claim() {
    let ops = gen_workload(2, 6);
    let mut host = Host::new(CAPS, MemoryStorage::new());
    run(&mut host, &ops);

    // The commit protocol really ran: a full superblock zone and one row
    // slot per commit, exactly as a disk-backed database would hold.
    assert_eq!(host.storage.image(FileId::Rows).len(), ops.len() * ROW_SIZE);
    assert!(host.storage.image(FileId::Superblock).len() >= 64);
    assert_eq!(
        host.storage.byte_len(),
        host.storage.image(FileId::Superblock).len() + ops.len() * ROW_SIZE
    );

    // Dropping the store is the "crash": there is nothing to recover from.
    drop(host);
    let mut fresh = Host::new(CAPS, MemoryStorage::new());
    assert_eq!(
        open_ok(&mut fresh),
        Ok(0),
        "an in-memory store must not resurrect a dropped database"
    );
}
