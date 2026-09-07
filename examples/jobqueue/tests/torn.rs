//! Two interrupted commits of DIFFERENT widths, and what dabqlite says
//! about the state they leave behind.
//!
//! # History of this file
//!
//! `RecoveryReport::rollback_evidence` is the library's loudest signal:
//! "at least one *acknowledged* commit was rolled back". This crate treats
//! it as fatal — `Queue::open` refuses to start and the worker exits 3 —
//! because that is what the library asks hosts to do.
//!
//! The flag is computed by scanning the checksum-valid rows past the
//! manifest and comparing the commit span each one claims. Rows of one
//! interrupted commit all agree; disagreement was taken as proof that two
//! different commits were visible past the manifest.
//!
//! That inference used to have a hole, and this file used to prove it. The
//! rows file was appended to and never truncated, and recovery did not
//! clear the region past the manifest, so a second interrupted commit
//! NARROWER than the first overwrote only its own prefix and left the tail
//! of the first one in place — two disagreeing spans from two commits
//! **neither of which was ever acknowledged**. The library called that
//! acknowledged data loss, permanently, on a database that had lost
//! nothing, and only `Db::compact()` could clear it.
//!
//! That is fixed: recovery now truncates the rows file to the manifest
//! before it lets anyone write, so residue cannot survive a restart and
//! cannot pile up across incarnations. These tests assert the fix from the
//! other side. They are the regression test for it, and they also pin the
//! two things that did NOT change: the alarm still fires when the evidence
//! is real, and it still cannot see a rollback whose width matches an
//! ordinary interrupted commit.

use std::path::{Path, PathBuf};

use dabqlite::{Db, Op, Value, MAX_VALUE_LEN};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-torn-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

/// The rows file is the biggest `rows-*.dabq` in the directory. The names
/// are schema-hash-derived and undocumented, which is a finding in itself:
/// there is no supported way for an operator to know which file holds what.
fn rows_file(dir: &Path) -> PathBuf {
    let mut best: Option<(u64, PathBuf)> = None;
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("rows-") && name.ends_with(".dabq") {
            let len = e.metadata().unwrap().len();
            if best.as_ref().is_none_or(|(b, _)| len > *b) {
                best = Some((len, e.path()));
            }
        }
    }
    best.expect("a rows file").1
}

fn rows_len(dir: &Path) -> u64 {
    std::fs::metadata(rows_file(dir)).unwrap().len()
}

const COMMITTED: u64 = 10;
const WIDE: u64 = 64;
const NARROW: u64 = 2;
const CAP: u64 = 4096;

fn v(byte: u8) -> Value {
    Value::from_bytes(&[byte; 16]).unwrap()
}

/// A database with `COMMITTED` rows in it, in `dir`.
fn seed(dir: &Path, byte: u8) {
    let mut db = Db::open_with(dir, CAP).unwrap();
    for i in 0..COMMITTED {
        db.insert(i, v(byte)).unwrap();
    }
    assert_eq!(db.stats().slots, COMMITTED);
}

/// The rows file as it looks at the instant a kill lands between the rows
/// of a commit hitting the file and the superblock flip that makes them
/// visible: build the same commit in a copy of the same starting point and
/// steal its rows file.
fn rows_after_interrupted(base: &Path, work: &Path, ops: &[Op]) -> Vec<u8> {
    copy_dir(base, work);
    let mut db = Db::open_with(work, CAP).unwrap();
    db.batch(ops).unwrap();
    drop(db);
    std::fs::read(rows_file(work)).unwrap()
}

