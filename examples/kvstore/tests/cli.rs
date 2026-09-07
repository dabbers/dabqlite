//! End-to-end tests: they run the real `kv` binary in real processes
//! against a real directory, so "survives a restart" means what it says.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn ok(self) -> Run {
        assert_eq!(self.code, 0, "expected success\nstdout: {}\nstderr: {}", self.out, self.err);
        self
    }
    fn lines(&self) -> Vec<&str> {
        self.out.lines().collect()
    }
}

fn dir_for(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("kvstore-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn kv_stdin(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kv"));
    cmd.arg("--db").arg(dir).args(args);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Make sure an inherited KV_DB can never reach the child.
    cmd.env_remove("KV_DB");
    let mut child = cmd.spawn().expect("spawn kv");
    if let Some(bytes) = stdin {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let out = child.wait_with_output().expect("run kv");
    Run {
        code: out.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn kv(dir: &Path, args: &[&str]) -> Run {
    kv_stdin(dir, args, None)
}

fn rows_file(dir: &Path) -> PathBuf {
    let mut best: Option<(u64, PathBuf)> = None;
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("rows-") && name.ends_with(".dabq") {
            let len = e.metadata().unwrap().len();
            if best.as_ref().is_none_or(|(b, _)| len > *b) {
                best = Some((len, e.path()));
            }
        }
    }
    // The public API never tells you which files are yours; this has to be
    // guessed from the directory listing.
    best.expect("no rows file").1
}

#[test]
fn a_value_survives_a_process_restart() {
    let dir = dir_for("restart");
    kv(&dir, &["set", "greeting", "hello world"]).ok();
    kv(&dir, &["set", "who", "the operator"]).ok();

    // A brand new process, nothing shared but the directory.
    let got = kv(&dir, &["get", "greeting"]).ok();
    assert_eq!(got.out, "hello world\n");
    assert_eq!(kv(&dir, &["get", "who"]).ok().out, "the operator\n");

    // And again, after a hundred more writes and another restart each time.
    for i in 0..20 {
        kv(&dir, &["set", &format!("k{i}"), &format!("v{i}")]).ok();
    }
    assert_eq!(kv(&dir, &["get", "k19"]).ok().out, "v19\n");
    assert_eq!(kv(&dir, &["get", "greeting"]).ok().out, "hello world\n");
    assert_eq!(kv(&dir, &["list"]).ok().lines().len(), 22);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn updating_a_key_replaces_it_rather_than_duplicating_it() {
    let dir = dir_for("update");
    kv(&dir, &["set", "k", "first"]).ok();
    let second = kv(&dir, &["set", "k", "a considerably longer second value"]).ok();
    assert!(second.out.starts_with("replaced"), "{}", second.out);
    assert_eq!(
        kv(&dir, &["get", "k"]).ok().out,
        "a considerably longer second value\n"
    );
    // Shrink it again: the leftover rows must not resurface.
    kv(&dir, &["set", "k", "3"]).ok();
    assert_eq!(kv(&dir, &["get", "k"]).ok().out, "3\n");
    assert_eq!(kv(&dir, &["list"]).ok().lines(), vec!["k"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deleting_removes_the_key_and_reports_it_to_scripts() {
    let dir = dir_for("delete");
    kv(&dir, &["set", "a", "1"]).ok();
    kv(&dir, &["set", "b", "2"]).ok();
    kv(&dir, &["del", "a"]).ok();

    let missing = kv(&dir, &["get", "a"]);
    assert_eq!(missing.code, 2, "a missing key must be distinguishable");
    assert!(missing.err.contains("no such key"), "{}", missing.err);

    assert_eq!(kv(&dir, &["del", "a"]).code, 2, "deleting twice is not success");
    assert_eq!(kv(&dir, &["list"]).ok().lines(), vec!["b"]);
    // Survives a restart as a deletion, not as a resurrection.
    assert_eq!(kv(&dir, &["get", "a"]).code, 2);
    assert_eq!(kv(&dir, &["get", "b"]).ok().out, "2\n");
    // The slot comes back for reuse.
    kv(&dir, &["set", "a", "reborn"]).ok();
    assert_eq!(kv(&dir, &["get", "a"]).ok().out, "reborn\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_finds_substrings_including_across_row_boundaries() {
    let dir = dir_for("search");
    // dabqlite stores 16 bytes per row; this needle straddles rows 1 and 2.
    kv(&dir, &["set", "doc/1", "0123456789abcdeneedle-in-the-tail"]).ok();
    kv(&dir, &["set", "doc/2", "nothing to see here at all"]).ok();
    kv(&dir, &["set", "doc/3", "another needle, earlier"]).ok();

    let hits = kv(&dir, &["search", "needle"]).ok();
    let keys: Vec<&str> = hits.lines().iter().map(|l| l.split('\t').next().unwrap()).collect();
    assert_eq!(keys, vec!["doc/1", "doc/3"]);

    assert!(kv(&dir, &["search", "absent"]).ok().out.is_empty());
    // --keys widens the search to key text.
    let by_key = kv(&dir, &["search", "doc/2", "--keys"]).ok();
    assert_eq!(by_key.lines().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn listing_is_ordered_and_filterable() {
    let dir = dir_for("list");
    for k in ["session/b", "session/a", "user/z", "user/a"] {
        kv(&dir, &["set", k, "x"]).ok();
    }
    assert_eq!(
        kv(&dir, &["list"]).ok().lines(),
        vec!["session/a", "session/b", "user/a", "user/z"]
    );
    assert_eq!(
        kv(&dir, &["list", "--prefix", "user/"]).ok().lines(),
        vec!["user/a", "user/z"]
    );
    assert_eq!(
        kv(&dir, &["list", "--values", "--prefix", "user/a"]).ok().out,
        "user/a\tx\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn values_may_be_binary_multiline_and_large() {
    let dir = dir_for("binary");
    let value: Vec<u8> = b"line one\nline two\0with a NUL\xff\xfe end".to_vec();
    kv_stdin(&dir, &["set", "blob", "-"], Some(&value)).ok();
    let got = Command::new(env!("CARGO_BIN_EXE_kv"))
        .arg("--db")
        .arg(&dir)
        .args(["get", "blob", "--raw"])
        .output()
        .unwrap();
    assert_eq!(got.stdout, value, "binary values must round-trip byte for byte");

    // A value spanning most of a record.
    let big = "z".repeat(2000);
    kv_stdin(&dir, &["set", "big", "-"], Some(big.as_bytes())).ok();
    assert_eq!(kv(&dir, &["get", "big"]).ok().out.trim_end(), big);

    // And one that does not fit is refused, not truncated.
    let toobig = "z".repeat(4000);
    let refused = kv_stdin(&dir, &["set", "toobig", "-"], Some(toobig.as_bytes()));
    assert_eq!(refused.code, 1);
    assert!(refused.err.contains("value is 4000 bytes"), "{}", refused.err);
    assert_eq!(kv(&dir, &["get", "toobig"]).code, 2, "a refused write must not land");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_expires_and_purge_retires_it() {
    let dir = dir_for("ttl");
    kv(&dir, &["set", "session/live", "keep", "--ttl", "3600"]).ok();
    kv(&dir, &["set", "session/short", "drop", "--ttl", "4"]).ok();
    assert_eq!(kv(&dir, &["get", "session/short"]).ok().out, "drop\n");
    assert!(kv(&dir, &["info", "session/short"]).ok().out.contains("expires  in"));

    std::thread::sleep(std::time::Duration::from_millis(4500));

    assert_eq!(kv(&dir, &["get", "session/short"]).code, 2, "an expired session must be gone");
    assert_eq!(kv(&dir, &["list"]).ok().lines(), vec!["session/live"]);
    assert!(kv(&dir, &["stats"]).ok().out.contains("expired     1"));

    let purged = kv(&dir, &["purge"]).ok();
    assert!(purged.out.contains("purged 1 expired key"), "{}", purged.out);
    assert!(kv(&dir, &["stats"]).ok().out.contains("expired     0"));
    assert_eq!(kv(&dir, &["get", "session/live"]).ok().out, "keep\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_full_database_refuses_writes_clearly_and_keeps_serving_reads() {
    let dir = dir_for("full");
    let mut stored = 0;
    let mut refusal = None;
    for i in 0..100 {
        let r = kv(&dir, &["--rows", "30", "set", &format!("k{i}"), "value"]);
        if r.code == 0 {
            stored += 1;
        } else {
            refusal = Some(r);
            break;
        }
    }
    let refusal = refusal.expect("a 30-row database must fill up");
    assert!(stored > 5, "only stored {stored} keys before filling");
    assert_eq!(refusal.code, 1);
    assert!(
        refusal.err.contains("not enough room") && refusal.err.contains("kv compact"),
        "the refusal must say what to do: {}",
        refusal.err
    );

    // Everything already stored is still readable and listable.
    assert_eq!(kv(&dir, &["get", "k0"]).ok().out, "value\n");
    assert_eq!(kv(&dir, &["list"]).ok().lines().len(), stored);

    // Deleting still works at the wall — that is what the reserve is for.
    // (dabqlite itself refuses a delete at capacity: a delete appends a
    // tombstone row, so it needs a free slot like any other write.)
    for i in 0..stored / 2 {
        kv(&dir, &["del", &format!("k{i}")]).ok();
    }
    assert_eq!(kv(&dir, &["get", "k0"]).code, 2);

    // Compaction gives the space back and writes resume.
    kv(&dir, &["compact"]).ok();
    kv(&dir, &["set", "after-compaction", "ok"]).ok();
    assert_eq!(kv(&dir, &["get", "after-compaction"]).ok().out, "ok\n");
    assert_eq!(kv(&dir, &["get", &format!("k{}", stored - 1)]).ok().out, "value\n");

    // A bigger ceiling is also a way out, and it sticks.
    kv(&dir, &["--rows", "200", "set", "roomy", "yes"]).ok();
    assert!(kv(&dir, &["stats"]).ok().out.contains("/ 200"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_declared_capacity_is_remembered_across_restarts() {
    let dir = dir_for("capacity");
    kv(&dir, &["--rows", "64", "set", "a", "1"]).ok();
    // dabqlite itself would silently reopen this at DEFAULT_ROWS; the
    // sidecar file is what keeps the ceiling stable.
    let stats = kv(&dir, &["stats"]).ok();
    assert!(stats.out.contains("/ 64"), "{}", stats.out);
    assert!(std::fs::read_to_string(dir.join("kv-capacity")).unwrap().starts_with("64"));

    // Asking for less than the data needs is refused with a true message.
    let shrunk = kv(&dir, &["--rows", "1", "set", "b", "2"]);
    assert_eq!(shrunk.code, 1);
    assert!(shrunk.err.contains("below"), "{}", shrunk.err);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compaction_preserves_every_key_and_reclaims_dead_rows() {
    let dir = dir_for("compact");
    for i in 0..15 {
        kv(&dir, &["set", &format!("k{i}"), &"x".repeat(100)]).ok();
    }
    for i in 0..15 {
        kv(&dir, &["set", &format!("k{i}"), &format!("short {i}")]).ok();
    }
    for i in 0..5 {
        kv(&dir, &["del", &format!("k{i}")]).ok();
    }
    let before = kv(&dir, &["list", "--values"]).ok().out;
    let stats_before = kv(&dir, &["stats"]).ok().out;
    assert!(stats_before.contains("orphan rows"), "{stats_before}");

    let done = kv(&dir, &["compact"]).ok();
    assert!(done.out.contains("compacted 10 keys"), "{}", done.out);

    assert_eq!(kv(&dir, &["list", "--values"]).ok().out, before, "compaction lost data");
    let stats_after = kv(&dir, &["stats"]).ok().out;
    assert!(stats_after.contains("dead rows   0"), "{stats_after}");
    assert!(stats_after.contains("orphan rows 0"), "{stats_after}");
    assert!(stats_after.contains("tombstones  0"), "{stats_after}");
    // The capacity setting survives the directory swap.
    assert!(dir.join("kv-capacity").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_backup_round_trips_into_a_fresh_database() {
    let dir = dir_for("backup");
    let dest = dir_for("backup-dest");
    let snap = std::env::temp_dir().join(format!("kvstore-it-{}.snap", std::process::id()));
    for i in 0..10 {
        kv(&dir, &["set", &format!("k{i}"), &format!("value {i}")]).ok();
    }
    kv(&dir, &["del", "k3"]).ok();
    let expected = kv(&dir, &["list", "--values"]).ok().out;

    kv(&dir, &["backup", snap.to_str().unwrap()]).ok();
    assert!(std::fs::metadata(&snap).unwrap().len() > 0);

    kv(&dest, &["restore", snap.to_str().unwrap()]).ok();
    assert_eq!(kv(&dest, &["list", "--values"]).ok().out, expected);
    assert_eq!(kv(&dest, &["get", "k3"]).code, 2);
    // The restored database is a working database, not a read-only copy.
    kv(&dest, &["set", "new", "written after restore"]).ok();
    assert_eq!(kv(&dest, &["get", "new"]).ok().out, "written after restore\n");

    // Restoring over a live database is refused rather than silently merging.
    let refused = kv(&dest, &["restore", snap.to_str().unwrap()]);
    assert_eq!(refused.code, 1);
    assert!(refused.err.contains("already holds a database"), "{}", refused.err);

    let _ = std::fs::remove_file(&snap);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_damaged_database_is_refused_and_then_rescued() {
    let dir = dir_for("damaged");
    let dest = dir_for("damaged-rescued");
    for i in 0..12 {
        kv(&dir, &["set", &format!("k{i:02}"), &format!("value {i}")]).ok();
    }
    let all_before = kv(&dir, &["list"]).ok().lines().len();

    // Rot one byte of one row.
    let rows = rows_file(&dir);
    let mut bytes = std::fs::read(&rows).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x40;
    std::fs::write(&rows, &bytes).unwrap();

    let refused = kv(&dir, &["get", "k00"]);
    assert_eq!(refused.code, 1);
    assert!(
        refused.err.contains("damaged") && refused.err.contains("rescue"),
        "a damaged database must say what to try: {}",
        refused.err
    );

    let rescued = kv(&dir, &["rescue", dest.to_str().unwrap()]).ok();
    assert!(rescued.out.contains("rescued"), "{}", rescued.out);
    let survivors = kv(&dest, &["list"]).ok().lines().len();
    assert!(
        survivors >= all_before - 2 && survivors < all_before + 1,
        "rescue should lose at most the damaged record: {survivors} of {all_before}"
    );
    // The rescued copy is a normal, writable database.
    kv(&dest, &["set", "fresh", "ok"]).ok();
    assert_eq!(kv(&dest, &["get", "fresh"]).ok().out, "ok\n");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn a_second_writer_is_told_the_database_is_in_use() {
    let dir = dir_for("locked");
    kv(&dir, &["set", "a", "1"]).ok();
    // Hold the single-writer lock the way another process would.
    let held = dabqlite::Db::open(&dir).expect("hold the lock");
    let blocked = kv(&dir, &["set", "b", "2"]);
    assert_eq!(blocked.code, 1);
    assert!(
        blocked.err.contains("already open in another process"),
        "{}",
        blocked.err
    );
    // Reads are blocked too: the store is single-writer, not reader/writer.
    assert_ne!(kv(&dir, &["get", "a"]).code, 0);
    drop(held);
    assert_eq!(kv(&dir, &["get", "a"]).ok().out, "1\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_usage_is_reported_without_touching_the_database() {
    let dir = dir_for("usage");
    kv(&dir, &["set", "a", "1"]).ok();
    for args in [
        vec!["frobnicate", "x"],
        vec!["get"],
        vec!["set", "only-a-key"],
        vec!["--rows", "not-a-number", "stats"],
    ] {
        let r = kv(&dir, &args);
        assert_eq!(r.code, 1, "{args:?} should fail");
        assert!(r.err.starts_with("kv: "), "{args:?}: {}", r.err);
    }
    assert_eq!(kv(&dir, &["list"]).ok().lines(), vec!["a"]);
    assert!(kv(&dir, &["--help"]).ok().out.contains("USAGE"));
    let _ = std::fs::remove_dir_all(&dir);
}
