//! The B+tree index (docs/DESIGN.md §4.6): ordered `u64 key → u64 row`
//! mapping backing range queries.
//!
//! - **Fixed node pool**, one allocation at init sized from the declared
//!   row capacity — no allocation afterward, ever. The pool bound is a
//!   proven property, not a hope: leaves keep >= ORDER/2 keys after any
//!   split and internals keep >= ORDER/2+1 children, so nodes <= N/4 +
//!   N/16 + roots < N/3 + O(log N); the fuzz suite asserts actual usage
//!   never approaches the bound.
//! - **Small order (8)** on purpose: splits happen constantly, so every
//!   test exercises the interesting paths instead of living inside one
//!   giant root.
//! - No deletes: the engine has no delete operation yet, and unreachable
//!   code cannot be tested honestly.
//!
//! Like the primary-key hash index, this structure is in-memory state
//! *derived* from the committed rows: rebuilt at every recovery, so it
//! inherits the engine's crash guarantees by construction and is
//! re-verified against reality on every open.

use alloc::vec;
use alloc::vec::Vec;

/// Max keys per node. Even, small (see module docs).
pub const ORDER: usize = 8;
const NIL: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Node {
    keys: [u64; ORDER],
    /// Leaf: row indices, parallel to `keys`. Internal: unused.
    vals: [u64; ORDER],
    /// Internal: `len + 1` children. Leaf: unused.
    children: [u32; ORDER + 1],
    /// Leaf: the next leaf in key order (the scan chain), NIL at the end.
    next: u32,
    len: u8,
    leaf: bool,
}

impl Node {
    fn empty_leaf() -> Node {
        Node {
            keys: [0; ORDER],
            vals: [0; ORDER],
            children: [NIL; ORDER + 1],
            next: NIL,
            len: 0,
            leaf: true,
        }
    }
}

pub struct BTreeIndex {
    pool: Vec<Node>,
    used: u32,
    root: u32,
    len: u64,
    /// Negative space: the pool must never move (no allocation after init).
    pool_addr: usize,
}

/// Pool size for a declared row capacity. See module docs for the bound
/// argument; the +16 covers small-N cases and root chains.
pub fn pool_nodes_for(rows: u64) -> u64 {
    rows / 3 + 16
}

/// Descent routing: does `key` belong strictly before separator `sep`?
/// STRICTLY — equality routes right, toward the leaf that owns the
/// separator's key. Through `insert` the equal case is unreachable
/// (duplicates are rejected before the index), which makes the strictness
/// untestable from the outside; naming the predicate lets the tests pin
/// it directly instead of excluding the mutant as "equivalent".
fn routes_before(key: u64, sep: u64) -> bool {
    key < sep
}

