//! A browser-style bookmark and tag index built on `dabqlite`.
//!
//! **One bookmark is one key and one value.** That sentence is the whole
//! port. Values used to be exactly [`VALUE_LEN`] = 16 bytes, so a URL was
//! spread over twenty rows, a title over another twenty, each tag over one
//! more, and a hand-packed "meta" row carried the lengths needed to put
//! them back together. The key had to carry structure — a 40-bit entity,
//! an 8-bit field, a 16-bit chunk ordinal — and this crate carried ~280
//! lines of packing, chunking, reassembly, length side-car,
//! `integrity()` and `repair()` to make that survivable.
//!
//! `Value` now holds any byte string up to [`MAX_VALUE_LEN`] = 2 KiB,
//! stored across several row slots in ONE commit and handed back byte for
//! byte. So a bookmark is:
//!
//! ```text
//! key   = the bookmark id
//! value = url \x1f title \x1f \x1e tag \x1e tag \x1e \x1f added \x1f visits
//! ```
//!
//! and there is nothing to reassemble, nothing to length-check, and no
//! half-written bookmark to repair. The record is plain UTF-8 on purpose:
//! [`Db::find`] searches the raw bytes of a value, so the text a user
//! searches for is literally in there.
//!
//! ## What the library still makes us do
//!
//! - **Separators are our idea.** `dabqlite` has no notion of a field, so
//!   "the tag is exactly `rust`" is spelled `find("\x1erust\x1e")` — we
//!   encode delimiters into the value and hope the user does not type one.
//! - **A needle may not exceed [`VALUE_LEN`] = 16 bytes** ([`Db::find_page`]
//!   refuses longer ones outright) even though a value may be 2048. So
//!   the index can serve `find("kernel")` and cannot serve
//!   `find("kernel-development")`.
//! - **`find` is byte-exact**, so it is case-sensitive. Case-insensitive
//!   search is a full scan in Rust.
//! - **Ordering, limits and compound predicates are ours.** See
//!   [`Store::query`], which is a scan and a sort, every time.
//!
//! Each of those has a test named after it in `tests/bookmarks.rs`.

use dabqlite::{
    Db, Error as DbErr, FindCursor, MemoryStorage, Op, Snapshot, Stats, Storage, Value,
    MAX_COMMIT_ROWS, MAX_VALUE_LEN, VALUE_LEN,
};

#[cfg(unix)]
use dabqlite::PosixStorage;

// ---------------------------------------------------------------------------
// The record format
// ---------------------------------------------------------------------------

/// Field separator inside a record. ASCII US, which cannot appear in a
/// URL and has no business in a title.
pub const FS: u8 = 0x1f;
/// Tag separator, and the delimiter that makes exact tag match expressible
/// as a substring. ASCII RS.
pub const TS: u8 = 0x1e;

/// The store header lives at key 0, so bookmark ids start at 1.
const HEADER_KEY: u64 = 0;
const HEADER_MAGIC: &str = "BMK3";

/// The longest record this crate will write.
///
/// [`MAX_VALUE_LEN`] is 2048 and [`MAX_COMMIT_ROWS`] is 128 ROW SLOTS — and
/// 2048/16 is exactly 128. A maximum-length value therefore consumes an
/// entire commit, leaving no room for the id-counter row that has to land
/// with it. One slot is reserved for that; see
/// `a_full_length_value_leaves_no_room_for_anything_else_in_its_commit`.
pub const MAX_RECORD: usize = MAX_VALUE_LEN - VALUE_LEN;

/// Field ceilings. Generous, because the binding constraint is
/// [`MAX_RECORD`] on the encoded whole, which is checked separately.
pub const MAX_URL: usize = 1024;
pub const MAX_TITLE: usize = 512;
pub const MAX_TAG: usize = 64;
pub const MAX_TAGS: usize = 16;

