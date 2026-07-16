# Wave 1–3 implementation spec (2026-07-16)

Authoritative per-item implementation prompts for the perf campaign in
`PERF_ANALYSIS_2026_07_13.md` (same directory — read it first for the strategic picture).
Written by the Fable 5 coordinator for Opus 4.8 @ xhigh implementation agents.
Base: branch `perf/w1-concurrency-read-path` off main `cd5b4ee` (v4.1.0 + LIMIT/OFFSET
normalization). W1.1 is ALREADY IMPLEMENTED in the working tree (uncommitted; snapshot
stash `203e55e2`).

## Status & regression picture (what this campaign answers)

**Benchmark of record** (pg35, 300-iter, quiet host): Nano **34 wins / 0 losses / 1 tie**
vs PostgreSQL 18.4. Previous session's regression history — the lesson that shapes the
rules below: commit `10862ed` wired normalization ahead of the raw result cache and
silently regressed stable-text queries **460×** (LIKE 9.84µs→4.54ms); unit tests stayed
green and only the pg35 A/B caught it (fixed in `6957cd4`). Hence: **measurement-first,
one item per commit, coordinator gates every item against pg35 + bench-engines before the
next lands.**

Standing gaps (the campaign targets, in priority order):
1. **Extended/prepared-protocol concurrency** — UNMEASURED (bench harness drove only
   simple protocol until W1.1's `PROTOCOLS` cells); mutex serialization verified by
   inspection: every parameterized read locks `current_transaction`. Simple-protocol
   point reads already beat PG 1.73–2.26× (~172k TPS) since v4.0.0.
2. **4-table JOIN parity** — 691µs vs PG 686µs at HEAD-300-iter (the only non-win);
   normalized multi-join misses INLJ because `Parameter` predicates don't push down.
3. **UPDATE-point anomaly** — 179µs vs sibling DML (DELETE point 4.56µs, INSERT 9.61µs):
   ~40× internal gap ⇒ a fast-path miss, still a pg35 win only because PG is 11.66ms.
4. **COPY**: ~3× vs PG on unconstrained tables; **FK/CHECK tables fall off the fast path
   (~14×)**; wire COPY buffers the entire stream in RAM (OOM vector — this host OOM-crashed
   2026-07-08).
5. **MVCC**: ~7 global lock acquisitions + 4 keys per DML; `version_retention=None`
   default ⇒ unbounded `v:`/`snapshot:` growth; in-txn reads pay 30–150× via
   `scan_table_at_snapshot`.

## Hard rules for every implementation agent

- **NO cargo/rustc/fmt/clippy/test execution — ever.** The coordinator owns the single
  heavy-op slot (host OOM-crashed from a runaway build; one heavy op fleet-wide).
  Validation happens at the coordinator gate. You reason about compile-correctness by
  reading code.
- **NO git state changes**: no checkout/stash/reset/commit/branch. Edit files in place
  only. The working tree carries uncommitted W1.1 work — PRESERVE it.
- **Minimal diff.** Match surrounding idiom (comment density, naming, error style). No
  drive-by refactors, no new dependencies.
- **No new magic numbers** (quality gate #5): any new threshold/size must be a config
  parameter (`config.example.toml` style) or derived from an existing one. A pure
  correctness cache with natural bounds (e.g. per-table-name entries) needs no knob, but
  say so explicitly in your report.
- **ACID non-negotiables**: WriteBatch atomicity for multi-key mutations; `durable_commit`
  contract; snapshot/AS-OF exactness; RLS must never be bypassable; branch isolation.
- Every new code path needs a test that would FAIL on the pre-change code (state which
  assertion flips).
- Cite every claim in your report as `file:line`.

---

## W1.2 — Stop the per-statement LogicalPlan deep-clone (parameterized path)

**Goal**: `query_params_inner` executes the cached plan via `Arc` when no tenant/RLS
context is active, instead of deep-cloning every execution.

**Files**: `src/lib.rs` only.

**Current behavior** (`src/lib.rs:14938-14949` at branch state): 
```rust
let mut plan = match plan_override {
    Some(p) => (**p).clone(),                                  // p: &Arc<LogicalPlan>
    None => (*self.parameterized_plan_cached(sql)?).clone(),   // Arc<LogicalPlan>
};
...
plan = self.apply_rls_to_plan(plan)?;
```
A point-read plan is 10–30 allocations; this clone runs per statement and feeds malloc
contention at high concurrency.

**Change**: mirror the raw-path precedent at `src/lib.rs:13455-13461` (`query()` already
Arc-executes when `get_current_context().is_none()` — read it and copy its exact
condition, including any RLS-enabled nuance):
```rust
let plan_arc: std::sync::Arc<sql::LogicalPlan> = match plan_override {
    Some(p) => std::sync::Arc::clone(p),
    None => self.parameterized_plan_cached(sql)?,
};
let owned_plan;                       // declared first for borrow lifetime
let plan: &sql::LogicalPlan = if /* same condition as :13455 */ {
    &plan_arc                         // no tenant context → no clone, no RLS rewrite
} else {
    owned_plan = self.apply_rls_to_plan((*plan_arc).clone())?;
    &owned_plan
};
```
Everything downstream (`matches!` DML check, `execute_plan_with_params(&plan, …)`,
`query_plan_with_params(&plan, …)`) already takes `&LogicalPlan` — keep it borrowing.

**Load-bearing verification step**: read `apply_rls_to_plan` end-to-end and confirm it is
a semantic no-op when the tenant context is `None` (that's the assumption the raw path
encodes). If it does ANY work without a context (e.g. deny-by-default policies), replicate
the raw path's exact gating — do not invent a weaker one.

**ACID/RLS risk**: the only hazard is an RLS bypass — a tenant-scoped query taking the
no-clone path. Guard = same condition as the raw path; prove with tests.

**Tests** (extend an existing RLS/multi-tenancy test file — find via
`grep -rl "apply_rls_to_plan\|set_tenant\|row_level" tests/ src/`):
1. With a tenant context + RLS policy active, `query_params` returns ONLY policy-visible
   rows (would fail if the fast path skipped RLS).
2. Without a context, `query_params` results are row-for-row identical before/after (use
   an existing differential pattern).
3. `plan_override` path (`query_plan_with_params` callers) unaffected.

**STOP rule**: if `apply_rls_to_plan` is NOT provably a no-op without context and the raw
path's gating can't be mirrored exactly, stop and report instead of guessing.

---

## W1.3 — Cache the two per-statement catalog existence probes

**Goal**: kill the 2 RocksDB point-gets (`mv_catalog().view_exists(t)` ‖
`!catalog().table_exists(t)`) that every indexed point/IN/range probe pays.

**Files**: `src/sql/executor/scan.rs` (probe sites at :290-291 and the repeat around
:417-423), plus ONE owning struct for the cache (see below), plus the DDL choke points.

**Change**: a generation-stamped existence cache:
- One `AtomicU64` **schema generation** + a `DashMap<String, (u64, TableKind)>` where
  `TableKind ∈ {Table, MatView, Missing}`, owned by the struct BOTH the scan path and DDL
  path can reach (follow how f786646's schema-cache DashMap was homed — grep
  `schema_cache\|get_table_schema` in `src/lib.rs`/storage; put this cache beside it, NOT
  a global static — multiple `EmbeddedDatabase` instances must not share).
- Lookup: entry generation == current generation → use; else recompute the two probes,
  store `(gen, kind)`.
- **Invalidation = bump the generation (O(1))** from the storage-layer mutation choke
  points so ALL interfaces are covered: CREATE/DROP/ALTER/RENAME TABLE, TRUNCATE,
  CREATE/DROP MATERIALIZED VIEW, **branch switch/create/delete**, `restore`, and recovery.
  Find them via the existing `plan_invalidates_sql_caches` (lib.rs) precedent + the
  catalog/mv-catalog mutation methods themselves. Prefer bumping inside the
  catalog-mutation methods (catches wire path, REPL, HTTP, embedded).

**Staleness analysis you must write into your report** (one line per direction):
- stale `Missing`/`MatView` → query errors or takes the slow path (safe, but a *correct*
  bump on CREATE avoids it);
- stale `Table` after DROP → the downstream scan must fail cleanly (verify what the
  probe's callers do when the table is really gone — cite the line);
- **branch switch** changes visibility WITHOUT table DDL — this is the direction that
  silently returns wrong-branch data if missed. Cite the exact branch-switch function you
  hooked.

**Tests**: (1) DROP TABLE then immediate re-query → clean "table not found", not stale
success; (2) CREATE MATERIALIZED VIEW then immediate probe honors MV semantics; (3) branch
A: create+insert, switch to branch B where the table doesn't exist → correct error; switch
back → correct rows. No new knob needed (bounded by live table names) — state this.

**STOP rule**: if there is no single reachable owning struct for both paths without a
signature cascade through >3 layers, stop and report the cascade.

---

## W1.4 — Push `Parameter` predicates into index probes (multi-join INLJ)

**Goal**: normalized/parameterized `WHERE c.id = $1` (and join-feeding equality
predicates) plan index-nested-loop joins exactly like their literal twins, closing the
4-table-JOIN parity category.

**Files**: `src/sql/optimizer/rules.rs` (`can_push_predicate` :1145-1147 matches ONLY
`(Column, Literal)`); the runtime comparison extraction in the executor
(`extract_comparison` — grep it) that will see `Expression::Parameter`; possibly the
selection-pushdown/join-predicate rules that call `can_push_predicate`.

**Static trace FIRST (report before editing)**: take the pg35 4-table-join shape (see
`docs/benchmarks/PG35_BENCHMARK.md`; harness SQL — grep the repo/docs for the pg35 query
set), normalize it mentally through `src/sql/normalize.rs` (literals → `$n`), and walk the
optimizer: show WHERE the literal version becomes a FilteredScan→INLJ and the `$n` version
falls to hash/nested-loop. Name the exact rule + match arm. THEN relax.

**Change**:
- Accept `Expression::Parameter(_)` wherever `Literal` is accepted in the pushdown
  legality checks feeding index probes (equality first; ranges only if the probe machinery
  already resolves params there — the executor half is param-aware:
  `estimate_index_nested_loop_probe_rows` exists, cite it).
- **NULL semantics move to runtime**: plan-time can't see the value, so the runtime probe
  (`extract_comparison` / the index-probe resolution) must treat a NULL param bound as
  "matches nothing" (SQL: `col = NULL` is UNKNOWN → zero rows) — NOT an error, NOT a
  NULL-key match. Cite the exact line where you enforce it.
- Do NOT widen `StorageFilterPushdownRule` unless you can cite where storage-level filter
  eval resolves parameters; scope creep here is how correctness bugs ship.

**Plan-cache safety argument (write it)**: the shared parameterized plan is reused across
param VALUES; index-probe selection depends only on param POSITION, so the plan is valid
for every value INCLUDING NULL (which yields an empty probe at runtime). State why no
value-dependent plan choice was introduced.

**Tests**:
1. Differential: 4-table-join shape, literal text vs `query_params` with identical values
   → row-for-row equal (use/extend the existing raw-vs-normalized differential oracle,
   `query_raw_unnormalized`).
2. NULL param on the pushed predicate → 0 rows, matches literal `WHERE c.id = NULL`.
3. Param on EACH side (`$1 = col` reversed) if the literal path accepts both.
4. A plan-shape assertion if the repo has EXPLAIN/plan introspection (grep `EXPLAIN`);
   otherwise assert via the existing optimizer unit-test pattern in rules.rs tests.

**STOP rule**: if the executor's probe path does NOT already resolve `Parameter` bounds
(i.e. the "executor half exists" premise fails), instrument + report; do not build new
executor machinery this increment.

---

## W1.5 — UPDATE-point 179µs anomaly: instrument, then fix only if conclusive

**Goal**: find WHY a point UPDATE (`UPDATE … SET col = <expr> WHERE pk = <lit>`, the pg35
shape) misses the fast path that makes DELETE-point 4.56µs, and fix it ONLY if the miss is
a gate gap with a provably-safe widening.

**Files**: `src/lib.rs` — BOTH UPDATE fast paths: the parameterized one
(`try_fast_update_params`, gates at :7386-7389) AND the literal/autocommit one that pg35's
plain-text `query()` actually exercises (`try_autocommit_fast_update_delete` →
`fast_*_update_spec` — grep and trace both).

**Increment-0 (do first, zero behavior change)**: add
`tracing::debug!(target: "helios::fastpath", reason = "<specific>", "fast-update bail")`
at EVERY early return of both spec builders + their gates (match the existing tracing
idiom, e.g. lib.rs:14943). Reasons must be distinct and specific
("set-expr-not-literal", "where-not-pk-eq", "has-triggers", "returning", …).

**Static root-cause (report)**: trace the exact pg35 UPDATE-point shape through both
paths by reading the gates; name the first bail it hits, `file:line`. (Suspects from the
analysis: the RLS/trigger/RETURNING bails do NOT apply to the pg35 shape — the miss is
likely in the SET-expression or WHERE-shape spec build, e.g. arithmetic
`SET x = x + 1`.)

**Fix (only if conclusive)**: widen the gate/spec so the shape qualifies, ROUTING THROUGH
THE SAME version-write + WriteBatch as the slow path (time-travel `v:` key, index
maintenance, result/row-cache invalidation, durable_commit — every side effect the slow
path performs, enumerated in your report with file:line). If the miss is structural (the
fast path fundamentally can't express the shape), STOP after instrumentation and report.

**Tests**: (1) a unit test running the exact pg35 shape asserting the updated row +
version-history visibility (`AS OF` sees the pre-image); (2) FK/trigger/RETURNING variants
still take the slow path (assert unchanged results); (3) if fixed: `AS OF` + rollback
semantics on the widened shape.

**Coordinator note**: runtime confirmation (RUST_LOG run + pg35 A/B) happens at the gate;
your static trace directs it.

---

## W2.1 — COPY/bulk INSERT fast path for FK+CHECK tables

**Files**: `src/lib.rs` (`fast_literal_insert_spec` FK/CHECK bail :6572-6576;
`copy_bulk_insert`), possibly `src/storage/` ART probe helpers.

**Change**: instead of bailing, pre-validate then batch:
1. Build the typed row batch as today.
2. **FK validation, batched**: per FK constraint, collect the batch's distinct referencing
   values; probe the referenced table's PK/unique ART index once per distinct value;
   ALSO check batch-local parents (a batch may insert parent+child — maintain a HashSet of
   the batch's own new keys per referenced table). Missing parent → error with the SAME
   SQLSTATE/message as the slow path (cite both sites), whole batch aborts.
3. **CHECK validation**: evaluate each CHECK expression per row by REUSING the slow path's
   evaluator on the typed tuple (cite the function; do not re-implement expression eval).
4. Deferred FKs (`deferred_fk_checks`) stay deferred — only immediate constraints
   validate here.
5. Then the single existing WriteBatch commit — atomicity by construction.

**Tests**: violation at row N ⇒ zero rows visible after failure (atomicity); parent
earlier in same batch ⇒ pass; parent pre-existing ⇒ pass; CHECK violation ⇒ same error
text as slow path; deferred-FK COPY inside txn still defers; bench tie: COPY 100k into a
2-FK table (Pagila-like), expect ≥5× (baseline ~14× off fast path).

## W2.2 — MVCC bookkeeping diet (four independent, separately-committed increments)

**Files**: `src/storage/engine.rs`, `src/storage/time_travel.rs`.
(a) `IteratorMode::From(prefix)` + prefix-bound break at engine.rs:11180 (recovery/scan
    currently iterates from start) — one-line pure win, do first.
(b) Version metadata timestamps: `Utc::now().to_rfc3339()` String → epoch-micros i64,
    with a **forever** fallback deserializer (existing on-disk data has RFC3339 — parse
    i64 first, fall back to string parse; never remove the fallback).
(c) Snapshot-cache invalidation O(1): per-table generation counter; entries carry their
    generation; lazy-evict on mismatch (replaces the up-to-1000-entry linear scan at
    time_travel.rs:1042-1069).
(d) Lock census LAST: enumerate the ~7 per-DML global lock acquisitions (list each
    file:line in the report), then collapse only the provably-combinable ones (e.g. merge
    invalidation flags into one atomic op). Anything with unclear ordering: report, don't
    change.
**Invariant**: metadata rides the SAME atomic WriteBatch as data — no reordering.
Tests: AS-OF exactness across (b) old+new format rows mixed; snapshot reads during
concurrent DML for (c); crash-recovery replay for (a).

## W2.3 — Extended-protocol Parse reuse

**Files**: `src/protocol/postgres/handler_extended.rs` (:29, :68-69 — private
parse + catalog-execute + plan per Parse).
**Change**: derive the Describe RowDescription + param-type OIDs from the SHARED
normalized/parameterized plan cache (the embedded path's `parameterized_plan_cached` +
normalizer param typing) instead of a private plan; keep the private path as fallback for
anything the normalizer bails on. **OID parity is the contract**: numeric must stay 1700
(the 3.58.3 regression class), text/varchar/int2/4/8 exactly as today.
**Tests: MUST be wire-level psycopg** (memory: embedded tests miss
`protocol/postgres/catalog.rs` — "always psycopg-test catalog/introspection changes"):
Describe-before-Bind types/names identical pre/post for: point read, join, aggregate
alias, numeric column, prepared reuse across differing literals.

## W2.4 — Streaming COPY decode + bounded peak memory

**Files**: `src/protocol/postgres/handler.rs` (`handle_copy` accumulates the whole stream
into one uncapped `Vec<u8>` :1408-1412).
**Change**: decode incrementally per CopyData frame — carry the partial trailing line
across frames (frames split mid-line and mid-UTF8; the CSV quote state must also carry),
append TYPED rows to the batch, drop raw bytes. Keep the SINGLE atomic `copy_bulk_insert`
at CopyDone (all-or-nothing preserved). Add `[server] copy_max_buffered_rows` (or _mb) to
config + `config.example.toml` with a generous default; exceeding it aborts the COPY with
a clean PG-style error (cite PG's analogous error). This retires the raw-byte 4–6×
multiplier and caps the rest.
**Tests**: frame split mid-line / mid-quoted-field / mid-UTF8; `\.` terminator handling;
result equality vs a small single-frame COPY; cap-exceeded aborts cleanly with zero rows;
`BEGIN; COPY; ROLLBACK` still leaks nothing.

## W2.5 — Per-table committed-write watermark for in-transaction reads

**Files**: `src/sql/executor/scan.rs` (:2088, :2405 route EVERY in-txn read through
`scan_table_at_snapshot`/`read_at_snapshot`), `src/lib.rs` commit path, storage write
choke points.
**Change**: maintain `DashMap<table, last_committed_ts>`; bump on EVERY committed write to
the table — the enumerable choke points: (i) txn commit via `txn.written_data_keys()`
table extraction (precedent: row-cache invalidation lib.rs:1961-1969), (ii) autocommit
fast-path writes, (iii) COPY, (iv) DDL/TRUNCATE (treat as writes). In-txn read: if
`watermark(table) <= txn.snapshot_ts` AND the txn has NO own writes to that table (its
write-set already knows) → serve via the normal fast read path; else the old snapshot
path. **One-directional risk**: a missed bump ⇒ stale in-txn read — so bump at the
LOWEST-level write funnel you can cite, and default-closed (no watermark entry ⇒ slow
path).
**Tests**: txn sees its own writes (write-set overrides); concurrent commit AFTER txn
snapshot → txn does NOT see it (snapshot isolation) but a fresh txn does; read-committed
config variant if the isolation setting changes visibility (check `[storage] isolation`);
DDL mid-txn forces slow path; the 30–150× microbench shape as a perf tie.

## W3.1–W3.4 — design-first (deliverable = design doc + instrumentation ONLY)

- **W3.1 lock-free hot-shape slot**: FIRST attribute the c≥32 plateau — add feature-gated
  contended-lock counters (parking_lot try-lock-spin sampling or equivalent) around the
  plan-cache shard mutex + ART registry RwLock; deliver
  `docs/plans/PERF_STABILITY_2026_07/W3_1_DESIGN.md` with the epoch-validated front
  design + invalidation matrix (DDL/TRUNCATE/REINDEX/branch-switch). STOP before
  implementation until the profile pins the plateau on these locks.
- **W3.2 single-copy latest version**: quantify first — a bench/instrumentation toggle
  measuring the `v:` byte-dup share of INSERT cost; design doc for the flagged `v_idx:`
  event + materialize-on-first-mutation + on-disk compat matrix (old data readable
  forever; mixed-version replication). NO format change this campaign.
- **W3.3 same-row statement retry**: ship the typed `WriteConflict` error variant + a
  contended-writer microbench (2 writers, same row — document today's 1s pessimistic spin,
  lock_manager.rs:186-193 with the in-code futility note at :173-185); design doc scoping
  retry = whole-statement re-execution against a fresh snapshot, never partial effects,
  deadlock story. STOP before engine change.
- **W3.4 ART maintenance**: instrument ART share of COPY wall time; **if <8%, STOP** and
  record (per the roadmap's stop rule); else per-table entry lists (O(own indexes)) +
  encode-once design.

---

## Coordinator gate (runs after each item, owns the heavy-op slot)

```bash
flock /home/gpc/HDB/sprint/coordination/build.lock \
  systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0 -- <cmd>
```
Per item: `cargo test --lib` → targeted integration tests for the touched area →
`cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` → commit.
Per wave: full integration suite (with the two documented skips) + doc tests +
pg35 300-iter A/B + `PROTOCOLS="simple extended prepared" bench-engines.sh` A/B vs the
main-built binary + `ci_perf_smoke.sh`. Cumulative degradation budget <3%; any category
regression ⇒ the offending commit is reverted or fixed before the next item starts.
Branch/merge: one branch per wave, one commit per item; merge to main only after the
wave gate. Wire-touching items (W2.3/W2.4) additionally run
`tests/protocol_tests` psycopg suite.
