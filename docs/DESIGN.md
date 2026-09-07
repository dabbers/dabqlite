# Design and Requirements

Status: seed document. Written before any code exists. Decisions here are load-bearing
on each other; the rationale matters more than the conclusion.

## 1. What this is

An embeddable, schema-compiled record store with a declared memory ceiling, a
deterministic core, and a web-native execution model.

A schema file (Postgres DDL plus annotations) is compiled into a storage engine plus
typed clients. Record layout, query plans, index structures, and file set are all fixed
at build time. Capacity is fixed at open time. The core performs no I/O, no allocation
after init, and no nondeterministic operations.

## 2. What this is not

Ruling these out early, because each one would dissolve a property the rest depends on.

- Not a general-purpose SQL database. Arbitrary runtime SQL reintroduces a planner, a
  heap, and an unbounded operation space.
- Not multi-tenant with user-defined schemas. The schema is not data.
- Not an analytics engine. No column store, no vectorized execution.
- Not a blob store. Values above the ceiling belong in object storage with a key in the row.
- Not multi-writer. One writer, always.

## 3. Goals, in priority order

1. **Correctness that is demonstrable, not asserted.** Every failure mode reproducible
   from a single integer seed.
2. **Predictability.** Memory ceiling, file count, and per-operation work all knowable
   before the program runs.
3. **Web-native.** wasm and OPFS are first-class targets, not a port.
4. **Developer experience.** Familiar schema language, existing tooling works.

Performance is explicitly not in the top four. It should be good because the design
tends that way (fixed layout, no deserialization, no allocation), not because it was
optimized for.

## 4. Core architectural decisions

### 4.1 The core is a pure state machine

```
fn tick(&mut self, input: Input) -> Outputs
```

No I/O, no clock, no randomness, no allocation inside the core. I/O is *returned* as a
request; the host performs it and feeds the result back as another input.

This single decision produces three separate wins and is the reason the project is
coherent:

- **Determinism.** The only nondeterminism sources are at the boundary, so they are
  substitutable with simulated versions.
- **Web-native.** The core never blocks on I/O, so there is no sync/async impedance to
  bridge. No Asyncify, no SharedArrayBuffer, no COOP/COEP headers, no JSPI dependency.
  This is the exact wall that every SQLite-wasm build hits.
- **Portability.** The same core runs against POSIX files, OPFS, or an in-memory
  simulator with no conditional compilation in the core itself.

### 4.2 Layout at compile time, capacity at open time

Field offsets, record widths, comparators, and query plans are generated from the schema
and are constants.

Capacities are supplied at `open()`:

```
open(path, Capacities { users: 1_000_000, events: 50_000_000, blob_bytes: 8 << 30 })
```

One arena allocation per zone at open. No allocation afterward, ever.

Rationale for splitting the two: compile-time layout buys memcpy-able records, zero
deserialization, and constant offsets. Compile-time *capacity* would buy nothing extra
while forcing a recompile whenever an operator needs more rows.

### 4.3 Queries are compiled

Queries are declared in the schema (sqlc / Diesel model: real SQL in, typed functions
out). No parser or planner is linked into the production core.

This is not an optimization. It is what keeps the operation space **finite and
enumerable**, which is the precondition that makes exhaustive simulation affordable.
Arbitrary runtime SQL means you can no longer state what the system might be asked to
do, and the oracle becomes another database.

### 4.4 Multiple files, one atomicity point

The file count is derived from the schema and is knowable before the program runs. One
file per zone: per-entity row slots, per-index, blob, changelog, superblock.

Rationale: independently resizable zones. Hitting the `events` ceiling means extending
one file and updating a manifest, not rewriting the whole arena. This turns capacity
exhaustion from an offline migration into a resize.

Crash consistency:

- The **superblock** is the sole atomicity point. Small, fixed-size, holds a monotonic
  generation number plus the authoritative file set (name, size, header checksum).
- Written with a copy set: N redundant copies, write to the stale ones, fsync, and the
  new generation becomes live.
- Everything else is either append-only or written and fsynced *before* the superblock
  that references it.
- Recovery: read all superblock copies, take the highest generation with a valid
  checksum, verify the file set, ignore anything on disk the manifest doesn't name.
  Orphan files are inert by construction.

**File creation happens only at open, resize, and migration.** Never on the write path.
Changing the file count requires a directory fsync, whose semantics are the least
portable thing in the design (`F_FULLFSYNC` on macOS, different again on Windows and
OPFS). Confining it to three checkpoints means writing that code once, in three places,
and simulating it precisely.

