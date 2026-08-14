# Fault model and coverage matrix

Every fault the simulator can express, what guarantee the engine makes
against it, and which suite validates it.

The model is deliberately aligned with TigerBeetle's published storage
fault model (see their [safety docs](https://docs.tigerbeetle.com/concepts/safety/)):
disks silently corrupt data, misdirect **both writes and reads**, lose
writes entirely, and lie about fsync. Where TigerBeetle *repairs* these
faults from other replicas, a single-node store can only detect, survive
via redundancy, or fail honestly — each row below names which. TigerBeetle
also found that single-**byte** fault injection reproduced bugs that
whole-sector corruption missed, which is why tears and flips here are
byte-granular.

## The contract: perfection in budget, honesty beyond it

**Fault budget** (what a single disk can absorb outright): machine crashes
with arbitrary settle of unsynced writes; EIO fail-stops with dirty page
caches; crashes during recovery; at most one unrepaired superblock media
fault per two generations; transient read corruption. **Within the budget
the store is perfect: zero loss, zero drift.** Every acknowledged insert
survives bit-exact, every read is exactly right, recovery always succeeds.
Validated by every sweep below and, critically, by `storm.rs`, which layers
ALL in-budget faults into single lifetimes and treats any loss — however
small — as failure.

**Beyond the budget** (lying fsyncs, simultaneous multi-faults), no
single-disk system can recover bits the hardware never stored. The contract
degrades in a fixed, tested order — never to silence:

1. **Never wrong**: data served is always exactly what was acknowledged.
   There is no fault, in or out of budget, that produces drift. (Every
   suite asserts this; it has no exception class.)
2. **Ordered, bounded loss**: what survives is an exact in-order prefix of
   acknowledged commits. A single lying fsync loses at most the final
   commit (`fsync_lies.rs` asserts the bound).
3. **Loud whenever evidence exists**: recovery scans past the manifest for
   checksum-valid orphan rows. One is the normal in-flight artifact; two or
   more are proof of rolled-back acknowledged commits, and
   `Engine::recovery_report()` raises `rollback_evidence` (deep-rollback
   test: 6 lost commits with surviving rows ⇒ flagged, mutation-verified).
   Hosts should treat that flag as an alarm.
4. **Silent only when physics wins**: a rollback is undetectable only when
   *no* distinguishing bit survives on the platter — at which point the
   state is indistinguishable, by any observer, from the commits never
   having happened. Repairing through even that requires a second copy of
   the truth: replication (design §4.9).

Two modes everywhere:

- **Exhaustive** — every point in the fault space is enumerated. Predictable:
  a pass means the whole space, not a sample.
- **Seeded random** — the space is too large to enumerate; sampled by
  ChaCha8 from a `u64` seed. Variable but reproducible: every failure
  message carries the seed (and index) that replays it bit-for-bit.

## Crash faults (unsynced writes at machine crash)

Each unsynced write independently receives a fate:

| Fate | Meaning |
|---|---|
| `Keep` | persisted despite the missing fsync |
| `Drop` | never reached the platter (kept-later + dropped-earlier = reordering) |
| `Prefix(n)` | torn: first `n` bytes persisted (byte-granular) |
| `Subset(mask)` | torn: arbitrary subset of 8-byte sectors (suffixes, holes) |
| `SubsetGarbage` | subset + one sector persisted with arbitrary bytes |

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Crash at **every I/O boundary** of a run | exhaustive boundaries × random fates | `crash_recovery.rs` | recover to exactly N or N+1 acked inserts |
| Every boundary × **every fate combination** of the unsynced window | exhaustive² (Drop/Keep/all sector prefixes/subsets) | `exhaustive_settle.rs` | N or N+1; window stays ≤ 2 writes and disjoint |
| Crash **during fresh init** | exhaustive boundaries | `crash_recovery.rs` | recovers to an empty, writable database |
| Crash **during recovery**, recover again | seeded random | `lifetime.rs` | same N/N+1, idempotent recovery |
| **Many crash/recover/continue cycles** in one lifetime | seeded random | `lifetime.rs`, `vopr` | oracle-exact after every recovery |
| Crash at the **capacity wall** (filling the last slot) | exhaustive boundaries × seeds | `capacity.rs` | wall holds; retry gives DuplicateId or succeeds, never double-applies |

## Capacity exhaustion (the declared ceiling, §6)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Fill to N-1 / N / N+1 | targeted | `capacity.rs` | `Full {{ entity, capacity }}` — the error names the config change needed; database unharmed, all rows readable |
| **Full rejection purity** | mechanical pin | `capacity.rs` | rejection performs ZERO I/O: `io_count` unchanged, every on-disk byte identical, generation untouched — nothing is ever partially applied |
| Crash at every I/O boundary of the final slot | exhaustive × seeds | `capacity.rs` | recovers to N-1 (retry succeeds) or N (retry = DuplicateId), then Full — never between, never double-applied |
| **EIO exactly at the wall** (each of the final insert's 5 ops) | exhaustive | `capacity.rs` | fail-stop, dirty-cache restart to an exact wall, then machine crash on top: full state durable |
| Reopen capacity boundaries | targeted | `capacity.rs` | capacity == data opens already-full; data-1 refuses with `CapacityBelowData {{ required, configured }}`; data+1 gives exactly one slot |
| 100%-full database serves reads | targeted | `capacity.rs` | every get, full ordered paged scan, empty/singleton ranges — all exact at the wall |
| Whole lifetimes lived at the wall, under crash/EIO schedules | seeded (floor-asserted) + swarm vopr (rows=4 configs) | `lifetime.rs`, `vopr` | Full interleaved with faults never corrupts; oracle-exact every cycle |
| Blob zone exhaustion | targeted + fuzz (arena sized to fill) | core `blob.rs`, `blob.rs` fuzz | `Full {{ block_bytes, capacity }}`; freed blocks reusable; zero leak |
| B+tree node pool | adversarial fill | core + `btree_oracle.rs` | derived bound never approached — index capacity can't be hit before row capacity |

## I/O failures (EIO on read/write/fsync; process restarts, machine does not)

The page cache (`current`) survives into the next incarnation — unsynced
state is visible but not durable. Failed writes still dirty the cache
(worst case); failed fsyncs sync nothing.

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Failure at **every I/O index** of a run | exhaustive | `io_failures.rs` | fail-stop: in-flight op errors `IoFailed`, everything after rejected |
| Reopen after fail-stop (dirty cache) | exhaustive | `io_failures.rs` | N or N+1, all-or-nothing |
| Machine crash **after** a dirty-cache recovery | exhaustive × seeded settle | `io_failures.rs` | **visible implies durable**: recovery fsyncs before OpenDone; shown state never regresses (mutation-verified) |
| Fail-stop cycles interleaved with crash cycles | seeded random | `lifetime.rs`, `vopr` | oracle-exact |

## Media faults (at rest)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Bit flip, **every byte of the superblock zone** | exhaustive (×2 bit positions) | `faults.rs` | zero loss: every generation lives in 2 of 4 slots |
| Bit flip, **every byte of every committed row** | exhaustive (×2 bit positions) | `faults.rs` | detected at open (`Corrupt`), never served wrong — padding validated, no dead zones (`layout.rs` unit test: every bit of every slot) |
| Both copies of the live generation corrupted | targeted | `faults.rs` | falls back exactly one generation (documented double-fault limit) |
| Corruption in stale slots / beyond committed rows | targeted | `faults.rs` | inert (orphan bytes are never read) |
| **Truncation** of either file at every 8-byte point | exhaustive | `faults.rs` | full recovery, honest older state, or `Corrupt` — never wrong data |
| **Garbage extension** of either file | sampled sizes × seeds | `faults.rs` | inert: the manifest defines the live region |

## Read-path faults (in flight; the disk itself is clean)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Transient bit flip in the superblock read, **every byte** | exhaustive (×2 bit positions) | `read_faults.rs` | zero loss: the live pair's twin is in the same buffer |
| Transient bit flip in the rows read, **every byte** | exhaustive (×2 bit positions) | `read_faults.rs` | detected (`Corrupt`) — never served; detection over availability |
| **Misdirected read**: valid bytes from the wrong offset (checksums pass) | shift grid × both reads × seeds | `read_faults.rs` | never silently wrong: positional validation (a copy is only trusted in its own pair slot) + structural checks |

## Lying fsync (fsyncgate: success reported, nothing persisted)

The one fault where single-disk acked-durability genuinely cannot hold
(TigerBeetle survives it via replicated repair). What is guaranteed instead:

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Lie at **every fsync index**, then power loss × several settles | exhaustive × seeded | `fsync_lies.rs` | **prefix consistency**: exactly the first N acked commits survive — in order, correct bytes, no holes, no phantoms — and the survivor is writable |
| Single lie: loss depth | (same sweep) | `fsync_lies.rs` | **bounded**: at most the final commit; when it is lost, the evidence scan sees the orphan row its honest rows-fsync left behind |
| Lie leaves the superblock referencing a never-persisted row | (subset of above) | `fsync_lies.rs` | detected (`Corrupt`), not served |
| Lie followed by a later honest fsync of the same file | (subset of above) | `fsync_lies.rs` | self-heals, zero loss |
| **Persistent lies** (all fsyncs no-op from some point; deep rollback) | targeted + seeded settles | `fsync_lies.rs` | exact prefix always; when row evidence survives, `rollback_evidence` fires (mutation-verified); evidence flag never fires without real loss |

## The storm (all in-budget faults at once)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Crashes + EIO fail-stops + recovery crashes + budget-gated media faults + transient read faults, interleaved across whole lifetimes | seeded random, coverage-floored | `storm.rs` | **perfection**: zero loss, zero drift, recovery always converges, no rollback evidence ever appears |

## Misdirected writes (firmware lies: success reported, bytes landed elsewhere)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Every write of a run × shift grid (±slot sizes) × cross-file | exhaustive grid × seeds | `misdirect.rs` | **never silently wrong**: open yields correct-or-absent rows (`n ≤ acked`), or a loud error |
| Superblock copy landing in a foreign slot | (found by the above) | `misdirect.rs` | slot position is part of copy validity; foreign-slot copies distrusted |

## Sequencing faults (host protocol seam)

The host contract is lockstep: one request, one completion. Violations are
bugs in the host and must be loud, not lenient.

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Completion with no request in flight | targeted | core `engine.rs` tests | panic ("protocol violation") |
| Completion/failure naming the **wrong file** | targeted | core tests | panic |
| `Open` on an opened engine | targeted | core tests | panic |
| Client ops mid-I/O (Insert/Get while inserting) | targeted | core tests | `Busy` — v1 serializes everything |
| Ops before open / after fail-stop | targeted | core tests | `NotOpen` / `IoFailed` |

## The compiled query surface (no runtime planner, by design)

Plans are fixed at build time (design §4.3); the "planner" is the codegen
shape validator, and the runtime obligation is the full result matrix of
the compiled operations — exercised exclusively through the generated
functions:

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Every malformed query shape (wrong table/columns/order/params/kind, non-PK predicates, unsupported verbs, duplicates, unterminated) | rejection grid | `dabqlite-codegen/tests/queries.rs` | loud build-time error naming the exact accepted shape |
| Empty / present / interleaved results at every fill level | targeted | `query_surface.rs` | exact bytes or honest absence |
| Error results: NotOpen, DuplicateId, Full, fail-stop IoFailed | targeted | `query_surface.rs` | first-class errors; database unharmed and re-queryable |
| **Large result (4096 rows) with faults halfway through**: at-rest and in-flight corruption at 1/4, 1/2, 3/4 of the result; EIO mid-result; crash at every recovery boundary | targeted + swept | `query_surface.rs` | detected or fail-stopped, never partial/wrong; clean retry sees every row; zero loss |
| Generated surface vs raw engine inputs | seeds | `query_surface.rs` | byte-identical disks, identical outputs (mutation-verified: a wrong-key wrapper fails 4 independent tests) |
| Crash sweep driven purely through compiled operations | exhaustive boundaries × seeds | `query_surface.rs` | N / N+1, same as the raw surface |
| Operation-space drift | golden + hash + CI regenerate-diff | codegen tests | `OPERATIONS` + `QUERY_SPACE_HASH` pinned; two binaries agree on what can be asked iff hashes match |

## The ordered index (B+tree) and multi-row results

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Structure vs `BTreeMap` oracle: random inserts × 5 key shapes, continuous range diffs, degenerate ranges | seeded fuzz | `btree_oracle.rs` | exact agreement; deep invariants (ordering, uniform depth, occupancy, leaf chain) checked continuously |
| **Every insertion order** of small key sets (5040 permutations) | exhaustive | core `btree.rs` tests | identical in-order output, valid structure, always |
| Node-pool bound (no allocation after init) | adversarial fill + fuzz assert | core + `btree_oracle.rs` | usage stays below the derived N/3 bound |
| Multi-row range results vs oracle: full-table, empty, inverted, singleton, arbitrary sub-ranges, page-boundary edges | seeded × grids | `query_surface.rs` | exact rows, strictly ascending, bounded pages |
| **Process restart halfway through a paged result**; writes landing ahead of the cursor between pages | targeted | `query_surface.rs` | continuation completes exactly (cursor is a plain key); committed rows ahead of the cursor appear |
| Range during in-flight insert I/O | targeted | `query_surface.rs` | `Busy` — v1 serializes, scans never interleave with writes |
| Rebuilt-at-recovery correctness under EVERY fault schedule | folded into `lifetime.rs` (and thus vopr) | ordered full scan == oracle, in order, after every crash / EIO / recovery-crash cycle (mutation-verified: rebuild-blindness fails 3 suites; a wrong split separator fails 4) |

## ACID (design §5: stated per-target, verified per-letter)

Consolidated in `acid.rs`; most evidence also lives distributed through the
fault suites. Current scope note: the transaction unit is a single insert —
there is no multi-operation transaction API yet, so atomicity claims bind
per-write (and will extend to batches when batches exist, §6).

| Letter | Claim | Verification |
|---|---|---|
| **A**tomicity | The superblock generation flip is the sole commit point; writes are all-or-nothing | crash at every boundary × settles: point-get and range observers agree, in-flight rows whole-or-absent; **exactly-once**: crashed-then-retried inserts appear exactly once in a full scan; Full rejections perform zero I/O |
| **C**onsistency | Schema enforcement + invariants at every observable state | PK uniqueness enforced live (and after churn, without mutation on duplicate attempts); strictly-ascending unique scans; observer count agreement; recovery cross-checks (checksums, duplicate detection, count-vs-file) reject inconsistent disks loudly |
| **I**solation | Serializable by construction: single writer, all access serialized (v1 recommendation adopted) | every client operation (insert/get/range) refused `Busy` at **every** intermediate I/O stage of a commit; a stage-count assert forces this coverage to grow with the protocol; scans read committed state only |
| **D**urability | Acknowledged implies durable (native targets: full fsync discipline) | crash-with-adversarial-settle at **every acknowledgment point**: nothing acked is ever lost; recovery fsyncs before serving (visible implies durable); beyond the fault budget, degradation is prefix-consistent and evidence-flagged (see lying-fsync section) |

The browser durability asterisk (§5: OPFS flush is best-effort, detection
over prevention) becomes applicable when the OPFS backend lands; the
detection machinery it relies on is already tested.

## Simulator/reality equivalence (the simulation is not a fiction)

Every fault above is injected against a simulated disk. These tests anchor
the simulation to real hardware behavior:

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Same seeded workload, same generic driver, sim vs. real POSIX files | seeds | `dabqlite-host/tests/equivalence.rs` | **byte-for-byte identical files** — any divergence means the storage contract differs and simulated results are suspect |
| Identical at-rest damage (flips, truncations) applied to both | fault grid | same | identical recovery outcomes, row for row, error for error |
| Close and reopen real files (real fsync path) | seeds | same | full recovery, no rollback evidence |

## Single-writer enforcement (design §2: one writer, always)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| Second open of a locked directory, same process | targeted | `dabqlite-host/tests/locking.rs` | refused (`WouldBlock`), error explains the policy; release-then-open works |
| A **real second OS process** while the database is in active use | spawned-process | same | refused at the door before it can touch data; the refusal harms nothing |
| Lock holder is **killed** (crashed writer) | spawned-and-killed process | same | the kernel releases `flock` with the process — no stale lock, no recovery step, the lock is crash-safe by construction |

## The schema version gate (§4.8)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| File written under a foreign schema (forged valid superblocks) | seeds | `version_gate.rs` | refused with `SchemaMismatch` naming BOTH hashes; engine fail-stopped |
| The rejected open's side effects | byte pin | same | **zero bytes changed** — a correct binary later finds the file pristine |
| Repeated confused retries | targeted | same | identical refusal every time, zero cumulative harm |
| Stray foreign copy in a stale slot (even with a higher generation) | targeted | same | ignored; healthy same-schema copies win |
| Direction / magnitude of the hash difference | grid | same | any difference refuses, both directions |

With the migration path landed (below), a `SchemaMismatch` naming the v1
hash is no longer a brick: the host runs the offline migration and
reopens. Any OTHER hash remains a safe brick — this binary migrates only
from schemas it carries codecs for.

## The migration path (§4.8, build step 7)

The migration runs inside the NEW binary, offline, under the same
single-writer lock. It is a second sans-I/O state machine
(`MigrationEngine`) driven through the identical host protocol, so every
fault knob in the simulator applies to it unchanged. The protocol: read
the legacy superblock, read and checksum-verify every legacy row,
transform through the pure `migrate_row` (totality property-tested over
the old type's value space: boundary ids × per-byte bit patterns × 10k
seeded pipelines), write the new rows file completely, fsync it, THEN
flip the superblock to the new schema hash at generation g+1 — which
lands in the other slot pair, so the legacy generation's copies are
never written. Rows files are NAMED by schema hash, so the superblock's
hash is also the name of the live file; after the flip the legacy file
is an orphan nothing names — inert by construction (§4.4).

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| The full upgrade path (gate refuses → migrate → clean open) | seeds × sizes (incl. 0 and at-capacity) | `migrate.rs` | every row present, values widened per the append-only policy, ordered index rebuilt correctly |
| The legacy rows file, under every outcome below | byte pin, everywhere | same | **read, never written** — byte-identical through success, crash, EIO, corrupt-refusal, and retry |
| Crash at EVERY I/O boundary × settle seeds | exhaustive × seeded | same | **two worlds only**: still-legacy (v1 named, migration re-runs to full success) or fully-migrated (v2 named over complete durable rows). Never mixed, never a third state — verified by a coverage floor that both worlds occur |
| EIO at every boundary | exhaustive | same | fail-stop with the file named; re-runnable to full success on the same disk |
| Re-running against an already-migrated file | targeted | same | idempotent: zero writes, byte-identical disk — but NOT zero fsyncs: the found superblock may be page-cache-only from a dead attempt, so the no-op fsyncs before acknowledging (visible-implies-durable, the recovery lesson applied twice) |
| Corrupt legacy row (any byte) | targeted grid | same | refused loudly naming the checksum; superblock unflipped; a migration never invents data |
| Legacy superblock names more rows than the legacy file holds | targeted | same | refused as `Corrupt` |
| Unknown third schema / capacity below legacy data | targeted | same | `SchemaMismatch` with both hashes / `CapacityBelowData` with both numbers |
| Determinism | replay | same | identical bytes and I/O count on identical input |
| Real files (POSIX), sim/real equivalence | end-to-end | `migrate_posix.rs` | gate → migrate → open on genuine files and fsyncs; migrated superblock AND rows byte-identical to the simulator's |
| Harness teeth | planted mutations | checked by hand | skip-rows-fsync dies on the I/O-shape pin; the self-consistent wrong-file-fsync (same I/O count, rows never durable) dies in the crash sweep as a THIRD WORLD at a specific boundary — the deep invariant catches what shape pins cannot |

## Scale (the extrapolation, checked)

| Scenario | Mode | Suite | Guarantee |
|---|---|---|---|
| **1,000,000 rows** through the full engine (interleaved keys → splits everywhere) | release-mode CI step | `scale.rs` | wall exact and zero-I/O at 1M; full 32 MB recovery re-verifying every checksum; ordered windows at start/middle/end; crash boundaries of the millionth insert: N/N+1 |
| **100,000 rows on real files** (200k genuine fsyncs) | release-mode CI step | `scale_posix.rs` | byte-identical to the simulator at volume; real recovery + sampled reads exact |

## Deadlock (and its lockstep analog, the protocol stall)

Classic lock-ordering deadlock is **structurally impossible**, and the
argument is enforced, not asserted:

- The core is a single-threaded sans-I/O state machine: no threads, no
  mutexes, no channels, no blocking calls. The wasm32 determinism gate
  makes this a build-time fact — the core is `no_std`, and `std::sync`
  does not exist to link against.
- The one OS lock (`flock` in `PosixStorage`) is taken with `LOCK_NB`:
  it never waits, it refuses instantly with `WouldBlock` (pinned by
  error kind in `locking.rs`). With no blocking acquisition anywhere,
  there is no wait-for graph to have a cycle in.
- The only mutex in the repository is test-only (the locking suite's
  serialization guard): a single lock, never nested, poison-recovering.

What the lockstep protocol CAN have is the deadlock's analog: a
**protocol stall** — a machine that keeps emitting I/O requests without
ever reaching a terminal output (the host drives forever), or a machine
that silently absorbs an input its peer expects an answer to (the peer
waits forever). Both defended, both teeth-tested (`deadlock.rs`):

| Scenario | Mode | Guarantee |
|---|---|---|
| Wedged machine (re-requests I/O forever) at open, mid-insert, mid-migration | arranged via the sim's `stall_from` knob | the **fuel watchdog** in BOTH drive loops (sim and production POSIX host) panics with "protocol stall (deadlock)" naming the tick count, budget, and last request — never a hang |
| Wedge at EVERY boundary of an insert | exhaustive | every arranged deadlock is caught; the disk survives intact — a caught stall is recoverable exactly like a crash at the same boundary |
| Liveness budgets | exact pins | fresh open = 3 I/Os, insert = 5, recovery = 4, get/range/rejections = 0 (cannot even stall), migration = n+6, no-op re-migration = 3. Termination is not "eventually" but "in exactly k steps"; new I/O in any path is a conscious, test-breaking change |
| Silent absorption (the deadlock seed: an ignored input starves the peer) | targeted | every unexpected `(state, input)` pair panics "protocol violation" — engine AND migration engine, client ops and unrequested completions alike |
| Watchdog teeth | planted mutation, checked by hand | a real engine wedge (recovery re-requesting the same superblock read forever) died by watchdog panic at tick 193 of a 192 budget in milliseconds — where it previously would have hung the suite until an external timeout |

The fuel budget is `64 + 8 × rows` — generous over the largest legit
operation (migration writes one row per row) yet finite, so a false
positive requires being ~8× over the worst real budget. Known limit:
the core cannot detect a HOST that stops feeding it inputs — the core
has no clock, by design (§7.1) — so stall detection lives in the
drivers, and an embedding that abandons its drive loop mid-operation
owns that outcome (the disk, as always, recovers as from a crash).

## Harness self-checks (a passing sweep must mean something)

- Coverage floors: crash counts, fail-stop counts, recovery-crash counts,
  in-flight-commit counts, Full-rejection counts, misdirection hit counts,
  scenario totals — all asserted with minimums so generator drift fails CI.
- Both outcome classes asserted present where two are legal (misdirection:
  survivable and detected).
- Determinism: `run_lifetime(seed)` twice → identical stats, bit-for-bit.
- Mutation-verified detectors: breaking fsync-before-superblock ordering,
  or serving recovered state without fsyncing it, makes the suites fail —
  checked by hand during development, and systematically by the weekly
  `cargo-mutants` workflow (`.github/workflows/mutants.yml`), which mutates
  the core and fails if any mutant survives the full suite.
- **First full mutation sweep, and what it taught**: 462 mutants across the
  core, each run against the entire workspace suite (`--test-workspace
  true` — load-bearing: cargo-mutants otherwise runs only the mutated
  package's own tests, and the killing power lives in the sim/host
  suites). First run: 387 caught outright, 11 killed by timeout (the
  mutant hangs a test — detection, just slower), 38 unviable, **26
  missed**. Every miss was closed the same day, by category:
  - *The one real hole*: a non-empty rows file with no valid superblock
    copy anywhere must be refused as `Corrupt` — the mutant flipped that
    check into a silent re-initialization over committed rows, and no test
    noticed. Now pinned (`mutation_gaps.rs`), including zero-byte harm on
    the refusal.
  - *An unpinned recovery semantic*: two checksum-valid copies of the same
    generation that disagree (out-of-model forgery) now deterministically
    resolve to the first slot of the pair (`mutation_gaps.rs`).
  - *Checker teeth* (the largest class): the invariant checkers themselves
    — btree structural validation, blob conservation accounting, engine
    tripwires, cycle/probe-termination guards — could be no-op'd without
    any test failing. Each checker now has tests that corrupt state with
    surgically exactly one defect and demand the panic (`should_panic`
    with the specific message), plus legal-boundary states that must NOT
    panic, so checkers can neither go blind nor go paranoid.
  - *Golden pins*: `pool_nodes_for`, `is_empty`/`nodes_used`, the
    `hash_slot` mixing function (mutations only degrade distribution, so
    the values themselves are the spec), and codec length gates in both
    directions (short refused, longer-than-slot decodes its prefix,
    generated and reference codecs agreeing).
  - *Two mutants were equivalent-under-contract, so the contract was made
    testable instead of excluding them*: strict-less descent routing
    (`key < sep`) only differs from `<=` on duplicate keys, which the
    engine rejects upstream — the comparison was extracted into a named
    predicate (`routes_before`) and pinned directly, equality included.
    Likewise the insert-side probe-termination guard, provably unreachable
    while the lookup-side guard exists, was merged into one shared
    `probe_next` with an exact `!=` guard that any operator flip trips
    immediately. **Zero mutants are excluded from the sweep.**
- **The verification sweep audited its own verdicts — and found false
  kills.** The full re-run after the closures (455 mutants) reported 401
  caught + 15 timeout-kills + 1 miss, but the miss was a mutant the FIRST
  run had "caught" — impossible if verdicts are sound. The first run's log
  showed the truth: that "kill" (and three verdicts in the re-run) came
  from an unrelated flaky test, not from the mutation. The flake was real
  and worth the trip: `Command::spawn` forks, and between fork and exec
  the child holds duplicates of every parent fd — including a flock'd
  lock file another locking test had just dropped, so a concurrent
  drop-then-reopen saw a phantom `WouldBlock`. The locking suite is now
  serialized with a documented mutex, verified stable across repeated
  runs. The lesson is structural: **a mutation kill is only as trustworthy
  as the determinism of the suite that produced it**, so any surviving or
  newly-appearing mutant across runs gets its log read, not just its
  verdict counted. The one true miss (range-descent `<` vs `<=`: routing
  `start == separator` left walks one extra leaf but yields identical
  output) was dissolved by routing the scan descent through the same
  pinned `routes_before` predicate as insert, and the three false-kill
  sites (range-page arena offset arithmetic) were re-verified with the
  flake fixed.
- **Swarm testing**: the `vopr` soak derives its entire lifetime
  configuration (cycle count, capacity, fault probabilities) from the seed,
  so the fleet explores config corners — tiny arenas living at the capacity
  wall, fault storms, long quiet runs — and one integer still reproduces
  everything.

## Simulated-time accounting (how to read soak numbers)

The simulator performs I/O in zero time, so a soak compresses large amounts
of simulated operation into seconds of wall clock — the same idea as
TigerBeetle's VOPR running clusters at ~1000x speed. To keep such claims
auditable rather than rhetorical, `vopr` converts its event counts into
simulated operational time using fixed, deliberately conservative
constants, printed with every soak:

| Event | Modeled cost | Rationale |
|---|---|---|
| fsync | 1 ms | fast NVMe flush; SATA/cloud disks are 5–20x slower |
| read / write | 20 µs | NVMe 4K I/O |
| crash / EIO / recovery-crash restart | 5 s | process death → supervisor restart → open |

`simulated time = I/O time + restart time`. The claim shape is:
"survived N hours of continuously faulted operation in M seconds of wall
clock" — where *every* one of those simulated hours is spent inside the
fault schedule (crashes mid-commit, EIO storms, recovery crashes), not
idling. Two honesty notes:

- These are modeled equivalents, not measured hardware time. The constants
  are chosen conservative (fast hardware = less simulated time claimed);
  slower assumed hardware would only inflate the number.
- Fault *density* here is millions of times production reality: a fleet
  sees a hard crash per node per week, not five per minute, and silent
  media corruption at ~0.031%/SSD-year (TigerBeetle's cited rate) — the
  soak injects in minutes what a production fleet would take decades of
  disk-years to encounter. Compressed calendar time is the point; the
  density multiplier is the real testing leverage.

## Known limits (deliberate, documented)

- **Lying fsync** voids single-disk acked-durability; the tested guarantee
  degrades to prefix consistency + detection (see the section above).
  Repairing through it requires replication (design §4.9, deferred).
- Double media faults on both live superblock copies lose exactly the last
  commit (tested, documented above); triple faults and worse degrade to
  `Corrupt`.
- **Gray failure** (a disk that is merely slow) is unmodeled: the core has
  no clock by design, so there is no timeout to test yet. Becomes relevant
  with concurrent readers or replication.
- Timing/concurrency faults beyond completion-ordering do not exist yet:
  v1 serializes all access by design (docs/DESIGN.md §5). When concurrent
  readers arrive, this matrix grows a new section.
