//! The trigram index — v1's "one hard one" (docs/DESIGN.md §4.6, §9
//! step 6): substring search over the fixed-width `value` field.
//!
//! ## Why trigram won the open decision (§10: "vector or trigram")
//!
//! The design demands every index be "a small isolated component with a
//! free oracle". Trigram's oracle — naive substring match — is EXACT:
//! every test can assert result equality, always. HNSW is approximate by
//! construction; its oracle can only bound recall statistically, which
//! is incompatible with the equality bar every other component here is
//! held to. Vector search remains first-class via `ExternalRef` (§4.5).
//!
//! ## Shape (the house pattern: fixed arenas, derived state, free oracle)
//!
//! - **Byte trigrams**: every 3 consecutive bytes of the value. A
//!   `VALUE_LEN`-byte value has exactly `VALUE_LEN - 2` of them, so the
//!   postings pool is EXACTLY `rows * TRIGRAMS_PER_ROW` slots, addressed
//!   as `row * TRIGRAMS_PER_ROW + k` — the pool bound is arithmetic, not
//!   an estimate, and there is no allocation bookkeeping to get wrong.
//! - Open-addressing trigram → chain-head table, sized for load <= 0.5
//!   (distinct trigrams <= min(rows * TRIGRAMS_PER_ROW, 2^24)), with the
//!   same exact `!=` probe-termination guard as the primary-key index.
//! - **Candidates are verified**: the index only accelerates; every
//!   returned row is checked against the actual value bytes, so results
//!   are exact and oracle-equal BY CONSTRUCTION, and a needle shorter
//!   than 3 bytes simply scans (bounded by rows, still exact).
//! - Bounded paging (§4.5): each page walks the candidate chain keeping
//!   the `page` smallest matches above a cursor — fixed memory, any
//!   result size.
//!
//! Like the btree, this is in-memory DERIVED state: rebuilt from
//! committed rows at every recovery, updated only at the commit point,
//! so it inherits the engine's crash guarantees and is re-verified
//! against reality on every open.

use alloc::vec;
use alloc::vec::Vec;

use crate::layout::VALUE_LEN;

/// Posting slots reserved per ROW.
///
/// A value of `VALUE_LEN` bytes has `VALUE_LEN - 2` trigram windows, so
/// two slots per row go unused for a value that fits in one slot. They are
/// reserved anyway because a value may SPAN rows: the trigram starting at
/// value offset `o` lives in the slot `head * TRIGRAMS_PER_ROW + o`, and a
/// window starting in the last two bytes of a slot runs into the next one.
/// Reserving `VALUE_LEN` slots per row keeps that mapping a bijection for
/// values of any length, which is what makes the pool bound arithmetic
/// rather than an estimate.
pub const TRIGRAMS_PER_ROW: usize = VALUE_LEN;

const NIL: u32 = u32::MAX;

/// Where a paged substring search left off.
///
/// Opaque to callers: `row` is the last row returned and `slot` the chain
/// position it came from. Carrying the chain position is what makes
/// paging linear rather than quadratic; carrying the row as well means a
/// cursor stays usable when the search switches between the chain and the
/// scan path between pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindCursor {
    pub row: u64,
    slot: u32,
}

impl FindCursor {
    /// A cursor that resumes strictly below `row`, with no chain position.
    /// Used when a caller reconstructs one from a row alone.
    pub fn below(row: u64) -> Self {
        FindCursor { row, slot: NIL }
    }
}
/// Table entry sentinel: no trigram ever hashes to this packed form
/// because trigrams are 24-bit and the tag bit marks occupancy.
const EMPTY: u64 = 0;

/// A trigram as a 24-bit integer (big-endian byte order within the key,
/// so byte order is part of the key and "abc" != "cba").
fn tri_key(window: &[u8]) -> u32 {
    ((window[0] as u32) << 16) | ((window[1] as u32) << 8) | (window[2] as u32)
}

