# W3.1 — Lock-free hot-shape slot: plateau attribution + epoch-validated design

Status: **DESIGN-FIRST. Instrumentation landed; lock-free front NOT implemented.**
Base: `perf/w3-design` off v4.2.0 (`a2e1b5b`). Companion analysis:
`PERF_ANALYSIS_2026_07_13.md` §"WAVE 3", spec `WAVE_IMPL_SPEC_2026_07_16.md` §W3.1.

> **STOP rule (binding).** The lock-free hot-shape slot is implemented ONLY if
> the contended-lock counters pin a **majority share of plateau wall-time** on
> the instrumented read-path locks (primarily the plan-cache shard mutex).
> Otherwise: record the actual attribution in §7 and stop. This document
> delivers the instrumentation and the design; the go/no-go is a coordinator
> gate decision made from the counters, not an assumption.

---

## 1. The plateau we are attributing

2026-07-17 gate numbers (post W1.1 mutex fast-out + W2.3 Parse reuse):

| protocol            | c=1    | c=16    | c=32     | c=64     | shape                    |
|---------------------|--------|---------|----------|----------|--------------------------|
| SELECT 1 (simple)   | —      | ~200k   | ~220k    | ~240k    | no table, no probe       |
| extended point-read | —      | ~120k   | ~127k    | ~132k    | `WHERE pk = $1`          |
| prepared point-read | —      | ~190k   | ~202k    | ~202k    | pinned plan, `WHERE pk=$1`|

Both parameterized curves flatten from c=32 to c=64 while the host is far from
CPU-saturated: **something serializes**. W1.1 already removed the
`current_transaction` mutex from the autocommit read path via the
`global_txn_active` atomic fast-out (`lib.rs:540`, invariant restated at
`13666`/`15466`/`15574`), so the residual plateau is downstream of that. Candidate
suspects from the analysis — to be **confirmed, not assumed**: the sharded
plan-cache shard mutex, the parse-cache/result-cache shard mutexes, and the ART
index-registry `RwLock`.

Prepared is the sharp diagnostic: R5.W2 pins `Arc<LogicalPlan>` so a prepared
execute should NOT hit `plan_cache.get` per statement — yet it still plateaus.
The prepared fast path (`try_fast_prepared_select_with_columns`, `lib.rs:1341`)
bails before parse/plan, so the only instrumented locks it crosses are three
**reader** locks: the prepared fast-select registry (`statement_registry`,
`prepared_fast_selects.read()` at `lib.rs:1249`/`1349`) and the two ART registry
reads. All three are `RwLock`/`parking_lot::RwLock` reads with the §2.1
cache-line blind spot — a read-only workload has no writer, so `try_read`
succeeds and `contended` stays near zero **by construction**; their
`acquisitions` counts prove the locks are on the prepared hot path but a low
`contended` there does NOT exonerate them. For prepared, therefore, the census
attributes by `acquisitions` + exclusion, and the go/no-go treats prepared
reader-lock `contended` as non-informative (§7) — the same rule the ART sites
carry. The result cache (`result_cache_shard`, and the single-entry
`hot_result_cache_entry`) is a poor candidate for parameterized reads with
differing `$1` values; its `acquisitions` count quantifies whether it is even on
the path.

---

## 2. Instrumentation delivered (this commit)

**Mechanism — try-lock-first sampling.** Every instrumented acquisition on the
read hot path try-locks first; a `WouldBlock` proves a holder is inside the
critical section, so it bumps a relaxed `AtomicU64` contention counter and
accumulates the blocked-wait nanos, then blocks normally. try-lock success and
poison-recovery bump only the acquisition counter. Module: `src/lock_census.rs`
(`mutex_lock` / `rwlock_read`).

**Zero cost when disabled — two gates.**
1. **Cargo feature `lock-census`** (`Cargo.toml`, off by default): when absent,
   `mutex_lock`/`rwlock_read` compile to `#[inline(always)]` pass-throughs
   identical to the previous `.lock()`/`.read()` + poison recovery, with the
   `Site` argument dropped. Release builds carry **no** instrumentation.
2. **Runtime knob `[performance] lock_census`** (`config.rs` `PerformanceConfig`,
   default `false`; documented in `config.example.toml`): within an instrumented
   build, a single relaxed `AtomicBool` fast-out gates all sampling — one
   `Relaxed` load, mirroring the `global_txn_active` precedent (`lib.rs:540`).
   Applied at `EmbeddedDatabase::with_config` via `lock_census::set_enabled`
   (process-global; last config wins — a diagnostic aggregate, like a metrics
   registry).

