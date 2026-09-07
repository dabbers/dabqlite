//! Lexicographic order over VALUE bytes, read straight out of the row
//! arena (docs/DESIGN.md §10, "keys that are not `u64`").
//!
//! All three sample applications named the same structural gap: the
//! primary key is a `u64`, so a bookmark store, a job queue and a
//! key/value store all hash their real key onto one, probe past
//! collisions, and answer "list everything under `session/`" with a full
//! scan and a sort, because id order is hash order.
//!
//! The answer is a second ordered index over the value bytes — and the
//! key insight is that it does not have to STORE those bytes. A value
//! already lives in the arena; the index stores the head ROW of each
//! record and compares by dereferencing. That is what this module is: the
//! comparison, and nothing else. The tree itself is the same
//! [`crate::btree::BTreeIndex`] the primary key uses, driven by these as
//! its probe.
//!
//! Two properties matter and both are held here rather than by
//! convention:
//!
//! - **No assembly.** A comparison walks the two runs slot by slot and
//!   stops at the first differing byte, so comparing two 2 KiB values
//!   that differ in byte 3 reads one slot of each. Nothing is copied into
//!   a scratch buffer, which is what lets this run in an engine that
//!   allocates nothing after init.
//! - **Total order.** Value bytes first, then the head row, so two
//!   records holding identical bytes still have distinct keys. The tree
//!   rejects duplicates by design, and rows are unique by construction.

use core::cmp::Ordering;

use crate::layout::{decode_row, RowKind};
use crate::{MAX_COMMIT_ROWS, ROW_SIZE, VALUE_LEN};

/// A value's bytes, one slot at a time, straight out of the arena.
///
/// The run ends where the file says it ends: at the first row that is not
/// a continuation of the same id. Bounded by [`MAX_COMMIT_ROWS`], because
/// a value is written whole inside one commit and a commit cannot be
/// longer than that — so a damaged arena cannot turn a comparison into an
/// unbounded walk.
struct RunReader<'a> {
    arena: &'a [u8],
    row_count: u64,
    row: u64,
    id: u64,
    started: bool,
    steps: u32,
}

impl<'a> RunReader<'a> {
    fn new(arena: &'a [u8], row_count: u64, head_row: u64) -> Self {
        RunReader {
            arena,
            row_count,
            row: head_row,
            id: 0,
            started: false,
            steps: 0,
        }
    }

    /// The next slot's payload, or `None` at the end of the run.
    fn next_slot(&mut self) -> Option<([u8; VALUE_LEN], usize)> {
        if self.row >= self.row_count || self.steps as usize >= MAX_COMMIT_ROWS {
            return None;
        }
        let off = (self.row as usize) * ROW_SIZE;
        let slot = decode_row(&self.arena[off..off + ROW_SIZE])?;
        if self.started {
            if slot.kind != RowKind::Chunk || slot.id != self.id {
                return None;
            }
        } else {
            // The head of a run is the record itself; a tombstone holds no
            // value at all and compares as empty.
            if slot.kind == RowKind::Tombstone {
                return None;
            }
            self.id = slot.id;
            self.started = true;
        }
        self.row += 1;
        self.steps += 1;
        Some((slot.value, slot.len as usize))
    }
}

/// Compare the value stored at `a` against the value stored at `b`.
pub fn cmp_runs(arena: &[u8], row_count: u64, a: u64, b: u64) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let mut ra = RunReader::new(arena, row_count, a);
    let mut rb = RunReader::new(arena, row_count, b);
    let (mut ca, mut cb) = (([0u8; VALUE_LEN], 0usize), ([0u8; VALUE_LEN], 0usize));
    let (mut ia, mut ib) = (0usize, 0usize);
    let (mut ea, mut eb) = (false, false);
    loop {
        if ia == ca.1 && !ea {
            match ra.next_slot() {
                Some(c) => {
                    ca = c;
                    ia = 0;
                }
                None => ea = true,
            }
        }
        if ib == cb.1 && !eb {
            match rb.next_slot() {
                Some(c) => {
                    cb = c;
                    ib = 0;
                }
                None => eb = true,
            }
        }
        match (ia < ca.1, ib < cb.1) {
            // Both exhausted: equal bytes, equal length.
            (false, false) => return Ordering::Equal,
            // A prefix sorts before what extends it.
            (false, true) => return Ordering::Less,
            (true, false) => return Ordering::Greater,
            (true, true) => {
                let n = (ca.1 - ia).min(cb.1 - ib);
                match ca.0[ia..ia + n].cmp(&cb.0[ib..ib + n]) {
                    Ordering::Equal => {
                        ia += n;
                        ib += n;
                    }
                    other => return other,
                }
            }
        }
    }
}