/// The longest needle [`Db::find`] accepts — the same ceiling a value
/// has, so any stored text can be searched for in full.
///
/// It used to be ONE ROW: sixteen bytes against a value of two thousand,
/// which meant `search("developer.mozilla.org")` was refused by a store
/// holding the whole URL, and every tag longer than fourteen characters
/// fell back to a scan. Both fallbacks are gone, and so is the reason
/// this constant was interesting.
pub const MAX_NEEDLE: usize = MAX_VALUE_LEN;

/// The longest tag whose EXACT match the index can serve. A tag is capped
/// at [`MAX_TAG`] long before it could reach this, so every tag is
/// index-served.
pub const MAX_INDEXED_TAG: usize = MAX_NEEDLE - 2;

/// Row slots a record of this length occupies. Every value costs at least
/// one slot, including an empty one.
pub fn slots_for(record_len: usize) -> usize {
    record_len.div_ceil(VALUE_LEN).max(1)
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
    TagTooLong {
        tag: String,
        max: usize,
    },
    TooManyTags {
        got: usize,
        max: usize,
    },
    TextTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// The encoded record does not fit one value.
    RecordTooLong {
        len: usize,
        max: usize,
    },
    /// A separator byte appeared in user text. The record format is ours,
    /// so guarding it is ours too.
    SeparatorInText {
        field: &'static str,
    },
    /// The value at this key is not a bookmark record. Only reachable by
    /// writing over a key behind the store's back, which the tests do.
    Malformed {
        id: u64,
    },
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
            StoreError::RecordTooLong { len, max } => write!(
                f,
                "this bookmark encodes to {len} bytes; one value holds {max}"
            ),
            StoreError::SeparatorInText { field } => {
                write!(f, "{field} contains a record separator byte")
            }
            StoreError::Malformed { id } => write!(f, "the value at key {id} is not a bookmark"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Db(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// A bookmark. One row, one value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bookmark {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Seconds since the epoch, as supplied by the caller.
    pub added: u64,
    /// Visit counter. A `u32` now: it shares a value with everything else
    /// instead of a hand-packed 16-byte slot, so its width is a choice.
    pub visits: u32,
}

impl Bookmark {
    /// Case-insensitive substring match over url, title and tags.
    pub fn matches(&self, lowercase_needle: &str) -> bool {
        self.url.to_lowercase().contains(lowercase_needle)
            || self.title.to_lowercase().contains(lowercase_needle)
            || self.tags.iter().any(|t| t.contains(lowercase_needle))
    }

    /// Byte-exact substring match over url, title and tags — the question
    /// [`Db::find`] answers, minus the timestamp and counter that also
    /// live in the value.
    pub fn matches_exact(&self, needle: &str) -> bool {
        self.url.contains(needle)
            || self.title.contains(needle)
            || self.tags.iter().any(|t| t.contains(needle))
    }

    /// The bytes this bookmark is stored as.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.url.len() + self.title.len() + 64);
        out.extend_from_slice(self.url.as_bytes());
        out.push(FS);
        out.extend_from_slice(self.title.as_bytes());
        out.push(FS);
        if !self.tags.is_empty() {
            // Delimited on BOTH sides so that `\x1erust\x1e` is an exact
            // tag match and not a prefix match. This is what passes for a
            // field in a store that has no fields.
            for t in &self.tags {
                out.push(TS);
                out.extend_from_slice(t.as_bytes());
            }
            out.push(TS);
        }
        out.push(FS);
        out.extend_from_slice(self.added.to_string().as_bytes());
        out.push(FS);
        out.extend_from_slice(self.visits.to_string().as_bytes());
        out
    }

    /// The inverse. `None` when the bytes are not a record this crate
    /// wrote.
    pub fn decode(id: u64, bytes: &[u8]) -> Option<Bookmark> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut parts = text.split(FS as char);
        let url = parts.next()?.to_string();
        let title = parts.next()?.to_string();
        let tag_section = parts.next()?;
        let added = parts.next()?.parse().ok()?;
        let visits = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        let tags: Vec<String> = tag_section
            .split(TS as char)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        Some(Bookmark {
            id,
            url,
            title,
            tags,
            added,
            visits,
        })
    }
}

