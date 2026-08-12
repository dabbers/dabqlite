//! The blob-zone allocator (docs/DESIGN.md §4.5).
//!
//! The enemy was never variable length; it was the opaque, unbounded,
//! process-shared allocator. This one is ours, inside our own arena:
//!
//! - Power-of-two size classes from [`MIN_BLOCK`] to [`BLOB_HARD_MAX`].
//! - Per-class intrusive LIFO free lists: a free block's first four bytes
//!   hold the arena offset of the next free block of its class.
//! - O(1) alloc and free. No coalescing, no external fragmentation.
//!   Internal fragmentation is bounded by the 2x class ratio and published
//!   via [`BlobAllocator::stats`].
//! - Capacity fixed at open; one arena allocation; no allocation afterward.
//!
//! The hard ceiling [`BLOB_HARD_MAX`] is a build-time constant (a schema may
//! lower it, never raise it). Everything above it belongs in object storage
//! with an `ExternalRef` key in the row. Consequences we want: all reads
//! materialize into a bounded buffer, and the class table is small enough
//! that its worst case fits in a sentence: *an allocation wastes at most
//! half its block, and a block is at most 256 KiB.*

use alloc::vec;
use alloc::vec::Vec;

/// Hard ceiling on a single blob, at build time. 256 KiB.
pub const BLOB_HARD_MAX: u32 = 1 << MAX_BLOCK_LOG2;
/// Smallest block class: 64 bytes (must hold at least the 4-byte free link).
pub const MIN_BLOCK: u32 = 1 << MIN_BLOCK_LOG2;

const MIN_BLOCK_LOG2: u32 = 6;
const MAX_BLOCK_LOG2: u32 = 18;
/// 64 B, 128 B, ..., 256 KiB.
pub const NUM_CLASSES: usize = (MAX_BLOCK_LOG2 - MIN_BLOCK_LOG2 + 1) as usize;

/// Sentinel for "no next free block".
const NIL: u32 = u32::MAX;

/// A ticket for a live blob. `offset`/`class` locate the block; `len` is the
/// exact byte length stored (docs/DESIGN.md §4.5's `{ page, class, len }`,
/// with a byte offset standing in for the page until paging exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobHandle {
    pub offset: u32,
    pub class: u8,
    pub len: u32,
}

/// Allocation failures, first-class per docs/DESIGN.md §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobError {
    /// The value exceeds the build-time ceiling. Store it in object storage
    /// and keep an `ExternalRef` in the row instead.
    TooLarge { len: u32, hard_max: u32 },
    /// The blob zone is out of space for this class. The error names the
    /// block size that failed and the configured capacity.
    Full { block_bytes: u32, capacity: u64 },
}

/// Exact accounting, published so hosts can alarm before hitting the wall
/// and so tests can assert conservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobStats {
    /// Arena bytes configured at open.
    pub capacity: u64,
    /// Bytes ever carved from the arena (monotonic; freed blocks recycle
    /// within their class rather than returning here).
    pub high_water: u64,
    /// Bytes in blocks currently allocated to live handles.
    pub live_block_bytes: u64,
    /// Bytes of those blocks actually requested (len sums). The difference
    /// from `live_block_bytes` is the published internal fragmentation.
    pub live_payload_bytes: u64,
    /// Bytes sitting on free lists, reusable for their class.
    pub free_list_bytes: u64,
    /// Live handle count.
    pub live_count: u64,
}

pub struct BlobAllocator {
    arena: Vec<u8>,
    /// Head of the free list per class (arena offset), NIL when empty.
    free_heads: [u32; NUM_CLASSES],
    /// Free blocks per class (for accounting and termination bounds).
    free_counts: [u32; NUM_CLASSES],
    high_water: u32,
    live_block_bytes: u64,
    live_payload_bytes: u64,
    live_count: u64,
    /// Negative space: the arena must never move (no allocation after init).
    arena_addr: usize,
}

/// Block size in bytes for a class index.
fn block_size(class: u8) -> u32 {
    debug_assert!((class as usize) < NUM_CLASSES);
    1 << (MIN_BLOCK_LOG2 + class as u32)
}

/// Smallest class whose block holds `len` bytes.
fn class_for(len: u32) -> u8 {
    debug_assert!(len <= BLOB_HARD_MAX);
    let needed = len.max(MIN_BLOCK).next_power_of_two();
    let class = (needed.trailing_zeros() - MIN_BLOCK_LOG2) as u8;
    // Pair assertion: the chosen class fits, and the one below would not.
    debug_assert!(block_size(class) >= len);
    debug_assert!(class == 0 || block_size(class - 1) < len.max(MIN_BLOCK));
    class
}

