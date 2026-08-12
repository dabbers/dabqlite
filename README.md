# dabqlite

An embeddable, schema-compiled record store with a declared memory ceiling, a
deterministic core, and a web-native execution model.

**Status: vertical slice** ([design §9 step 1](docs/DESIGN.md#9-build-order)) —
one table, insert and get-by-id, arena at open, superblock durability, and a
deterministic simulation harness with a crash injector. The
[§7.3 crash-recovery property](docs/DESIGN.md#73-the-test-that-means-the-harness-works)
passes: crash at every I/O boundary, recover, state is exactly N or N+1
committed inserts, never in between — reproducible from a single integer seed.

Read [docs/DESIGN.md](docs/DESIGN.md) first. The testing strategy is the
primary objective of the project; the database is the vehicle.

## Layout

```
crates/
  dabqlite-core   The pure state machine: tick(input) -> output. No I/O, no
                  clock, no randomness, no allocation after init. Zero
                  dependencies; must always build for wasm32-unknown-unknown.
  dabqlite-sim    The deterministic simulator: simulated disk with a crash
                  model (writes survive / vanish / tear), crash-boundary
                  injection, seeded workloads, model-based oracles.
docs/
  DESIGN.md       The seed design & requirements document. Load-bearing.
```

## Running the tests

```sh
cargo test --workspace                                  # everything
cargo test -p dabqlite-sim --test crash_recovery        # the §7.3 sweep
cargo build -p dabqlite-core --target wasm32-unknown-unknown  # determinism gate
```

Every simulation failure message carries the `(seed, boundary)` pair that
reproduces it exactly, on any machine.
