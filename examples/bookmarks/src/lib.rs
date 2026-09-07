//! A browser-style bookmark and tag index built on `dabqlite`.
//!
//! The model here is the one the library's `in_memory` backend is aimed at:
//! the database lives entirely in RAM, and persistence is one opaque blob
//! that the host (IndexedDB, a download, a POST) is responsible for keeping.
//! [`with_store`] is a whole session — load a blob, do work, hand back a new
//! blob.
//!
//! # Two shapes forced on this crate by the library
//!
//! **1. Nothing here can own a `Db`.** `Db::in_memory()` returns
//! `Db<MemoryStorage>`, and `MemoryStorage` is not re-exported from
//! `dabqlite` (nor is the `Storage` trait), so a downstream crate cannot
//! name the type. Every one of these fails to compile:
//!
//! ```text
//! struct Store<S> { db: Db<S> }              // S: Storage — unnameable
//! fn touch<S>(db: &mut Db<S>) -> u64         // same
//! fn make() -> Db<dabqlite::MemoryStorage>   // private struct
//! ```
//!
//! The database can therefore only exist as a local binding whose type is
//! inferred. To get anything resembling a store object, this crate erases
//! the type by hand: [`Cmd`]/[`Reply`] plus a `&mut dyn FnMut` in
//! [`Store`]. That whole layer is plumbing that exists only because a type
//! could not be spelled.
//!
//! **2. A bookmark does not fit in a row.** Values are exactly 16 bytes.
//! URLs and titles are not, so a bookmark is spread over many rows and the
//! *key* carries the structure:
//!
//! ```text
//!  63                    24 23      16 15        0
//! +------------------------+----------+-----------+
//! |      entity id (40)    | field (8)| ord (16)  |
//! +------------------------+----------+-----------+
//! ```
//!
//! which makes `range(entity<<24 ..= entity<<24|0xFFFFFF)` the "read one
//! bookmark" query and keeps `all()` in bookmark order for free. Tags are
//! the one part of the model that genuinely fits a 16-byte row, so they get
//! one row each — and that is the only field the library's own substring
//! index can search correctly (see [`Store::search_tag`] versus
//! [`Store::search`]).

use dabqlite::{Db, Error as DbErr, Row, Snapshot, Stats, Value, DEFAULT_ROWS, VALUE_LEN};

// ---------------------------------------------------------------------------
// Key layout
// ---------------------------------------------------------------------------

const ENTITY_SHIFT: u32 = 24;
const FIELD_SHIFT: u32 = 16;
const ORD_MASK: u64 = 0xFFFF;
const FIELD_MASK: u64 = 0xFF;

/// Largest entity id the key layout can hold.
pub const MAX_ENTITY: u64 = (1 << 40) - 1;

const F_HEADER: u64 = 0;
const F_META: u64 = 1;
const F_URL: u64 = 2;
const F_TITLE: u64 = 3;
const F_TAG: u64 = 4;

/// The store header lives on the reserved entity 0, so bookmark ids start
/// at 1 and `all()` yields the header first.
const HEADER_KEY: u64 = key(0, F_HEADER, 0);
const HEADER_MAGIC: &[u8; 4] = b"BMK1";

/// Longest URL or title this crate accepts. Nothing in the library imposes
/// it; it keeps the per-bookmark row count (and therefore the slot burn)
/// something a caller can reason about.
pub const MAX_TEXT: usize = 4096;
/// A tag must fit one row, because tag rows are what the substring index
/// can actually search.
pub const MAX_TAG: usize = VALUE_LEN;
/// Tag ordinals share the 16-bit `ord` field; this is a sanity ceiling.
pub const MAX_TAGS: usize = 64;