pub struct TrigramIndex {
    /// Open addressing: packed `(1 << 63) | (tri << 32) | (head + 1)`;
    /// EMPTY (0) = free slot. The tag bit keeps trigram 0x000000 with
    /// head 0 distinguishable from a free slot.
    table: Vec<u64>,
    /// Posting slot `row * TRIGRAMS_PER_ROW + k`: the next posting in
    /// this trigram's chain (a slot index), or NIL at the end. The row a
    /// posting refers to is its own slot index / TRIGRAMS_PER_ROW — no
    /// stored row id, nothing to corrupt.
    next: Vec<u32>,
    /// Rows currently indexed; postings for rows >= len are dead.
    len: u64,
    table_addr: usize,
    next_addr: usize,
    /// Postings walked to CHOOSE a chain. See `peek_steps`.
    peeks: core::cell::Cell<u64>,
}

impl TrigramIndex {
    /// One allocation pair at init (docs/DESIGN.md §4.2), sized from the
    /// declared capacity.
    pub fn new(rows: u64) -> Self {
        let postings = (rows as usize)
            .checked_mul(TRIGRAMS_PER_ROW)
            .expect("rows capacity overflows trigram pool");
        // Load factor <= 0.5 over the worst-case DISTINCT trigram count,
        // which is capped by the 24-bit key space itself.
        let distinct_max = postings.min(1 << 24);
        let table_len = distinct_max
            .checked_mul(2)
            .and_then(|n| n.checked_next_power_of_two())
            .expect("rows capacity overflows trigram table")
            .max(2);
        let table = vec![EMPTY; table_len];
        let next = vec![NIL; postings];
        let table_addr = table.as_ptr() as usize;
        let next_addr = next.as_ptr() as usize;
        TrigramIndex {
            table,
            next,
            len: 0,
            table_addr,
            next_addr,
            peeks: core::cell::Cell::new(0),
        }
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(
            self.table.as_ptr() as usize,
            self.table_addr,
            "trigram table moved: allocation after init is forbidden"
        );
        debug_assert_eq!(
            self.next.as_ptr() as usize,
            self.next_addr,
            "trigram pool moved: allocation after init is forbidden"
        );
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn slot_of(&self, tri: u32) -> usize {
        // Same mixing family as the primary-key index; pinned by golden
        // test (mutations only degrade clustering, so values are spec).
        let mixed = (tri as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed as usize) & (self.table.len() - 1)
    }

    /// Advance to the next probe slot; exact `!=` termination guard,
    /// unreachable while the load factor holds (same reasoning as the
    /// engine's `probe_next`).
    fn probe_next(&self, slot: usize, probes: &mut usize) -> usize {
        *probes += 1;
        assert!(
            *probes != self.table.len(),
            "trigram probe loop must terminate"
        );
        (slot + 1) & (self.table.len() - 1)
    }

    fn pack(tri: u32, head: u32) -> u64 {
        (1u64 << 63) | ((tri as u64) << 32) | (head as u64 + 1)
    }

    fn unpack(entry: u64) -> (u32, u32) {
        debug_assert!(entry != EMPTY);
        (((entry >> 32) & 0x00FF_FFFF) as u32, (entry as u32) - 1)
    }

    /// Chain head for a trigram, or NIL.
    fn head(&self, tri: u32) -> u32 {
        let mut slot = self.slot_of(tri);
        let mut probes = 0usize;
        loop {
            match self.table[slot] {
                EMPTY => return NIL,
                entry => {
                    let (t, head) = Self::unpack(entry);
                    if t == tri {
                        return head;
                    }
                }
            }
            slot = self.probe_next(slot, &mut probes);
        }
    }

    fn set_head(&mut self, tri: u32, head: u32) {
        let mut slot = self.slot_of(tri);
        let mut probes = 0usize;
        loop {
            match self.table[slot] {
                EMPTY => {
                    self.table[slot] = Self::pack(tri, head);
                    return;
                }
                entry => {
                    let (t, _) = Self::unpack(entry);
                    if t == tri {
                        self.table[slot] = Self::pack(tri, head);
                        return;
                    }
                }
            }
            slot = self.probe_next(slot, &mut probes);
        }
    }

    /// Index the next row's value. Rows are append-only and must be
    /// inserted in row order (the engine's arena order) — pinned so the
    /// slot arithmetic (`row * TRIGRAMS_PER_ROW + k`) stays a bijection.
    pub fn insert(&mut self, row: u64, value: &[u8; VALUE_LEN]) {
        self.insert_value(row, 1, value);
    }

    /// Index one value, which may occupy several consecutive rows.
    ///
    /// `head_row` is the row the value starts in and `rows` how many it
    /// occupies; both are the caller's, because only the caller knows
    /// where a value ends. Trigram windows are taken over the WHOLE value,
    /// so a substring straddling a slot boundary is found like any other —
    /// which is the entire reason the postings are addressed by value
    /// offset rather than by row.
    ///
    /// Rows are append-only and must arrive in row order (the engine's
    /// arena order), so that `row * TRIGRAMS_PER_ROW + k` stays a
    /// bijection.
    pub fn insert_value(&mut self, head_row: u64, rows: u64, value: &[u8]) {
        self.assert_invariants();
        assert_eq!(head_row, self.len, "trigram index rows are append-only");
        assert!(rows >= 1, "a value occupies at least one row");
        let end = (head_row + rows) as usize;
        assert!(
            end * TRIGRAMS_PER_ROW <= self.next.len(),
            "trigram pool exhausted: capacity invariant violated"
        );
        assert!(
            value.len() <= rows as usize * VALUE_LEN,
            "value longer than the rows holding it"
        );
        let windows = value.len().saturating_sub(2);
        for o in 0..windows {
            let tri = tri_key(&value[o..o + 3]);
            // One posting per DISTINCT trigram per VALUE: a duplicate
            // window (e.g. "aaaa") must not chain the same row twice, and
            // for a multi-row value "the same row" means the same value.
            if (0..o).any(|j| tri_key(&value[j..j + 3]) == tri) {
                continue;
            }
            // The posting lives in the slot for the row the window STARTS
            // in, so a candidate always resolves back to the value's head
            // by way of that row.
            let slot = (head_row as usize) * TRIGRAMS_PER_ROW + o;
            self.next[slot] = self.head(tri);
            self.set_head(tri, slot as u32);
        }
        self.len = head_row + rows;
        self.assert_invariants();
    }

    /// Account for a row WITHOUT indexing it, keeping the append cursor
    /// (and therefore every later row's number) correct.
    ///
    /// Used only by a salvage open, where a damaged row is quarantined:
    /// it must never be searchable — nothing about it was verified — but
    /// the rows after it must keep their true row numbers, or the whole
    /// arena would shift under the index. Its `TRIGRAMS_PER_ROW` slots
    /// simply stay empty, so the `row * TRIGRAMS_PER_ROW + k` bijection
    /// is preserved exactly.
    pub fn skip_row(&mut self, row: u64) {
        self.assert_invariants();
        assert_eq!(row, self.len, "trigram index rows are append-only");
        assert!(
            ((row as usize) + 1) * TRIGRAMS_PER_ROW <= self.next.len(),
            "trigram pool exhausted: capacity invariant violated"
        );
        self.len = row + 1;
        self.assert_invariants();
    }

    /// One bounded page of matching rows, NEWEST FIRST, resuming exactly
    /// where the previous page stopped.
    ///
    /// `matches(row)` is the caller's verifier — the index never trusts
    /// itself.
    ///
    /// ## Why newest-first, and why the cursor is not just a row number
    ///
    /// Postings are prepended, so a trigram's chain visits rows in
    /// strictly descending order. Emitting pages in that order lets a
    /// continuation resume at a chain POSITION, which makes paging cost
    /// `O(page)` per page and `O(matches)` overall.
    ///
    /// Emitting them ascending cannot: the smallest matches are at the far
    /// END of the chain, so every page had to walk the whole chain and
    /// verify nearly every match again. That is quadratic in the number of
    /// matches, and it was not theoretical — a bookmark store measured a
    /// needle matching 50,000 of 50,000 rows at 31 SECONDS, against 58 ms
    /// for a brute-force scan of the same data. The index was 538x slower
    /// than no index at all, and got worse the more it matched.
    ///
    /// Descending is also the more useful order for a search box, and it
    /// is exactly as deterministic and stable as ascending was.
    ///
    /// A page walks the chain as it stood when paging began: rows written
    /// during a paged scan are prepended ahead of the cursor and are not
    /// visited. That is the same snapshot property ascending order had
    /// from the other end, and the same one a `range` scan gives.
    ///
    /// Exactness argument, unchanged: for needles >= 3 bytes, any row
    /// containing the needle contains its first trigram, so walking that
    /// one chain visits a superset of the answer; verification removes the
    /// rest. For shorter needles there is no trigram to look up: scan all
    /// rows (bounded by len; still exact).
    ///
    /// A value longer than one row is indexed over its WHOLE text, so a
    /// posting for a window at offset 40 lives in the third slot-row of
    /// that value rather than in its head. The chain therefore hands back
    /// rows that are continuations, and `head_of` maps one back to the row
    /// that owns it. Without that mapping the chain is not a superset —
    /// which is why this used to fall back to scanning every row the
    /// moment a database held a single long value, at a cost of four
    /// orders of magnitude on a selective needle.
    ///
    /// `page` receives HEAD rows; the cursor tracks the chain by posting
    /// row, which is what makes a continuation resume in the chain instead
    /// of walking it again.
    /// How many postings the candidate chain for `needle` holds, counted
    /// up to `cap`.
    ///
    /// The number a compound search needs in order to CHOOSE which of its
    /// predicates drives the walk. Every predicate's chain is a valid
    /// superset of the answer, so any of them is correct and the cheapest
    /// one is the one to walk. "Cheapest" has to be measured: a chain is
    /// keyed on a needle's FIRST THREE BYTES, so a longer needle is not a
    /// rarer trigram — "the-quick-brown-fox" and "the" walk exactly the
    /// same chain, and picking by length would have been a guess dressed
    /// up as a heuristic.
    ///
    /// Capped so that choosing costs a bounded peek rather than a full
    /// walk of every chain it declines. Past `cap` postings the chains
    /// are all expensive and the difference stops being worth measuring.
    /// A needle too short to have a trigram has no chain at all and would
    /// scan every row, so it reports the worst possible cost and is never
    /// chosen over one that has a chain.
    pub fn chain_len_capped(&self, needle: &[u8], cap: u32) -> u32 {
        if needle.len() < 3 {
            return u32::MAX;
        }
        let mut slot = self.head(tri_key(&needle[0..3]));
        let mut n = 0u32;
        while slot != NIL && n < cap {
            n += 1;
            slot = self.next[slot as usize];
        }
        self.peeks.set(self.peeks.get() + n as u64);
        n
    }

    /// Postings walked while CHOOSING which chain to search, since this
    /// index was built.
    ///
    /// Choosing is only worth anything when there is a choice. A
    /// single-needle search has exactly one chain it could walk, and a
    /// resumed page's chain was fixed by its cursor — measuring either
    /// would be work added to the hot path for an answer already known,
    /// and it would be invisible, because the results are identical
    /// whether or not the peek happened. This counter is what makes it
    /// visible.
    pub fn peek_steps(&self) -> u64 {
        self.peeks.get()
    }

    pub fn find_page<M: Fn(u64) -> bool, H: Fn(u64) -> Option<u64>>(
        &self,
        needle: &[u8],
        cursor: Option<FindCursor>,
        page: &mut [u64],
        matches: M,
        head_of: H,
    ) -> (usize, Option<FindCursor>) {
        self.assert_invariants();
        let mut found = 0usize;

        if needle.len() < 3 {
            // Descending scan from just below the cursor.
            let mut row = match cursor {
                Some(c) if c.row == 0 => return (0, None),
                Some(c) => c.row - 1,
                None if self.len == 0 => return (0, None),
                None => self.len - 1,
            };
            loop {
                if matches(row) {
                    page[found] = row;
                    found += 1;
                    if found == page.len() {
                        return (found, (row > 0).then_some(FindCursor { row, slot: NIL }));
                    }
                }
                if row == 0 {
                    return (found, None);
                }
                row -= 1;
            }
        }

        let tri = tri_key(&needle[0..3]);
        // Resume where the last page stopped, or start at the chain head.
        // A resume point is a slot the previous page already returned, so
        // the walk continues from the slot AFTER it.
        let mut slot = match cursor {
            Some(c) if c.slot != NIL => self.next[c.slot as usize],
            Some(_) => self.head(tri),
            None => self.head(tri),
        };
        let below = cursor.map(|c| c.row);
        let mut steps = 0u64;
        let mut previous: Option<u64> = None;
        while slot != NIL {
            assert!(
                steps <= self.len * TRIGRAMS_PER_ROW as u64,
                "trigram chain cycle"
            );
            let row = (slot as usize / TRIGRAMS_PER_ROW) as u64;
            // Postings are prepended in row order and deduplicated per
            // value, so a chain visits rows STRICTLY descending. Checking
            // it here is what makes the walk safe to stop early: a page
            // that fills after four steps never reaches the step bound, so
            // without this a corrupted chain would quietly return the same
            // row four times instead of saying the chain is broken.
            if let Some(prev) = previous {
                assert!(
                    row < prev,
                    "trigram chain cycle: row {row} follows {prev}, but a chain descends"
                );
            }
            previous = Some(row);
            // A cursor whose slot was lost (the caller crossed between the
            // chain and the scan path) still bounds the walk by row.
            let past = below.is_some_and(|b| row >= b);
            // The posting names the row its window STARTS in, which for a
            // value spanning several rows is a continuation. Resolve it to
            // the row that owns the value before asking whether the value
            // matches: one posting per distinct trigram per VALUE means a
            // chain still reaches each head at most once.
            if !past {
                if let Some(head) = head_of(row) {
                    if matches(head) {
                        page[found] = head;
                        found += 1;
                        if found == page.len() {
                            return (found, Some(FindCursor { row, slot }));
                        }
                    }
                }
            }
            slot = self.next[slot as usize];
            steps += 1;
        }
        (found, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(pattern: &[u8]) -> [u8; VALUE_LEN] {
        let mut v = [0u8; VALUE_LEN];
        v[..pattern.len().min(VALUE_LEN)].copy_from_slice(&pattern[..pattern.len().min(VALUE_LEN)]);
        v
    }

    fn contains(hay: &[u8; VALUE_LEN], needle: &[u8]) -> bool {
        needle.is_empty() || hay.windows(needle.len().max(1)).any(|w| w == needle)
    }

    /// Every match, in ascending row order — pages arrive newest-first, so
    /// the oracle comparison reverses them at the end.
    fn find_all(t: &TrigramIndex, values: &[[u8; VALUE_LEN]], needle: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages <= 1 + values.len(), "paging did not terminate");
            let mut page = [0u64; 4];
            let (n, next) = t.find_page(
                needle,
                cursor,
                &mut page,
                |row| contains(&values[row as usize], needle),
                // One row per value here, so a posting row IS its head.
                Some,
            );
            // Pages descend, and never repeat a row.
            for w in page[..n].windows(2) {
                assert!(w[0] > w[1], "a page was not descending: {page:?}");
            }
            out.extend_from_slice(&page[..n]);
            match next {
                Some(c) => cursor = Some(c),
                None => {
                    out.reverse();
                    return out;
                }
            }
        }
    }

    /// The chain peek is a real count of the chain it names, capped —
    /// which is what makes "walk the cheapest condition" a measurement
    /// and not a hope.
    #[test]
    fn the_chain_peek_counts_the_chain_it_names() {
        let mut t = TrigramIndex::new(64);
        // "rare" appears once; "abc" appears in every row.
        for row in 0..30u64 {
            t.insert(row, &value(b"abcabcabc"));
        }
        t.insert(30, &value(b"rare-abc"));

        // Naive count of a chain, by walking it with no cap at all.
        let walk = |needle: &[u8]| t.chain_len_capped(needle, u32::MAX);

        // Every row holds "abc"; only one holds "rar".
        assert_eq!(walk(b"rar"), 1);
        assert_eq!(walk(b"rare-abc"), 1, "only the first trigram keys it");
        assert_eq!(walk(b"abc"), 31);
        // A trigram nothing holds has an empty chain, which is the
        // cheapest possible answer and still a correct superset.
        assert_eq!(walk(b"zzz"), 0);

        // The cap truncates rather than lying about the direction.
        assert_eq!(t.chain_len_capped(b"abc", 4), 4);
        assert_eq!(t.chain_len_capped(b"rar", 4), 1);
        assert_eq!(t.chain_len_capped(b"abc", 0), 0);

        // A needle with no trigram has no chain: it reports the worst
        // possible cost, because searching it means scanning every row.
        assert_eq!(walk(b"ab"), u32::MAX);
        assert_eq!(walk(b""), u32::MAX);
        assert_eq!(t.chain_len_capped(b"ab", 4), u32::MAX);
    }

    /// A chain is keyed on the FIRST THREE BYTES, so needle length is not
    /// a proxy for selectivity. This is the fact that made picking the
    /// longest needle a guess.
    #[test]
    fn a_longer_needle_is_not_a_rarer_chain() {
        let mut t = TrigramIndex::new(64);
        for row in 0..20u64 {
            t.insert(row, &value(b"the quick brown"));
        }
        t.insert(20, &value(b"zqx"));
        assert_eq!(
            t.chain_len_capped(b"the quick brown", u32::MAX),
            t.chain_len_capped(b"the", u32::MAX),
            "the same first trigram is the same chain, whatever the length"
        );
        assert!(
            t.chain_len_capped(b"zqx", u32::MAX) < t.chain_len_capped(b"the quick brown", u32::MAX),
            "the short needle is the cheap one here"
        );
    }

    #[test]
    fn matches_the_naive_oracle_on_crafted_values() {
        let values = [
            value(b"hello, world!!"),
            value(b"hello again"),
            value(b"worldly matters"),
            value(b"aaaaaaaaaaaaaaaa"),
            value(b"abcabcabcabcabca"),
            value(&[0xFF; 16]),
            value(b""),
        ];
        let mut t = TrigramIndex::new(values.len() as u64);
        for (row, v) in values.iter().enumerate() {
            t.insert(row as u64, v);
        }
        let needles: &[&[u8]] = &[
            b"hello",
            b"world",
            b"aaa",
            b"abc",
            b"cab",
            b"zzz",
            b"o, w",
            b"\xFF\xFF\xFF",
            b"\x00\x00\x00",
            b"lo", // shorter than a trigram: scan path
            b"a",  // scan path
            b"",   // matches everything
        ];
        for needle in needles {
            let expected: Vec<u64> = values
                .iter()
                .enumerate()
                .filter(|(_, v)| contains(v, needle))
                .map(|(i, _)| i as u64)
                .collect();
            assert_eq!(find_all(&t, &values, needle), expected, "needle {needle:?}");
        }
    }

    #[test]
    fn duplicate_windows_do_not_duplicate_results() {
        // "aaaaaaaaaaaaaaaa" holds the same trigram 14 times; the row
        // must appear in results exactly once.
        let values = [value(b"aaaaaaaaaaaaaaaa"), value(b"baaab")];
        let mut t = TrigramIndex::new(2);
        for (row, v) in values.iter().enumerate() {
            t.insert(row as u64, v);
        }
        assert_eq!(find_all(&t, &values, b"aaa"), vec![0, 1]);
    }

    #[test]
    fn pool_is_exactly_rows_times_trigrams() {
        // One slot per byte of a row, not per trigram window: a value may
        // span rows, and a window starting in a row's last two bytes runs
        // into the next one. Reserving the extra two keeps the posting
        // address a pure function of the value offset.
        assert_eq!(TRIGRAMS_PER_ROW, VALUE_LEN);
        let t = TrigramIndex::new(8);
        assert_eq!(t.next.len(), 8 * VALUE_LEN);
        // Load <= 0.5 over worst-case distinct trigrams.
        assert!(t.table.len() >= 2 * 8 * VALUE_LEN);
        assert!(t.table.len().is_power_of_two());
    }

    /// A value spanning several rows is searchable as ONE value, wherever
    /// the match falls: across a slot seam, and wholly inside a
    /// continuation slot.
    ///
    /// The second case is the one that used to force the whole database
    /// onto the scan path. A posting is filed under the row its window
    /// STARTS in, so a match at offset 34 is filed under the value's third
    /// row; without `head_of` mapping that back, the chain silently
    /// dropped it and the index stopped being a superset.
    #[test]
    fn a_value_spanning_rows_is_searchable_across_its_seams() {
        // 48 bytes: three rows. "needle" crosses the first seam;
        // "deeper" sits entirely inside the third row.
        let mut long = vec![b'.'; 48];
        long[14..20].copy_from_slice(b"needle");
        long[34..40].copy_from_slice(b"deeper");
        let mut t = TrigramIndex::new(8);
        t.insert_value(0, 3, &long);

        // Every row of the run resolves to the head; nothing else exists.
        let head_of = |row: u64| (row < 3).then_some(0u64);
        for needle in [&b"needle"[..], b"deeper", b"..needle", b"deeper.."] {
            let mut page = [0u64; 4];
            let (n, _) = t.find_page(needle, None, &mut page, |row| row == 0, head_of);
            assert_eq!(
                &page[..n],
                &[0],
                "{:?} was not found in the value that contains it",
                core::str::from_utf8(needle)
            );
        }

        // A posting whose head cannot be resolved is not a candidate: the
        // walk drops it rather than returning a continuation row.
        let mut page = [0u64; 4];
        let (n, _) = t.find_page(b"deeper", None, &mut page, |row| row == 0, |_| None);
        assert_eq!(n, 0);
    }

    #[test]
    fn slot_hash_matches_reference_mix() {
        let t = TrigramIndex::new(8);
        for tri in [0u32, 1, 0x616263, 0xFF_FFFF] {
            let expected =
                ((tri as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize) & (t.table.len() - 1);
            assert_eq!(t.slot_of(tri), expected, "tri={tri:#08x}");
        }
    }

    #[test]
    #[should_panic(expected = "append-only")]
    fn out_of_order_insert_is_refused() {
        let mut t = TrigramIndex::new(4);
        t.insert(1, &value(b"skip a row"));
    }

    /// The probe-termination guard has teeth (same forged-full-table
    /// pattern as the engine's index): under the counter-disabling
    /// mutant this hangs — a timeout kill.
    #[test]
    #[should_panic(expected = "probe loop must terminate")]
    fn probe_guard_has_teeth() {
        let mut t = TrigramIndex::new(2);
        for slot in t.table.iter_mut() {
            *slot = TrigramIndex::pack(0x111111, 0);
        }
        t.head(0x222222);
    }

    /// The chain cycle guard has teeth.
    #[test]
    #[should_panic(expected = "chain cycle")]
    fn cycle_guard_has_teeth() {
        let values = [value(b"abcdefghijklmnop"), value(b"abczzzzzzzzzzzzz")];
        let mut t = TrigramIndex::new(2);
        for (row, v) in values.iter().enumerate() {
            t.insert(row as u64, v);
        }
        // Corrupt the chain for "abc" into a self-loop.
        let head = t.head(tri_key(b"abc"));
        assert_ne!(head, NIL);
        t.next[head as usize] = head;
        let mut page = [0u64; 4];
        let _ = t.find_page(b"abc", None, &mut page, |_| true, Some);
    }
}