/// A bookmark on its way in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewBookmark {
    pub url: String,
    pub title: String,
    pub tags: Vec<String>,
    pub added: u64,
}

impl NewBookmark {
    pub fn new(url: &str, title: &str, tags: &[&str], added: u64) -> Self {
        NewBookmark {
            url: url.to_string(),
            title: title.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            added,
        }
    }
}

/// The query a bookmark manager actually wants, and which the library
/// still cannot take: substring, tags, and a date range, ordered, limited.
///
/// Every field here is evaluated by [`Store::query`] in Rust, over a full
/// scan. Nothing in `dabqlite` composes predicates, and its one index
/// answers exactly one question.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    /// Case-insensitive substring over url, title and tags.
    pub text: Option<String>,
    /// Every one of these tags must be present (AND).
    pub tags: Vec<String>,
    /// `added >= since`.
    pub since: Option<u64>,
    /// `added <= until`.
    pub until: Option<u64>,
    pub order: Order,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    Id,
    NewestFirst,
    MostVisited,
    TitleAsc,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A bookmark store over any `dabqlite` backend.
pub struct Store<S: Storage> {
    db: Db<S>,
    next_id: u64,
}

impl<S: Storage> std::fmt::Debug for Store<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("next_id", &self.next_id)
            .field("db", &self.db)
            .finish()
    }
}

/// In-memory constructors: the browser story, where persistence is one
/// opaque blob the host keeps.
impl Store<MemoryStorage> {
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::attach(Db::in_memory()?)
    }

    pub fn in_memory_with(rows: u64) -> Result<Self, StoreError> {
        Self::attach(Db::in_memory_with(rows)?)
    }

    /// Reopen from the bytes [`Store::to_blob`] produced.
    ///
    /// The capacity travels IN the blob now, so this is one call. It used
    /// to be a load, a `CapacityTooSmall`, and a second load with the
    /// number the first one reported.
    pub fn load(blob: &[u8]) -> Result<Self, StoreError> {
        Self::attach(Db::load(&Snapshot::from_bytes(blob)?)?)
    }

    /// As [`Store::load`], overriding the recorded capacity.
    pub fn load_with(blob: &[u8], rows: u64) -> Result<Self, StoreError> {
        Self::attach(Db::load_with(&Snapshot::from_bytes(blob)?, rows)?)
    }

    /// Load a DAMAGED blob in salvage mode: the bookmarks whose rows
    /// still verify are readable, the rest are quarantined, and the store
    /// is read-only until it is rebuilt.
    ///
    /// This is the rescue path for the deployment this crate is about. A
    /// bookmark store in a browser lives as one opaque blob in IndexedDB,
    /// with no directory to point a repair tool at — and for a while that
    /// meant one flipped byte cost the whole database, with an error
    /// advising a salvage mode that could not be reached from bytes.
    ///
    /// Recover with [`Store::rebuild_from_salvage`].
    pub fn salvage(blob: &[u8]) -> Result<Self, StoreError> {
        Self::attach(Db::load_salvaged(&Snapshot::from_bytes(blob)?)?)
    }

    /// Take what survived a salvage load and build a healthy store from
    /// it. The result is writable and carries no quarantine.
    pub fn rebuild_from_salvage(&mut self) -> Result<Self, StoreError> {
        Self::attach(self.db.compact_to_memory()?)
    }

    /// True when this store was salvaged and some rows could not be read.
    pub fn is_degraded(&self) -> bool {
        self.db.is_degraded()
    }

    /// The database's bytes: hand them to IndexedDB, a file, a POST.
    pub fn to_blob(&mut self) -> Result<Vec<u8>, StoreError> {
        Ok(self.db.snapshot()?.to_bytes())
    }

    /// Rebuild in a fresh arena, dropping dead slots.
    pub fn compact(&mut self) -> Result<(), StoreError> {
        self.db = self.db.compact_to_memory()?;
        Ok(())
    }
}

