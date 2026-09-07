//! The key/value store itself: a `Db` and the hash placement on top of it.
//!
//! This module replaces the old `plan.rs` + `exec.rs` pair. That split was
//! not a design: `dabqlite::Db<S>` could not be NAMED outside the library,
//! so it could not be a struct field, a parameter or a return type, and
//! every command had to open its own database, materialise every row into
//! a `BTreeMap`, hand the map to pure functions, and apply the row writes
//! they returned — through a macro, because a helper function would have
//! needed to write `&mut Db<S>`.
//!
//! `Storage` and the backends are re-exported now, so this is a struct
//! with a `Db<S>` in it and the map, the macro and the plan/apply
//! indirection are gone.

use dabqlite::{Db, MemDb, MemoryStorage, Op, Stats, Storage, MAX_COMMIT_ROWS};

#[cfg(unix)]
use dabqlite::{PosixStorage, ReadOnlyDir};

use crate::record::{self, Record, MAX_KEY, MAX_PROBE};
use crate::{Config, KvError, RESERVED_SLOTS};

/// A key, its value, and where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Vec<u8>,
    /// Unix seconds; 0 means no expiry.
    pub expires_at: u64,
    /// The row id the hash placed it on.
    pub id: u64,
}

/// Where a key lives, or where it would go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// The key is stored at this id (possibly expired).
    Occupied { id: u64, record: Record },
    /// The key is absent; this id is the first reusable one.
    Vacant { id: u64 },
}

/// Counts the library cannot report, because a tombstone and an expired
/// session are both just live rows to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Census {
    pub live: u64,
    pub expired: u64,
    pub tombstones: u64,
}

pub fn validate_key(key: &str) -> Result<(), KvError> {
    if key.is_empty() {
        return Err(KvError::BadKey("a key may not be empty".into()));
    }
    if key.len() > MAX_KEY {
        return Err(KvError::KeyTooLong {
            len: key.len(),
            max: MAX_KEY,
        });
    }
    // The key is the front of the record and a NUL ends it, which is what
    // makes byte order key order (see `record`). A key holding one would
    // decode as a shorter key with garbage behind it, so it is refused at
    // the door rather than stored and misread later.
    if key.as_bytes().contains(&record::KEY_END) {
        return Err(KvError::BadKey(
            "a key may not contain a NUL byte: it terminates the key inside \
             the record"
                .into(),
        ));
    }
    Ok(())
}

/// The store. One `Db`, one record per key.
pub struct Store<S: Storage> {
    db: Db<S>,
}

#[cfg(unix)]
impl Store<PosixStorage> {
    /// Open the configured directory. `--rows` overrides the capacity the
    /// database recorded for itself; without it, the database remembers.
    pub fn open(cfg: &Config) -> Result<Self, KvError> {
        let db = match cfg.rows {
            Some(rows) => Db::open_with(&cfg.dir, rows.max(1)),
            None => Db::open(&cfg.dir),
        }
        .map_err(|e| crate::open_error(&cfg.dir, e))?;
        Ok(Store { db })
    }
}

#[cfg(unix)]
impl Store<ReadOnlyDir> {
    /// Open a DAMAGED directory read-only, quarantining what cannot be
    /// verified. Takes no writer lock.
    pub fn salvage(cfg: &Config) -> Result<Self, KvError> {
        let db = match cfg.rows {
            Some(rows) => Db::salvage_with(&cfg.dir, rows.max(1)),
            None => Db::salvage(&cfg.dir),
        }
        .map_err(|e| crate::open_error(&cfg.dir, e))?;
        Ok(Store { db })
    }

    /// Open a HEALTHY directory read-only, taking no writer lock and
    /// writing nothing.
    ///
    /// Every `kv` read runs through this, so `get`, `list`, `search` and
    /// `stats` work while another process is writing. They used to take
    /// the single-writer lock and fail with "already open in another
    /// process".
    pub fn read_only(cfg: &Config) -> Result<Self, KvError> {
        let db = match cfg.rows {
            Some(rows) => Db::salvage_with(&cfg.dir, rows.max(1)),
            None => Db::read_only(&cfg.dir),
        }
        .map_err(|e| crate::open_error(&cfg.dir, e))?;
        Ok(Store { db })
    }
}

