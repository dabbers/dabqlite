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

What the harness currently proves — the full fault matrix with suites and
guarantees lives in [docs/FAULTS.md](docs/FAULTS.md):

- **Crash faults**: every unsynced write independently survives, vanishes,
  tears to a prefix, tears to an arbitrary sector subset, or persists a
  garbage sector. Swept at every I/O boundary with random fates, *and*
  exhaustively enumerated (every boundary × every fate combination), plus
  multi-crash lifetimes including crashes *during* recovery.
- **I/O failures** (EIO, fail-stop): swept at every I/O index. Recovery
  fsyncs before serving, so state visible after a restart never regresses
  at the next power loss (mutation-verified).
- **Media faults**: exhaustive per-byte bit rot over the superblock zone
  (zero loss) and all committed rows (always detected, never served wrong);
  truncation and garbage-extension sweeps of both files.
- **Misdirected writes** (firmware lies): every write × shift/cross-file
  grid — never silently wrong. This sweep found a real bug on first run.
- **Read-path faults**: transient bit flips in read buffers (exhaustive,
  every byte of both recovery reads) and misdirected reads returning valid
  bytes from the wrong offset — positional validation keeps them honest.
- **Lying fsyncs** (fsyncgate): swept at every fsync; loss is bounded to the
  final commit for a single lie, survivors are always an exact in-order
  prefix, and recovery scans past the manifest for orphaned rows —
  rolled-back acknowledged commits are *flagged* (`rollback_evidence`)
  whenever their evidence survives. Silent only when no distinguishing bit
  exists on disk.
- **The storm** (`storm.rs`): every in-budget fault class layered into
  single lifetimes — crashes, EIO fail-stops, recovery crashes, media
  faults, transient read faults — under a strict perfection invariant:
  zero loss, zero drift, always. The fault-budget contract is spelled out
  in [docs/FAULTS.md](docs/FAULTS.md).
- **Sequencing faults**: host-protocol violations panic loudly; client ops
  mid-I/O get `Busy` (v1 serializes everything).
- **Capacity walls**: fill to N-1 / N / N+1, crash at the boundary of the
  last slot, recover — the wall holds and the error names the fix.
- **Allocator invariants** (§7.5): no two live blocks overlap, byte
  accounting is exact, a full alloc-then-free cycle leaks zero.
- **Harness self-checks**: coverage floors on every interesting path, and a
  determinism meta-test (same seed → bit-for-bit identical lifetime).
- **Ordered index + range queries**: B+tree with a `BTreeMap` oracle
  (random fuzz plus all 5,040 insertion orders of small sets), paged
  `:many` range scans checked row-exact against the oracle — including
  process restarts halfway through a paged result — and the rebuilt tree
  re-verified in-order after every fault schedule in every lifetime.

Read [docs/DESIGN.md](docs/DESIGN.md) first. The testing strategy is the
primary objective of the project; the database is the vehicle.

## Layout

```
schema/
  records.sql     The schema: Postgres DDL + annotations. Single source of
                  truth for layout and SCHEMA_HASH.
crates/
  dabqlite-core   The pure state machine: tick(input) -> output. No I/O, no
                  clock, no randomness, no allocation after init. Zero
                  dependencies; must always build for wasm32-unknown-unknown.
                  Also home of the blob-zone allocator (power-of-two classes,
                  intrusive LIFO free lists, O(1) alloc/free).
  dabqlite-codegen The schema compiler: DDL in, layout + typed Rust codec
                  out. The generated codec is proven byte-identical to the
                  hand codec the fault matrix validated; SCHEMA_HASH is
                  derived, pinned by test, drift-checked in CI.
  dabqlite-host   The storage seam (trait shaped by OPFS sync access
                  handles) plus the generic host driver and the real POSIX
                  file backend. Equivalence tests prove the simulator and
                  real disk produce byte-identical files and identical
                  fault outcomes — the simulation is not a fiction.
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

The soak reports auditable simulated-time accounting (model documented in
[docs/FAULTS.md](docs/FAULTS.md)). A 20,000-lifetime sample run:

```
vopr: 20000 lifetimes | 1018993 commits | 2334702 reads, 3781788 writes, 4495950 fsyncs
vopr: faults survived: 331281 crashes, 83047 EIO fail-stops, 207198 crashes mid-recovery
vopr: simulated operational time: 864.5 h (1.3 h of device I/O + 621526 restart cycles at 5s)
vopr: wall clock 10.9 s -> 285129x real time
```

Every one of those simulated hours is spent inside the fault schedule —
crashes mid-commit, EIO storms, crashes during recovery — at a fault
density millions of times production reality, with every acknowledged
commit verified against the oracle after every recovery.

Every simulation failure message carries the `(seed, boundary)` pair that
reproduces it exactly, on any machine.