### 4.5 Variable-length data

Records stay fixed-width. A varlen field occupies a fixed-width slot:

- **Inline** up to a threshold (32 or 48 bytes): one length byte plus payload in the slot.
- **Spilled** above it: a fixed 32-byte prefix (for comparison and indexing) plus a
  handle `{ page: u32, class: u8, len: u32 }`.

The blob zone is a declared region with a hand-written allocator: power-of-two size
classes, per-class intrusive LIFO free lists, O(1) alloc and free, no coalescing, no
external fragmentation. Internal fragmentation is bounded and published.

The enemy was never variable length. It was the opaque, unbounded, process-shared
allocator. An allocator you wrote, inside your own arena, is deterministic and
assertable.

**Hard ceiling on blob size, on the order of a few hundred KB**, expressed as
`BLOB_HARD_MAX` at build time. A schema may lower it, never raise it. Consequences:

- All reads materialize into a bounded buffer. No streaming API.
- Extent lists are fixed-size arrays in the record, not chains. Nothing to traverse,
  nothing whose termination needs proving.
- The size class table is small and its worst case fits in a sentence.

The schema ships an `ExternalRef` field type so the intended alternative is first-class.
An opinion with a supported alternative reads as a design stance; without one it reads
as a limitation.

### 4.6 Indices are declared and annotated

Postgres vocabulary: `@index(btree)`, `@index(hash)`, `@index(hnsw, m=16)`,
`@index(trigram)`.

Each method is its own zone with its own arena and its own capacity, and each is a small
isolated component **with a free oracle**: brute-force scan for HNSW, naive substring
for trigram, a HashMap for hash. This is the argument for making indices an extension
point rather than a fixed set.

v1 ships btree, hash, and exactly one hard one, to prove the extension point holds.

Varlen indexing covers the inline prefix. Equality needs a stored hash of the full
value. Ordered scans work on the prefix and read the blob only on prefix ties.

### 4.7 Postgres compatibility, split in two

- **Wire protocol: yes.** pgwire v3 is documented and stable, with prior art in
  CockroachDB, Materialize, QuestDB, and DuckDB. Implementing simple + extended query
  and a subset of type OIDs means psql, DBeaver, TablePlus, and every Postgres driver
  work. This is most of the tooling story for free.
- **Dialect: no.** Decades of surface area. Not a goal.

The tension (a wire protocol accepts arbitrary SQL, which we said kills static
allocation) is resolved by splitting the binary:

- **Embedded core**: compiled queries only, no parser linked, memory ceiling holds.
  This is the product.
- **Admin server**: separate crate, feature-flagged, contains a parser and a simple
  interpreter, allowed to allocate. Debugging and tooling only. Compiled out of
  production builds.

Schema language is Postgres DDL plus annotations. `CREATE TABLE` with `@merge`,
`@index`, and capacity hints.

### 4.8 Migration

Compiled schemas make migration safer and louder, not harder.

- The schema hash lives in the file header. An old binary opening a new file fails at
  startup instead of misreading field offsets. Silent corruption becomes a startup error.
- A migration is a pure function `OldRow -> NewRow`. Both types are generated. Fixed
  widths make the rewrite a sequential memcpy-and-map, and totality over the old type's
  value space is property-testable in a way `ALTER TABLE` never is.
- Protobuf-style field discipline: append fields at the end, never reorder, never
  remove (tombstone instead). Additive changes are then zero-copy and old readers keep
  working; only destructive changes need the rewrite.

The cost that cannot be designed away: rolling deploys. Two binary versions against one
file is the failure mode. Single writer plus version gate plus offline migration handles
it. State this constraint up front.

### 4.9 Sync (deferred, but shapes the schema now)

Not v1. Listed here because three decisions must be made *before* v1 or they can't be
retrofitted.

The reason this design can do sync well: **every write path is generated code**, so
change capture is emitted inside the same generated function that performs the write.
Not a trigger, not a WAL tail, not bypassable, not lossy on schema change. And because
records are fixed-width, a change entry is fixed-width, so the changelog is another
declared zone with declared capacity and declared retention.

Decide now:

- **Row identity**: 128-bit IDs (UUIDv7 / ULID). Sequential PKs on a synced table are a
  compile error. Two offline nodes must create rows without colliding.
- **Causality**: HLC plus node ID on every write.
- **Merge policy, declared per field**: `@merge(lww)`, `@merge(counter)`,
  `@merge(server_authoritative)`, `@merge(orset)`. The compiler generates a merge
  function that is pure, total, and a function of two states only. That shape is what
  makes it oracle-testable: the model is "apply all ops in canonical order."