/// File-backed constructors: real fsyncs, real durability, one writer.
#[cfg(unix)]
impl Store<PosixStorage> {
    /// The capacity is remembered by the database, so reopening does not
    /// have to be told it again.
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        Self::attach(Db::open(dir)?)
    }

    pub fn open_with(dir: impl AsRef<std::path::Path>, rows: u64) -> Result<Self, StoreError> {
        Self::attach(Db::open_with(dir, rows)?)
    }

    /// In-place compaction, which the library does crash-safely and this
    /// crate therefore does not have to think about. `&mut self` now, so
    /// it no longer consumes and rebuilds the `Store`.
    pub fn compact(&mut self) -> Result<(), StoreError> {
        Ok(self.db.compact()?)
    }
}

impl<S: Storage> Store<S> {
    fn attach(db: Db<S>) -> Result<Self, StoreError> {
        let mut s = Store { db, next_id: 1 };
        s.next_id = s.read_next_id()?;
        Ok(s)
    }

    /// The database underneath. Public because a real app on this API ends
    /// up needing it (paging, `find`, `stats`), and because the tests use
    /// it to fake damage.
    pub fn db(&mut self) -> &mut Db<S> {
        &mut self.db
    }

    pub fn stats(&self) -> Stats {
        self.db.stats()
    }

    /// What recovery found when this database was opened.
    pub fn recovery_report(&self) -> dabqlite::RecoveryReport {
        self.db.recovery_report()
    }

    /// The id the next bookmark will get.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    // -- header ------------------------------------------------------------

    fn read_next_id(&mut self) -> Result<u64, StoreError> {
        if let Some(v) = self.db.get(HEADER_KEY)? {
            if let Some(rest) = v.text().strip_prefix(HEADER_MAGIC) {
                if let Ok(n) = rest.trim_start_matches(FS as char).parse::<u64>() {
                    return Ok(n);
                }
            }
        }
        // No header: fall back to a full scan for the highest id. There is
        // still no "max key", no descending range and no "last row", so
        // this is O(rows) — see `there_is_no_way_to_ask_for_the_largest_key`.
        Ok(self.db.all()?.iter().map(|(k, _)| *k).max().unwrap_or(0) + 1)
    }

    fn header_op(next_id: u64) -> Op {
        Op::put(
            HEADER_KEY,
            Value::from_text(&format!("{HEADER_MAGIC}\x1f{next_id}"))
                .expect("the header is a dozen bytes"),
        )
    }

    // -- validation --------------------------------------------------------

    fn normalize(
        id: u64,
        url: &str,
        title: &str,
        tags: &[String],
        added: u64,
        visits: u32,
    ) -> Result<Bookmark, StoreError> {
        for (field, s, max) in [("url", url, MAX_URL), ("title", title, MAX_TITLE)] {
            if s.len() > max {
                return Err(StoreError::TextTooLong {
                    field,
                    len: s.len(),
                    max,
                });
            }
            if s.bytes().any(|b| b == FS || b == TS) {
                return Err(StoreError::SeparatorInText { field });
            }
        }
        let mut norm: Vec<String> = Vec::new();
        for t in tags {
            // `find` has no collation, so tags are folded at write time or
            // exact tag lookup would be case-sensitive.
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
            if t.bytes().any(|b| b == FS || b == TS) {
                return Err(StoreError::SeparatorInText { field: "tag" });
            }
            if !norm.contains(&t) {
                norm.push(t);
            }
        }
        norm.sort();
        if norm.len() > MAX_TAGS {
            return Err(StoreError::TooManyTags {
                got: norm.len(),
                max: MAX_TAGS,
            });
        }
        let b = Bookmark {
            id,
            url: url.to_string(),
            title: title.to_string(),
            tags: norm,
            added,
            visits,
        };
        let len = b.encode().len();
        if len > MAX_RECORD {
            return Err(StoreError::RecordTooLong {
                len,
                max: MAX_RECORD,
            });
        }
        Ok(b)
    }

    fn put_op(b: &Bookmark) -> Op {
        Op::put(
            b.id,
            Value::from_vec(b.encode()).expect("normalize enforced MAX_RECORD"),
        )
    }

