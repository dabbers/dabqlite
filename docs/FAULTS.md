# Fault model and coverage matrix

Every fault the simulator can express, what guarantee the engine makes
against it, and which suite validates it. Two modes everywhere:

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

## Harness self-checks (a passing sweep must mean something)

- Coverage floors: crash counts, fail-stop counts, recovery-crash counts,
  in-flight-commit counts, Full-rejection counts, misdirection hit counts,
  scenario totals — all asserted with minimums so generator drift fails CI.
- Both outcome classes asserted present where two are legal (misdirection:
  survivable and detected).
- Determinism: `run_lifetime(seed)` twice → identical stats, bit-for-bit.
- Mutation-verified detectors: breaking fsync-before-superblock ordering,
  or serving recovered state without fsyncing it, makes the suites fail
  (checked by hand; candidates for automated `cargo-mutants` later).

## Known limits (deliberate, documented)

- **fsync that lies silently** (reports success, persists nothing) is not
  modeled: undetectable-by-construction at write time; the guarantee
  degrades to detection-at-next-open via checksums.
- Double media faults on both live superblock copies lose exactly the last
  commit (tested, documented above); triple faults and worse degrade to
  `Corrupt`.
- Timing/concurrency faults beyond completion-ordering do not exist yet:
  v1 serializes all access by design (docs/DESIGN.md §5). When concurrent
  readers arrive, this matrix grows a new section.