const fn key(entity: u64, field: u64, ord: u64) -> u64 {
    (entity << ENTITY_SHIFT) | (field << FIELD_SHIFT) | ord
}
const fn entity_of(k: u64) -> u64 {
    k >> ENTITY_SHIFT
}
const fn field_of(k: u64) -> u64 {
    (k >> FIELD_SHIFT) & FIELD_MASK
}
const fn ord_of(k: u64) -> u64 {
    k & ORD_MASK
}
const fn entity_lo(entity: u64) -> u64 {
    key(entity, 0, 0)
}
const fn entity_hi(entity: u64) -> u64 {
    key(entity, FIELD_MASK, ORD_MASK)
}

fn chunks_for(len: usize) -> usize {
    len.div_ceil(VALUE_LEN)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The library said no.
    Db(DbErr),
    /// No bookmark with this id.
    NotFound(u64),
    /// A tag is longer than one row.
    TagTooLong { tag: String, max: usize },
    TooManyTags { got: usize, max: usize },
    TextTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// The rows for a bookmark do not add up. Only reachable if a write was
    /// torn — there are no multi-row transactions.
    Damaged { entity: u64, what: String },
}

impl From<DbErr> for StoreError {
    fn from(e: DbErr) -> Self {
        StoreError::Db(e)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Db(e) => write!(f, "{e}"),
            StoreError::NotFound(id) => write!(f, "no bookmark {id}"),
            StoreError::TagTooLong { tag, max } => {
                write!(f, "tag {tag:?} is longer than {max} bytes")
            }
            StoreError::TooManyTags { got, max } => write!(f, "{got} tags; the limit is {max}"),
            StoreError::TextTooLong { field, len, max } => {
                write!(f, "{field} is {len} bytes; the limit is {max}")
            }
            StoreError::Damaged { entity, what } => {
                write!(f, "bookmark {entity} is damaged: {what}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

// ---------------------------------------------------------------------------
// The hand-rolled type-erasure layer
// ---------------------------------------------------------------------------

/// One database operation, as data.
///
/// This exists purely so [`Store`] can hold *something* that talks to a
/// database. `Db<MemoryStorage>` cannot be named downstream, so it cannot be
/// a struct field, a function parameter, or a trait bound here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Get(u64),
    Range(u64, u64),
    All,
    Find(Vec<u8>),
    Put(u64, Value),
    Delete(u64),
    Stats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Value(Option<Value>),
    Rows(Vec<Row>),
    Removed(bool),
    Unit,
    Stats(Stats),
}

/// A bookmark, reassembled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bookmark {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub tags: Vec<String>,
}

impl Bookmark {
    /// Case-insensitive substring match over every field.
    pub fn matches(&self, lowercase_needle: &str) -> bool {
        self.url.to_lowercase().contains(lowercase_needle)
            || self.title.to_lowercase().contains(lowercase_needle)
            || self.tags.iter().any(|t| t.contains(lowercase_needle))
    }
}

/// What a consistency scan found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Integrity {
    pub bookmarks: u64,
    /// Entities with rows but no meta row: a multi-row write that did not
    /// finish. The library has no transaction to prevent this.
    pub orphans: Vec<u64>,
    pub stray_rows: u64,
}

/// A bookmark store bound to a live database for the length of a session.
pub struct Store<'e> {
    exec: &'e mut dyn FnMut(Cmd) -> Result<Reply, DbErr>,
    next_id: u64,
    header_dirty: bool,
    compact: bool,
}

// The reply unwrappers below mirror, one level up, exactly what the library
// does internally over its own `Output` enum. Both layers exist for the same
// reason and neither can fail in practice.
fn as_rows(r: Reply) -> Vec<Row> {
    match r {
        Reply::Rows(v) => v,
        other => unreachable!("expected rows, got {other:?}"),
    }
}
fn as_value(r: Reply) -> Option<Value> {
    match r {
        Reply::Value(v) => v,
        other => unreachable!("expected value, got {other:?}"),
    }
}
fn as_removed(r: Reply) -> bool {
    match r {
        Reply::Removed(b) => b,
        other => unreachable!("expected removed, got {other:?}"),
    }
}
fn as_stats(r: Reply) -> Stats {
    match r {
        Reply::Stats(s) => s,
        other => unreachable!("expected stats, got {other:?}"),
    }
}