    // -- writes ------------------------------------------------------------

    /// Add one bookmark: one key, one value, one commit.
    pub fn add(
        &mut self,
        url: &str,
        title: &str,
        tags: &[String],
        added: u64,
    ) -> Result<u64, StoreError> {
        let ids = self.add_many(&[NewBookmark {
            url: url.into(),
            title: title.into(),
            tags: tags.to_vec(),
            added,
        }])?;
        Ok(ids[0])
    }

    /// Add a set of bookmarks as ONE commit — an import that either
    /// happens or does not.
    ///
    /// Bounded by [`MAX_COMMIT_ROWS`] = 128 ROW SLOTS, and a typical bookmark is
    /// nine of them, so about thirteen bookmarks plus the id counter. Past
    /// that the library refuses with [`dabqlite::Error::BatchTooLong`],
    /// which says how many slots were needed and how many there are — the
    /// message that used to read "database is full at capacity 64".
    pub fn add_many(&mut self, items: &[NewBookmark]) -> Result<Vec<u64>, StoreError> {
        let mut ops = Vec::with_capacity(items.len() + 1);
        let mut ids = Vec::with_capacity(items.len());
        let mut next = self.next_id;
        for item in items {
            let b = Self::normalize(next, &item.url, &item.title, &item.tags, item.added, 0)?;
            ops.push(Self::put_op(&b));
            ids.push(next);
            next += 1;
        }
        ops.push(Self::header_op(next));
        self.db.batch(&ops)?;
        self.next_id = next;
        Ok(ids)
    }

    /// Add many bookmarks, packing as many as fit into each commit.
    ///
    /// The set is not atomic — a crash halfway leaves the first part — but
    /// each commit is, and there are ~13x fewer of them than bookmarks.
    /// On durable storage that is the whole difference between an import
    /// and a coffee break, because a commit is two fsyncs regardless of
    /// how much it carries.
    pub fn import(&mut self, items: &[NewBookmark]) -> Result<Vec<u64>, StoreError> {
        let mut ids = Vec::with_capacity(items.len());
        let mut ops: Vec<Op> = Vec::new();
        // One slot is reserved for the header row that closes every commit.
        let mut staged = 1usize;
        let mut next = self.next_id;
        for item in items {
            let b = Self::normalize(next, &item.url, &item.title, &item.tags, item.added, 0)?;
            let cost = slots_for(b.encode().len());
            if !ops.is_empty()
                && (staged + cost > MAX_COMMIT_ROWS || ops.len() + 1 >= MAX_COMMIT_ROWS)
            {
                ops.push(Self::header_op(next));
                self.db.batch(&ops)?;
                self.next_id = next;
                ops.clear();
                staged = 1;
            }
            staged += cost;
            ops.push(Self::put_op(&b));
            ids.push(next);
            next += 1;
        }
        if !ops.is_empty() {
            ops.push(Self::header_op(next));
            self.db.batch(&ops)?;
            self.next_id = next;
        }
        Ok(ids)
    }

