//! The only module that opens a database.
//!
//! Every function here opens its own `Db`, uses it, and drops it before
//! returning, because an open `dabqlite::Db<S>` has no nameable type: it
//! cannot be stored, returned, or passed. The rows it reads (`Vec<(u64,
//! Value)>`) *are* nameable, so all sharing happens by materialising rows
//! and handing them to the pure code in [`crate::plan`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use dabqlite::{Db, RecoveryReport, Row, Snapshot, Stats};

use crate::codec::MAX_PAYLOAD;
use crate::plan::{self, Entry, RecordCensus, RowOp, Rows};
use crate::{now_secs, open_error, write_capacity, Config, KvError, RESERVED_SLOTS};

/// Apply planned row writes to an open database.
///
/// This is a macro rather than a function because a function would have to
/// write `db: &mut Db<S>` and there is no way to satisfy the `S: Storage`
/// bound from outside the library — `Storage` is not re-exported.
macro_rules! apply_ops {
    ($db:expr, $ops:expr) => {{
        let mut r: Result<(), $crate::KvError> = Ok(());
        for op in $ops {
            r = match op {
                RowOp::Put(id, v) => $db.put(id, v).map_err($crate::KvError::from),
                RowOp::Del(id) => $db.remove(id).map(|_| ()).map_err($crate::KvError::from),
            };
            if r.is_err() {
                break;
            }
        }
        r
    }};
}

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Set {
        key: String,
        value: Vec<u8>,
        ttl: Option<u64>,
    },
    Get {
        key: String,
        raw: bool,
    },
    Del {
        key: String,
    },
    List {
        prefix: Option<String>,
        values: bool,
    },
    Search {
        needle: Vec<u8>,
        keys: bool,
    },
    Info {
        key: String,
    },
    Stats,
    Purge,
    Compact,
    Backup {
        file: PathBuf,
    },
    Restore {
        file: PathBuf,
    },
    Rescue {
        dest: PathBuf,
    },
}

/// What happened, before it is rendered.
#[derive(Debug, Clone)]
pub enum Outcome {
    Stored {
        key: String,
        replaced: bool,
        rows: u64,
    },
    Value {
        value: Vec<u8>,
        raw: bool,
    },
    Missing {
        key: String,
    },
    Deleted {
        key: String,
    },
    Entries {
        entries: Vec<Entry>,
        values: bool,
    },
    Info(Entry),
    Report(Box<Report>),
    Purged(Vec<String>),
    Compacted {
        before: Stats,
        after: Stats,
        keys: usize,
    },
    BackedUp {
        file: PathBuf,
        bytes: usize,
    },
    Restored {
        file: PathBuf,
        keys: usize,
    },
    Rescued {
        dest: PathBuf,
        keys: usize,
        quarantined: u64,
        unreadable: u64,
    },
}

/// `kv stats`.
#[derive(Debug, Clone)]
pub struct Report {
    pub dir: PathBuf,
    pub stats: Stats,
    pub census: RecordCensus,
    pub recovery: RecoveryReport,
    pub max_payload: usize,
}

/// Run one command.
pub fn execute(cfg: &Config, cmd: &Command, warn: &mut dyn Write) -> Result<Outcome, KvError> {
    match cmd {
        Command::Compact => compact(cfg, warn),
        Command::Restore { file } => restore(cfg, file),
        Command::Rescue { dest } => rescue(cfg, dest),
        other => on_open_db(cfg, other, warn),
    }
}