Known hard parts, not solved here: tombstone retention windows and the full-resync path
for nodes offline past the window; partial replication (PowerSync calls these sync
rules, and it is where most of their complexity lives).

Bidirectional sync is a larger project than the storage engine. Treating it as item nine
rather than a co-equal goal is what keeps this finishable.

## 5. ACID position

State per-target rather than claiming uniform ACID.

- **Atomicity**: superblock generation flip. One commit point, one fsync.
- **Consistency**: generated schema enforcement plus assertion density (target: average
  two per function, paired across write and read paths).
- **Isolation**: single writer, so serializable by construction. **Open decision:**
  whether v1 permits concurrent readers during a write (requires generation pinning and
  copy-on-write pages) or serializes all access. Recommendation: serialize for v1.
- **Durability**:
  - Native: full. `fsync`, `F_FULLFSYNC` on macOS.
  - Browser: best-effort. OPFS `flush()` does not carry POSIX-level crash guarantees,
    and the UA may evict storage regardless. Torn writes and corruption are *detected*
    via end-to-end checksums even where they cannot be prevented. Detecting corruption
    you cannot prevent is still a real guarantee, and more than SQLite-wasm offers.

Do not paper over the browser asterisk. Publish it.

## 6. Capacity exhaustion

A first-class, user-visible error, not an edge case. It is reachable in normal
operation, it is per-zone rather than global, and it is a consequence of our design
rather than the OS's, so the failure must be excellent.

- Every write returns `Result<_, Full>`. The error carries the entity, the configured
  capacity, and the config change needed. It should read like documentation.
- `usage()` per zone so hosts can alarm at 80%. Hitting a hard ceiling with no runway is
  the real failure; hitting it after three weeks of warnings is a planning miss.
- Batches are rejected atomically. Partial application turns a capacity limit into a
  corruption bug.
- A sizing tool reads a schema and prints arena bytes for a given row count. "1M users
  and 50M events costs 6.4 GB" is a question an operator can answer. "Pick a capacity
  for each of twelve tables" is not.

The upside: this is a *specified, testable* failure mode with a finite matrix (fill to
N-1, N, N+1; delete and refill; crash mid-batch at the boundary and recover). It runs in
the simulator with the arena set absurdly small so every run hits the wall. The
alternative is a system that allocates until the allocator, the OS, or the OOM killer
decides, at a moment you cannot predict, in a state you cannot reproduce.

## 7. Testing strategy

This is the primary objective of the project. The database is the vehicle.

### 7.1 Determinism enforcement

- The core crate must build for `wasm32-unknown-unknown`. This is the CI gate against
  ambient nondeterminism and it cannot be argued with: that target has no clock, no
  randomness, and no filesystem, so `SystemTime::now()` does not merely misbehave, it
  fails to link.
- Clippy deny list for the native build: `SystemTime::now`, `Instant::now`,
  `thread_rng`, default-hasher `HashMap` iteration.
- `BTreeMap` or fixed-seed hashers only. Never iterate a `HashMap`.
- One `ChaCha8Rng` seeded from a `u64`, threaded through the *simulator*, never through
  the core.
- Single-threaded core. Any parallelism lives outside the deterministic boundary.

wasm32 is a deterministic execution target by specification: strict IEEE 754 with no x87
extended precision, defined integer overflow, flat linear memory, no undefined behavior.
A seed that fails locally fails identically in CI and in a browser. Avoid the two
exceptions: NaN bit patterns are underspecified, and relaxed-SIMD is nondeterministic by
design.

### 7.2 The four techniques, in learning order

1. **Seed as the only input.** Every run prints its seed; a failure is an integer pasted
   back. If reproduction takes more than that, the harness is wrong and everything
   downstream is guesswork.
2. **Shrinking.** Finding a 40,000-operation failing trace is easy; reducing it to the 6
   operations that matter is what makes the bug fixable. Start with `proptest`'s
   machinery.
3. **Model-based testing.** The oracle is a dumb, obviously-correct implementation
   (`HashMap<Id, Row>`). Generate op sequences, run both, diff after every step. This
   finds more real bugs than fault injection and is far less work.
4. **Fault injection at the trait boundary.** Only after the above works. Torn writes,
   reordered writes, writes that succeed then vanish, a crash between any two I/O
   operations.