impl<'e> Store<'e> {
    fn attach(exec: &'e mut dyn FnMut(Cmd) -> Result<Reply, DbErr>) -> Result<Self, StoreError> {
        let mut s = Store {
            exec,
            next_id: 1,
            header_dirty: false,
            compact: false,
        };
        s.next_id = s.read_next_id()?;
        Ok(s)
    }

    /// The raw command channel. Public on purpose: a real app on this API
    /// ends up needing one, and the tests use it to fake a torn write.
    pub fn exec_raw(&mut self, cmd: Cmd) -> Result<Reply, DbErr> {
        (self.exec)(cmd)
    }

    fn get(&mut self, k: u64) -> Result<Option<Value>, StoreError> {
        Ok(as_value(self.exec_raw(Cmd::Get(k))?))
    }
    fn put(&mut self, k: u64, v: Value) -> Result<(), StoreError> {
        self.exec_raw(Cmd::Put(k, v))?;
        Ok(())
    }
    fn del(&mut self, k: u64) -> Result<bool, StoreError> {
        Ok(as_removed(self.exec_raw(Cmd::Delete(k))?))
    }
    fn range(&mut self, lo: u64, hi: u64) -> Result<Vec<Row>, StoreError> {
        Ok(as_rows(self.exec_raw(Cmd::Range(lo, hi))?))
    }
    fn all_rows(&mut self) -> Result<Vec<Row>, StoreError> {
        Ok(as_rows(self.exec_raw(Cmd::All)?))
    }

    /// Row-slot accounting, straight from the library.
    pub fn stats(&mut self) -> Result<Stats, StoreError> {
        Ok(as_stats(self.exec_raw(Cmd::Stats)?))
    }

    /// Ask the session to compact before it snapshots. Compaction has to
    /// happen out there, because it replaces the whole `Db` value.
    pub fn request_compaction(&mut self) {
        self.compact = true;
    }

    // -- header ------------------------------------------------------------

    fn read_next_id(&mut self) -> Result<u64, StoreError> {
        if let Some(v) = self.get(HEADER_KEY)? {
            let raw = v.raw();
            if &raw[0..4] == HEADER_MAGIC {
                return Ok(u64::from_le_bytes(raw[4..12].try_into().expect("fixed")));
            }
        }
        // No header (or a foreign one): fall back to a full scan for the
        // highest entity. There is no "max key" or descending scan, so this
        // is O(rows).
        let max = self
            .all_rows()?
            .iter()
            .map(|(k, _)| entity_of(*k))
            .max()
            .unwrap_or(0);
        Ok(max + 1)
    }

    fn flush_header(&mut self) -> Result<(), StoreError> {
        if !self.header_dirty {
            return Ok(());
        }
        let mut raw = [0u8; VALUE_LEN];
        raw[0..4].copy_from_slice(HEADER_MAGIC);
        raw[4..12].copy_from_slice(&self.next_id.to_le_bytes());
        raw[12..14].copy_from_slice(&1u16.to_le_bytes());
        self.put(HEADER_KEY, Value::from(raw))?;
        self.header_dirty = false;
        Ok(())
    }

    // -- writes ------------------------------------------------------------

    fn write_chunks(
        &mut self,
        entity: u64,
        field: u64,
        bytes: &[u8],
        old_chunks: usize,
    ) -> Result<(), StoreError> {
        for (i, c) in bytes.chunks(VALUE_LEN).enumerate() {
            self.put(key(entity, field, i as u64), Value::from_bytes(c)?)?;
        }
        for i in chunks_for(bytes.len())..old_chunks {
            self.del(key(entity, field, i as u64))?;
        }
        Ok(())
    }

    fn write_tags(
        &mut self,
        entity: u64,
        tags: &[String],
        old_count: usize,
    ) -> Result<(), StoreError> {
        for (i, t) in tags.iter().enumerate() {
            self.put(key(entity, F_TAG, i as u64), Value::from_text(t)?)?;
        }
        for i in tags.len()..old_count {
            self.del(key(entity, F_TAG, i as u64))?;
        }
        Ok(())
    }

