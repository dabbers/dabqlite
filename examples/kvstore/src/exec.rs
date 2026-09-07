//! One command in, one outcome out.
//!
//! This used to be the "only module that opens a database", because an
//! open `Db<S>` had no nameable type and so could not leave the function
//! that made it. It is now just dispatch: [`crate::store::Store`] holds
//! the database and does the work.

use std::io::Write;
use std::path::{Path, PathBuf};

use dabqlite::{FileDb, MemDb, RecoveryReport, Snapshot, Stats};

use crate::record::MAX_PAYLOAD;
use crate::store::{rebuild, snapshot_bytes, Census, Entry, Store};
use crate::{now_secs, open_error, Config, KvError};

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
    pub census: Census,
    pub recovery: RecoveryReport,
    pub max_payload: usize,
}

/// Run one command.
pub fn execute(cfg: &Config, cmd: &Command, warn: &mut dyn Write) -> Result<Outcome, KvError> {
    match cmd {
        Command::Compact => compact(cfg, warn),
        Command::Restore { file } => restore(cfg, file),
        Command::Rescue { dest } => rescue(cfg, dest),
        Command::Get { .. }
        | Command::Info { .. }
        | Command::List { .. }
        | Command::Search { .. }
        | Command::Stats
        | Command::Backup { .. } => on_reader(cfg, cmd, warn),
        other => on_writer(cfg, other, warn),
    }
}

/// Reads: opened read-only, taking no lock and writing nothing, so they
/// run alongside a writer in another process.
fn on_reader(cfg: &Config, cmd: &Command, warn: &mut dyn Write) -> Result<Outcome, KvError> {
    // `Db::open` CREATES, so a reader asked about a path that holds
    // nothing must check first — otherwise `kv get` on a typo makes a
    // database. This used to hardcode the superblock's filename, because
    // the library had no way to be asked.
    if !FileDb::exists(&cfg.dir) {
        let mut empty = Store::in_memory(cfg.rows.unwrap_or(dabqlite::DEFAULT_ROWS))?;
        return read(cfg, &mut empty, cmd);
    }
    let mut store = Store::read_only(cfg)?;
    if store.is_degraded() {
        return Err(KvError::Damaged {
            dir: cfg.dir.clone(),
            what: format!(
                "{} row(s) failed verification",
                store.recovery_report().quarantined_rows
            ),
        });
    }
    alarm(cfg, &store.recovery_report(), warn);
    read(cfg, &mut store, cmd)
}

/// The read commands, over any backend.
///
/// A plain generic function. It used to be a `macro_rules!`, because
/// `&mut Db<S>` could not be written down outside the library.
fn read<S: dabqlite::Storage>(
    cfg: &Config,
    store: &mut Store<S>,
    cmd: &Command,
) -> Result<Outcome, KvError> {
    let now = now_secs();
    Ok(match cmd {
        Command::Get { key, raw } => match store.get(key, now)? {
            Some(e) => Outcome::Value {
                value: e.value,
                raw: *raw,
            },
            None => Outcome::Missing { key: key.clone() },
        },
        Command::Info { key } => match store.get(key, now)? {
            Some(e) => Outcome::Info(e),
            None => Outcome::Missing { key: key.clone() },
        },
        Command::List { prefix, values } => {
            let mut entries = store.entries(now)?;
            if let Some(p) = prefix {
                entries.retain(|e| e.key.starts_with(p.as_str()));
            }
            Outcome::Entries {
                entries,
                values: *values,
            }
        }
        Command::Search { needle, keys } => Outcome::Entries {
            entries: store.search(needle, *keys, now)?,
            values: true,
        },
        Command::Stats => Outcome::Report(Box::new(Report {
            dir: cfg.dir.clone(),
            stats: store.stats(),
            census: store.census(now)?,
            recovery: store.recovery_report(),
            max_payload: MAX_PAYLOAD,
        })),
        Command::Backup { file } => {
            let bytes = snapshot_bytes(store)?;
            std::fs::write(file, &bytes).map_err(|err| KvError::Io {
                what: format!("writing {}", file.display()),
                err,
            })?;
            Outcome::BackedUp {
                file: file.clone(),
                bytes: bytes.len(),
            }
        }
        other => unreachable!("{other:?} is not a read"),
    })
}