impl BlobAllocator {
    /// One arena allocation for the whole zone. `capacity` is the open-time
    /// declared size (docs/DESIGN.md §4.2); offsets are u32 so the zone is
    /// capped below 4 GiB.
    pub fn new(capacity: u64) -> Self {
        assert!(capacity >= MIN_BLOCK as u64, "zone smaller than one block");
        assert!(capacity < u32::MAX as u64, "blob zone offsets are u32");
        let arena = vec![0u8; capacity as usize];
        let arena_addr = arena.as_ptr() as usize;
        BlobAllocator {
            arena,
            free_heads: [NIL; NUM_CLASSES],
            free_counts: [0; NUM_CLASSES],
            high_water: 0,
            live_block_bytes: 0,
            live_payload_bytes: 0,
            live_count: 0,
            arena_addr,
        }
    }

    pub fn stats(&self) -> BlobStats {
        BlobStats {
            capacity: self.arena.len() as u64,
            high_water: self.high_water as u64,
            live_block_bytes: self.live_block_bytes,
            live_payload_bytes: self.live_payload_bytes,
            free_list_bytes: self.free_list_bytes(),
            live_count: self.live_count,
        }
    }

    fn free_list_bytes(&self) -> u64 {
        let mut total = 0u64;
        for class in 0..NUM_CLASSES {
            total += self.free_counts[class] as u64 * block_size(class as u8) as u64;
        }
        total
    }

    fn assert_invariants(&self) {
        debug_assert_eq!(
            self.arena.as_ptr() as usize,
            self.arena_addr,
            "arena moved: allocation after init is forbidden"
        );
        // Conservation: every byte below the high-water mark is either in a
        // live block or on a free list. Exactly.
        debug_assert_eq!(
            self.live_block_bytes + self.free_list_bytes(),
            self.high_water as u64,
            "byte accounting leaked"
        );
        // Payloads never exceed their blocks.
        debug_assert!(self.live_payload_bytes <= self.live_block_bytes);
        debug_assert!(self.high_water as usize <= self.arena.len());
    }

    /// Allocate a block for `len` bytes. O(1): pop the class free list or
    /// bump the high-water mark.
    pub fn alloc(&mut self, len: u32) -> Result<BlobHandle, BlobError> {
        self.assert_invariants();
        if len > BLOB_HARD_MAX {
            return Err(BlobError::TooLarge {
                len,
                hard_max: BLOB_HARD_MAX,
            });
        }
        let class = class_for(len);
        let size = block_size(class);

        let offset = if self.free_heads[class as usize] != NIL {
            // Pop the LIFO free list; the link lives in the block itself.
            let offset = self.free_heads[class as usize];
            debug_assert!(offset + size <= self.high_water, "free link out of bounds");
            self.free_heads[class as usize] = self.read_link(offset);
            self.free_counts[class as usize] -= 1;
            offset
        } else {
            // Bump. Blocks are carved exactly once; only the free lists
            // recycle them, so no two live blocks can ever overlap.
            let offset = self.high_water;
            let Some(new_high) = offset
                .checked_add(size)
                .filter(|&h| h as usize <= self.arena.len())
            else {
                return Err(BlobError::Full {
                    block_bytes: size,
                    capacity: self.arena.len() as u64,
                });
            };
            self.high_water = new_high;
            offset
        };

        self.live_block_bytes += size as u64;
        self.live_payload_bytes += len as u64;
        self.live_count += 1;
        let handle = BlobHandle { offset, class, len };
        self.assert_invariants();
        Ok(handle)
    }

    /// Return a block to its class free list. O(1).
    pub fn free(&mut self, handle: BlobHandle) {
        self.assert_invariants();
        let size = block_size(handle.class);
        assert!(handle.len <= BLOB_HARD_MAX);
        assert_eq!(
            class_for(handle.len),
            handle.class,
            "handle len does not match its class: forged or corrupted handle"
        );
        assert!(
            handle.offset + size <= self.high_water,
            "handle points beyond allocated space"
        );
        // Push onto the LIFO list, link stored in the block's first bytes.
        self.write_link(handle.offset, self.free_heads[handle.class as usize]);
        self.free_heads[handle.class as usize] = handle.offset;
        self.free_counts[handle.class as usize] += 1;
        self.live_block_bytes -= size as u64;
        self.live_payload_bytes -= handle.len as u64;
        self.live_count -= 1;
        self.assert_invariants();
    }