Order matters. Fault injection without a model produces crashes with no notion of
correctness, which is where most fuzzing efforts stall.

### 7.3 The test that means the harness works

Crash the simulated process at every I/O boundary in a run, recover, and assert the
state is exactly N or N+1 and never in between. Reproducible from a seed.

Until this passes, no other feature is worth building.

### 7.4 Assertion discipline

Assertions are the detector; the simulator only supplies chaos. Without density, a
fuzzer just runs the code faster.

- Average two assertions per function: arguments, return values, pre/postconditions,
  invariants.
- **Pair assertions**: check the same property on two different code paths. Validate
  before writing to disk and again immediately after reading back.
- Assert the negative space (what must not happen), not just the positive.
- Split compound assertions. `assert(a); assert(b);` gives better failure information
  than `assert(a && b)`.

Assertions downgrade catastrophic correctness bugs into liveness bugs. That trade is the
whole point.

### 7.5 Per-component oracles

Deliberate consequence of the architecture. Each of these is a few-hundred-line fuzz
target with an obviously correct reference:

| Component | Oracle |
|---|---|
| Blob allocator | `HashMap<Handle, Vec<u8>>` |
| Btree index | `BTreeMap` |
| Hash index | `HashMap` |
| HNSW index | brute-force scan |
| Trigram index | naive substring match |
| Storage engine | `HashMap<Id, Row>` |
| Merge functions (later) | apply all ops in canonical order |

Allocator invariants worth asserting: no two live handles overlap, free-byte accounting
is exact, a full alloc-then-free cycle returns to the initial state with zero leak.

## 8. Language and platform

**Rust.** The requirement to compile to wasm and embed in other languages settles it:

- Go's wasm output carries the runtime and GC (multi-MB); TinyGo shrinks it but has
  stdlib gaps.
- Embeddability means being a *guest* in someone else's process. Go's `c-shared` drags
  the scheduler and GC into the host. Rust's `cdylib` plus C ABI is what every language's
  FFI already expects.
- Go actively randomizes map iteration and gives no control over goroutine scheduling.
  Rust's nondeterminism sources are enumerable and lintable.
- Static allocation after init is expressible in Rust. In Go it means fighting the GC.

Targets: native (`x86_64`, `aarch64`), `wasm32-unknown-unknown` (browser),
`wasm32-wasip1` (edge runtimes).

### 8.1 Browser specifics

- OPFS sync access handles have been baseline across browsers since March 2023, but only
  inside **dedicated workers**.
- Acquiring a handle is itself async. Not a problem here: the file set is declared, so
  all handles are acquired during async `open()` and held for the session.
- Sync handles take an exclusive per-file lock unless opened `readwrite-unsafe`.
  Single-writer plus a `BroadcastChannel` election for multi-tab. **Decide at step 1,
  not step 6.**
- Safari incognito has no OPFS. An IndexedDB fallback is required.
- The OPFS storage implementation must exist from the first commit, not as a later port.
  The trait should be shaped by the most awkward platform, not by POSIX.

## 9. Build order

1. **Vertical slice**: one table, one field, insert and get-by-id, arena at open,
   superblock durability, deterministic harness with a crash injector. No indices, no
   queries beyond PK lookup, no blobs, no sync, no pgwire.
   Done when §7.3 passes.
2. **Minimal OPFS storage impl.** Even a bad one. Shapes the trait before six layers are
   built on top of a POSIX assumption.
3. **Codegen pipeline**: Postgres DDL in, Rust types out. Even for one table.
4. **Blob allocator** with its HashMap oracle. Small, self-contained, highest
   bug-density-per-line component in the design. Fuzzing it early also proves the
   fuzzing setup works.
5. **Compiled queries** and the finite operation space.
6. **Indices**: btree, then one hard one.
7. **Migration path** and schema hashing.
8. **Inspector CLI** and generated file-format documentation. Budget for this in v1, not
   later. Losing `sqlite3 file.db` is a larger adoption tax than it sounds, and skipping
   it makes the project feel unfinished regardless of correctness.
9. **pgwire admin server.**
10. **Sync.**

Steps 1 through 5 are the learning project and land in weeks. Everything after is
optional and should be treated as such.

## 10. Open decisions

Eight of these have since been decided by building the thing. They are kept
here with the reasoning rather than deleted, because the reasoning is the
part worth reading.