    fn write_meta(&mut self, entity: u64, b: &Bookmark) -> Result<(), StoreError> {
        let mut raw = [0u8; VALUE_LEN];
        raw[0..2].copy_from_slice(&(b.url.len() as u16).to_le_bytes());
        raw[2..4].copy_from_slice(&(b.title.len() as u16).to_le_bytes());
        raw[4..6].copy_from_slice(&(b.tags.len() as u16).to_le_bytes());
        self.put(key(entity, F_META, 0), Value::from(raw))
    }

    fn normalize(url: &str, title: &str, tags: &[String]) -> Result<Bookmark, StoreError> {
        if url.len() > MAX_TEXT {
            return Err(StoreError::TextTooLong {
                field: "url",
                len: url.len(),
                max: MAX_TEXT,
            });
        }
        if title.len() > MAX_TEXT {
            return Err(StoreError::TextTooLong {
                field: "title",
                len: title.len(),
                max: MAX_TEXT,
            });
        }
        if tags.len() > MAX_TAGS {
            return Err(StoreError::TooManyTags {
                got: tags.len(),
                max: MAX_TAGS,
            });
        }
        let mut norm: Vec<String> = Vec::new();
        for t in tags {
            // The index has no collation, so tags are folded at write time
            // or `search_tag` would be case-sensitive.
            let t = t.trim().to_lowercase();
            if t.is_empty() {
                continue;
            }
            if t.len() > MAX_TAG {
                return Err(StoreError::TagTooLong {
                    tag: t,
                    max: MAX_TAG,
                });
            }
            if !norm.contains(&t) {
                norm.push(t);
            }
        }
        norm.sort();
        Ok(Bookmark {
            id: 0,
            url: url.to_string(),
            title: title.to_string(),
            tags: norm,
        })
    }

    /// Add a bookmark and return its id.
    pub fn add(&mut self, url: &str, title: &str, tags: &[String]) -> Result<u64, StoreError> {
        let mut b = Self::normalize(url, title, tags)?;
        let id = self.next_id;
        assert!(id <= MAX_ENTITY, "entity id space exhausted");
        b.id = id;
        // Payload first, meta last: a torn write then leaves an entity that
        // reads as absent rather than as a half-populated bookmark.
        self.write_chunks(id, F_URL, b.url.as_bytes(), 0)?;
        self.write_chunks(id, F_TITLE, b.title.as_bytes(), 0)?;
        self.write_tags(id, &b.tags, 0)?;
        self.write_meta(id, &b)?;
        self.next_id += 1;
        self.header_dirty = true;
        self.flush_header()?;
        Ok(id)
    }

    /// Replace a bookmark's fields wholesale.
    pub fn update(
        &mut self,
        id: u64,
        url: &str,
        title: &str,
        tags: &[String],
    ) -> Result<(), StoreError> {
        let old = self.get_bookmark(id)?.ok_or(StoreError::NotFound(id))?;
        let mut b = Self::normalize(url, title, tags)?;
        b.id = id;
        self.write_chunks(id, F_URL, b.url.as_bytes(), chunks_for(old.url.len()))?;
        self.write_chunks(id, F_TITLE, b.title.as_bytes(), chunks_for(old.title.len()))?;
        self.write_tags(id, &b.tags, old.tags.len())?;
        self.write_meta(id, &b)
    }

    pub fn set_title(&mut self, id: u64, title: &str) -> Result<(), StoreError> {
        let b = self.get_bookmark(id)?.ok_or(StoreError::NotFound(id))?;
        self.update(id, &b.url, title, &b.tags)
    }

    pub fn set_url(&mut self, id: u64, url: &str) -> Result<(), StoreError> {
        let b = self.get_bookmark(id)?.ok_or(StoreError::NotFound(id))?;
        self.update(id, url, &b.title, &b.tags)
    }

