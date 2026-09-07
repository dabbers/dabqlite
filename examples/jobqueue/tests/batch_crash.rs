//! Kill the writer *inside* a commit, as many times as possible, and try
//! to catch `Db::batch` applying one in part — or a value that spans
//! several row slots coming back SHORT.
//!
//! The harness drives the `churn` workload: batches packed to a ROW-SLOT
//! budget out of mixed `insert`/`update`/`delete` operations on
//! VARIABLE-LENGTH values, plus a digest row that must always equal the
//! digest of every other row. Three independent things are checked after
//! every kill:
//!
//! * every cell's stored bytes match its own declared length — a torn
//!   multi-slot value fails this even if nothing else notices;
//! * the digest row equals the digest of the cells — a batch applied in
//!   part fails this;
//! * the resulting `(digest, cell count)` was an intent the workload wrote
//!   down BEFORE the batch — a state nobody asked for fails this.
//!
//! Two kill strategies, because "random" is not good enough on its own:
//!
//! * **Targeted.** The worker writes its intent record `D <round>
//!   <digest> <cells>` with one unbuffered `write(2)` and then immediately
//!   calls `Db::batch`. The harness polls the journal's size, and the
//!   instant it grows it sleeps a random sliver of a millisecond and sends
//!   SIGKILL. That aims the signal squarely at the interval between the
//!   first row hitting the rows file and the superblock flip that makes
//!   the commit visible.
//! * **Uniform.** Kill at a uniformly random time, to cover everything the
//!   targeted strategy might systematically miss (compaction, open,
//!   recovery).
//!
//! The proof that the aim works is `orphan_valid_rows`: dabqlite counts
//! checksum-valid rows sitting past the manifest at open. One is what a
//! single interrupted one-slot write leaves. **Two or more can only come
//! from an interrupted multi-row commit** — a batch, or one long value, or
//! both. The tests assert plenty of those and how wide the widest was.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dabqlite::{MAX_COMMIT_ROWS, VALUE_LEN};
use jobqueue::{audit, churn_verify, ChurnConfig};

const EXE: &str = env!("CARGO_BIN_EXE_jobqueue");

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jobqueue-batch-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn journal_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn spawn(cfg: &ChurnConfig, seed: u64) -> Child {
    Command::new(EXE)
        .arg("churn")
        .arg("--root")
        .arg(&cfg.root)
        .arg("--journal")
        .arg(&cfg.journal)
        .arg("--rounds")
        .arg(cfg.rounds.to_string())
        .arg("--batch")
        .arg(cfg.batch.to_string())
        .arg("--max-cell")
        .arg(cfg.max_cell.to_string())
        .arg("--cells")
        .arg(cfg.cells.to_string())
        .arg("--capacity")
        .arg(cfg.capacity.to_string())
        .arg("--compact-at")
        .arg(cfg.compact_at.to_string())
        .arg("--seed")
        .arg(seed.to_string())
        .args(if cfg.vary { vec!["--vary"] } else { vec![] })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn churn worker")
}

#[derive(Debug, Default)]
struct Tally {
    cycles: u64,
    killed: u64,
    /// Opens that found exactly one checksum-valid row past the manifest:
    /// a single one-slot write in flight.
    opens_with_one_orphan: u64,
    /// Opens that found two or more: **a multi-row commit in flight**.
    opens_with_orphan_batch: u64,
    widest_orphan_batch: u64,
    total_intents: u64,
    total_acks: u64,
    max_cells: usize,
    /// The longest value seen intact in the database after a kill. Proof
    /// the kills were aimed at commits that write MANY slots.
    widest_cell: usize,
}

/// One generation of the hammer.
struct Hammer {
    tag: &'static str,
    /// Row-slot budget per batch, before the digest row.
    batch: usize,
    /// Longest cell payload in bytes. Above 8 bytes a cell spans slots.
    max_cell: usize,
    /// How many distinct cells. The live set is `cells * slots-per-cell`,
    /// so long values need fewer cells or a bigger capacity.
    cells: u64,
    /// Vary the batch width per round.
    vary: bool,
    capacity: u64,
    cycles: u64,
    seed: u64,
    /// How far the journal may grow before the targeted kill fires, in
    /// bytes. Small values kill the worker after only one or two batches.
    grow_by_max: u64,
}