**Instrumented sites** (`Site` enum, `lock_census.rs:31`):

| Site ordinal / view name  | Lock                                    | Wired at |
|---------------------------|-----------------------------------------|----------|
| `plan_cache_shard`        | `ShardedLruCache` shard `Mutex` (plan)  | `sharded_lru.rs` `lock_shard`, labelled via `.with_site(Site::PlanCache)` in the three `EmbeddedDatabase` constructors |
| `parse_cache_shard`       | shard `Mutex` (parse)                   | `.with_site(Site::ParseCache)` |
| `result_cache_shard`      | shard `Mutex` (result)                  | `.with_site(Site::ResultCache)` |
| `art_index_registry`      | `ArtIndexManager.indexes` `RwLock` read | `art_manager.rs` `indexes_read()` used by `get_index`/`pk_index_lookup`/`pk_index_contains` |
| `art_pk_registry`         | `ArtIndexManager.pk_indexes` `RwLock` read | `art_manager.rs` `pk_indexes_read()` used by `get_pk_index`/`pk_index_lookup`/`pk_index_contains` |
| `statement_registry`      | `prepared_fast_selects` `parking_lot::RwLock` read | `lib.rs` via `lock_census::pl_rwlock_read(Site::StatementRegistry, …)` at `prepared_plan_or_lazy_fast_plan` (`:1249`) and the prepared fast path `try_fast_prepared_select_with_columns` (`:1349`) |

The write-path DML spec caches (`fast_param_*`, `fast_select`) keep the default
`Site::Unlabeled` and are never sampled. The per-tree ART lock (`entry.tree.read()`)
is intentionally NOT instrumented — the target is the *registry*, not tree probes.