    pub fn set_tags(&mut self, id: u64, tags: &[String]) -> Result<(), StoreError> {
        let b = self.get_bookmark(id)?.ok_or(StoreError::NotFound(id))?;
        self.update(id, &b.url, &b.title, tags)
    }

    /// Delete a bookmark and every row that belongs to it. `false` if it was
    /// not there.
    ///
    /// This is N deletes, not one. Nothing in the library makes them atomic.
    pub fn remove(&mut self, id: u64) -> Result<bool, StoreError> {
        if id == 0 {
            return Ok(false);
        }
        let keys: Vec<u64> = self
            .range(entity_lo(id), entity_hi(id))?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        if keys.is_empty() {
            return Ok(false);
        }
        // Meta first, so an interrupted delete hides the bookmark instead of
        // leaving a truncated one visible.
        let mut removed = false;
        let meta = key(id, F_META, 0);
        if keys.contains(&meta) {
            removed = self.del(meta)?;
        }
        for k in keys {
            if k != meta {
                self.del(k)?;
            }
        }
        Ok(removed)
    }

    // -- reads -------------------------------------------------------------

    fn assemble(entity: u64, rows: &[Row]) -> Result<Option<Bookmark>, StoreError> {
        let meta = rows
            .iter()
            .find(|(k, _)| field_of(*k) == F_META && ord_of(*k) == 0);
        let Some((_, meta)) = meta else {
            return Ok(None);
        };
        let raw = meta.raw();
        let url_len = u16::from_le_bytes(raw[0..2].try_into().expect("fixed")) as usize;
        let title_len = u16::from_le_bytes(raw[2..4].try_into().expect("fixed")) as usize;
        let tag_count = u16::from_le_bytes(raw[4..6].try_into().expect("fixed")) as usize;

        let gather = |field: u64, len: usize| -> Result<String, StoreError> {
            let mut buf: Vec<u8> = Vec::with_capacity(len);
            let mut expect = 0u64;
            for (k, v) in rows.iter().filter(|(k, _)| field_of(*k) == field) {
                if ord_of(*k) != expect {
                    return Err(StoreError::Damaged {
                        entity,
                        what: format!("missing chunk {expect} of field {field}"),
                    });
                }
                expect += 1;
                buf.extend_from_slice(&v.raw());
            }
            if buf.len() < len {
                return Err(StoreError::Damaged {
                    entity,
                    what: format!("field {field} is {} bytes, meta says {len}", buf.len()),
                });
            }
            buf.truncate(len);
            Ok(String::from_utf8_lossy(&buf).into_owned())
        };

        let url = gather(F_URL, url_len)?;
        let title = gather(F_TITLE, title_len)?;
        let mut tags = Vec::with_capacity(tag_count);
        for (k, v) in rows.iter().filter(|(k, _)| field_of(*k) == F_TAG) {
            let _ = k;
            tags.push(v.text());
        }
        if tags.len() != tag_count {
            return Err(StoreError::Damaged {
                entity,
                what: format!("{} tag rows, meta says {tag_count}", tags.len()),
            });
        }
        Ok(Some(Bookmark {
            id: entity,
            url,
            title,
            tags,
        }))
    }

    /// Read one bookmark: a single range scan over its key block.
    pub fn get_bookmark(&mut self, id: u64) -> Result<Option<Bookmark>, StoreError> {
        if id == 0 {
            return Ok(None);
        }
        let rows = self.range(entity_lo(id), entity_hi(id))?;
        if rows.is_empty() {
            return Ok(None);
        }
        Self::assemble(id, &rows)
    }