    /// The payload bytes for a live handle.
    pub fn data(&self, handle: BlobHandle) -> &[u8] {
        let start = handle.offset as usize;
        &self.arena[start..start + handle.len as usize]
    }

    /// Mutable payload bytes for a live handle.
    pub fn data_mut(&mut self, handle: BlobHandle) -> &mut [u8] {
        let start = handle.offset as usize;
        &mut self.arena[start..start + handle.len as usize]
    }

    fn read_link(&self, offset: u32) -> u32 {
        let start = offset as usize;
        u32::from_le_bytes(self.arena[start..start + 4].try_into().expect("fixed"))
    }

    fn write_link(&mut self, offset: u32, next: u32) {
        let start = offset as usize;
        self.arena[start..start + 4].copy_from_slice(&next.to_le_bytes());
    }

    /// Walk every free list, yielding `(offset, class)` per free block.
    /// For test-side validation (disjointness against live handles, list
    /// integrity); deterministic order.
    pub fn for_each_free_block(&self, mut f: impl FnMut(u32, u8)) {
        for class in 0..NUM_CLASSES {
            let mut cursor = self.free_heads[class];
            let mut steps = 0u32;
            while cursor != NIL {
                // Termination: a cycle in a free list would exceed the count.
                assert!(
                    steps <= self.free_counts[class],
                    "free list cycle detected in class {class}"
                );
                f(cursor, class as u8);
                cursor = self.read_link(cursor);
                steps += 1;
            }
            assert_eq!(
                steps, self.free_counts[class],
                "free list length diverged from its count"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_selection() {
        assert_eq!(class_for(0), 0);
        assert_eq!(class_for(1), 0);
        assert_eq!(class_for(64), 0);
        assert_eq!(class_for(65), 1);
        assert_eq!(class_for(128), 1);
        assert_eq!(class_for(BLOB_HARD_MAX), (NUM_CLASSES - 1) as u8);
        assert_eq!(block_size((NUM_CLASSES - 1) as u8), BLOB_HARD_MAX);
    }

    #[test]
    fn too_large_is_first_class() {
        let mut a = BlobAllocator::new(1 << 20);
        assert_eq!(
            a.alloc(BLOB_HARD_MAX + 1),
            Err(BlobError::TooLarge {
                len: BLOB_HARD_MAX + 1,
                hard_max: BLOB_HARD_MAX
            })
        );
    }

    #[test]
    fn lifo_reuse_is_exact() {
        let mut a = BlobAllocator::new(1 << 20);
        let h1 = a.alloc(100).unwrap();
        let h2 = a.alloc(100).unwrap();
        assert_ne!(h1.offset, h2.offset);
        a.free(h1);
        // LIFO: the freed block is the next one handed out for its class.
        let h3 = a.alloc(90).unwrap();
        assert_eq!(h3.offset, h1.offset);
        assert_eq!(h3.class, h1.class);
    }

    #[test]
    fn full_names_the_block_size() {
        let mut a = BlobAllocator::new(256);
        let _ = a.alloc(64).unwrap();
        let _ = a.alloc(64).unwrap();
        let _ = a.alloc(64).unwrap();
        let _ = a.alloc(30).unwrap();
        assert_eq!(
            a.alloc(64),
            Err(BlobError::Full {
                block_bytes: 64,
                capacity: 256
            })
        );
    }

    #[test]
    fn full_cycle_returns_to_initial_state() {
        // The §7.5 allocator invariant: alloc-then-free everything leaks
        // exactly zero.
        let mut a = BlobAllocator::new(1 << 20);
        let mut handles = alloc::vec::Vec::new();
        for i in 0..64u32 {
            handles.push(a.alloc((i * 37) % 4096).unwrap());
        }
        let s = a.stats();
        assert_eq!(s.live_count, 64);
        for h in handles {
            a.free(h);
        }
        let s = a.stats();
        assert_eq!(s.live_count, 0);
        assert_eq!(s.live_block_bytes, 0);
        assert_eq!(s.live_payload_bytes, 0);
        assert_eq!(s.free_list_bytes, s.high_water, "leak: bytes unaccounted");
    }

    #[test]
    fn payload_roundtrip() {
        let mut a = BlobAllocator::new(1 << 16);
        let h = a.alloc(11).unwrap();
        a.data_mut(h).copy_from_slice(b"hello world");
        assert_eq!(a.data(h), b"hello world");
        assert_eq!(a.data(h).len(), 11);
    }
}
