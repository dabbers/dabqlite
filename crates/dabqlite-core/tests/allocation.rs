// The ONLY unsafe code in the entire workspace (production code has
// none; `unsafe_code` is denied workspace-wide): a global allocator must
// be `unsafe impl` by language rule — there is no safe-Rust way to
// observe every allocation. It delegates every call verbatim to `System`
// and only increments a counter; the unsafety is the trait signature,
// not any behavior.
#![allow(unsafe_code)]
//! Memory exhaustion, made structurally impossible after init — and
//! PROVEN as a count, not argued from discipline.
//!
//! The design (docs/DESIGN.md §4.2) allocates every arena at init, sized
//! from declared capacities. The pointer-move tripwires catch an arena
//! that reallocated; this suite goes further with a counting global
//! allocator: after `Engine::new`, driving the engine through init,
//! commits, reads, range scans, substring searches, AND a full recovery
//! performs **zero heap allocations** — provided the host lends borrowed
//! read buffers, which the sans-I/O protocol makes natural
//! (`ReadDone { data: &[u8] }`).
//!
//! An engine that cannot allocate after init cannot OOM after init: OOM
//! is confined to construction, where Rust's allocator failure aborts
//! before a single file byte is written — proven against a REAL
//! RLIMIT_AS-induced OOM in `dabqlite-host/tests/oom.rs`.
//!
//! NOTE: this file deliberately contains exactly ONE #[test], so no
//! sibling test thread can allocate inside the measured window.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

struct CountingAlloc;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

use dabqlite_core::{Capacities, Engine, Input, Output, VALUE_LEN};

const CAPS: Capacities = Capacities { rows: 64 };
const SB_MAX: usize = 4096;
const ROWS_MAX: usize = 64 * 32;

/// Drive the engine against preallocated file images, lending borrowed
/// slices for reads — the zero-allocation host. Returns the terminal.
fn drive(engine: &mut Engine, sb: &mut [u8], rows: &mut [u8], first: Input<'_>) -> Output {
    let mut out = engine.tick(first);
    loop {
        match out {
            Output::Read { file, offset, len } => {
                let buf: &[u8] = match file {
                    dabqlite_core::FileId::Superblock => sb,
                    dabqlite_core::FileId::Rows => rows,
                    dabqlite_core::FileId::RowsOld => &[],
                };
                let start = (offset as usize).min(buf.len());
                let end = ((offset + len) as usize).min(buf.len());
                // The borrow lives only for this tick; no copy, no alloc.
                let data = &buf[start..end];
                // SAFETY-free split: engine and buffers are disjoint.
                out = engine.tick(Input::ReadDone { file, data });
            }
            Output::Write { file, offset, data } => {
                let buf: &mut [u8] = match file {
                    dabqlite_core::FileId::Superblock => sb,
                    dabqlite_core::FileId::Rows => rows,
                    dabqlite_core::FileId::RowsOld => unreachable!("engine never writes RowsOld"),
                };
                let end = offset as usize + data.as_slice().len();
                assert!(end <= buf.len(), "preallocated image too small");
                buf[offset as usize..end].copy_from_slice(data.as_slice());
                out = engine.tick(Input::WriteDone { file });
            }
            Output::Fsync { file } => {
                out = engine.tick(Input::FsyncDone { file });
            }
            terminal => return terminal,
        }
    }
}