/// Everything that fits inside a single open/act/close.
fn on_open_db(cfg: &Config, cmd: &Command, warn: &mut dyn Write) -> Result<Outcome, KvError> {
    let capacity = cfg.capacity();
    let existed = cfg.dir.join("superblock.dabq").exists();
    // `open_with` distinguishes "the database is full" from "you asked for
    // a capacity smaller than the data already stored" (`CapacityTooSmall`),
    // so `open_error` can classify both without guessing from `--rows`.
    let mut db = Db::open_with(&cfg.dir, capacity).map_err(|e| open_error(&cfg.dir, e))?;

    // The library asks callers to alarm on this; nothing else will.
    let recovery = db.recovery_report();
    if recovery.rollback_evidence {
        let _ = writeln!(
            warn,
            "kv: WARNING: {} shows evidence that an acknowledged write was rolled \
             back by a storage fault. Data committed just before the last restart \
             may be missing.",
            cfg.dir.display()
        );
    }
    if !existed || cfg.rows.is_some() {
        write_capacity(&cfg.dir, capacity)?;
    }

    let now = now_secs();
    let rows: Rows = db.all()?.into_iter().collect();

    // Read-only commands first.
    match cmd {
        Command::Get { key, raw } => {
            return Ok(match plan::lookup(&rows, key, now)? {
                Some(e) => Outcome::Value {
                    value: e.value,
                    raw: *raw,
                },
                None => Outcome::Missing { key: key.clone() },
            })
        }
        Command::Info { key } => {
            return Ok(match plan::lookup(&rows, key, now)? {
                Some(e) => Outcome::Info(e),
                None => Outcome::Missing { key: key.clone() },
            })
        }
        Command::List { prefix, values } => {
            let mut entries = plan::scan(&rows, now)?;
            if let Some(p) = prefix {
                entries.retain(|e| e.key.starts_with(p.as_str()));
            }
            return Ok(Outcome::Entries {
                entries,
                values: *values,
            });
        }
        Command::Search { needle, keys } => {
            return Ok(Outcome::Entries {
                entries: plan::search(&rows, needle, *keys, now)?,
                values: true,
            })
        }
        Command::Stats => {
            return Ok(Outcome::Report(Box::new(Report {
                dir: cfg.dir.clone(),
                stats: db.stats(),
                census: plan::census(&rows, now)?,
                recovery,
                max_payload: MAX_PAYLOAD,
            })))
        }
        Command::Backup { file } => {
            let bytes = db.snapshot()?.to_bytes();
            std::fs::write(file, &bytes).map_err(|err| KvError::Io {
                what: format!("writing {}", file.display()),
                err,
            })?;
            return Ok(Outcome::BackedUp {
                file: file.clone(),
                bytes: bytes.len(),
            });
        }
        _ => {}
    }

    // Writers: plan first, check capacity, then apply.
    let (ops, outcome, reserve) = match cmd {
        Command::Set { key, value, ttl } => {
            let expires_at = ttl.map(|t| now.saturating_add(t)).unwrap_or(0);
            let p = plan::plan_set(&rows, key, value, expires_at, now)?;
            let rows_written = p.ops.len() as u64;
            (
                p.ops,
                Outcome::Stored {
                    key: key.clone(),
                    replaced: p.replaced,
                    rows: rows_written,
                },
                RESERVED_SLOTS,
            )
        }
        Command::Del { key } => match plan::plan_delete(&rows, key, now)? {
            Some(ops) => (ops, Outcome::Deleted { key: key.clone() }, 0),
            None => return Ok(Outcome::Missing { key: key.clone() }),
        },
        Command::Purge => {
            let (keys, ops) = plan::plan_purge(&rows, now)?;
            (ops, Outcome::Purged(keys), 0)
        }
        other => unreachable!("{other:?} is handled elsewhere"),
    };

    check_room(&db.stats(), ops.len() as u64, reserve)?;
    apply_ops!(db, ops)?;
    Ok(outcome)
}

/// dabqlite refuses *every* write at capacity, deletes included, so the
/// room check has to happen before the first row goes down rather than
/// being discovered halfway through a multi-row record.
fn check_room(stats: &Stats, needed: u64, reserve: u64) -> Result<(), KvError> {
    let free = stats.capacity.saturating_sub(stats.slots);
    if needed + reserve > free {
        return Err(KvError::NoRoom {
            needed,
            free,
            capacity: stats.capacity,
        });
    }
    Ok(())
}

/// Rebuild a database into `dir` from a plain list of rows.
///
/// Rows are nameable (`Vec<(u64, Value)>`) even though the database is
/// not, so this is the only shape a "copy a database" helper can take.
fn write_fresh(dir: &Path, rows: &[Row], capacity: u64) -> Result<(), KvError> {
    let mut db = Db::open_with(dir, capacity).map_err(|e| open_error(dir, e))?;
    for &(id, value) in rows {
        db.insert(id, value)?;
    }
    write_capacity(dir, capacity)
}

/// Re-derive the record layout from scratch: live, unexpired entries only.
fn rebuild_rows(entries: &[Entry], now: u64) -> Result<Vec<Row>, KvError> {
    let mut fresh: Rows = BTreeMap::new();
    for e in entries {
        let p = plan::plan_set(&fresh, &e.key, &e.value, e.expires_at, now)?;
        for op in p.ops {
            match op {
                RowOp::Put(id, v) => {
                    fresh.insert(id, v);
                }
                RowOp::Del(id) => {
                    fresh.remove(&id);
                }
            }
        }
    }
    Ok(fresh.into_iter().collect())
}

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "kvdata".into());
    let parent = dir.parent().unwrap_or(Path::new("."));
    parent.join(format!("{name}.{suffix}{}", std::process::id()))
}