- **Concurrent readers during a write** — DECIDED: readers, and any number of
  them. Writers stay serialized (one writer, always), but a reader takes no
  lock and writes nothing at all, and sees the committed generation it opened
  on. That fell out of the commit protocol rather than being added to it: a
  commit becomes visible only when the superblock flips, and the previous
  generation survives in its own pair of slots throughout, so a reader can
  never observe a half-commit. All three sample applications named the
  absence of readers as a hard limitation before this existed.
- **Inline threshold for varlen fields** — DECIDED, and the question turned
  out to be the wrong one. There is no threshold and no spill: a value too
  long for one row slot is a RUN of slots written inside one commit, so a
  long value is atomic for the same reason a short one is, and there is no
  second allocator to keep consistent with the first. The ceiling is
  `MAX_VALUE_LEN`, 2 KiB today, and beyond that the answer is object
  storage with a reference stored here, as §4.5 always said.
- **How wide a commit is, next to how long a value is** — DECIDED: they are
  two numbers, and making them one was a mistake worth recording. Row
  format v5 spent one byte on the commit span, which capped a commit at
  128 row slots — exactly what a `MAX_VALUE_LEN` value costs. The two
  ceilings coincided, and the consequence was not a size limit but an
  ATOMICITY limit: a maximum-length value filled a commit by itself, so it
  could never land in the same commit as anything else. All three sample
  applications hit it independently and all three worked around it the
  same way, by holding back a slot and documenting a ceiling the library
  never mentions — a job queue cannot enqueue a full-size job and move its
  watermark, a bookmark store cannot write a full-size record and the
  counter that names it. Format v6 widens the span to two bytes (`u16`,
  little-endian, at offsets 25..27, inside the checksummed region and
  consuming the row's last padding byte, so every byte of a row is now
  covered by the CRC rather than by a zero check). A commit holds
  `MAX_COMMIT_ROWS` = 1024 slots; a value still costs at most 128 of them.
  The longest value the library will store now composes with seven more of
  its kind, and the workarounds are gone from all three samples. The
  general rule the episode teaches: a limit that exists for a FORMAT
  reason must not be spent as if it were a limit that exists for a
  CAPACITY reason.