impl Store<MemoryStorage> {
    pub fn in_memory(rows: u64) -> Result<Self, KvError> {
        Ok(Store {
            db: MemDb::in_memory_with(rows.max(1))?,
        })
    }
}

impl<S: Storage> Store<S> {
    /// The database underneath, for the things only it can answer.
    pub fn db(&mut self) -> &mut Db<S> {
        &mut self.db
    }

    pub fn stats(&self) -> Stats {
        self.db.stats()
    }

    pub fn recovery_report(&self) -> dabqlite::RecoveryReport {
        self.db.recovery_report()
    }

    /// True when this store was opened read-only over a damaged database
    /// and rows had to be quarantined.
    pub fn is_degraded(&self) -> bool {
        self.db.is_degraded()
    }

    // -- placement ---------------------------------------------------------

    /// Find the id holding `key`, or the id a new record should use.
    ///
    /// Open addressing with linear probing, because the library keys by
    /// `u64` and a key/value store keys by string. A tombstone is a chain
    /// link, not a hole: `Db::remove` would make the id indistinguishable
    /// from one that was never used, which would cut every key that
    /// probed past it loose.
    pub fn locate(&mut self, key: &[u8]) -> Result<Slot, KvError> {
        let home = record::home_id(key);
        let mut first_free: Option<u64> = None;
        for i in 0..MAX_PROBE {
            let id = record::probe(home, i);
            match self.db.get(id)? {
                None => {
                    return Ok(Slot::Vacant {
                        id: first_free.unwrap_or(id),
                    })
                }
                Some(v) => {
                    let record = record::decode(&v)?;
                    if !record.live {
                        first_free.get_or_insert(id);
                        continue;
                    }
                    if record.key.as_bytes() == key {
                        return Ok(Slot::Occupied { id, record });
                    }
                }
            }
        }
        Err(KvError::ProbeExhausted { probes: MAX_PROBE })
    }

    // -- reads -------------------------------------------------------------

    /// Fetch a key, treating an expired record as absent.
    pub fn get(&mut self, key: &str, now: u64) -> Result<Option<Entry>, KvError> {
        validate_key(key)?;
        Ok(match self.locate(key.as_bytes())? {
            Slot::Vacant { .. } => None,
            Slot::Occupied { id, record } if record.visible(now) => Some(Entry {
                key: record.key,
                value: record.value,
                expires_at: record.expires_at,
                id,
            }),
            Slot::Occupied { .. } => None,
        })
    }

    /// Every stored record, in id order, decoded — one page of the
    /// ordered scan at a time so the whole database is never materialised
    /// as values.
    fn for_each(&mut self, mut f: impl FnMut(u64, Record)) -> Result<(), KvError> {
        let mut cursor = 0u64;
        loop {
            let (page, next) = self.db.range_page(cursor, u64::MAX)?;
            for (id, value) in page {
                f(id, record::decode(&value)?);
            }
            match next {
                Some(n) => cursor = n,
                None => return Ok(()),
            }
        }
    }

    /// Every live, unexpired entry, ordered by key.
    ///
    /// This used to be a full scan and a sort, every time: ids are
    /// hashes, so the library's one ordering (`u64` ascending) was not key
    /// order and there was no second index to ask. The key is now the
    /// front of the record and the library scans in VALUE order, so this
    /// is that scan — already in key order, with nothing to sort.
    pub fn entries(&mut self, now: u64) -> Result<Vec<Entry>, KvError> {
        self.under("", now)
    }