**Surface (interface-coverage gate #5).** System view `heliosdb_lock_census`
(registered in `sql/phase3/system_views.rs`, dispatched to
`execute_heliosdb_lock_census`) — one row per site with columns
`lock_site, acquisitions, contended, contended_wait_nanos`. Reachable as
`SELECT * FROM heliosdb_lock_census` (bare, via the scan path) and
`heliosdb_lock_census()` (function form, via `planner.rs` table-factor →
`is_system_view`). Empty on a non-`lock-census` build (snapshot returns `[]`).
Also surfaced as a `\stats` hint in the REPL (`repl/commands.rs`, `ShowStats`).

**How the gate runs it** (coordinator, single heavy-op slot):
```
# instrumented build
cargo build --release --features lock-census
# config.toml: [performance] lock_census = true
PROTOCOLS="extended prepared" benches/.../bench-engines.sh   # drive c=32/64 point-read
psql -c 'SELECT * FROM heliosdb_lock_census;'                 # read the attribution
```
Compute `contended / acquisitions` per site and `contended_wait_nanos` share of
wall-time. The go/no-go (§7) reads directly off these.

### 2.1 Known blind spot (document, don't paper over)

try-lock sampling catches **lock-blocking** contention (a holder forces you to
block), NOT **cache-line** contention on a lock's internal atomics. This matters
for the three **reader** sites — `art_index_registry`, `art_pk_registry`, and
`statement_registry`: they are `RwLock`/`parking_lot::RwLock` **reads**, and a
read-only benchmark has no writer to those maps (they change only on DDL /
prepare/dealloc), so `try_read` almost always *succeeds* → near-zero
`contended`, high `acquisitions`. Yet the `read` still touches a shared reader
atomic, whose cache line ping-pongs across cores under many readers — real cost
the counters will UNDER-report. Interpretation rule for the gate:

- `plan_cache_shard` high `contended` ⇒ **mutex serialization** — the plateau
  mechanism; the lock-free slot is justified. (A single hot query shape hashes to
  ONE shard, so every concurrent reader of that shape serializes on one mutex on
  `get`/`put` — the smoking gun.)
- Reader sites (`art_*_registry`, `statement_registry`) high `acquisitions` +
  ~zero `contended` ⇒ NOT lock-blocking. **A NO-GO cannot be inferred from a
  reader site's `contended` alone**: `contended≈0` on these sites is EXPECTED and
  does NOT prove the lock is not the serializer — the reader cache-line
  under-reports (see above). If the plateau persists with the mutex-backed caches
  pinned lock-free, the residual is a reader cache-line (or allocator/socket) and
  needs a *different* fix (pinned probe handles / a seqlock registry) — record and
  re-scope, do not build the hot-shape slot to chase it.

---

## 3. Read-path lock census (the map)

Per extended point-read (`WHERE pk = $1`), the locks crossed after the W1.1
fast-out lets it skip `current_transaction`:

1. `parse_cache.get(sql)` → parse_cache shard `Mutex` (may miss → `put`).
2. `parameterized_plan_cached(sql)` (`lib.rs:11040`) → plan_cache shard `Mutex`
   (`get`; on miss, plan + `put`). Extended non-prepared pays this per statement;
   prepared pins the `Arc` (R5.W2) and skips it.
3. result_cache shard `Mutex` (`get`/`put`) — parameterized reads with differing
   `$1` values are poor result-cache candidates; acquisitions here quantify
   whether it is even on the hot path.
4. ART probe: `art_pk_registry` read (resolve PK index name) → `art_index_registry`
   read (resolve handle) → per-tree read (probe). Registry reads shared; see §2.1.

The **prepared** fast path (`try_fast_prepared_select_with_columns`,
`lib.rs:1336`) is shorter: it crosses `statement_registry`
(`prepared_fast_selects.read()`, `lib.rs:1349`) to resolve the pinned spec, then
goes straight to the ART probe (step 4) — it does NOT touch parse_cache,
plan_cache (bails before parse/plan), or the result cache. So of the six sites,
prepared usefully crosses only the three reader sites (`statement_registry`,
`art_pk_registry`, `art_index_registry`), all under the §2.1 blind spot; its
attribution is `acquisitions` + exclusion, not `contended` (§7).

`current_transaction` (`lib.rs:415` field) is **already** fast-outed by W1.1 and
is NOT a plateau suspect for autocommit reads (expect zero if it were
instrumented) — noted for completeness; not wired.

---

## 4. Design: epoch-validated lock-free hot-shape slot

**Goal.** Remove the plan-cache shard `Mutex` (and, for prepared, the
result-cache mutex) from the steady-state read path for the *hottest* query
shape, by serving a pre-resolved `(plan, probe handle)` through a lock-free front
validated against a monotonic epoch that every invalidation class already bumps.

### 4.1 What the slot caches

A small, fixed set of **hot-shape slots** (start: 1, tunable — see §6), each:

```
struct HotShapeSlot {
    fingerprint: u64,              // hash of the normalized SQL text
    plan_epoch: u64,               // plan_cache.epoch() at population
    schema_generation: u64,        // storage.schema_generation() at population
    plan: Arc<LogicalPlan>,        // == parameterized_plan_cached output
    // (phase 2, optional) resolved point-probe handle: SharedArtIndex + column meta
}
```

Both cached artifacts survive DML: DML mutates ART **tree contents** under the
per-tree lock, not the registry map and not the plan shape. Only DDL / branch /
MV / view / (future) config-reload change plan shape or which index exists — and
every one of those already bumps one of the two epochs below.

### 4.2 The two existing epoch funnels (reuse, don't invent)

- **`plan_cache` epoch** — `ShardedLruCache.epoch` (`sharded_lru.rs:136`),
  incremented by `clear()` (`sharded_lru.rs:127`). `invalidate_plan_cache`
  (`lib.rs:8242`) calls `plan_cache.clear()`, fired from the SQL execution funnel
  whenever `plan_invalidates_sql_caches` (`lib.rs:932`) is true. This is the
  authoritative *plan-shape* invalidation and already covers CREATE/DROP/ALTER
  TABLE, CREATE INDEX, TRUNCATE, CREATE/DROP MV, CREATE/DROP VIEW, and
  USE/MERGE/DROP BRANCH (the enum arms are enumerated in `plan_invalidates_sql_caches`).
  `wire_parameterized_plan` (`lib.rs:11067`) already returns `(Arc<Plan>, epoch)`
  and R5.W2 prepared statements already pin+validate against it — **precedent for
  exactly this front.**
- **`schema_generation`** — `StorageEngine.schema_generation` (`engine.rs:1847`),
  `bump_schema_generation` (`engine.rs:2576`), read via `schema_generation()`
  (`engine.rs:2568`). Bumped at the **lowest** catalog/branch funnel, so it
  catches every interface (wire / REPL / HTTP / embedded / restore / WAL-recovery)
  — the W1.3 existence cache already validates against it. Critically it also
  bumps on **branch switch** (`set_current_branch` `engine.rs:12422`,
  `clear_current_branch` `engine.rs:12437`) and **merge-to-main** (`engine.rs:8960`,
  `9034`), the wrong-data directions.

The slot validates against **both**: `plan_epoch` guards the plan; `schema_generation`
guards the resolved probe handle and the storage-level existence/branch view.

### 4.3 Read protocol (lock-free fast path)

```
let g_plan = plan_cache.epoch();          // Acquire load, no lock
let g_schema = storage.schema_generation();// Acquire load, no lock
let slot = SLOT.load();                    // ArcSwap / seqlock read, no shard mutex
if slot.fingerprint == fp
   && slot.plan_epoch == g_plan
   && slot.schema_generation == g_schema {
       execute(slot.plan, params);         // hit: zero shard-mutex acquisitions
} else {
       let (plan, ep) = wire_parameterized_plan(sql)?;   // miss: existing locked path
       SLOT.store(HotShapeSlot { fp, ep, g_schema_now, plan });
       execute(plan, params);
}
```

Mechanism options for `SLOT` (decide at implementation from the profile):
- **`arc-swap`** single-slot `ArcSwapOption<HotShapeSlot>` — RCU-style, wait-free
  reads, one atomic load. Simplest; a new crate dep (evaluate `deny.toml`).
- **Seqlock over an `AtomicU64` version + `Arc` cell** — no new dep, but hand-
  rolled; readers retry on odd/mismatched version. Prefer only if arc-swap is
  disallowed.

Validation is monotonic-counter equality, so a stale slot can only ever
*fall back* to the locked path (safe) — never serve a wrong plan.

Both epochs are loaded **before** `SLOT.load()` (not after the slot read, despite
the spec's "validate epoch after lock-free read" phrasing). This is the
conservative-correct direction: a slot stamped under a NEWER epoch than the one
the reader loaded is rejected (equality fails) and re-taken through the locked
path — a spurious miss, never a wrong serve; only a *stale* slot could be
falsely served, and that requires the reader to have loaded an epoch NEWER than
the slot's, which the load-before ordering makes impossible. It mirrors the
verified R5.W2 prepared-plan pin ordering (`wire_parameterized_plan`
epoch-before-build, `lib.rs:11067`).

### 4.4 Why this is correct (invariants)

- **No wrong plan.** A plan-shape change bumps `plan_cache.epoch` (via
  `invalidate_plan_cache`); the equality check fails → fallback. There is no
  window where an invalidation is visible to the data path but not to the epoch:
  `clear()` bumps the epoch (`sharded_lru.rs:127`) under the same call, and the
  reader loads the epoch *before* reading the slot (Acquire), so a slot populated
  under an old epoch is rejected.
- **No wrong-branch/wrong-existence data.** Branch switch and catalog change bump
  `schema_generation` at the storage funnel (§4.2); the probe-handle validation
  fails → fallback to `wire_parameterized_plan` + fresh resolve.
- **DML is a non-event.** No DML path bumps either epoch, and it must not: the
  plan and the index handle stay valid; only tree contents change, read under the
  per-tree lock at execute time.
- **RLS / tenant.** The slot is populated only when `get_current_context().is_none()`
  (mirror W1.2's gate `lib.rs`); a tenant-scoped statement bypasses the slot and
  takes the RLS-rewriting path. The lock-free front must replicate that gate
  exactly — an RLS bypass is the one catastrophic failure mode.

---

## 5. Complete invalidation matrix

Every class that can invalidate a cached `(plan, probe handle)`, mapped to the
epoch it already bumps. "plan_cache clear" = `invalidate_plan_cache`
(`lib.rs:8242`) via `plan_invalidates_sql_caches` (`lib.rs:932`); "schema_gen" =
`bump_schema_generation` (`engine.rs:2576`).

| Invalidation class            | plan_cache epoch bump                          | schema_generation bump                          | Slot outcome |
|-------------------------------|------------------------------------------------|-------------------------------------------------|--------------|
| CREATE TABLE                  | ✓ `CreateTable` arm → clear                     | ✓ `catalog.create_table` (`catalog.rs:260`)     | reject → refill |
| DROP TABLE                    | ✓ `DropTable` arm                               | ✓ `catalog.drop_table` (`catalog.rs:396`)       | reject → clean error on refill |
| ALTER TABLE (add/drop col, rename, FK, constraint) | ✓ `AlterTable*` arms         | ✓ add-column (`catalog.rs:351`), rename (`catalog.rs:1250`) | reject → refill |
| TRUNCATE                      | ✓ `Truncate` arm (`lib.rs:953`)                 | N/A — does NOT bump; TRUNCATE (`lib.rs:11667`) clears index trees in place (`clear_table_indexes`, `art_manager.rs:1826`) without swapping the registry entry, so a resolved handle stays valid (empty tree → 0 rows) | plan_epoch covers it → reject → refill |
| CREATE INDEX                  | ✓ `CreateIndex` arm (`lib.rs:950`)              | N/A — `handle_create_index` (`ddl.rs:185`) adds a registry entry without a `bump_schema_generation` call; the plan_epoch bump forces re-plan that resolves the new handle | plan_epoch covers it → reject → refill (new handle) |
| REINDEX                       | n/a — **not supported in this base** (no rebuild) | n/a                                           | no-op today; **if a real rebuild lands (cf. `1a47098`) that swaps handles, it MUST bump schema_generation** |
| CREATE / DROP MATERIALIZED VIEW | ✓ `Create/DropMaterializedView` arms          | ✓ `materialized_view.rs:168` / `:234`           | reject → refill |
| MV REFRESH                    | — (data-only, plan unchanged)                   | ✓ `materialized_view.rs:168`/`:234` (drops/recreates data table) | conservatively rejects → refill (safe) |
| CREATE / DROP VIEW            | ✓ `Create/DropView` arms (`lib.rs:967`/`:968`; view is inlined into plan) | N/A — plan_epoch fully covers (views are plan-inlined, no probe handle of their own) | plan_epoch guards it → reject → refill |
| USE BRANCH (switch, incl. → main) | ✓ `UseBranch` arm; REPL pre-detect calls `invalidate_plan_cache` directly (`repl/shell.rs`) | ✓ `set_current_branch` (`engine.rs:12422`) / `clear_current_branch` (`engine.rs:12437`) | reject → refill against new branch |
| MERGE BRANCH                  | ✓ `MergeBranch` arm                             | ✓ **only for `merge_to_main`** (`engine.rs:8960`/`:9034`, both gated on `if merge_to_main` at `:8959`/`:9033`); non-main merges write `bdata:` and rely on the plan_epoch (`MergeBranch` arm) + the branch-switch row-cache clear | reject → refill |
| DROP BRANCH                   | ✓ `DropBranch` arm                              | (branch metadata; current-branch unaffected)    | plan_epoch guards → reject → refill |
| CREATE BRANCH                 | — (excluded by design: snapshots without changing current visibility) | — | slot stays valid (correct: current view unchanged) |
| Config reload (optimizer cost params, etc.) | — **no live reload in this base** (config bound at construction; `SET` changes session settings only) | — | N/A today; **if live reload of plan-affecting params lands, it MUST bump plan_cache epoch** |

Audit items resolved (no open questions in the matrix): TRUNCATE and CREATE INDEX
do NOT bump `schema_generation` (verified: no `bump_schema_generation` call in
the TRUNCATE funnel `lib.rs:11667-11800` nor in `handle_create_index`
`ddl.rs:185`), and they do NOT need to — the `plan_epoch` guard rejects the slot
via their `plan_invalidates_sql_caches` arms (`lib.rs:953` / `:950`), and neither
invalidates an existing probe handle (TRUNCATE clears trees in place without
swapping the entry; CREATE INDEX only ADDS an entry, and the forced re-plan
resolves it). CREATE/DROP VIEW likewise is fully covered by `plan_epoch` alone
(`CreateView`/`DropView` arms, `lib.rs:967`/`:968` — views are plan-inlined, per
the comment there), so its `schema_generation` column is N/A. The one remaining
forward-looking guard: **if** a REINDEX/rebuild that swaps handles (cf. `1a47098`)
or a live config reload of plan-affecting params ever lands, it MUST bump
`schema_generation` / `plan_cache` epoch respectively (flagged in the table).

---

## 6. Interface coverage & tunables (gate #5)

- **Instrumentation knob:** `[performance] lock_census` (config.toml) + build
  feature `lock-census`. Documented in `config.example.toml`. Surfaced by
  `heliosdb_lock_census` (SQL) and `\stats` (REPL).
- **Slot sizing (phase 2, when implemented):** the number of hot-shape slots must
  be a config parameter, e.g. `[performance] hot_shape_slots` (default 1), NOT a
  hardcoded constant — matching the no-magic-numbers rule. A single slot suits the
  point-read microbench; multi-shape workloads may want a small N.

---

## 7. Go / No-Go rule (coordinator decision)

**GO** — implement the lock-free slot — iff the census (instrumented+enabled
build, c=32/64 extended point-read) shows:
- `plan_cache_shard` `contended / acquisitions` is a **majority** of acquisitions,
  AND `contended_wait_nanos` on `plan_cache_shard` is a majority share of the
  measured per-statement wall-time gap between c=1 and c=32/64.

**NO-GO / re-scope** — record and stop — if instead:
- Contention is dominated by `result_cache_shard` (then the fix is result-cache
  scoping, a different item), OR
- The **mutex** sites (`plan_cache_shard`, `parse_cache_shard`,
  `result_cache_shard`) all show low `contended` (then the serializer is outside
  the lock-blocking path — a reader cache-line per §2.1, allocator, or socket —
  and the slot would not move the plateau; capture the real attribution here).
  **Do NOT read low `contended` on the reader sites (`art_*_registry`,
  `statement_registry`) as evidence of a NO-GO**: `contended≈0` there is expected
  by construction (§2.1) and is non-informative — the GO decision is anchored on
  the mutex sites only, and a residual after pinning the mutex caches lock-free is
  what re-scopes the reader sites to a seqlock/pinned-handle registry.

**Prepared vs extended are decided separately.** Prepared skips `plan_cache.get`
and `parse_cache`, so its only instrumented on-path locks are the three reader
sites (`statement_registry`, `art_pk_registry`, `art_index_registry`), all under
the §2.1 blind spot. Their `acquisitions` confirm the path but their `contended`
is non-informative — **a prepared verdict CANNOT be concluded from `contended`
alone with this instrumentation set.** Concretely: prepared shows GO for the
hot-shape slot only if the plateau is attributable to a mutex site prepared
actually crosses (it crosses none of `plan_cache`/`parse_cache`/`result_cache` on
the fast path), so the hot-shape slot as designed does NOT target prepared; if
prepared plateaus with the extended mutex caches pinned, that residual is a
reader cache-line (`statement_registry`/ART) and re-scopes to a pinned-handle /
seqlock registry — a different fix, recorded here, not the plan-cache slot.

Record the actual counter readings inline here when the gate runs:

```
# MEASURED 2026-07-17 (coordinator gate, chain B run bm5ek1l05):
# c=32, protocol=extended, pgbench point-read 50k rows, -T 15, tps=129,181
# build=lock-census @ c5c762a, [performance] lock_census=true
plan_cache_shard      acq=1,937,308  contended=321,484  wait_ns=381,446,234   (mutex — GO anchor)
parse_cache_shard     acq=29         contended=1        wait_ns=340           (mutex)
result_cache_shard    acq=0          contended=0        wait_ns=0             (mutex)
art_index_registry    acq=0          contended=0        wait_ns=0             (reader — §2.1 blind)
art_pk_registry       acq=0          contended=0        wait_ns=0             (reader — §2.1 blind)
statement_registry    acq=0          contended=0        wait_ns=0             (reader — prepared-path, §2.1 blind)
Verdict: NO-GO  because the contended RATE is high (321,484/1,937,308 = 16.6%
of acquisitions) but the blocked TIME is negligible: 381 ms total across
32 clients x 15 s = 480 client-seconds, i.e. ~0.08% of wall-time. Failed
try-locks resolve almost instantly under parking_lot's adaptive spin; nothing
queues. The c>=32 plateau (129k tps @c32 -> 130k @c64 extended in this same
harness) is NOT lock-blocking on any instrumented mutex — per the §7 NO-GO
arm, the serializer is outside the lock-blocking path (reader cache-line
per §2.1, allocator, or socket/runtime). The epoch-validated slot as designed
would not move the plateau; do not implement. Next measurement round should
attribute via perf/off-CPU profiling rather than lock counters.

Instrumentation bug found during the run (does not affect the verdict — the
first sweep is a complete 15 s sample): counters FROZE after the first pgbench
sweep; the three subsequent sweeps (extended c=64, prepared c=32/64 — tps
130k/194k/203k) added zero acquisitions. Suspected label loss on a cache
rebuild after the 32-session disconnect storm (rebuilt shards default to
Unlabeled) — census sites must survive cache reconstruction. Filed with the
W3-implementation lease.
```