    /// Replace a bookmark's fields wholesale. One key, one value, one op.
    ///
    /// There are no stale chunks to retire any more: the old value is
    /// superseded by the new one whatever their relative lengths.
    pub fn update(
        &mut self,
        id: u64,
        url: &str,
        title: &str,
        tags: &[String],
    ) -> Result<(), StoreError> {
        let old = self.get_bookmark(id)?.ok_or(StoreError::NotFound(id))?;
        let b = Self::normalize(id, url, title, tags, old.added, old.visits)?;
        self.db
            .put(id, Value::from_vec(b.encode()).expect("checked"))?;
        Ok(())
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

    /// Record a visit.
    ///
    /// The one place the new shape costs more than the old one: the
    /// counter lives inside the bookmark's value, so bumping it rewrites
    /// every slot of that value — nine of them for a typical bookmark,
    /// where the old design's stand-alone meta row cost exactly one. See
    /// `bumping_a_counter_rewrites_the_whole_bookmark`.
    pub fn visit(&mut self, id: u64) -> Result<u32, StoreError> {
        let mut b = self.get_bookmark(id)?.ok_or(StoreError::NotFound(id))?;
        b.visits = b.visits.saturating_add(1);
        self.db
            .put(id, Value::from_vec(b.encode()).expect("checked"))?;
        Ok(b.visits)
    }

    /// Delete a bookmark.
    pub fn remove(&mut self, id: u64) -> Result<bool, StoreError> {
        Ok(self.remove_many(&[id])? == 1)
    }

    /// Delete several bookmarks as ONE commit. One op each now, so a
    /// "select all, delete" of up to [`MAX_COMMIT_ROWS`] bookmarks cannot
    /// half-happen. Returns how many were actually there.
    pub fn remove_many(&mut self, ids: &[u64]) -> Result<usize, StoreError> {
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut ops = Vec::with_capacity(ids.len());
        for id in ids {
            if id == HEADER_KEY {
                continue;
            }
            // `Op::Delete` inside a batch is strict — it refuses the whole
            // batch for an absent id — so filter first.
            if self.db.contains(id)? {
                ops.push(Op::delete(id));
            }
        }
        if ops.is_empty() {
            return Ok(0);
        }
        let hit = ops.len();
        self.db.batch(&ops)?;
        Ok(hit)
    }

    /// Rename a tag everywhere it appears.
    ///
    /// Atomic per COMMIT, not across the collection: the rename is packed
    /// into commits of [`MAX_COMMIT_ROWS`] slots, so a crash can still leave it
    /// half-applied — just in far fewer places than when a bookmark was
    /// fifty rows. Returns how many bookmarks it touched.
    pub fn retag(&mut self, from: &str, to: &str) -> Result<usize, StoreError> {
        let from = from.trim().to_lowercase();
        let to = to.trim().to_lowercase();
        let victims: Vec<Bookmark> = self.by_tag(&from)?;
        let mut ops: Vec<Op> = Vec::new();
        let mut staged = 0usize;
        for b in &victims {
            let mut tags: Vec<String> = b
                .tags
                .iter()
                .map(|t| if *t == from { to.clone() } else { t.clone() })
                .collect();
            tags.sort();
            tags.dedup();
            let next = Self::normalize(b.id, &b.url, &b.title, &tags, b.added, b.visits)?;
            let cost = slots_for(next.encode().len());
            if !ops.is_empty() && (staged + cost > MAX_COMMIT_ROWS || ops.len() >= MAX_COMMIT_ROWS)
            {
                self.db.batch(&ops)?;
                ops.clear();
                staged = 0;
            }
            staged += cost;
            ops.push(Self::put_op(&next));
        }
        if !ops.is_empty() {
            self.db.batch(&ops)?;
        }
        Ok(victims.len())
    }

    // -- reads -------------------------------------------------------------

    /// Read one bookmark: one `Db::get`.
    pub fn get_bookmark(&mut self, id: u64) -> Result<Option<Bookmark>, StoreError> {
        if id == HEADER_KEY {
            return Ok(None);
        }
        match self.db.get(id)? {
            None => Ok(None),
            Some(v) => Bookmark::decode(id, v.as_bytes())
                .map(Some)
                .ok_or(StoreError::Malformed { id }),
        }
    }

    /// Every bookmark, in id order, from one full scan.
    pub fn list(&mut self) -> Result<Vec<Bookmark>, StoreError> {
        let rows = self.db.all()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, v) in rows {
            if id == HEADER_KEY {
                continue;
            }
            out.push(Bookmark::decode(id, v.as_bytes()).ok_or(StoreError::Malformed { id })?);
        }
        Ok(out)
    }