impl BTreeIndex {
    /// One pool allocation for the declared capacity (docs/DESIGN.md §4.2).
    pub fn new(rows: u64) -> Self {
        let cap = pool_nodes_for(rows);
        assert!(cap <= u32::MAX as u64, "node pool exceeds u32 addressing");
        let pool = vec![Node::empty_leaf(); cap as usize];
        let pool_addr = pool.as_ptr() as usize;
        let mut t = BTreeIndex {
            pool,
            used: 0,
            root: 0,
            len: 0,
            pool_addr,
        };
        t.root = t.alloc_node();
        t
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Nodes in use, for the pool-bound assertions in tests.
    pub fn nodes_used(&self) -> u32 {
        self.used
    }

    fn alloc_node(&mut self) -> u32 {
        // The bound is derived from occupancy invariants; exhausting it
        // means the math (or the tree) is wrong. Loud, immediate.
        assert!(
            (self.used as usize) < self.pool.len(),
            "btree node pool exhausted: occupancy invariant violated"
        );
        let id = self.used;
        self.pool[id as usize] = Node::empty_leaf();
        self.used += 1;
        id
    }

    fn node(&self, id: u32) -> &Node {
        &self.pool[id as usize]
    }

    fn node_mut(&mut self, id: u32) -> &mut Node {
        &mut self.pool[id as usize]
    }

    /// Insert a key. Keys are unique (the engine rejects duplicates before
    /// reaching the index); inserting a duplicate is a caller bug.
    pub fn insert(&mut self, key: u64, row: u64) {
        debug_assert_eq!(
            self.pool.as_ptr() as usize,
            self.pool_addr,
            "btree pool moved: allocation after init is forbidden"
        );
        debug_assert!(self.get(key).is_none(), "duplicate key {key} in btree");
        if let Some((sep, right)) = self.insert_into(self.root, key, row) {
            // Root split: the tree grows one level.
            let old_root = self.root;
            let new_root = self.alloc_node();
            let n = self.node_mut(new_root);
            n.leaf = false;
            n.len = 1;
            n.keys[0] = sep;
            n.children[0] = old_root;
            n.children[1] = right;
            self.root = new_root;
        }
        self.len += 1;
        // Pair assertion: what went in must come out.
        debug_assert_eq!(self.get(key), Some(row));
    }

    /// Point lookup (used only by assertions; the hash index serves gets).
    pub fn get(&self, key: u64) -> Option<u64> {
        let mut id = self.root;
        loop {
            let n = self.node(id);
            let len = n.len as usize;
            if n.leaf {
                return n.keys[..len]
                    .iter()
                    .position(|&k| k == key)
                    .map(|i| n.vals[i]);
            }
            let mut child = len; // rightmost unless a separator exceeds key
            for (i, &k) in n.keys[..len].iter().enumerate() {
                if key < k {
                    child = i;
                    break;
                }
            }
            id = n.children[child];
        }
    }

    /// In-order visit of every `(key, row)` with `key >= start`, until the
    /// callback returns `false`. Bounded work per call is the caller's job
    /// (the engine pages); termination here is the leaf chain's finiteness.
    pub fn for_each_from(&self, start: u64, mut f: impl FnMut(u64, u64) -> bool) {
        // Descend to the leaf that could contain `start`.
        let mut id = self.root;
        loop {
            let n = self.node(id);
            if n.leaf {
                break;
            }
            let len = n.len as usize;
            let mut child = len;
            for (i, &k) in n.keys[..len].iter().enumerate() {
                // Same routing predicate as insert: start == separator
                // descends RIGHT, straight to the leaf that owns the key.
                // (Routing left would still be correct — the chain walk
                // filters — just one leaf slower, which is why only the
                // shared pinned predicate can hold the strictness.)
                if routes_before(start, k) {
                    child = i;
                    break;
                }
            }
            id = n.children[child];
        }
        // Walk the chain.
        let mut steps = 0u64;
        while id != NIL {
            assert!(steps <= self.used as u64, "leaf chain cycle");
            let n = self.node(id);
            for i in 0..n.len as usize {
                if n.keys[i] >= start && !f(n.keys[i], n.vals[i]) {
                    return;
                }
            }
            id = n.next;
            steps += 1;
        }
    }

    /// Recursive insert; returns `Some((separator, new_right))` if `id`
    /// split.
    fn insert_into(&mut self, id: u32, key: u64, row: u64) -> Option<(u64, u32)> {
        if self.node(id).leaf {
            return self.insert_into_leaf(id, key, row);
        }
        let len = self.node(id).len as usize;
        let mut child_idx = len;
        for i in 0..len {
            if routes_before(key, self.node(id).keys[i]) {
                child_idx = i;
                break;
            }
        }
        let child = self.node(id).children[child_idx];
        let (sep, right) = self.insert_into(child, key, row)?;

        // The child split: insert (sep, right) into this internal node.
        let len = self.node(id).len as usize;
        if len < ORDER {
            let n = self.node_mut(id);
            let mut i = len;
            while i > child_idx {
                n.keys[i] = n.keys[i - 1];
                n.children[i + 1] = n.children[i];
                i -= 1;
            }
            n.keys[child_idx] = sep;
            n.children[child_idx + 1] = right;
            n.len += 1;
            return None;
        }

        // Internal overflow: ORDER+1 keys conceptually; middle moves up.
        let mut keys = [0u64; ORDER + 1];
        let mut children = [NIL; ORDER + 2];
        {
            let n = self.node(id);
            keys[..child_idx].copy_from_slice(&n.keys[..child_idx]);
            keys[child_idx] = sep;
            keys[child_idx + 1..].copy_from_slice(&n.keys[child_idx..]);
            children[..child_idx + 1].copy_from_slice(&n.children[..child_idx + 1]);
            children[child_idx + 1] = right;
            children[child_idx + 2..].copy_from_slice(&n.children[child_idx + 1..len + 1]);
        }
        const MID: usize = ORDER / 2; // keys[MID] moves up
        let new_right = self.alloc_node();
        {
            let r = self.node_mut(new_right);
            r.leaf = false;
            r.len = (ORDER - MID) as u8;
            r.keys[..ORDER - MID].copy_from_slice(&keys[MID + 1..]);
            r.children[..ORDER - MID + 1].copy_from_slice(&children[MID + 1..]);
        }
        {
            let n = self.node_mut(id);
            n.len = MID as u8;
            n.keys[..MID].copy_from_slice(&keys[..MID]);
            n.children[..MID + 1].copy_from_slice(&children[..MID + 1]);
        }
        Some((keys[MID], new_right))
    }

    fn insert_into_leaf(&mut self, id: u32, key: u64, row: u64) -> Option<(u64, u32)> {
        let len = self.node(id).len as usize;
        let pos = self.node(id).keys[..len]
            .iter()
            .position(|&k| routes_before(key, k))
            .unwrap_or(len);
        if len < ORDER {
            let n = self.node_mut(id);
            let mut i = len;
            while i > pos {
                n.keys[i] = n.keys[i - 1];
                n.vals[i] = n.vals[i - 1];
                i -= 1;
            }
            n.keys[pos] = key;
            n.vals[pos] = row;
            n.len += 1;
            return None;
        }

        // Leaf overflow: ORDER+1 entries; split in half, copy-up the
        // right's first key as separator, link the chain.
        let mut keys = [0u64; ORDER + 1];
        let mut vals = [0u64; ORDER + 1];
        {
            let n = self.node(id);
            keys[..pos].copy_from_slice(&n.keys[..pos]);
            keys[pos] = key;
            keys[pos + 1..].copy_from_slice(&n.keys[pos..]);
            vals[..pos].copy_from_slice(&n.vals[..pos]);
            vals[pos] = row;
            vals[pos + 1..].copy_from_slice(&n.vals[pos..]);
        }
        const HALF: usize = ORDER.div_ceil(2); // left keeps HALF, right the rest
        let new_right = self.alloc_node();
        let old_next = self.node(id).next;
        {
            let r = self.node_mut(new_right);
            r.leaf = true;
            r.len = (ORDER + 1 - HALF) as u8;
            r.keys[..ORDER + 1 - HALF].copy_from_slice(&keys[HALF..]);
            r.vals[..ORDER + 1 - HALF].copy_from_slice(&vals[HALF..]);
            r.next = old_next;
        }
        {
            let n = self.node_mut(id);
            n.len = HALF as u8;
            n.keys[..HALF].copy_from_slice(&keys[..HALF]);
            n.vals[..HALF].copy_from_slice(&vals[..HALF]);
            n.next = new_right;
        }
        Some((keys[HALF], new_right))
    }

    /// Deep structural validation, for tests and the debug builds the
    /// simulator runs. Checks: strict key order within and across nodes,
    /// uniform leaf depth, occupancy minimums, separator bounds, leaf-chain
    /// completeness, and the length accounting.
    pub fn check_invariants(&self) {
        let mut leaf_depth: Option<u32> = None;
        let mut count = 0u64;
        self.check_node(self.root, 0, None, None, &mut leaf_depth, &mut count, true);
        assert_eq!(count, self.len, "btree length accounting diverged");

        // The leaf chain must visit exactly the in-order keys, sorted.
        let mut last: Option<u64> = None;
        let mut chained = 0u64;
        self.for_each_from(0, |k, _| {
            if let Some(l) = last {
                assert!(k > l, "leaf chain out of order: {l} then {k}");
            }
            last = Some(k);
            chained += 1;
            true
        });
        assert_eq!(chained, self.len, "leaf chain missed keys");
    }

    #[allow(clippy::too_many_arguments)]
    fn check_node(
        &self,
        id: u32,
        depth: u32,
        lo: Option<u64>,
        hi: Option<u64>,
        leaf_depth: &mut Option<u32>,
        count: &mut u64,
        is_root: bool,
    ) {
        let n = self.node(id);
        let len = n.len as usize;
        if !is_root {
            let min = if n.leaf { HALF_MIN } else { ORDER / 2 };
            assert!(len >= min, "under-occupied node ({len} < {min})");
        }
        for w in n.keys[..len].windows(2) {
            assert!(w[0] < w[1], "node keys out of order");
        }
        for &k in &n.keys[..len] {
            if let Some(lo) = lo {
                assert!(k >= lo, "key {k} below subtree bound {lo}");
            }
            if let Some(hi) = hi {
                assert!(k < hi, "key {k} at/above subtree bound {hi}");
            }
        }
        if n.leaf {
            match *leaf_depth {
                None => *leaf_depth = Some(depth),
                Some(d) => assert_eq!(d, depth, "leaves at unequal depth"),
            }
            *count += len as u64;
        } else {
            assert!(len >= 1, "internal node with no separators");
            for i in 0..=len {
                let child_lo = if i == 0 { lo } else { Some(n.keys[i - 1]) };
                let child_hi = if i == len { hi } else { Some(n.keys[i]) };
                self.check_node(
                    n.children[i],
                    depth + 1,
                    child_lo,
                    child_hi,
                    leaf_depth,
                    count,
                    false,
                );
            }
        }
    }
}

/// Minimum keys in a non-root leaf after a split.
const HALF_MIN: usize = ORDER.div_ceil(2) - 1;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec as StdVec;

