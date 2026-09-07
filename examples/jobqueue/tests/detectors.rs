//! A negative control. The crash test's whole value rests on its
//! detectors firing when data really is lost, so here we lose some on
//! purpose — through the public API — and check that each detector
//! catches it. Without this, "no failures in 150 kills" means nothing.

use std::path::PathBuf;

use dabqlite::{Db, Value};
use jobqueue::{
    audit, encode_meta, expected_checksum, inspect, payload_of, run, Config, Job, Journal, DONE,
    JOB_HEADER, PENDING, ROW_COMMIT_WATERMARK,
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
    cfg.capacity = 4096;
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
    assert!(ins.short_payloads.is_empty());
    assert!(a.duplicate_commits.is_empty());
    assert!(*a.committed.last().unwrap() <= ins.committed);
    // The workload really did write values that span slots.
    assert!(
        a.widest_payload > 1_000,
        "the payload mixture never produced a big one: {a:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Detector 3: the order- and content-sensitive checksum. Roll the commit
/// watermark back by one without touching the checksum.
#[test]
fn a_rolled_back_commit_is_caught_by_the_checksum() {
    let (root, cfg) = drained_queue("chk");
    {
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
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
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
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
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
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

/// Detector 5: a HALF-APPLIED BATCH. Forge each intermediate state that
/// `Db::batch` is supposed to make unreachable.
#[test]
fn a_forged_half_applied_enqueue_batch_is_caught() {
    let (root, cfg) = drained_queue("halfe");
    assert!(inspect(&cfg).unwrap().half_batch.is_none());
    {
        // The enqueue batch is [insert job..., put watermark]. Land only
        // an insert: a job row appears at (not before) the watermark.
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
        db.insert(
            JOBS + 1,
            Job::new(PENDING, 0, payload_of(JOBS + 1)).encode().unwrap(),
        )
        .unwrap();
    }
    let why = inspect(&cfg)
        .unwrap()
        .half_batch
        .expect("the half-batch detector missed an insert without its watermark");
    assert!(why.contains("enqueue watermark"), "{why}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_forged_half_applied_commit_batch_is_caught() {
    let (root, cfg) = drained_queue("halfk");
    {
        // The commit batch is [put watermark, delete job]. Land only the
        // watermark: a job row survives at or below it.
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
        db.insert(7, Job::new(DONE, 1, payload_of(7)).encode().unwrap())
            .unwrap();
    }
    let why = inspect(&cfg)
        .unwrap()
        .half_batch
        .expect("the half-batch detector missed a watermark without its delete");
    assert!(why.contains("commit watermark"), "{why}");
    std::fs::remove_dir_all(&root).ok();
}

/// Detector 6, the one this revision exists for: a payload that comes back
/// SHORT.
///
/// Two ways it could show, and both are checked, because a store that
/// truncated a multi-slot value could plausibly produce either:
///
/// * the value is shorter than its own declared length — what a lost tail
///   slot looks like;
/// * the value is the right length but not the right bytes — what a lost
///   MIDDLE slot, refilled from a stale incarnation, would look like.
///
/// Forged through the public API, because there is no way to make the
/// library do it on purpose.
#[test]
fn a_forged_short_payload_is_caught() {
    let (root, cfg) = drained_queue("short");
    assert!(inspect(&cfg).unwrap().short_payloads.is_empty());

    // Pick an id whose payload spans several slots, so the truncation
    // below removes whole rows and not just bytes inside one.
    let victim = (JOBS + 1..)
        .find(|&id| jobqueue::payload_of(id).len() > 3 * 16)
        .expect("some id has a multi-slot payload");
    let full = Job::new(PENDING, 0, payload_of(victim)).encode().unwrap();

    // (a) truncated: the header still says how long it was.
    {
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
        let cut = full.as_bytes()[..JOB_HEADER + 5].to_vec();
        db.put(victim, Value::from_vec(cut).unwrap()).unwrap();
    }
    let ins = inspect(&cfg).unwrap();
    let (id, declared, got) = *ins
        .short_payloads
        .first()
        .expect("the short-payload detector missed a truncated value");
    assert_eq!(id, victim);
    assert!(declared > got, "{declared} vs {got}");

    // (b) right length, wrong bytes.
    {
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
        let mut b = full.as_bytes().to_vec();
        let n = b.len();
        b[n - 1] ^= 0xFF;
        db.put(victim, Value::from_vec(b).unwrap()).unwrap();
    }
    let ins = inspect(&cfg).unwrap();
    assert_eq!(
        ins.short_payloads.first().map(|&(id, _, _)| id),
        Some(victim),
        "the detector missed a corrupted-but-correctly-sized payload"
    );

    std::fs::remove_dir_all(&root).ok();
}
