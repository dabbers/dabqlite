//! A negative control. The crash test's whole value rests on its
//! detectors firing when data really is lost, so here we lose some on
//! purpose — through the public API — and check that each detector
//! catches it. Without this, "no failures in 150 kills" means nothing.

use std::path::PathBuf;

use dabqlite::Db;
use jobqueue::{
    audit, encode_meta, expected_checksum, inspect, run, Config, Journal, Layout,
    ROW_COMMIT_WATERMARK,
};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-det-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const JOBS: u64 = 40;

fn drained_queue(tag: &str) -> (PathBuf, Config) {
    let root = scratch(tag);
    let journal_path = root.join("j.log");
    let mut cfg = Config::new(root.join("db"), &journal_path, JOBS);
    cfg.capacity = 128;
    cfg.window = 4;
    let mut journal = Journal::open(&journal_path).unwrap();
    let rep = run(&cfg, &mut journal).unwrap();
    assert!(rep.drained);
    (root, cfg)
}

#[test]
fn a_healthy_queue_passes_every_detector() {
    let (root, cfg) = drained_queue("ok");
    let ins = inspect(&cfg).unwrap();
    let a = audit(&cfg.journal).unwrap();
    assert_eq!(ins.committed, JOBS);
    assert_eq!(ins.checksum, expected_checksum(ins.committed));
    assert!(a.duplicate_commits.is_empty());
    assert!(*a.committed.last().unwrap() <= ins.committed);
    std::fs::remove_dir_all(&root).ok();
}

/// Detector 3: the order-sensitive checksum. Roll the commit watermark
/// back by one without touching the checksum — the arithmetic no longer
/// describes an exact in-order commit of `1..=committed`.
#[test]
fn a_rolled_back_commit_is_caught_by_the_checksum() {
    let (root, cfg) = drained_queue("chk");
    {
        let layout = Layout::new(&cfg.root);
        let mut db = Db::open_with(layout.live(), cfg.capacity).unwrap();
        let stale = expected_checksum(JOBS);
        db.put(ROW_COMMIT_WATERMARK, encode_meta(JOBS - 1, stale))
            .unwrap();
    }
    let ins = inspect(&cfg).unwrap();
    assert_ne!(
        ins.checksum,
        expected_checksum(ins.committed),
        "the checksum detector failed to notice a rolled-back commit"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Detector 2: a *clean* rollback — watermark and checksum both moved
/// back one commit, so the database looks internally consistent. Only the
/// out-of-band journal, which saw the acknowledgement, can tell.
#[test]
fn a_clean_rollback_is_caught_by_the_journal() {
    let (root, cfg) = drained_queue("jrn");
    {
        let layout = Layout::new(&cfg.root);
        let mut db = Db::open_with(layout.live(), cfg.capacity).unwrap();
        db.put(
            ROW_COMMIT_WATERMARK,
            encode_meta(JOBS - 1, expected_checksum(JOBS - 1)),
        )
        .unwrap();
    }
    let ins = inspect(&cfg).unwrap();
    assert_eq!(
        ins.checksum,
        expected_checksum(ins.committed),
        "this rollback is internally consistent by construction"
    );
    let a = audit(&cfg.journal).unwrap();
    assert!(
        *a.committed.last().unwrap() > ins.committed,
        "the journal detector failed to notice a clean rollback"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Detector 1: monotonicity. A watermark that goes backwards between two
/// restarts is lost acknowledged data, whatever else looks fine.
#[test]
fn a_backwards_watermark_is_caught_by_monotonicity() {
    let (root, cfg) = drained_queue("mono");
    let before = inspect(&cfg).unwrap().committed;
    {
        let layout = Layout::new(&cfg.root);
        let mut db = Db::open_with(layout.live(), cfg.capacity).unwrap();
        db.put(ROW_COMMIT_WATERMARK, encode_meta(3, expected_checksum(3)))
            .unwrap();
    }
    assert!(inspect(&cfg).unwrap().committed < before);
    std::fs::remove_dir_all(&root).ok();
}

/// Detector 4: duplicate commits in the journal. This is what a lost
/// acknowledged commit looks like from the outside: the restart redoes it.
#[test]
fn a_duplicated_commit_is_caught_by_the_audit() {
    let (root, cfg) = drained_queue("dup");
    assert!(audit(&cfg.journal).unwrap().duplicate_commits.is_empty());
    {
        let mut j = Journal::open(&cfg.journal).unwrap();
        j.record("K 17").unwrap();
    }
    assert_eq!(
        audit(&cfg.journal).unwrap().duplicate_commits,
        vec![17],
        "the audit failed to notice a job committed twice"
    );
    std::fs::remove_dir_all(&root).ok();
}
