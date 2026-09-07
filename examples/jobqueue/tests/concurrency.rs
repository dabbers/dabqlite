//! A job queue wants more than one worker. dabqlite is single-writer by
//! construction, so this is about how the refusal is reported and whether
//! an application can act on it.

use std::path::PathBuf;
use std::process::Command;

use dabqlite::{Db, Error};
use jobqueue::{audit, expected_checksum, inspect, Config};

const EXE: &str = env!("CARGO_BIN_EXE_jobqueue");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-conc-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// There is no reader. Not "one writer and many readers" — *no* reader.
///
/// A dashboard that wants to show the queue depth while the worker runs
/// has nothing to open: `Db::open` takes the exclusive lock, and every
/// read method (`get`, `range`, `len`... ) takes `&mut self`, so even a
/// second handle in the same process could not share one. The only
/// lock-free open is `Db::salvage`, which is documented for DAMAGED
/// databases — and it does work on a healthy one, but its contract is
/// "questions the quarantine makes unanswerable return `Degraded`", which
/// is not something to build a status page on.
#[test]
fn there_is_no_concurrent_reader_only_a_salvage_open() {
    let dir = scratch("reader").join("db");
    {
        let mut db = Db::open(&dir).expect("writer");
        for i in 0..10u64 {
            db.insert(i, dabqlite::Value::from_text("x").unwrap())
                .unwrap();
        }
    }
    let mut writer = Db::open(&dir).expect("writer");

    // A would-be reader is refused exactly like a second writer.
    assert!(matches!(Db::open(&dir), Err(Error::Locked { .. })));

    // `salvage` takes no lock, and on a healthy database it reads fine.
    let reader = dabqlite::SalvageDb::salvage(&dir).expect("salvage open");
    assert_eq!(reader.len(), 10);
    assert_eq!(reader.get(3).unwrap().unwrap().text(), "x");

    // But it is a SNAPSHOT of the moment it opened, and it will not see
    // anything the writer does afterwards. There is no way to refresh it
    // short of reopening.
    writer
        .insert(99, dabqlite::Value::from_text("new").unwrap())
        .unwrap();
    assert_eq!(reader.get(99).unwrap(), None, "the salvage handle is stale");
    assert_eq!(reader.len(), 10);
    let fresh = dabqlite::SalvageDb::salvage(&dir).expect("salvage open");
    assert_eq!(
        fresh.get(99).unwrap().map(|v| v.text().to_string()),
        Some("new".into())
    );

    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
}

/// The second writer is refused, and the refusal is its OWN error variant.
///
/// This is the branch a queue actually needs: a worker that loses the race
/// should back off and retry, while a genuine I/O failure should page
/// someone. `Error::Locked` makes that a match arm rather than a substring
/// search over a `Debug`-rendered `std::io::Error` — and, crucially, an
/// `Error::Io` here now means a real I/O failure, so retrying on it would
/// be wrong.
#[test]
fn a_second_writer_is_refused_with_a_distinct_locked_error() {
    let dir = scratch("lock").join("db");
    let _first = Db::open(&dir).expect("first writer");

    let second = Db::open(&dir);
    let detail = match second {
        Err(Error::Locked { detail }) => detail,
        other => panic!("expected Error::Locked, got {other:?}"),
    };
    // The detail is for humans; the variant is the contract. Both should
    // point at the single-writer rule rather than at errno trivia.
    assert!(
        detail.contains("single-writer"),
        "the lock message should explain the rule it enforces: {detail}"
    );
    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
}

/// The classification is total: contention is `Locked` and NOTHING else
/// reported by a healthy open is. A worker that retries on `Locked` must
/// not silently retry a corrupt or unreadable database forever.
#[test]
fn a_healthy_uncontended_open_is_never_reported_as_locked() {
    let dir = scratch("unlocked").join("db");
    {
        let _first = Db::open(&dir).expect("first writer");
    } // lock released on drop

    match Db::open(&dir) {
        Ok(_) => {}
        other => panic!("expected a clean reopen after the lock was dropped, got {other:?}"),
    }
    std::fs::remove_dir_all(dir.parent().unwrap()).ok();
}

/// Two worker processes racing on the same queue: one wins the lock, the
/// other fails cleanly, and the queue stays exactly correct. This is the
/// honest deployment story — the second worker is not a worker, it is an
/// error.
#[test]
fn two_racing_workers_do_not_corrupt_the_queue() {
    let root = scratch("race");
    let journal = root.join("j.log");
    let jobs = 120u64;
    let mut cfg = Config::new(root.join("db"), &journal, jobs);
    cfg.capacity = 4096;
    cfg.window = 4;

    let args = |c: &Config| {
        vec![
            "run".to_string(),
            "--root".into(),
            c.root.display().to_string(),
            "--journal".into(),
            c.journal.display().to_string(),
            "--jobs".into(),
            c.jobs.to_string(),
            "--capacity".into(),
            c.capacity.to_string(),
            "--window".into(),
            c.window.to_string(),
            "--delay-us".into(),
            "200".into(),
        ]
    };

    let mut started = 0;
    let mut refusals = 0;
    // Four rounds of three overlapping workers. Exactly one holds the
    // flock at a time; the others die on open.
    for _ in 0..4 {
        let mut kids: Vec<_> = (0..3)
            .map(|_| Command::new(EXE).args(args(&cfg)).spawn().unwrap())
            .collect();
        for k in kids.iter_mut() {
            let st = k.wait().unwrap();
            started += 1;
            match st.code() {
                Some(0) | Some(10) => {}
                Some(1) => refusals += 1,
                other => panic!("unexpected worker exit {other:?}"),
            }
        }
    }
    assert_eq!(started, 12);
    assert!(
        refusals > 0,
        "the workers never actually contended for the lock"
    );

    // Drain and check the queue is exactly right regardless.
    loop {
        let out = Command::new(EXE).args(args(&cfg)).output().unwrap();
        if out.status.code() == Some(0) {
            break;
        }
    }
    let ins = inspect(&cfg).unwrap();
    assert_eq!(ins.committed, jobs);
    assert!(ins.short_payloads.is_empty(), "{:?}", ins.short_payloads);
    assert_eq!(ins.checksum, expected_checksum(jobs));
    let a = audit(&journal).unwrap();
    assert!(a.duplicate_commits.is_empty(), "{:?}", a.duplicate_commits);
    assert!(
        a.duplicate_enqueues.is_empty(),
        "{:?}",
        a.duplicate_enqueues
    );
    std::fs::remove_dir_all(&root).ok();
}