    #[test]
    fn small_inserts_and_lookups() {
        let mut t = BTreeIndex::new(64);
        for (row, key) in [5u64, 1, 9, 3, 7, 2, 8, 4, 6, 0].iter().enumerate() {
            t.insert(*key, row as u64);
            t.check_invariants();
        }
        assert_eq!(t.len(), 10);
        assert_eq!(t.get(7), Some(4));
        assert_eq!(t.get(10), None);
    }

    #[test]
    fn every_insertion_order_of_small_sets_is_identical() {
        // Exhaustive: all permutations of 0..7 (5040 orders). Every one
        // must yield a valid tree with identical in-order output.
        fn permutations(items: &mut StdVec<u64>, k: usize, out: &mut StdVec<StdVec<u64>>) {
            if k == items.len() {
                out.push(items.clone());
                return;
            }
            for i in k..items.len() {
                items.swap(k, i);
                permutations(items, k + 1, out);
                items.swap(k, i);
            }
        }
        let mut orders = StdVec::new();
        permutations(&mut (0..7u64).collect(), 0, &mut orders);
        assert_eq!(orders.len(), 5040);
        for order in orders {
            let mut t = BTreeIndex::new(16);
            for (row, &k) in order.iter().enumerate() {
                t.insert(k, row as u64);
            }
            t.check_invariants();
            let mut seen = StdVec::new();
            t.for_each_from(0, |k, _| {
                seen.push(k);
                true
            });
            assert_eq!(seen, (0..7u64).collect::<StdVec<_>>(), "order {order:?}");
        }
    }