/// Drop tombstones, expired records and unreachable payload rows.
///
/// `Db::compact_to_memory` exists but returns an in-memory database, and
/// there is no way to write one back to a directory, so compaction of a
/// file-backed store is: read every row, rebuild, write a new directory,
/// rename it into place.
fn compact(cfg: &Config, warn: &mut dyn Write) -> Result<Outcome, KvError> {
    let capacity = cfg.capacity();
    let now = now_secs();
    let (before, entries) = {
        let mut db =
            Db::open_with(&cfg.dir, capacity).map_err(|e| open_error(&cfg.dir, e))?;
        let rows: Rows = db.all()?.into_iter().collect();
        (db.stats(), plan::scan(&rows, now)?)
        // `db` is dropped here, releasing the single-writer lock before
        // the directory is renamed out from under it.
    };
    let fresh = rebuild_rows(&entries, now)?;

    let tmp = sibling(&cfg.dir, "compact.");
    let _ = std::fs::remove_dir_all(&tmp);
    write_fresh(&tmp, &fresh, capacity)?;

    let after = {
        let db = Db::open_with(&tmp, capacity).map_err(|e| open_error(&tmp, e))?;
        db.stats()
    };

    let old = sibling(&cfg.dir, "old.");
    let _ = std::fs::remove_dir_all(&old);
    swap_dirs(&cfg.dir, &tmp, &old, warn)?;
    Ok(Outcome::Compacted {
        before,
        after,
        keys: entries.len(),
    })
}

fn swap_dirs(dir: &Path, tmp: &Path, old: &Path, warn: &mut dyn Write) -> Result<(), KvError> {
    let io = |what: String| move |err| KvError::Io { what, err };
    std::fs::rename(dir, old).map_err(io(format!(
        "moving {} aside to {}",
        dir.display(),
        old.display()
    )))?;
    if let Err(err) = std::fs::rename(tmp, dir) {
        let _ = writeln!(
            warn,
            "kv: rebuild failed to move into place; the previous database is at {}",
            old.display()
        );
        return Err(KvError::Io {
            what: format!("moving {} to {}", tmp.display(), dir.display()),
            err,
        });
    }
    let _ = std::fs::remove_dir_all(old);
    Ok(())
}

/// Load a snapshot blob into an empty database directory.
fn restore(cfg: &Config, file: &Path) -> Result<Outcome, KvError> {
    if cfg.dir.join("superblock.dabq").exists() {
        return Err(KvError::Usage(format!(
            "{} already holds a database; restore into an empty --db directory",
            cfg.dir.display()
        )));
    }
    let bytes = std::fs::read(file).map_err(|err| KvError::Io {
        what: format!("reading {}", file.display()),
        err,
    })?;
    let capacity = cfg.capacity();
    // A snapshot only reopens in memory; getting it back onto disk means
    // copying every row across by hand.
    let rows = {
        let snapshot = Snapshot::from_bytes(&bytes)?;
        let mut mem = Db::load_with(&snapshot, capacity)?;
        mem.all()?
    };
    write_fresh(&cfg.dir, &rows, capacity)?;
    let map: Rows = rows.into_iter().collect();
    let (entries, _) = plan::scan_recovered(&map, now_secs());
    Ok(Outcome::Restored {
        file: file.to_path_buf(),
        keys: entries.len(),
    })
}

/// Read a damaged database read-only and write what survives to `dest`.
fn rescue(cfg: &Config, dest: &Path) -> Result<Outcome, KvError> {
    if dest.exists() {
        return Err(KvError::Usage(format!(
            "{} already exists; rescue writes a fresh directory",
            dest.display()
        )));
    }
    let capacity = cfg.capacity();
    let (rows, quarantined) = {
        let mut damaged =
            Db::salvage_with(&cfg.dir, capacity).map_err(|e| open_error(&cfg.dir, e))?;
        let quarantined = damaged.recovery_report().quarantined_rows;
        let mut lifted = damaged.compact_to_memory()?;
        (lifted.all()?, quarantined)
    };
    let map: Rows = rows.into_iter().collect();
    let (entries, unreadable) = plan::scan_recovered(&map, now_secs());
    let fresh = rebuild_rows(&entries, now_secs())?;
    write_fresh(dest, &fresh, capacity)?;
    Ok(Outcome::Rescued {
        dest: dest.to_path_buf(),
        keys: entries.len(),
        quarantined,
        unreadable,
    })
}