    /// Every bookmark, in id order, from one full scan.
    pub fn list(&mut self) -> Result<Vec<Bookmark>, StoreError> {
        let rows = self.all_rows()?;
        let mut out = Vec::new();
        for (entity, group) in group_by_entity(&rows) {
            if entity == 0 {
                continue;
            }
            if let Some(b) = Self::assemble(entity, group)? {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// Exact substring search over URL, title and tags.
    ///
    /// A full scan, deliberately: the library's own index (`Db::find`) only
    /// sees inside individual 16-byte values, so it cannot match a needle
    /// that straddles two chunks of a URL. See [`Store::search_tag`] for the
    /// one field where the index is usable.
    pub fn search(&mut self, needle: &str) -> Result<Vec<Bookmark>, StoreError> {
        let n = needle.to_lowercase();
        if n.is_empty() {
            return self.list();
        }
        Ok(self
            .list()?
            .into_iter()
            .filter(|b| b.matches(&n))
            .collect())
    }

    /// Substring search over tags only, served by the library's index.
    ///
    /// Correct because a tag is stored in exactly one row, so no match can
    /// straddle a value boundary. Needles longer than a row are rejected by
    /// the library, so they fall back to a scan.
    pub fn search_tag(&mut self, needle: &str) -> Result<Vec<Bookmark>, StoreError> {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return self.list();
        }
        if n.len() > VALUE_LEN {
            // `find` refuses a needle longer than a value; a tag that long
            // cannot exist anyway.
            return Ok(Vec::new());
        }
        let hits = as_rows(self.exec_raw(Cmd::Find(n.into_bytes()))?);
        let mut ids: Vec<u64> = hits
            .iter()
            .filter(|(k, _)| field_of(*k) == F_TAG)
            .map(|(k, _)| entity_of(*k))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let mut out = Vec::new();
        for id in ids {
            if let Some(b) = self.get_bookmark(id)? {
                out.push(b);
            }
        }
        Ok(out)
    }

    pub fn count(&mut self) -> Result<u64, StoreError> {
        Ok(self.list()?.len() as u64)
    }

    /// Scan for entities whose rows do not form a bookmark — the damage a
    /// half-finished multi-row write leaves behind.
    pub fn integrity(&mut self) -> Result<Integrity, StoreError> {
        let rows = self.all_rows()?;
        let mut r = Integrity::default();
        for (entity, group) in group_by_entity(&rows) {
            if entity == 0 {
                continue;
            }
            let has_meta = group
                .iter()
                .any(|(k, _)| field_of(*k) == F_META && ord_of(*k) == 0);
            if has_meta && Self::assemble(entity, group).is_ok() {
                r.bookmarks += 1;
            } else {
                r.orphans.push(entity);
                r.stray_rows += group.len() as u64;
            }
        }
        Ok(r)
    }

    /// Delete the rows [`Store::integrity`] found orphaned.
    pub fn repair(&mut self) -> Result<u64, StoreError> {
        let orphans = self.integrity()?.orphans;
        let mut n = 0;
        for e in orphans {
            for (k, _) in self.range(entity_lo(e), entity_hi(e))? {
                if self.del(k)? {
                    n += 1;
                }
            }
        }
        Ok(n)
    }
}

fn group_by_entity(rows: &[Row]) -> Vec<(u64, &[Row])> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < rows.len() {
        let entity = entity_of(rows[start].0);
        let mut end = start;
        while end < rows.len() && entity_of(rows[end].0) == entity {
            end += 1;
        }
        out.push((entity, &rows[start..end]));
        start = end;
    }
    out
}

// ---------------------------------------------------------------------------
// Sessions: the only place a live database can exist
// ---------------------------------------------------------------------------

/// What a session did, beyond the caller's own return value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReport {
    pub stats: Stats,
    pub compacted: bool,
    pub blob_len: usize,
}

/// Run `body` against a store loaded from `blob` (or a fresh one), then hand
/// back a new blob to persist.
///
/// This is the browser cycle: read the bytes out of IndexedDB, work, write
/// the bytes back. It is also the only structure available, since the
/// database value cannot leave this function's stack frame with a name.
pub fn with_store<R>(
    blob: Option<&[u8]>,
    body: impl FnOnce(&mut Store) -> Result<R, StoreError>,
) -> Result<(R, Vec<u8>, SessionReport), StoreError> {
    with_store_capacity(blob, DEFAULT_ROWS, body)
}