    #[test]
    fn ascending_descending_and_boundary_keys() {
        for keys in [
            (0..200u64).collect::<StdVec<_>>(),
            (0..200u64).rev().collect(),
            vec![0, u64::MAX, 1, u64::MAX - 1, u64::MAX / 2],
        ] {
            let mut t = BTreeIndex::new(256);
            for (row, &k) in keys.iter().enumerate() {
                t.insert(k, row as u64);
                t.check_invariants();
            }
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            let mut seen = StdVec::new();
            t.for_each_from(0, |k, _| {
                seen.push(k);
                true
            });
            assert_eq!(seen, sorted);
        }
    }

    #[test]
    fn range_start_positions_are_exact() {
        let mut t = BTreeIndex::new(128);
        for k in (0..100u64).map(|i| i * 3) {
            t.insert(k, k);
        }
        // Start exactly on a key, between keys, past the end.
        for start in [0u64, 1, 3, 148, 149, 150, 297, 298, 1000] {
            let mut first = None;
            t.for_each_from(start, |k, _| {
                first = Some(k);
                false
            });
            let expected = (0..100u64).map(|i| i * 3).find(|&k| k >= start);
            assert_eq!(first, expected, "start={start}");
        }
    }

    #[test]
    fn pool_bound_holds_at_capacity() {
        let rows = 4096u64;
        let mut t = BTreeIndex::new(rows);
        // Adversarial-ish order: interleave ends to force splits everywhere.
        for i in 0..rows {
            let k = if i % 2 == 0 { i } else { rows * 2 - i };
            t.insert(k, i);
        }
        t.check_invariants();
        assert_eq!(t.len(), rows);
        assert!(
            (t.nodes_used() as u64) < pool_nodes_for(rows),
            "pool usage {} not below bound {}",
            t.nodes_used(),
            pool_nodes_for(rows)
        );
    }