    /// A bounded page of bookmarks with id > `after`, ascending.
    ///
    /// Built on [`Db::range_page`], which pages in fixed 8-row units the
    /// caller does not choose. One row per bookmark now, so a page of 20
    /// is 3 engine calls rather than ~25.
    pub fn page(&mut self, after: u64, limit: usize) -> Result<Vec<Bookmark>, StoreError> {
        let mut out = Vec::with_capacity(limit.min(64));
        let mut cursor = after.saturating_add(1);
        while out.len() < limit {
            let (rows, next) = self.db.range_page(cursor, u64::MAX)?;
            for (id, v) in rows {
                if id == HEADER_KEY {
                    continue;
                }
                out.push(Bookmark::decode(id, v.as_bytes()).ok_or(StoreError::Malformed { id })?);
                if out.len() == limit {
                    return Ok(out);
                }
            }
            match next {
                Some(n) => cursor = n,
                None => break,
            }
        }
        Ok(out)
    }

    /// The `limit` most recently ADDED bookmarks, newest first.
    ///
    /// Ids ascend with insertion here, so this is `Db::last` and it costs
    /// `limit` rows. It used to be [`Store::query`] with
    /// [`Order::NewestFirst`] — a full scan and a sort, 312 ms at 50,000
    /// bookmarks for an answer twenty rows wide — because the only scan
    /// on offer started at the lowest id.
    ///
    /// The header row lives at id 0, below every bookmark, so it only
    /// turns up once the store has fewer bookmarks than `limit`; asking
    /// for one extra and skipping it covers that case.
    pub fn newest(&mut self, limit: usize) -> Result<Vec<Bookmark>, StoreError> {
        let mut out = Vec::with_capacity(limit.min(64));
        if limit == 0 {
            return Ok(out);
        }
        for (id, v) in self.db.last(limit + 1)? {
            if id == HEADER_KEY {
                continue;
            }
            out.push(Bookmark::decode(id, v.as_bytes()).ok_or(StoreError::Malformed { id })?);
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }

    /// Case-insensitive substring search over url, title and tags.
    ///
    /// A full scan in Rust, because [`Db::find`] is byte-exact and refuses
    /// needles longer than one row slot. This is the search a bookmark
    /// manager ships.
    pub fn search(&mut self, needle: &str) -> Result<Vec<Bookmark>, StoreError> {
        let n = needle.to_lowercase();
        if n.is_empty() {
            return self.list();
        }
        Ok(self.list()?.into_iter().filter(|b| b.matches(&n)).collect())
    }

    /// One page of an index-backed search, newest-written first, plus
    /// where to continue from.
    ///
    /// This is [`Db::find_page`] with the store's own verification on top,
    /// and it is what a search box would call. Three things about it are
    /// worth knowing before you use it, and each has a test:
    ///
    /// - it is **byte-exact**, so `"Rust"` does not find `"rust"`;
    /// - the needle may be as long as a whole record ([`MAX_NEEDLE`]);
    /// - "newest" means most recently WRITTEN, not most recently added —
    ///   editing a bookmark moves it to the front.
    pub fn search_page(
        &mut self,
        needle: &str,
        after: Option<FindCursor>,
        limit: usize,
    ) -> Result<(Vec<Bookmark>, Option<FindCursor>), StoreError> {
        let mut out = Vec::with_capacity(limit.min(64));
        let mut cursor = after;
        loop {
            let (rows, next) = self.db.find_page(needle.as_bytes(), cursor)?;
            for (id, v) in rows {
                if id == HEADER_KEY {
                    continue;
                }
                let b = Bookmark::decode(id, v.as_bytes()).ok_or(StoreError::Malformed { id })?;
                // `find` matched the WHOLE value, which also holds the
                // timestamp and the visit counter. A search box wants
                // neither, so the store re-checks the fields it meant.
                if b.matches_exact(needle) {
                    out.push(b);
                }
            }
            cursor = next;
            if cursor.is_none() || out.len() >= limit {
                out.truncate(limit);
                return Ok((out, cursor));
            }
        }
    }

    /// Every bookmark whose url, title or tags contain `needle` exactly,
    /// via the index. Newest-written first.
    pub fn find_exact(&mut self, needle: &str) -> Result<Vec<Bookmark>, StoreError> {
        let (hits, _) = self.search_page(needle, None, usize::MAX)?;
        Ok(hits)
    }

    /// Substring search over TAGS only.
    ///
    /// Index-backed when the needle plus its leading delimiter fits a row;
    /// a scan otherwise.
    pub fn search_tag(&mut self, needle: &str) -> Result<Vec<Bookmark>, StoreError> {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return self.list();
        }
        // A tag always begins right after a `\x1e`, so anchoring the needle
        // with one turns "contains" into "a tag starts with". No length
        // fallback any more: the needle ceiling is a record, not a row.
        let mut probe = vec![TS];
        probe.extend_from_slice(n.as_bytes());
        let hits = self.db.find(&probe)?;
        self.gather(hits, |b| b.tags.iter().any(|t| t.contains(&n)))
    }