/// THE REGRESSION TEST.
///
/// Crash inside a wide commit, restart, crash inside a narrower one. That
/// sequence used to leave the wide commit's tail past the manifest under
/// the narrow one's prefix and raise a permanent false alarm. It no longer
/// can, because the restart in the middle truncates the rows file to the
/// manifest before the process is allowed to write anything.
#[test]
fn a_narrow_torn_commit_after_a_wide_one_no_longer_raises_the_alarm() {
    let root = scratch("mix");
    let (base, work, victim) = (root.join("base"), root.join("work"), root.join("victim"));

    seed(&base, 0x11);
    let clean_len = rows_len(&base);

    // ---- crash #1: inside a WIDE commit -------------------------------
    let wide_ops: Vec<Op> = (100..100 + WIDE).map(|i| Op::put(i, v(0x11))).collect();
    let wide_rows = rows_after_interrupted(&base, &work, &wide_ops);
    copy_dir(&base, &victim);
    std::fs::write(rows_file(&victim), &wide_rows).unwrap();
    assert!(rows_len(&victim) > clean_len);

    // ---- restart #1: an ordinary open ---------------------------------
    let db = Db::open_with(&victim, CAP).unwrap();
    let rec = db.recovery_report();
    eprintln!("after the wide tear: {rec:?}");
    assert_eq!(rec.row_count, COMMITTED, "the committed prefix is intact");
    assert_eq!(
        rec.orphan_valid_rows, WIDE,
        "the interrupted commit's rows are still there to be counted"
    );
    assert!(
        !rec.rollback_evidence,
        "one interrupted commit is not a rollback"
    );
    drop(db);

    // THE FIX, stated as a byte count: recovery truncated the residue
    // away, so the next incarnation writes onto a clean file.
    assert_eq!(
        rows_len(&victim),
        clean_len,
        "recovery left {} bytes of residue past the manifest; that residue is \
         what the false alarm was made of",
        rows_len(&victim) - clean_len
    );

    // ---- crash #2: inside a NARROWER commit ---------------------------
    // Because the file was truncated, a narrow interrupted commit lands on
    // a clean file and its rows are the only ones past the manifest.
    let narrow_ops: Vec<Op> = (100..100 + NARROW).map(|i| Op::put(i, v(0x11))).collect();
    let narrow_rows = rows_after_interrupted(&base, &work, &narrow_ops);
    std::fs::write(rows_file(&victim), &narrow_rows).unwrap();

    // ---- restart #2 ---------------------------------------------------
    let mut db = Db::open_with(&victim, CAP).unwrap();
    let rec = db.recovery_report();
    eprintln!("after the narrow tear: {rec:?}");
    assert_eq!(rec.row_count, COMMITTED);
    assert_eq!(rec.orphan_valid_rows, NARROW);
    assert!(
        !rec.rollback_evidence,
        "two interrupted commits of different widths, separated by a restart, \
         are now correctly read as what they are: {rec:?}"
    );

    // And the data is exactly the committed prefix, as always.
    assert_eq!(db.len(), COMMITTED);
    for i in 0..COMMITTED {
        assert_eq!(db.get(i).unwrap(), Some(v(0x11)), "row {i}");
    }
    for i in 100..100 + WIDE {
        assert_eq!(db.get(i).unwrap(), None, "no unacknowledged row is visible");
    }
    drop(db);

    // The alarm does not come back on later opens either.
    for attempt in 0..5 {
        let db = Db::open_with(&victim, CAP).unwrap();
        assert!(
            !db.recovery_report().rollback_evidence,
            "attempt {attempt}: the alarm came back"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

/// The same sequence where the wide commit is ONE LONG VALUE rather than a
/// wide batch.
///
/// A value over 16 bytes is stored as a run of row slots inside one
/// commit, so an interrupted insert of a 2 KiB value leaves up to 128 rows
/// past the manifest — the widest residue the format can produce, from a
/// single-operation write. Before the fix this was the easiest way for an
/// ordinary application to walk into the false alarm, because nothing
/// about it looks like a "batch".
#[test]
fn a_torn_long_value_followed_by_a_short_write_does_not_alarm() {
    let root = scratch("long");
    let (base, work, victim) = (root.join("base"), root.join("work"), root.join("victim"));

    seed(&base, 0x22);
    let clean_len = rows_len(&base);

    let long = Value::from_bytes(&vec![0x5A; MAX_VALUE_LEN]).unwrap();
    let long_rows = rows_after_interrupted(&base, &work, &[Op::put(500, long.clone())]);
    copy_dir(&base, &victim);
    std::fs::write(rows_file(&victim), &long_rows).unwrap();

    let db = Db::open_with(&victim, CAP).unwrap();
    let rec = db.recovery_report();
    eprintln!("after the torn long value: {rec:?}");
    assert_eq!(rec.row_count, COMMITTED);
    assert_eq!(
        rec.orphan_valid_rows,
        MAX_VALUE_LEN as u64 / 16,
        "a 2 KiB value is a 128-row commit, and all of it was in flight"
    );
    assert!(!rec.rollback_evidence, "{rec:?}");
    drop(db);
    assert_eq!(rows_len(&victim), clean_len, "the residue was truncated");

    // A one-row write in flight afterwards.
    let short_rows = rows_after_interrupted(&base, &work, &[Op::put(600, v(0x22))]);
    std::fs::write(rows_file(&victim), &short_rows).unwrap();
    let mut db = Db::open_with(&victim, CAP).unwrap();
    let rec = db.recovery_report();
    assert_eq!(rec.orphan_valid_rows, 1);
    assert!(!rec.rollback_evidence, "{rec:?}");
    assert_eq!(db.get(500).unwrap(), None);
    assert_eq!(db.get(600).unwrap(), None);
    assert_eq!(db.len(), COMMITTED);

    std::fs::remove_dir_all(&root).ok();
}

/// The detector is still armed.
///
/// Splice two disagreeing commit spans into one rows file with NO open in
/// between. That state is now unreachable from any crash sequence — every
/// open truncates before the process may write — so if it appears, an
/// acknowledged commit really was rolled back by a fault outside the
/// design's budget. The library still says so.
///
/// This is the same byte-level construction the old false-alarm test used.
/// The bytes did not change; what changed is that they now mean what the
/// library says they mean.
#[test]
fn disagreeing_spans_past_the_manifest_still_raise_the_alarm() {
    let root = scratch("armed");
    let (base, work, victim) = (root.join("base"), root.join("work"), root.join("victim"));

    seed(&base, 0x33);
    let wide_ops: Vec<Op> = (100..100 + WIDE).map(|i| Op::put(i, v(0x33))).collect();
    let wide_rows = rows_after_interrupted(&base, &work, &wide_ops);
    let narrow_ops: Vec<Op> = (100..100 + NARROW).map(|i| Op::put(i, v(0x33))).collect();
    let narrow_rows = rows_after_interrupted(&base, &work, &narrow_ops);
    assert!(wide_rows.len() > narrow_rows.len());

    copy_dir(&base, &victim);
    let mut spliced = narrow_rows.clone();
    spliced.extend_from_slice(&wide_rows[narrow_rows.len()..]);
    std::fs::write(rows_file(&victim), &spliced).unwrap();

    let db = Db::open_with(&victim, CAP).unwrap();
    let rec = db.recovery_report();
    eprintln!("spliced victim: {rec:?}");
    assert_eq!(rec.row_count, COMMITTED, "the committed prefix is intact");
    assert!(
        rec.rollback_evidence,
        "the alarm no longer fires on genuinely disagreeing spans, which \
         means nothing would report a rolled-back acknowledged commit: {rec:?}"
    );
    drop(db);

    // And the alarm CLEARS after that one open, because the residue it was
    // made of is now truncated away. Worth pinning: the alarm is a
    // one-shot report about what the open found, not a sticky flag, so a
    // host that ignores it once never sees it again. Nothing in the API
    // records that it fired.
    let db = Db::open_with(&victim, CAP).unwrap();
    assert!(
        !db.recovery_report().rollback_evidence,
        "the second open re-raised it; then the residue was not truncated"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The documented limit of the method, demonstrated: a rollback of a
/// single acknowledged commit is INVISIBLE.
///
/// Roll the superblock back to the previous generation while leaving the
/// rows in place — what a lying fsync on the superblock file looks like.
/// The acknowledged commit is gone, the rows it wrote sit past the
/// manifest and all agree on one span, and `rollback_evidence` stays down
/// because that is indistinguishable from an interrupted commit of the
/// same width.
///
/// This is why this crate keeps an out-of-band journal. The library's
/// alarm is a bonus, not the detector.
#[test]
fn a_rollback_of_one_commit_is_invisible_to_the_alarm() {
    let root = scratch("silent");
    let (dir, before) = (root.join("db"), root.join("before"));

    seed(&dir, 0x44);
    // Snapshot the superblock BEFORE an acknowledged commit.
    copy_dir(&dir, &before);

    {
        let mut db = Db::open_with(&dir, CAP).unwrap();
        let ops: Vec<Op> = (100..100 + WIDE).map(|i| Op::put(i, v(0x44))).collect();
        db.batch(&ops).unwrap();
        // Acknowledged: the library returned Ok and the rows are readable.
        assert_eq!(db.len(), COMMITTED + WIDE);
    }

    // The fault: the superblock reverts to its previous contents. The rows
    // file keeps everything.
    for e in std::fs::read_dir(&before).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("sb-") || name.starts_with("super") {
            std::fs::copy(e.path(), dir.join(e.file_name())).unwrap();
        }
    }

    let mut db = Db::open_with(&dir, CAP).unwrap();
    let rec = db.recovery_report();
    eprintln!("after a rolled-back acknowledged commit: {rec:?}");
    assert_eq!(db.len(), COMMITTED, "{WIDE} acknowledged rows are gone");
    assert_eq!(db.get(100).unwrap(), None);
    assert!(
        !rec.rollback_evidence,
        "if this ever fails the library got STRICTLY better: it would mean a \
         single rolled-back commit is now detectable"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The old control, kept: two interrupted commits of the SAME width leave
/// a region whose rows all agree, and the flag correctly stays down.
#[test]
fn two_torn_commits_of_the_same_width_do_not_raise_the_alarm() {
    let root = scratch("same");
    let (base, work, victim) = (root.join("base"), root.join("work"), root.join("victim"));

    seed(&base, 0x11);
    let a_ops: Vec<Op> = (100..100 + WIDE).map(|i| Op::put(i, v(0x11))).collect();
    let a_rows = rows_after_interrupted(&base, &work, &a_ops);
    let b_ops: Vec<Op> = (100..100 + WIDE).map(|i| Op::put(i, v(0x22))).collect();
    let b_rows = rows_after_interrupted(&base, &work, &b_ops);

    copy_dir(&base, &victim);
    // The second attempt got a third of the way through before dying.
    let row_size = a_rows.len() / (COMMITTED + WIDE) as usize;
    let cut = (COMMITTED as usize + (WIDE as usize / 3)) * row_size;
    let mut spliced = b_rows[..cut].to_vec();
    spliced.extend_from_slice(&a_rows[cut..]);
    std::fs::write(rows_file(&victim), &spliced).unwrap();

    let db = Db::open_with(&victim, CAP).unwrap();
    let rec = db.recovery_report();
    eprintln!("same-width victim: {rec:?}");
    assert_eq!(rec.row_count, COMMITTED);
    assert!(
        !rec.rollback_evidence,
        "equal-width torn commits are indistinguishable from one, correctly"
    );
    std::fs::remove_dir_all(&root).ok();
}