/// `Instant::now` is banned inside the deterministic boundary by the
/// repository's clippy config; this is a harness that has to kill a real
/// process at a real moment.
#[allow(clippy::disallowed_methods)]
fn hammer(h: Hammer, t: &mut Tally) {
    let Hammer {
        tag,
        batch,
        max_cell,
        cells,
        vary,
        capacity,
        cycles,
        seed,
        grow_by_max,
    } = h;
    let root = scratch(tag);
    let mut cfg = ChurnConfig::new(root.join("db"), root.join("j.log"));
    cfg.batch = batch;
    cfg.max_cell = max_cell;
    cfg.cells = cells;
    cfg.vary = vary;
    cfg.capacity = capacity;
    cfg.rounds = 1_000_000;
    cfg.compact_at = 0.7;

    let mut rng = Rng(seed);
    let killed_before = t.killed;
    let orphan_batches_before = t.opens_with_orphan_batch;
    let mut gen_widest_orphan = 0u64;
    let mut gen_widest_cell = 0usize;

    for cycle in 0..cycles {
        let mut child = spawn(&cfg, rng.next());
        let start = journal_len(&cfg.journal);
        let mut killed = false;

        if cycle % 4 == 3 {
            // Uniform: somewhere in the first 200 ms of the run.
            std::thread::sleep(Duration::from_micros(500 + rng.below(200_000)));
            killed = child.kill().is_ok();
        } else {
            // Targeted: the moment the journal grows, a `D` intent record
            // has just been written and `Db::batch` is running RIGHT NOW.
            let grow_by = 1 + rng.below(grow_by_max);
            let deadline = Instant::now() + Duration::from_millis(3_000);
            loop {
                if journal_len(&cfg.journal) >= start + grow_by {
                    std::thread::sleep(Duration::from_nanos(rng.below(900_000)));
                    killed = child.kill().is_ok();
                    break;
                }
                if child.try_wait().expect("try_wait").is_some() {
                    break;
                }
                if Instant::now() > deadline {
                    killed = child.kill().is_ok();
                    break;
                }
                std::thread::sleep(Duration::from_micros(50));
            }
        }

        let out = child.wait_with_output().expect("wait");
        if killed && out.status.code().is_none() {
            t.killed += 1;
        }
        if let Some(code) = out.status.code() {
            assert_eq!(
                code,
                0,
                "cycle {cycle}: churn worker exited {code}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // ---- the audit: is the database in a state some COMPLETE batch
        //      left it in? ----------------------------------------------
        let v = churn_verify(&cfg).expect("verify after kill");
        assert!(
            v.short_values.is_empty(),
            "cycle {cycle}: A MULTI-SLOT VALUE CAME BACK SHORT. {v:?}"
        );
        assert_eq!(
            v.half_batch, None,
            "cycle {cycle}: A BATCH LANDED IN PART. {v:?}"
        );
        assert!(
            !v.rollback_evidence,
            "cycle {cycle}: dabqlite reports rolled-back acknowledged commits, and \
             the workload's own digest says nothing was lost — that is the false \
             alarm `tests/torn.rs` covers, come back: {v:?}"
        );
        match v.orphan_valid_rows {
            0 => {}
            1 => t.opens_with_one_orphan += 1,
            n => {
                t.opens_with_orphan_batch += 1;
                t.widest_orphan_batch = t.widest_orphan_batch.max(n);
                gen_widest_orphan = gen_widest_orphan.max(n);
            }
        }
        t.max_cells = t.max_cells.max(v.cells);
        t.widest_cell = t.widest_cell.max(v.widest_cell);
        gen_widest_cell = gen_widest_cell.max(v.widest_cell);
        t.cycles += 1;
    }

    eprintln!(
        "  generation {tag:>5}: {} kills, {} restarts inside a multi-row commit \
         (widest {} rows), widest value {} bytes",
        t.killed - killed_before,
        t.opens_with_orphan_batch - orphan_batches_before,
        gen_widest_orphan,
        gen_widest_cell
    );

    let a = audit(&cfg.journal).expect("audit");
    t.total_intents += a.churn_intents as u64;
    t.total_acks += a.churn_acks as u64;
    assert!(
        a.churn_acks <= a.churn_intents,
        "more batches acknowledged than intended: {} > {}",
        a.churn_acks,
        a.churn_intents
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The headline test: hundreds of kills aimed at the inside of a commit,
/// across four batch shapes.
#[test]
fn sigkill_inside_a_batch_never_leaves_half_of_it_or_a_short_value() {
    let mut t = Tally::default();

    // Widest batches the format allows, out of one-slot values: 127 cell
    // ops plus the digest row.
    hammer(
        Hammer {
            tag: "wide",
            batch: MAX_COMMIT_ROWS - 1,
            max_cell: 8,
            cells: 200,
            vary: false,
            capacity: 8192,
            cycles: 40,
            seed: 0xBA7C_4001,
            grow_by_max: 2_500,
        },
        &mut t,
    );
    // The case this revision is about: ONE value per batch, nearly as long
    // as the format allows, so the commit is a run of ~126 row slots
    // belonging to a SINGLE value. A kill inside it is the best shot
    // anyone has at a short read.
    hammer(
        Hammer {
            tag: "huge",
            batch: MAX_COMMIT_ROWS - 1,
            max_cell: (MAX_COMMIT_ROWS - 2) * VALUE_LEN - 8,
            // 24 cells x 126 slots = 3024 live slots at worst, under the
            // 0.7 x 8192 compaction threshold. Capacity is in slots, so
            // this sum is the application's to do.
            cells: 24,
            vary: false,
            capacity: 8192,
            cycles: 40,
            seed: 0xBA7C_4004,
            grow_by_max: 400,
        },
        &mut t,
    );
    // Middling values, small capacity so compaction runs often and some
    // kills land inside the rebuild instead.
    hammer(
        Hammer {
            tag: "mid",
            batch: 32,
            max_cell: 300,
            cells: 64,
            vary: false,
            capacity: 4096,
            cycles: 40,
            seed: 0xBA7C_4002,
            grow_by_max: 2_500,
        },
        &mut t,
    );
    // Two-slot batches: the shape the job queue's own commit uses.
    hammer(
        Hammer {
            tag: "pair",
            batch: 2,
            max_cell: 8,
            cells: 100,
            vary: false,
            capacity: 512,
            cycles: 40,
            seed: 0xBA7C_4003,
            grow_by_max: 2_500,
        },
        &mut t,
    );

    eprintln!("batch-crash tally: {t:#?}");
    assert!(t.killed >= 100, "not enough kills landed: {t:?}");
    assert!(
        t.total_acks > 3_000,
        "not enough batches were actually committed to be meaningful: {t:?}"
    );
    assert!(
        t.opens_with_orphan_batch >= 20,
        "the kills never landed inside a multi-row commit — dabqlite reported \
         one in flight at fewer than 20 restarts, so this test proves nothing: {t:?}"
    );
    assert!(
        t.widest_orphan_batch >= 8,
        "the widest interrupted commit seen was only {} rows; the kill never \
         landed deep inside a wide commit: {t:?}",
        t.widest_orphan_batch
    );
    assert!(
        t.widest_cell > 1_000,
        "the workload never stored a value spanning many slots ({} bytes was \
         the widest), so the short-value detector had nothing to catch: {t:?}",
        t.widest_cell
    );
}

/// The same hammer, but every batch is a DIFFERENT width.
///
/// This is the shape that used to be impossible to run with the alarm
/// treated as fatal. The rows file was appended to and never truncated, so
/// a narrow interrupted commit landing where a wider one had been left the
/// wider one's tail past the manifest, and `rollback_evidence` fired on a
/// database that had lost nothing. This test counted the false alarms and
/// had to pass `alarm_is_fatal: false` to survive them.
///
/// Recovery now truncates the rows file to the manifest, so the residue
/// cannot survive a restart. The alarm is fatal here, like everywhere
/// else, and the mixed widths are just another shape.
#[test]
fn mixed_width_torn_commits_no_longer_raise_the_rollback_alarm() {
    let mut t = Tally::default();
    hammer(
        Hammer {
            tag: "vary",
            batch: MAX_COMMIT_ROWS - 1,
            max_cell: 600,
            cells: 48,
            vary: true,
            capacity: 4096,
            cycles: 60,
            seed: 0x7EAD_1234,
            // Kill after ONE round, so the rows file still holds the
            // previous life's torn commit when the next one lands on it —
            // exactly the sequence that used to produce the false alarm.
            grow_by_max: 40,
        },
        &mut t,
    );
    eprintln!("mixed-width tally: {t:#?}");
    assert!(
        t.opens_with_orphan_batch >= 5,
        "the kills never landed inside a multi-row commit: {t:?}"
    );
    // Every restart in this generation went through the `!rollback_evidence`
    // assertion inside `hammer`. Reaching here means none of them fired.
    assert!(
        t.widest_orphan_batch >= 4,
        "the mixed widths never produced a wide interrupted commit: {t:?}"
    );
}

/// Negative controls for the two detectors, forged through the public API.
/// Without these, "no half-batches and no short values in 220 kills" means
/// nothing.
#[test]
fn forged_damage_is_caught_by_the_churn_detectors() {
    use dabqlite::{Db, Value};

    let root = scratch("forge");
    let mut cfg = ChurnConfig::new(root.join("db"), root.join("j.log"));
    cfg.batch = 32;
    cfg.max_cell = 300;
    cfg.cells = 64;
    cfg.capacity = 2048;
    cfg.rounds = 40;

    let out = Command::new(EXE)
        .arg("churn")
        .arg("--root")
        .arg(&cfg.root)
        .arg("--journal")
        .arg(&cfg.journal)
        .arg("--rounds")
        .arg("40")
        .arg("--batch")
        .arg("32")
        .arg("--max-cell")
        .arg("300")
        .arg("--cells")
        .arg("64")
        .arg("--capacity")
        .arg("2048")
        .output()
        .expect("run churn");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let clean = churn_verify(&cfg).expect("verify");
    assert_eq!(clean.half_batch, None, "{clean:?}");
    assert!(clean.cells > 0);
    assert!(clean.widest_cell > VALUE_LEN, "{clean:?}");

    // (1) Land "half" of a batch: change one cell and not the digest row.
    {
        let mut db = Db::open_with(&cfg.root, cfg.capacity).unwrap();
        let (id, v) = db.range(1, jobqueue::CHURN_CELLS).unwrap().remove(0);
        let mut b = v.as_bytes().to_vec();
        let n = b.len();
        b[n - 1] ^= 0xFF;
        db.update(id, Value::from_vec(b).unwrap()).unwrap();
    }
    let broken = churn_verify(&cfg).expect("verify");
    let why = broken
        .half_batch
        .expect("the churn detector missed a cell change without its digest");
    assert!(why.contains("applied IN PART"), "{why}");

    // (2) Truncate a multi-slot cell, leaving its declared length alone:
    // exactly what a lost tail slot would look like.
    let root2 = scratch("forge2");
    let mut cfg2 = ChurnConfig::new(root2.join("db"), root2.join("j.log"));
    cfg2.capacity = 2048;
    cfg2.cells = 8;
    {
        let mut db = Db::open_with(&cfg2.root, cfg2.capacity).unwrap();
        let long = {
            let mut b = (200u64).to_le_bytes().to_vec();
            b.extend_from_slice(&[0x7Eu8; 200]);
            b
        };
        db.insert(1, Value::from_vec(long.clone()).unwrap())
            .unwrap();
        db.update(1, Value::from_vec(long[..8 + 20].to_vec()).unwrap())
            .unwrap();
    }
    let torn = churn_verify(&cfg2).expect("verify");
    assert_eq!(
        torn.short_values,
        vec![(1, 208, 28)],
        "the short-value detector missed a truncated multi-slot value: {torn:?}"
    );
    assert!(
        torn.half_batch.unwrap().contains("came back SHORT"),
        "a short value should be reported as a short value"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&root2).ok();
}