- **Keys that are not `u64`** — DECIDED, and the smaller of the two
  answers turned out to be the right one. All three sample applications
  named this as the largest structural gap and each paid the same price:
  a bookmark store, a job queue and a key/value store all hash their real
  key onto a `u64`, probe past collisions, keep their own tombstones (a
  library delete on a chain link loses the key behind it), and cannot
  compact without re-placing every row. "List everything under
  `session/`" was a full scan and a sort in all three, because id order
  is hash order.
  Two shapes were plausible. A **byte-keyed table** moves the primary key
  itself, which every index, the row format and the migration path are
  built on — a much larger change, still not taken. A **second ordered
  index over VALUE bytes** gives key-ordered range scans to any
  application that puts its key at the front of the record, with no
  schema change and no second table, and that is what shipped:
  `range_by_value`, its descending and paging forms, and `prefix`.
  The implementation is smaller than the description. The index stores no
  bytes: a value already lives in the row arena, so it holds the head ROW
  of each record and compares by dereferencing, walking two runs slot by
  slot and stopping at the first differing byte. And it is the SAME
  B+tree the primary key uses — every search in that tree compares one
  fixed target against the stored keys it meets, so the target became a
  *probe* closure ("how does this stored key order against what I am
  looking for?"), which is `stored.cmp(&target)` for the `u64` index and
  a dereference for this one. Nothing structural changed, so the pool
  bound, the invariant checker and the existing mutation surface cover
  both.
  The order is value bytes, then id, then row. The id breaks a tie
  between records holding the same bytes, and it is the tie-break rather
  than the row because it is the only one a caller can predict: rows are
  renumbered by a compaction. The row is last and no two live entries
  reach it; it exists because the index is append-only like the trigram
  postings, so writing the same value to the same id twice leaves both
  rows in the tree.
- **Which hard index ships in v1** — DECIDED: trigram, for the reason §4.6
  gives. Its oracle is exact, and every other index in this project is held
  to equality against an oracle.
- **Whether a transaction can read** — DECIDED: it can assert, which is the
  part that matters, and it costs nothing. A batch may carry `Expect { id,
  value }`, checked against the state the batch's earlier ops would leave
  and refusing the whole batch if the row holds anything else. That is
  compare-and-set, and it is what read-then-write needs to be safe: "claim
  the head of the queue if it is still pending" stops being a `get` and a
  `put` with a gap between them. A general read returning bytes INTO the
  batch was considered and rejected — it would need somewhere to put the
  answer, which means either a result buffer in the commit protocol or a
  stored procedure, and neither is worth what an assertion already buys.
  An assertion stages no row, so a batch of nothing but assertions
  performs no I/O at all.

- **What a reader does about commits made after it opened** — DECIDED: it
  catches up incrementally, and the incremental replay is the recovery
  replay. A reader sees the generation it opened on and nothing after it,
  which is what makes a read lock-free and a scan self-consistent; the
  cost was that a live view meant closing and reopening in a loop, paying
  a full replay of every committed row to learn about three new ones.
  `Db::refresh` re-reads the superblock and, if the writer has moved on,
  reads and replays ONLY the rows appended since. Rows already held are
  immutable — the file is append-only within a generation lineage — so
  applying the tail applies history. The load-bearing decision is that
  this shares `replay_rows` with recovery rather than having a reader's
  own version of it: a reader that applied a row differently from the way
  recovery applies it would be a SECOND definition of what the file
  means, and the whole design rests on there being one.
  Three answers are refusals rather than failures, each pointing
  somewhere different: `Diverged` (the files are not a continuation of
  what this handle holds — a compaction swapped the database out, or
  commits went missing; reopen), `CapacityTooSmall` (the writer grew past
  this handle's arenas; grow, then refresh), and `Degraded` (this handle
  was salvaged and holds rows it could not verify, so there is no honest
  incremental answer). In all three the handle keeps the view it had and
  keeps answering.
  A failed READ during a refresh is the one exception to fail-stop, and
  not a weakening of it: fail-stop exists because a half-done WRITE
  leaves the engine unable to say what is durable, and a refresh writes
  nothing. The reader keeps a view that was verified when it was built
  and is not made doubtful by a later read failing.
- **What happens when a database fills up** — DECIDED: it grows, on the
  handle that is open, and the static-allocation rule survives intact.
  Capacity is what every arena is sized from (§4.2), so it was fixed for
  the life of a handle and the only way past the ceiling was to drop the
  database and reopen it larger. For a directory that means releasing the
  single-writer lock and racing whoever is waiting for it; for an
  in-memory or browser database it means a snapshot and a reload, which
  copies every byte. And a store that cannot take another write, with no
  way forward that does not go through dropping the database, reads to
  whoever is using it as data loss — whatever the file says.
  `Db::grow` re-runs recovery against the SAME storage handle at a larger
  capacity: new arenas, one replay, lock never let go, no file rewritten.
  §4.2 is unchanged, because it was never "one allocation per process" —
  it is "no allocation in steady state, only at named lifecycle points",
  and a grow is as named a point as an open. It never shrinks (that would
  be discarding room the data might be using, which belongs at a door
  where the caller sees the refusal), it writes nothing until the next
  commit, and a fault at any of its I/O boundaries leaves the durable
  bytes exactly as they were — swept and asserted, not argued.

Still open:

- Size class growth ratio: 2x (simple, ~50% worst-case internal fragmentation) versus
  1.25x (tighter, more free lists).
- `BLOB_HARD_MAX` exact value.
- Page size, and whether it is fixed or schema-declared.
- Multi-tab coordination mechanism in the browser.

## 11. Prior art to read before building

- **TigerBeetle**: TIGER_STYLE.md, the VOPR simulator, static allocation, assertion
  discipline, superblock copy sets.
- **FoundationDB**: the origin of deterministic simulation testing. Watch the
  "Testing Distributed Systems w/ Deterministic Simulation" talk.
- **Realm**: schema-at-open with zero-copy mmap on mobile. Its migration block API and
  version gating are close to what is needed here; its pain points are instructive.
- **eXtremeDB**: the commercial version of DDL-compiler-plus-static-allocation for
  realtime and embedded. Worth reading for market shape.
- **Cap'n Proto / FlatBuffers**: layout and codegen mechanics.
- **sqlc / Diesel**: query compilation ergonomics being copied wholesale.
- **wa-sqlite and sqlite-wasm**: the four bad bridges (Asyncify, SharedArrayBuffer,
  JSPI, SQLITE_BUSY retry) that §4.1 exists to avoid.
- **madsim, turmoil, shuttle, proptest, cargo-fuzz**: Rust simulation and fuzzing tooling.

## 12. Success criteria

The project is a success if, at the end:

- A failing test is reproducible from a single integer, on any machine, in any browser.
- The crash-recovery property in §7.3 holds under fault injection.
- The memory ceiling declared at open is the memory actually used, forever.
- The whole thing runs in Safari without cross-origin isolation headers.

Everything else is a bonus.
