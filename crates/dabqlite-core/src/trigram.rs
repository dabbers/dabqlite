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

/// Trigrams in one value: one per 3-byte window.
pub const TRIGRAMS_PER_ROW: usize = VALUE_LEN - 2;

const NIL: u32 = u32::MAX;
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
        self.assert_invariants();
        assert_eq!(row, self.len, "trigram index rows are append-only");
        assert!(
            ((row as usize) + 1) * TRIGRAMS_PER_ROW <= self.next.len(),
            "trigram pool exhausted: capacity invariant violated"
        );
        for k in 0..TRIGRAMS_PER_ROW {
            let tri = tri_key(&value[k..k + 3]);
            // One posting per DISTINCT trigram per row: a duplicate
            // window (e.g. "aaaa") must not chain the same row twice.
            let dup = (0..k).any(|j| tri_key(&value[j..j + 3]) == tri);
            if dup {
                continue;
            }
            let slot = (row as usize) * TRIGRAMS_PER_ROW + k;
            self.next[slot] = self.head(tri);
            self.set_head(tri, slot as u32);
        }
        self.len = row + 1;
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

    /// The `page` smallest matching rows strictly above `cursor`, in
    /// ascending row order, plus the count of matches found for the
    /// page (callers detect "more" by requesting again from the last
    /// row returned). `matches(row)` is the caller's verifier — the
    /// index never trusts itself.
    ///
    /// Exactness argument: for needles >= 3 bytes, any row containing
    /// the needle contains its first trigram, so walking that one chain
    /// visits a superset of the answer; verification removes the rest.
    /// For shorter needles there is no trigram to look up: scan all
    /// rows (bounded by len; still exact).
    pub fn find_page<F: Fn(u64) -> bool>(
        &self,
        needle: &[u8],
        cursor: Option<u64>,
        page: &mut [u64],
        matches: F,
    ) -> usize {
        self.assert_invariants();
        let lo = cursor.map_or(0, |c| c + 1);
        let mut found = 0usize;
        let consider = |row: u64, page: &mut [u64], found: &mut usize| {
            if row < lo || !matches(row) {
                return;
            }
            // Insertion into the bounded ascending page (page.len() is
            // small and fixed: this is O(page) per candidate, O(1) mem).
            let mut i = *found;
            if i == page.len() {
                if row >= page[i - 1] {
                    return;
                }
                i -= 1;
            } else {
                *found += 1;
            }
            while i > 0 && page[i - 1] > row {
                page[i] = page[i - 1];
                i -= 1;
            }
            page[i] = row;
        };

        if needle.len() < 3 {
            for row in lo..self.len {
                consider(row, page, &mut found);
                // Ascending scan: a full page of the smallest is final.
                if found == page.len() {
                    break;
                }
            }
            return found;
        }

        let tri = tri_key(&needle[0..3]);
        let mut slot = self.head(tri);
        let mut steps = 0u64;
        while slot != NIL {
            assert!(
                steps <= self.len * TRIGRAMS_PER_ROW as u64,
                "trigram chain cycle"
            );
            let row = (slot as usize / TRIGRAMS_PER_ROW) as u64;
            consider(row, page, &mut found);
            slot = self.next[slot as usize];
            steps += 1;
        }
        found
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

    fn find_all(t: &TrigramIndex, values: &[[u8; VALUE_LEN]], needle: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let mut page = [0u64; 4];
            let n = t.find_page(needle, cursor, &mut page, |row| {
                contains(&values[row as usize], needle)
            });
            out.extend_from_slice(&page[..n]);
            if n < page.len() {
                return out;
            }
            cursor = Some(page[n - 1]);
        }
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
        assert_eq!(TRIGRAMS_PER_ROW, 14);
        let t = TrigramIndex::new(8);
        assert_eq!(t.next.len(), 8 * 14);
        // Load <= 0.5 over worst-case distinct trigrams.
        assert!(t.table.len() >= 2 * 8 * 14);
        assert!(t.table.len().is_power_of_two());
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
        t.find_page(b"abc", None, &mut page, |_| true);
    }
}