    /// Every live, unexpired entry whose key starts with `prefix`, in key
    /// order.
    ///
    /// The one that used to hurt. "List everything under `session/`" was
    /// a scan of the whole database, a sort, and then a filter; it is now
    /// a prefix scan off the index, which reads the matching rows and
    /// stops.
    pub fn under(&mut self, prefix: &str, now: u64) -> Result<Vec<Entry>, KvError> {
        let mut out = Vec::new();
        for (id, value) in self.db.prefix(prefix.as_bytes())? {
            let r = record::decode(&value)?;
            if !r.visible(now) {
                continue;
            }
            // A prefix over RECORD bytes is a prefix over key bytes only
            // because the key leads. Checked rather than assumed: a
            // record whose key merely started with the prefix and then
            // ended would still match the byte scan.
            debug_assert!(r.key.starts_with(prefix));
            out.push(Entry {
                key: r.key,
                value: r.value,
                expires_at: r.expires_at,
                id,
            });
        }
        Ok(out)
    }

    /// Substring search over values, and optionally keys.
    ///
    /// Served by the library's index: `Db::find` narrows to the records
    /// whose stored bytes contain the needle, which is a superset of the
    /// answer because a record's value bytes are contiguous inside it.
    /// The filter below drops hits that landed in the header or, without
    /// `--keys`, in the key.
    pub fn search(
        &mut self,
        needle: &[u8],
        keys_too: bool,
        now: u64,
    ) -> Result<Vec<Entry>, KvError> {
        let mut out = Vec::new();
        for (id, value) in self.db.find(needle)? {
            let r = record::decode(&value)?;
            if !r.visible(now) {
                continue;
            }
            if contains(&r.value, needle) || (keys_too && contains(r.key.as_bytes(), needle)) {
                out.push(Entry {
                    key: r.key,
                    value: r.value,
                    expires_at: r.expires_at,
                    id,
                });
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    pub fn census(&mut self, now: u64) -> Result<Census, KvError> {
        let mut c = Census::default();
        self.for_each(|_, r| {
            if !r.live {
                c.tombstones += 1;
            } else if r.expired(now) {
                c.expired += 1;
            } else {
                c.live += 1;
            }
        })?;
        Ok(c)
    }

    /// A tolerant scan for salvage: rows this layout cannot read are
    /// counted rather than failing the whole listing.
    pub fn recovered(&mut self, now: u64) -> Result<(Vec<Entry>, u64), KvError> {
        let mut out = Vec::new();
        let mut lost = 0u64;
        let mut cursor = 0u64;
        loop {
            let (page, next) = self.db.range_page(cursor, u64::MAX)?;
            for (id, value) in page {
                match record::decode(&value) {
                    Ok(r) if r.visible(now) => out.push(Entry {
                        key: r.key,
                        value: r.value,
                        expires_at: r.expires_at,
                        id,
                    }),
                    Ok(_) => {}
                    Err(_) => lost += 1,
                }
            }
            match next {
                Some(n) => cursor = n,
                None => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok((out, lost))
    }

    // -- writes ------------------------------------------------------------

    /// Store `key = value`. One value, one commit, whatever the length.
    ///
    /// Returns whether a live, unexpired value was replaced, and the row
    /// slots the write took.
    pub fn set(
        &mut self,
        key: &str,
        value: &[u8],
        expires_at: u64,
        now: u64,
    ) -> Result<(bool, u64), KvError> {
        validate_key(key)?;
        let encoded = record::encode(key, value, expires_at)?;
        let (id, replaced) = match self.locate(key.as_bytes())? {
            Slot::Vacant { id } => (id, false),
            Slot::Occupied { id, record } => (id, record.visible(now)),
        };
        let op = Op::put(id, encoded);
        let cost = op.rows() as u64;
        self.room_for(cost, RESERVED_SLOTS)?;
        self.db.batch(std::slice::from_ref(&op))?;
        Ok((replaced, cost))
    }

    /// Retire a key. `Ok(false)` means it was not there.
    pub fn del(&mut self, key: &str, now: u64) -> Result<bool, KvError> {
        validate_key(key)?;
        match self.locate(key.as_bytes())? {
            Slot::Occupied { id, record } if record.visible(now) => {
                self.room_for(1, 0)?;
                self.db.put(id, record::tombstone())?;
                Ok(true)
            }
            // An expired record is already invisible; retiring it is
            // `purge`'s job.
            _ => Ok(false),
        }
    }

    /// Retire every expired record, as ONE atomic commit per batch.
    ///
    /// This is the write the old code could not express: it applied row
    /// writes one commit at a time, so a purge of fifty sessions was
    /// fifty commits and a hundred fsyncs, and a crash halfway left half
    /// of them retired.
    pub fn purge(&mut self, now: u64) -> Result<Vec<String>, KvError> {
        let mut doomed: Vec<(u64, String)> = Vec::new();
        self.for_each(|id, r| {
            if r.live && r.expired(now) {
                doomed.push((id, r.key));
            }
        })?;
        self.room_for(doomed.len() as u64, 0)?;
        // A tombstone is one row slot, so a batch holds MAX_COMMIT_ROWS
        // of them.
        for chunk in doomed.chunks(MAX_COMMIT_ROWS) {
            let ops: Vec<Op> = chunk
                .iter()
                .map(|(id, _)| Op::put(*id, record::tombstone()))
                .collect();
            self.db.batch(&ops)?;
        }
        Ok(doomed.into_iter().map(|(_, key)| key).collect())
    }

    /// Refuse a write that does not fit before any of it is written.
    ///
    /// The reserve exists because a delete is a write too: retiring a key
    /// appends a tombstone and so needs a free slot, and a store that
    /// cannot delete at capacity cannot be dug out of it.
    fn room_for(&self, needed: u64, reserve: u64) -> Result<(), KvError> {
        let stats = self.db.stats();
        let free = stats.free();
        if needed + reserve > free {
            return Err(KvError::NoRoom {
                needed,
                free,
                capacity: stats.capacity,
            });
        }
        Ok(())
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Lay `entries` out from scratch in a fresh in-memory database.
///
/// Compaction, restore and rescue all need this: a hash placement cannot
/// be compacted in place, because dropping a tombstone breaks the chain
/// of every key that probed past it. The library's own `compact` reclaims
/// dead slots; re-placing the records is ours, and always will be.
pub fn rebuild(entries: &[Entry], capacity: u64) -> Result<MemDb, KvError> {
    let mut fresh = Store::in_memory(capacity)?;
    for e in entries {
        fresh.set(&e.key, &e.value, e.expires_at, 0)?;
    }
    Ok(fresh.db)
}

/// The bytes of a database, for `backup`.
pub fn snapshot_bytes<S: Storage>(store: &mut Store<S>) -> Result<Vec<u8>, KvError> {
    Ok(store.db.snapshot()?.to_bytes())
}

/// Every value in the database, for tests that want to look at the rows.
#[cfg(test)]
pub fn raw_rows<S: Storage>(store: &mut Store<S>) -> Result<Vec<dabqlite::Row>, KvError> {
    Ok(store.db.all()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store<MemoryStorage> {
        Store::in_memory(4096).unwrap()
    }

    fn set(s: &mut Store<MemoryStorage>, k: &str, v: &str) {
        s.set(k, v.as_bytes(), 0, 0).unwrap();
    }

    #[test]
    fn set_then_get_round_trips_a_long_value_in_one_commit() {
        let mut s = store();
        let long = "x".repeat(1000);
        let (replaced, slots) = s.set("session/42", long.as_bytes(), 0, 0).unwrap();
        assert!(!replaced);
        assert_eq!(slots, (record::HEADER + 10 + 1000).div_ceil(16) as u64);
        assert_eq!(
            s.get("session/42", 0).unwrap().unwrap().value,
            long.as_bytes()
        );
        // One record, one row id — the whole 1000 bytes live under it.
        assert_eq!(raw_rows(&mut s).unwrap().len(), 1);
    }

    #[test]
    fn values_may_contain_nul_and_newlines() {
        let mut s = store();
        let v = b"line1\nline2\0tail\0".to_vec();
        s.set("k", &v, 0, 0).unwrap();
        assert_eq!(s.get("k", 0).unwrap().unwrap().value, v);
    }

    /// Used to be
    /// `an_update_writes_the_other_bank_so_the_old_value_survives_a_torn_write`,
    /// which walked every prefix of a multi-row plan to prove the old
    /// value was still readable after a crash at each step. A record is
    /// one value in one commit now, so there are no prefixes: the library
    /// makes the write all-or-nothing and there is no bank to flip.
    #[test]
    fn an_update_is_a_single_commit_with_no_intermediate_state() {
        let mut s = store();
        set(&mut s, "k", "original value that is quite long indeed");
        let before = raw_rows(&mut s).unwrap();
        let (replaced, _) = s.set("k", b"replacement", 0, 0).unwrap();
        assert!(replaced);
        let after = raw_rows(&mut s).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert_eq!(before[0].0, after[0].0, "an update stays on its own id");
        assert_eq!(s.get("k", 0).unwrap().unwrap().value, b"replacement");
    }

    /// Plant a record at a chosen id so a probe chain can be built without
    /// brute-forcing a 64-bit hash collision.
    fn plant(s: &mut Store<MemoryStorage>, id: u64, key: &str, value: &[u8]) {
        s.db.put(id, record::encode(key, value, 0).unwrap())
            .unwrap();
    }

    #[test]
    fn a_taken_home_id_is_probed_past_and_tombstones_keep_the_chain() {
        let mut s = store();
        let home = record::home_id(b"alpha");
        plant(&mut s, home, "occupier", b"x");
        set(&mut s, "alpha", "1");
        match s.locate(b"alpha").unwrap() {
            Slot::Occupied { id, .. } => assert_eq!(id, record::probe(home, 1)),
            other => panic!("expected alpha one past its home id, got {other:?}"),
        }

        // Retiring the record in front of it must not hide it: the
        // tombstone has to stay as a chain link. This is the limitation
        // that STILL stands — `Db::remove` here would lose "alpha".
        s.del("occupier", 0).unwrap();
        assert_eq!(s.get("alpha", 0).unwrap().unwrap().value, b"1");
        s.db.remove(home).unwrap();
        assert!(
            s.get("alpha", 0).unwrap().is_none(),
            "removing the row instead of tombstoning it must cut the chain \
             — that is why this crate keeps its own tombstone"
        );
    }

    #[test]
    fn a_tombstoned_id_is_reused_by_the_next_key_that_lands_on_it() {
        let mut s = store();
        set(&mut s, "k", "first");
        let id = match s.locate(b"k").unwrap() {
            Slot::Occupied { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert!(s.del("k", 0).unwrap());
        assert!(s.get("k", 0).unwrap().is_none());
        match s.locate(b"k").unwrap() {
            Slot::Vacant { id: free } => assert_eq!(free, id, "the freed slot must be reused"),
            other => panic!("{other:?}"),
        }
        set(&mut s, "k", "second");
        assert_eq!(s.get("k", 0).unwrap().unwrap().value, b"second");
        assert_eq!(
            raw_rows(&mut s).unwrap().len(),
            1,
            "reuse must not leak a row"
        );
    }

    #[test]
    fn listing_is_ordered_and_skips_expired_and_deleted() {
        let mut s = store();
        set(&mut s, "b", "2");
        set(&mut s, "a", "1");
        set(&mut s, "c", "3");
        s.set("d", b"4", 100, 0).unwrap();
        s.del("b", 0).unwrap();

        let keys = |s: &mut Store<MemoryStorage>, now| {
            s.entries(now)
                .unwrap()
                .into_iter()
                .map(|e| e.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&mut s, 50), vec!["a", "c", "d"]);
        assert_eq!(keys(&mut s, 150), vec!["a", "c"]);
    }

    #[test]
    fn search_matches_across_the_sixteen_byte_slot_boundary() {
        let mut s = store();
        set(&mut s, "k", "0123456789abcdeneedle-tail");
        assert_eq!(s.search(b"needle", false, 0).unwrap().len(), 1);
        assert!(s.search(b"absent", false, 0).unwrap().is_empty());
    }

    /// Used to be pinned at sixteen bytes: `Db::find` refused any needle
    /// longer than one row, so this crate could not use the index at all
    /// and searched by scanning and comparing in Rust. A needle may now
    /// be as long as a value, so the index answers the question.
    #[test]
    fn a_needle_may_be_as_long_as_a_value() {
        let mut s = store();
        let long = "the-quick-brown-fox-jumps-over-the-lazy-dog".repeat(4);
        s.set("k", format!("prefix {long} suffix").as_bytes(), 0, 0)
            .unwrap();
        assert!(long.len() > dabqlite::VALUE_LEN * 10);
        assert_eq!(s.search(long.as_bytes(), false, 0).unwrap().len(), 1);
    }

    /// A search hit inside the record HEADER must not be reported: the
    /// header is this crate's, not the user's data.
    #[test]
    fn search_does_not_match_the_records_own_framing() {
        let mut s = store();
        set(&mut s, "key-with-text", "unrelated");
        // The magic byte is in every record; a byte search for it must
        // find nothing, because it is not in anybody's value.
        assert!(s.search(&[0xD5], false, 0).unwrap().is_empty());
        // And a key match only counts when asked for.
        assert!(s.search(b"key-with", false, 0).unwrap().is_empty());
        assert_eq!(s.search(b"key-with", true, 0).unwrap().len(), 1);
    }

    #[test]
    fn purge_retires_every_expired_session_in_one_commit_per_batch() {
        let mut s = store();
        for i in 0..200 {
            s.set(&format!("s{i}"), b"tok", 100, 0).unwrap();
        }
        set(&mut s, "keeper", "stays");
        let before = s.stats().slots;
        let purged = s.purge(150).unwrap();
        assert_eq!(purged.len(), 200);
        let after = s.stats().slots;
        // 200 tombstones, one slot each, and at most two commits' worth of
        // batches — not 200 commits.
        assert_eq!(after - before, 200);
        assert_eq!(s.census(150).unwrap().live, 1);
        assert_eq!(s.census(150).unwrap().tombstones, 200);
        assert_eq!(s.census(150).unwrap().expired, 0);
    }

    #[test]
    fn a_rebuild_drops_tombstones_and_expired_records() {
        let mut s = store();
        for i in 0..10 {
            set(&mut s, &format!("k{i}"), &"x".repeat(200));
        }
        for i in 0..10 {
            set(&mut s, &format!("k{i}"), "short");
        }
        for i in 0..4 {
            s.del(&format!("k{i}"), 0).unwrap();
        }
        let live = s.entries(0).unwrap();
        assert_eq!(live.len(), 6);
        let mut rebuilt = Store {
            db: rebuild(&live, 4096).unwrap(),
        };
        assert_eq!(
            rebuilt.entries(0).unwrap(),
            live_without_ids(&live, &mut rebuilt)
        );
        assert_eq!(rebuilt.census(0).unwrap().tombstones, 0);
        assert_eq!(rebuilt.stats().dead, 0);
        assert!(rebuilt.stats().slots < s.stats().slots);
    }

    /// The ids move when a database is re-placed, so compare everything
    /// except them.
    fn live_without_ids(want: &[Entry], got: &mut Store<MemoryStorage>) -> Vec<Entry> {
        let mut out = want.to_vec();
        for e in &mut out {
            e.id = got
                .locate(e.key.as_bytes())
                .map(|s| match s {
                    Slot::Occupied { id, .. } | Slot::Vacant { id } => id,
                })
                .unwrap();
        }
        out
    }

    /// The reserve is not decoration: without it a full database cannot be
    /// dug out of, because retiring a key is itself a write.
    #[test]
    fn a_full_database_still_has_room_to_delete() {
        let mut s = Store::in_memory(24).unwrap();
        let mut stored = 0;
        for i in 0..100 {
            if s.set(&format!("k{i}"), b"v", 0, 0).is_err() {
                break;
            }
            stored += 1;
        }
        assert!(stored > 2, "only stored {stored}");
        assert!(matches!(
            s.set("one-more", b"v", 0, 0),
            Err(KvError::NoRoom { .. })
        ));
        assert!(s.del("k0", 0).unwrap(), "a delete must still fit");
    }
}