    // ---- mutation-gap closures ------------------------------------------
    //
    // Each test below kills a mutant that survived the first full
    // cargo-mutants run: constants and accessors pinned to exact values,
    // and — the larger class — proof that the invariant CHECKERS have
    // teeth. A checker that silently stops checking is worse than no
    // checker, because everything else in this project leans on it.

    #[test]
    fn pool_sizing_and_checker_constants_are_exact() {
        assert_eq!(pool_nodes_for(0), 16);
        assert_eq!(pool_nodes_for(1), 16);
        assert_eq!(pool_nodes_for(3), 17);
        assert_eq!(pool_nodes_for(96), 48);
        assert_eq!(pool_nodes_for(1_000_000), 333_349);
        // ORDER=8: ceil(8/2) - 1 = 3 keys minimum in a non-root leaf.
        assert_eq!(HALF_MIN, 3);
    }

    #[test]
    fn routing_is_strictly_less() {
        // Equality routes RIGHT: the separator's key lives in the right
        // subtree. Unreachable via insert (duplicates rejected upstream),
        // so pinned here directly.
        assert!(routes_before(1, 2));
        assert!(!routes_before(2, 2));
        assert!(!routes_before(3, 2));
    }

    #[test]
    fn emptiness_and_node_accounting_are_observable() {
        let mut t = BTreeIndex::new(64);
        assert!(t.is_empty());
        assert_eq!(t.nodes_used(), 1, "a fresh tree is exactly its root");
        t.insert(1, 0);
        assert!(!t.is_empty());
        // ORDER+1 keys force the first split: root + two leaves.
        for k in 2..=9u64 {
            t.insert(k, k);
        }
        assert_eq!(t.nodes_used(), 3, "first split must cost exactly 2 nodes");
    }

    /// Hand-build a tree whose ONLY defect is an under-occupied non-root
    /// internal node — every other invariant (order, bounds, depth up to
    /// the panic point) holds, so nothing but the occupancy check can
    /// catch it.
    #[test]
    #[should_panic(expected = "under-occupied")]
    fn checker_catches_under_occupied_internal() {
        let mut t = BTreeIndex::new(64);
        let leaf = |keys: &[u64], next: u32| {
            let mut n = Node::empty_leaf();
            for (i, &k) in keys.iter().enumerate() {
                n.keys[i] = k;
            }
            n.len = keys.len() as u8;
            n.next = next;
            n
        };
        // root(internal) -> [under-occupied internal, leaf]
        t.pool[0] = Node {
            keys: [100, 0, 0, 0, 0, 0, 0, 0],
            vals: [0; ORDER],
            children: [1, 2, NIL, NIL, NIL, NIL, NIL, NIL, NIL],
            next: NIL,
            len: 1,
            leaf: false,
        };
        t.pool[1] = Node {
            keys: [40, 0, 0, 0, 0, 0, 0, 0],
            vals: [0; ORDER],
            children: [3, 4, NIL, NIL, NIL, NIL, NIL, NIL, NIL],
            next: NIL,
            len: 1, // the defect: internals need >= ORDER/2 separators
            leaf: false,
        };
        t.pool[2] = leaf(&[100, 110, 120, 130], NIL);
        t.pool[3] = leaf(&[10, 20, 30], 4);
        t.pool[4] = leaf(&[40, 50, 60], 2);
        t.root = 0;
        t.used = 5;
        t.len = 10;
        t.check_invariants();
    }

