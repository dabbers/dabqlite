# dabqlite

An embeddable, schema-compiled record store with a declared memory ceiling, a
deterministic core, and a web-native execution model.

**Status: vertical slice + hardened durability + blob allocator**
([design §9 steps 1 and 4](docs/DESIGN.md#9-build-order)) — one table, insert
and get-by-id, arena at open, superblock copy-set durability, the blob-zone
allocator, and a deterministic simulation harness with crash and media-fault
injection. The
[§7.3 crash-recovery property](docs/DESIGN.md#73-the-test-that-means-the-harness-works)
passes: crash at every I/O boundary, recover, state is exactly N or N+1
committed inserts, never in between — reproducible from a single integer seed.

What the harness currently proves:

- **Crash faults**: every unsynced write independently survives, vanishes, or
  tears at crash. Swept at every I/O boundary, plus multi-crash lifetimes
  (crash → recover → keep writing → crash again, including crashes *during*
  recovery).
- **Media faults**: each superblock generation lives in two of four slots, so
  any single corrupted copy loses nothing; a corrupted committed row is
  *detected* and reported, never served silently wrong.
- **Capacity walls**: fill to N-1 / N / N+1, crash at the boundary of the
  last slot, recover — the wall holds and the error names the fix.
- **Allocator invariants** (§7.5): no two live blocks overlap, byte
  accounting is exact, a full alloc-then-free cycle leaks zero.

Read [docs/DESIGN.md](docs/DESIGN.md) first. The testing strategy is the
primary objective of the project; the database is the vehicle.

## Layout

```
crates/
  dabqlite-core   The pure state machine: tick(input) -> output. No I/O, no
                  clock, no randomness, no allocation after init. Zero
                  dependencies; must always build for wasm32-unknown-unknown.
                  Also home of the blob-zone allocator (power-of-two classes,
                  intrusive LIFO free lists, O(1) alloc/free).
  dabqlite-sim    The deterministic simulator: simulated disk with crash and
                  bit-rot fault models, crash-boundary injection, seeded
                  workloads, model-based oracles, whole-lifetime runs, and
                  the `vopr` soak binary.
docs/
  DESIGN.md       The seed design & requirements document. Load-bearing.
```

## Running the tests

```sh
cargo test --workspace                                  # everything
cargo test -p dabqlite-sim --test crash_recovery        # the §7.3 sweep
cargo build -p dabqlite-core --target wasm32-unknown-unknown  # determinism gate

# Soak: randomized lifetimes until interrupted; every run prints its seed.
cargo run --release -p dabqlite-sim --bin vopr
cargo run --release -p dabqlite-sim --bin vopr -- 12345   # reproduce a seed
```

Every simulation failure message carries the `(seed, boundary)` pair that
reproduces it exactly, on any machine.