/// Compare the value stored at `row` against `needle`.
pub fn cmp_run_bytes(arena: &[u8], row_count: u64, row: u64, needle: &[u8]) -> Ordering {
    let mut r = RunReader::new(arena, row_count, row);
    let mut at = 0usize;
    loop {
        let Some((chunk, len)) = r.next_slot() else {
            // The value ended. It is a prefix of the needle unless the
            // needle ended too.
            return if at < needle.len() {
                Ordering::Less
            } else {
                Ordering::Equal
            };
        };
        if len == 0 {
            continue;
        }
        let n = len.min(needle.len() - at);
        match chunk[..n].cmp(&needle[at..at + n]) {
            Ordering::Equal => {}
            other => return other,
        }
        if n < len {
            // The needle ran out mid-slot and everything before matched,
            // so the value extends it.
            return Ordering::Greater;
        }
        at += n;
    }
}

/// The id stored in a row, for the order's tie-break.
fn id_at(arena: &[u8], row: u64) -> u64 {
    let off = (row as usize) * ROW_SIZE;
    match arena.get(off..off + ROW_SIZE).and_then(decode_row) {
        Some(slot) => slot.id,
        None => 0,
    }
}

/// The index's total order: value bytes, then id, then head row.
///
/// Three levels, and each one earns its place.
///
/// The bytes are the order the caller asked for. The **id** is what
/// breaks a tie between two different records holding the same bytes —
/// and it is the tie-break rather than the row because it is the only one
/// a caller can predict: row numbers are an internal detail that a
/// compaction renumbers, so "equal values come back in id order" is a
/// promise that survives a rebuild and "in insertion order" is not.
///
/// The **row** is last and is never reached by two LIVE entries, because
/// an id has one live record at a time. It is there because this index is
/// append-only: writing the same value to the same id twice leaves the
/// superseded row in the tree beside its replacement, equal on both of
/// the first two levels, and a B+tree that rejects duplicate keys would
/// reject the write.
pub fn order(arena: &[u8], row_count: u64, a: u64, b: u64) -> Ordering {
    cmp_runs(arena, row_count, a, b)
        .then_with(|| id_at(arena, a).cmp(&id_at(arena, b)))
        .then(a.cmp(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::encode_row;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Build an arena from `(id, value)` pairs, each stored as a run of
    /// slots the way the engine writes them. Returns the arena and the
    /// head row of each value.
    fn arena_of(values: &[(u64, &[u8])]) -> (Vec<u8>, Vec<u64>) {
        let mut arena = Vec::new();
        let mut heads = Vec::new();
        for &(id, v) in values {
            let slots = v.len().div_ceil(VALUE_LEN).max(1);
            heads.push((arena.len() / ROW_SIZE) as u64);
            for i in 0..slots {
                let from = i * VALUE_LEN;
                let to = (from + VALUE_LEN).min(v.len());
                let chunk = &v[from..to];
                let mut padded = [0u8; VALUE_LEN];
                padded[..chunk.len()].copy_from_slice(chunk);
                let mut out = [0u8; ROW_SIZE];
                let kind = if i == 0 {
                    RowKind::Record
                } else {
                    RowKind::Chunk
                };
                encode_row(
                    kind,
                    0,
                    chunk.len() as u8,
                    i + 1 < slots,
                    id,
                    &padded,
                    &mut out,
                );
                arena.extend_from_slice(&out);
            }
        }
        (arena, heads)
    }

    /// The whole contract in one property: comparing two runs must agree
    /// with comparing the bytes they hold, for every pair, at every
    /// length that crosses a slot seam.
    #[test]
    fn comparing_runs_agrees_with_comparing_the_bytes() {
        let mut cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            b"a".to_vec(),
            b"ab".to_vec(),
            b"abc".to_vec(),
            b"session/".to_vec(),
            b"session/0".to_vec(),
            b"session/z".to_vec(),
            b"sessions".to_vec(),
            vec![0xff; 1],
            vec![0xff; VALUE_LEN],
        ];
        // Lengths either side of every slot boundary up to four slots.
        for slots in 1..=4usize {
            for d in [-1i32, 0, 1] {
                let len = (slots * VALUE_LEN) as i32 + d;
                if len <= 0 {
                    continue;
                }
                let mut v = vec![b'k'; len as usize];
                // Differ in the last byte so the seam cases are real.
                *v.last_mut().unwrap() = b'a' + (slots as u8);
                cases.push(v);
            }
        }
        let pairs: Vec<(u64, &[u8])> = cases
            .iter()
            .enumerate()
            .map(|(i, v)| (i as u64, v.as_slice()))
            .collect();
        let (arena, heads) = arena_of(&pairs);
        let n = (arena.len() / ROW_SIZE) as u64;
        for (i, a) in cases.iter().enumerate() {
            for (j, b) in cases.iter().enumerate() {
                assert_eq!(
                    cmp_runs(&arena, n, heads[i], heads[j]),
                    a.cmp(b),
                    "run {i} vs run {j}"
                );
                assert_eq!(
                    cmp_run_bytes(&arena, n, heads[i], b),
                    a.as_slice().cmp(b.as_slice()),
                    "run {i} vs needle {j}"
                );
            }
        }
    }

    /// Equal bytes must not compare equal in the INDEX order, or the tree
    /// would reject the second one as a duplicate key. The tie-break is
    /// the row.
    #[test]
    fn equal_values_are_still_distinct_keys() {
        let (arena, heads) = arena_of(&[(2, b"same"), (1, b"same"), (3, b"other")]);
        let n = (arena.len() / ROW_SIZE) as u64;
        assert_eq!(cmp_runs(&arena, n, heads[0], heads[1]), Ordering::Equal);
        // Equal bytes: the ID decides, not the row. Row 0 holds id 2 and
        // row 1 holds id 1, so the LATER row sorts first — which is the
        // whole point of the tie-break being the id.
        assert_eq!(order(&arena, n, heads[0], heads[1]), Ordering::Greater);
        assert_eq!(order(&arena, n, heads[1], heads[0]), Ordering::Less);
        assert_eq!(order(&arena, n, heads[0], heads[0]), Ordering::Equal);
        // And the tie-break never overrides the bytes: "other" sorts
        // before "same" whatever the ids are.
        assert_eq!(order(&arena, n, heads[2], heads[0]), Ordering::Less);
        assert_eq!(order(&arena, n, heads[0], heads[2]), Ordering::Greater);
    }

    /// The same id AND the same bytes, twice — which is what an
    /// append-only index sees when a value is rewritten unchanged. The
    /// third level of the order is what keeps those two entries distinct.
    #[test]
    fn the_same_value_written_twice_to_one_id_stays_two_keys() {
        let (arena, heads) = arena_of(&[(7, b"v"), (7, b"v")]);
        let n = (arena.len() / ROW_SIZE) as u64;
        assert_eq!(cmp_runs(&arena, n, heads[0], heads[1]), Ordering::Equal);
        assert_eq!(id_at(&arena, heads[0]), id_at(&arena, heads[1]));
        assert_eq!(order(&arena, n, heads[0], heads[1]), Ordering::Less);
        assert_eq!(order(&arena, n, heads[1], heads[0]), Ordering::Greater);
    }

    /// A run stops where the file says it stops. A continuation belonging
    /// to another id is a different value, not more of this one — which is
    /// what keeps a comparison from reading past the end of a record into
    /// its neighbour.
    #[test]
    fn a_run_never_reads_into_its_neighbour() {
        let (arena, heads) = arena_of(&[(1, b"aaaaaaaaaaaaaaaaBBBB"), (2, b"zzzz")]);
        let n = (arena.len() / ROW_SIZE) as u64;
        assert_eq!(
            cmp_run_bytes(&arena, n, heads[0], b"aaaaaaaaaaaaaaaaBBBB"),
            Ordering::Equal
        );
        assert_eq!(
            cmp_run_bytes(&arena, n, heads[0], b"aaaaaaaaaaaaaaaaBBBBzzzz"),
            Ordering::Less,
            "the neighbour's bytes must not extend this value"
        );
        assert_eq!(cmp_run_bytes(&arena, n, heads[1], b"zzzz"), Ordering::Equal);
    }

    /// The row-count bound is a real bound: a run is never followed past
    /// the committed end of the arena, because rows beyond it are staged,
    /// stale or zero.
    #[test]
    fn the_committed_bound_stops_the_walk() {
        let (arena, heads) = arena_of(&[(1, b"aaaaaaaaaaaaaaaaBBBB")]);
        assert_eq!(heads, vec![0]);
        // Told there is only one committed row, the second slot of the
        // value is out of bounds and the value reads as its first slot.
        assert_eq!(
            cmp_run_bytes(&arena, 1, 0, b"aaaaaaaaaaaaaaaa"),
            Ordering::Equal
        );
        assert_eq!(
            cmp_run_bytes(&arena, 2, 0, b"aaaaaaaaaaaaaaaa"),
            Ordering::Greater
        );
    }

    /// A tombstone holds no value. It must compare as empty rather than
    /// as whatever bytes happen to sit in its slot.
    #[test]
    fn a_tombstone_compares_as_the_empty_value() {
        let mut arena = vec![0u8; ROW_SIZE];
        let mut out = [0u8; ROW_SIZE];
        encode_row(
            RowKind::Tombstone,
            0,
            0,
            false,
            7,
            &[0u8; VALUE_LEN],
            &mut out,
        );
        arena.copy_from_slice(&out);
        assert_eq!(cmp_run_bytes(&arena, 1, 0, b""), Ordering::Equal);
        assert_eq!(cmp_run_bytes(&arena, 1, 0, b"a"), Ordering::Less);
    }
}