    /// Hand-build a tree whose ONLY defect is leaves at unequal depth —
    /// occupancy, ordering, bounds, count, and the leaf chain are all
    /// consistent, so only the depth-uniformity check can catch it.
    #[test]
    #[should_panic(expected = "unequal depth")]
    fn checker_catches_unequal_leaf_depth() {
        let mut t = BTreeIndex::new(64);
        let leaf = |keys: &[u64], next: u32| {
            let mut n = Node::empty_leaf();
            for (i, &k) in keys.iter().enumerate() {
                n.keys[i] = k;
            }
            n.len = keys.len() as u8;
            n.next = next;
            n
        };
        // root -> [leaf at depth 1, internal at depth 1 -> 5 leaves at 2]
        t.pool[0] = Node {
            keys: [100, 0, 0, 0, 0, 0, 0, 0],
            vals: [0; ORDER],
            children: [1, 2, NIL, NIL, NIL, NIL, NIL, NIL, NIL],
            next: NIL,
            len: 1,
            leaf: false,
        };
        t.pool[1] = leaf(&[10, 20, 30], 3);
        t.pool[2] = Node {
            keys: [200, 300, 400, 500, 0, 0, 0, 0],
            vals: [0; ORDER],
            children: [3, 4, 5, 6, 7, NIL, NIL, NIL, NIL],
            next: NIL,
            len: 4,
            leaf: false,
        };
        t.pool[3] = leaf(&[110, 120, 130], 4);
        t.pool[4] = leaf(&[210, 220, 230], 5);
        t.pool[5] = leaf(&[310, 320, 330], 6);
        t.pool[6] = leaf(&[410, 420, 430], 7);
        t.pool[7] = leaf(&[510, 520, 530], NIL);
        t.root = 0;
        t.used = 8;
        t.len = 18;
        t.check_invariants();
    }

    /// A legal 3-key non-root leaf must PASS: the checker's minimum is
    /// HALF_MIN = 3, and a checker that rejects legal trees is as broken
    /// as one that accepts corrupt ones.
    #[test]
    fn checker_accepts_minimum_occupancy_leaf() {
        let mut t = BTreeIndex::new(64);
        let leaf = |keys: &[u64], next: u32| {
            let mut n = Node::empty_leaf();
            for (i, &k) in keys.iter().enumerate() {
                n.keys[i] = k;
            }
            n.len = keys.len() as u8;
            n.next = next;
            n
        };
        t.pool[0] = Node {
            keys: [100, 0, 0, 0, 0, 0, 0, 0],
            vals: [0; ORDER],
            children: [1, 2, NIL, NIL, NIL, NIL, NIL, NIL, NIL],
            next: NIL,
            len: 1,
            leaf: false,
        };
        t.pool[1] = leaf(&[10, 20, 30], 2); // exactly HALF_MIN keys
        t.pool[2] = leaf(&[100, 110, 120, 130], NIL);
        t.root = 0;
        t.used = 3;
        t.len = 7;
        t.check_invariants();
    }

    /// The leaf-chain cycle guard must fire on an actual cycle. (Under the
    /// mutant that disables the counter this test hangs instead of
    /// panicking, which the mutation runner counts as a timeout kill.)
    #[test]
    #[should_panic(expected = "leaf chain cycle")]
    fn chain_walk_detects_cycles() {
        let mut t = BTreeIndex::new(64);
        for k in 0..30u64 {
            t.insert(k, k);
        }
        for id in 0..t.used {
            if t.pool[id as usize].leaf {
                t.pool[id as usize].next = id; // every leaf now self-loops
            }
        }
        t.for_each_from(0, |_, _| true);
    }
}