/// [`with_store`] with an explicit row capacity for a *fresh* database.
///
/// Note that the capacity is not part of the snapshot: a blob written by a
/// database declared at 1,024 rows reloads at whatever capacity the loader
/// names. The caller has to remember it out of band.
pub fn with_store_capacity<R>(
    blob: Option<&[u8]>,
    rows: u64,
    body: impl FnOnce(&mut Store) -> Result<R, StoreError>,
) -> Result<(R, Vec<u8>, SessionReport), StoreError> {
    let mut db = match blob {
        None => Db::in_memory_with(rows)?,
        Some(bytes) => {
            let snap = Snapshot::from_bytes(bytes)?;
            match Db::load_with(&snap, rows) {
                // `Full` means two different things: "you hit the ceiling"
                // from a write, and "the ceiling is below the data, and here
                // is what it needs" from a load. Only the second is
                // recoverable, and this is the recovery.
                Err(DbErr::Full { capacity }) => {
                    Db::load_with(&snap, capacity.saturating_mul(2).max(rows))?
                }
                other => other?,
            }
        }
    };

    let (out, want_compact) = {
        let mut exec = |cmd: Cmd| -> Result<Reply, DbErr> {
            Ok(match cmd {
                Cmd::Get(k) => Reply::Value(db.get(k)?),
                Cmd::Range(lo, hi) => Reply::Rows(db.range(lo, hi)?),
                Cmd::All => Reply::Rows(db.all()?),
                Cmd::Find(n) => Reply::Rows(db.find(&n)?),
                Cmd::Put(k, v) => {
                    db.put(k, v)?;
                    Reply::Unit
                }
                Cmd::Delete(k) => Reply::Removed(db.remove(k)?),
                Cmd::Stats => Reply::Stats(db.stats()),
            })
        };
        let mut store = Store::attach(&mut exec)?;
        let out = body(&mut store)?;
        store.flush_header()?;
        (out, store.compact)
    };

    // Every insert, update and delete burns a slot forever; compaction is
    // the only way back, and it is a whole-database copy that replaces the
    // value, which is why it cannot live behind `Store`.
    let s = db.stats();
    let compacted = want_compact || (s.dead > s.live && s.fill() > 0.5);
    if compacted {
        db = db.compact_to_memory()?;
    }

    let bytes = db.snapshot()?.to_bytes();
    let report = SessionReport {
        stats: db.stats(),
        compacted,
        blob_len: bytes.len(),
    };
    Ok((out, bytes, report))
}

/// A read-only session: same thing, without paying for a snapshot.
pub fn read_store<R>(
    blob: &[u8],
    body: impl FnOnce(&mut Store) -> Result<R, StoreError>,
) -> Result<R, StoreError> {
    let snap = Snapshot::from_bytes(blob)?;
    let mut db = match Db::load(&snap) {
        Err(DbErr::Full { capacity }) => Db::load_with(&snap, capacity.saturating_mul(2))?,
        other => other?,
    };
    let mut exec = |cmd: Cmd| -> Result<Reply, DbErr> {
        Ok(match cmd {
            Cmd::Get(k) => Reply::Value(db.get(k)?),
            Cmd::Range(lo, hi) => Reply::Rows(db.range(lo, hi)?),
            Cmd::All => Reply::Rows(db.all()?),
            Cmd::Find(n) => Reply::Rows(db.find(&n)?),
            Cmd::Put(k, v) => {
                db.put(k, v)?;
                Reply::Unit
            }
            Cmd::Delete(k) => Reply::Removed(db.remove(k)?),
            Cmd::Stats => Reply::Stats(db.stats()),
        })
    };
    let mut store = Store::attach(&mut exec)?;
    body(&mut store)
}

/// The key of a bookmark's meta row. Exposed so a test can simulate a
/// half-finished write; a real app would never need it.
pub fn meta_key(id: u64) -> u64 {
    key(id, F_META, 0)
}

/// The key of a bookmark's `n`th URL chunk, for the same reason.
pub fn url_chunk_key(id: u64, chunk: u64) -> u64 {
    key(id, F_URL, chunk)
}