/// Writes: the single-writer lock, one open, one commit.
fn on_writer(cfg: &Config, cmd: &Command, warn: &mut dyn Write) -> Result<Outcome, KvError> {
    let mut store = Store::open(cfg)?;
    alarm(cfg, &store.recovery_report(), warn);
    let now = now_secs();
    Ok(match cmd {
        Command::Set { key, value, ttl } => {
            let expires_at = ttl.map(|t| now.saturating_add(t)).unwrap_or(0);
            let (replaced, rows) = store.set(key, value, expires_at, now)?;
            Outcome::Stored {
                key: key.clone(),
                replaced,
                rows,
            }
        }
        Command::Del { key } => {
            if store.del(key, now)? {
                Outcome::Deleted { key: key.clone() }
            } else {
                Outcome::Missing { key: key.clone() }
            }
        }
        Command::Purge => Outcome::Purged(store.purge(now)?),
        other => unreachable!("{other:?} is handled elsewhere"),
    })
}

/// The library asks callers to alarm on rollback evidence; nothing else
/// will.
fn alarm(cfg: &Config, recovery: &RecoveryReport, warn: &mut dyn Write) {
    if recovery.rollback_evidence {
        let _ = writeln!(
            warn,
            "kv: WARNING: {} shows evidence that an acknowledged write was rolled \
             back by a storage fault. Data committed just before the last restart \
             may be missing.",
            cfg.dir.display()
        );
    }
}

/// Rebuild in place: drop tombstones, expired records and the dead slots
/// that updates left behind.
///
/// The directory dance this used to do — build a sibling, rename the live
/// database aside, rename the new one in, clean up, and warn the user
/// where the old copy is if the second rename failed — is gone.
/// [`dabqlite::Db::restore`] does the crash-safe swap, and finishes an
/// interrupted one on the next open.
///
/// Re-placing the records is still ours: ids are hashes and a hash table
/// cannot drop a tombstone without re-homing everything that probed past
/// it, which is not something the library can know. But the rebuild now
/// happens UNDER the writer lock.
///
/// It used to read the entries, drop the store to free the lock, rebuild,
/// and swap — and anything another process committed in that window was
/// acknowledged, fsynced, and then thrown away. `Db::rebuild_with` hands
/// the transform the live rows and keeps the lock from the read to the
/// reopen, so the window is gone.
fn compact(cfg: &Config, _warn: &mut dyn Write) -> Result<Outcome, KvError> {
    let now = now_secs();
    let mut store = Store::open(cfg)?;
    let before = store.stats();
    let entries = store.entries(now)?;
    let keys = entries.len();
    let capacity = cfg.rows.map(|r| r.max(1)).unwrap_or(before.capacity);

    // The transform cannot fail, so a rebuild that would not fit is
    // caught here rather than half-way through the swap.
    let laid_out = rebuild(&entries, capacity)?.all()?;
    store
        .db()
        .rebuild_with(move |_| laid_out)
        .map_err(|e| open_error(&cfg.dir, e))?;
    let after = store.stats();
    Ok(Outcome::Compacted {
        before,
        after,
        keys,
    })
}

/// Load a snapshot blob into an empty database directory.
fn restore(cfg: &Config, file: &Path) -> Result<Outcome, KvError> {
    if FileDb::exists(&cfg.dir) {
        return Err(KvError::Usage(format!(
            "{} already holds a database; restore into an empty --db directory",
            cfg.dir.display()
        )));
    }
    let bytes = std::fs::read(file).map_err(|err| KvError::Io {
        what: format!("reading {}", file.display()),
        err,
    })?;
    // A snapshot goes straight back onto disk, atomically. It used to
    // reopen only in memory, so this was a load followed by copying every
    // row across by hand into a directory built here.
    let snapshot = Snapshot::from_bytes(&bytes)?;
    MemDb::restore(&cfg.dir, &snapshot).map_err(|e| open_error(&cfg.dir, e))?;
    let mut store = Store::open(cfg)?;
    let keys = store.entries(now_secs())?.len();
    Ok(Outcome::Restored {
        file: file.to_path_buf(),
        keys,
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
    let now = now_secs();
    // `salvage` takes no writer lock and writes nothing, so this is safe
    // to run against a database another process is still holding.
    let mut damaged = Store::salvage(cfg)?;
    let quarantined = damaged.recovery_report().quarantined_rows;
    let (entries, unreadable) = damaged.recovered(now)?;
    let capacity = cfg
        .rows
        .map(|r| r.max(1))
        .unwrap_or(damaged.stats().capacity);
    drop(damaged);

    let mut fresh = rebuild(&entries, capacity)?;
    let snapshot = fresh.snapshot()?;
    MemDb::restore(dest, &snapshot).map_err(|e| open_error(dest, e))?;
    Ok(Outcome::Rescued {
        dest: dest.to_path_buf(),
        keys: entries.len(),
        quarantined,
        unreadable,
    })
}