    /// EXACT tag match, served by the substring index because the record
    /// format delimits tags on both sides.
    ///
    /// `find("\x1erust\x1e")` cannot match `rustaceans`. The delimiters
    /// are still how a TAG is distinguished from the rest of the record —
    /// the library's `find_exact` anchors to the whole value, and a tag is
    /// a field inside one — but the length fallback is gone: a needle may
    /// now be as long as the record it is looking inside.
    pub fn by_tag(&mut self, tag: &str) -> Result<Vec<Bookmark>, StoreError> {
        let t = tag.trim().to_lowercase();
        if t.is_empty() {
            return Ok(Vec::new());
        }
        let mut probe = Vec::with_capacity(t.len() + 2);
        probe.push(TS);
        probe.extend_from_slice(t.as_bytes());
        probe.push(TS);
        let hits = self.db.find(&probe)?;
        self.gather(hits, |b| b.tags.contains(&t))
    }

    /// Decode the rows a `find` returned, keep the ones that really match,
    /// and put them back in id order.
    fn gather(
        &mut self,
        hits: Vec<(u64, Value)>,
        keep: impl Fn(&Bookmark) -> bool,
    ) -> Result<Vec<Bookmark>, StoreError> {
        let mut out = Vec::with_capacity(hits.len());
        for (id, v) in hits {
            if id == HEADER_KEY {
                continue;
            }
            let b = Bookmark::decode(id, v.as_bytes()).ok_or(StoreError::Malformed { id })?;
            if keep(&b) {
                out.push(b);
            }
        }
        out.sort_by_key(|b| b.id);
        Ok(out)
    }

    /// The compound query: substring AND tags AND a date range, ordered,
    /// limited.
    ///
    /// Still a full scan plus Rust, and still the honest answer: the
    /// library has one index, over the bytes of a value, and no way to
    /// combine it with anything, order by anything, or stop early on
    /// anything but its own newest-first chain.
    pub fn query(&mut self, q: &Query) -> Result<Vec<Bookmark>, StoreError> {
        let needle = q.text.as_ref().map(|t| t.to_lowercase());
        let want: Vec<String> = q.tags.iter().map(|t| t.trim().to_lowercase()).collect();
        let mut out: Vec<Bookmark> = self
            .list()?
            .into_iter()
            .filter(|b| needle.as_ref().is_none_or(|n| b.matches(n)))
            .filter(|b| want.iter().all(|t| b.tags.contains(t)))
            .filter(|b| q.since.is_none_or(|s| b.added >= s))
            .filter(|b| q.until.is_none_or(|u| b.added <= u))
            .collect();
        match q.order {
            Order::Id => {}
            Order::NewestFirst => out.sort_by(|a, b| b.added.cmp(&a.added).then(b.id.cmp(&a.id))),
            Order::MostVisited => out.sort_by(|a, b| b.visits.cmp(&a.visits).then(a.id.cmp(&b.id))),
            Order::TitleAsc => {
                out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            }
        }
        if let Some(n) = q.limit {
            out.truncate(n);
        }
        Ok(out)
    }

    /// How many bookmarks. One row per bookmark, so this is the library's
    /// own live count minus the header — no scan.
    pub fn count(&mut self) -> Result<u64, StoreError> {
        Ok(self.db.len().saturating_sub(1))
    }
}