#[test]
fn steady_state_performs_zero_heap_allocations() {
    // Construction allocates (the one and only time) — count it happening.
    let before_new = ALLOCATIONS.load(Ordering::SeqCst);
    let mut engine = Engine::new(CAPS);
    let init_allocs = ALLOCATIONS.load(Ordering::SeqCst) - before_new;
    assert!(init_allocs > 0, "init must be where allocation happens");

    // Preallocated file images and lengths — the host's read buffers.
    let mut sb = vec![0u8; SB_MAX];
    let mut rows = vec![0u8; ROWS_MAX];
    let mut sb_len = 0u64;
    let mut rows_len = 0u64;

    // --- the measured window: EVERYTHING after init ---------------------
    let start = ALLOCATIONS.load(Ordering::SeqCst);

    // Fresh init-open (writes + fsyncs, no reads).
    match drive(
        &mut engine,
        &mut sb,
        &mut rows,
        Input::Open {
            superblock_len: sb_len,
            rows_len,
        },
    ) {
        Output::OpenDone { result: Ok(0) } => {}
        other => panic!("fresh open: {other:?}"),
    }
    sb_len = 256;

    // Commits, point reads, range pages, substring pages — a real workload.
    for i in 0..CAPS.rows {
        let value = [(i as u8).wrapping_mul(37); VALUE_LEN];
        match drive(
            &mut engine,
            &mut sb,
            &mut rows,
            Input::Insert { id: i, value },
        ) {
            Output::InsertDone { result: Ok(()), .. } => {}
            other => panic!("insert {i}: {other:?}"),
        }
        rows_len = (i + 1) * 32;
    }
    for i in 0..CAPS.rows {
        match drive(&mut engine, &mut sb, &mut rows, Input::Get { id: i }) {
            Output::GetDone {
                result: Ok(Some(_)),
                ..
            } => {}
            other => panic!("get {i}: {other:?}"),
        }
    }
    let mut cursor = 0u64;
    loop {
        let page = match drive(
            &mut engine,
            &mut sb,
            &mut rows,
            Input::Range {
                lo: cursor,
                hi: u64::MAX,
            },
        ) {
            Output::RangeDone { result: Ok(p) } => p,
            other => panic!("range: {other:?}"),
        };
        match page.next {
            Some(n) => cursor = n,
            None => break,
        }
    }
    let needle_owner = [37u8.wrapping_mul(1); VALUE_LEN];
    let mut needle = [0u8; VALUE_LEN];
    needle.copy_from_slice(&needle_owner);
    match drive(
        &mut engine,
        &mut sb,
        &mut rows,
        Input::Find {
            needle,
            needle_len: 3,
            after: None,
        },
    ) {
        Output::FindDone { result: Ok(_) } => {}
        other => panic!("find: {other:?}"),
    }

    // Full recovery on a SECOND engine constructed BEFORE the window
    // would hide its init allocs — so construct it outside afterwards
    // is impossible; instead: recovery on the SAME engine is a protocol
    // violation. Measure recovery separately below.
    let steady_allocs = ALLOCATIONS.load(Ordering::SeqCst) - start;
    assert_eq!(
        steady_allocs, 0,
        "the engine allocated {steady_allocs} times after init — \
         docs/DESIGN.md §4.2 forbids every one of them"
    );

    // --- recovery window: a new engine's post-init recovery ------------
    let mut engine2 = Engine::new(CAPS); // allocates: outside the window
    let start = ALLOCATIONS.load(Ordering::SeqCst);
    match drive(
        &mut engine2,
        &mut sb,
        &mut rows,
        Input::Open {
            superblock_len: sb_len,
            rows_len,
        },
    ) {
        Output::OpenDone { result: Ok(n) } if n == CAPS.rows => {}
        other => panic!("recovery: {other:?}"),
    }
    // Post-recovery reads on the recovered engine, still zero.
    for i in (0..CAPS.rows).step_by(7) {
        match drive(&mut engine2, &mut sb, &mut rows, Input::Get { id: i }) {
            Output::GetDone {
                result: Ok(Some(_)),
                ..
            } => {}
            other => panic!("recovered get {i}: {other:?}"),
        }
    }
    let recovery_allocs = ALLOCATIONS.load(Ordering::SeqCst) - start;
    assert_eq!(
        recovery_allocs, 0,
        "recovery allocated {recovery_allocs} times — rebuilding every \
         index must reuse the init-time arenas"
    );
}
