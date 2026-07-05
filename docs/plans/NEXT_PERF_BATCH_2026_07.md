# HeliosDB-Nano — Next Perf Batch Roadmap (post-v4.0.0) — v2.1

_v1 (2026-07-05): 7-way parallel subsystem analysis + adversarial synthesis (git history @85c978f)._
_v2 (2026-07-05): per-topic design refinement — each item re-examined for a strictly better
mechanism, staging, or risk profile. What changed vs v1 is called out per item._
_v2.1 (2026-07-05): **code-verified key points** added per item from direct source analysis —
several change the implementation (Item 2's columnar gate, Item 2's exact AS-OF hook,
Item 7's side-cache requirement). Marked as **Code-verified key points (v2.1)** blocks._

> **To execute:** read [`NEXT_PERF_BATCH_EXECUTOR_HANDBOOK.md`](NEXT_PERF_BATCH_EXECUTOR_HANDBOOK.md)
> first — it has the verified code anchors (symbol + grep), the campaign's non-negotiable
> invariants, the exact gate/bench/release commands, and the per-item traps this roadmap
> assumes you know.

Ranking is unchanged except where noted (#5 becomes promotable — its blocker is resolved by
design rather than by product decision).

---

## NEXT MILESTONE

### 1. Normalizer widening — IN/BETWEEN/casts **with arity padding** (S / low)

**v1 proposal:** remove `in`/`between` from `is_predicate_bail_kw`, pass `::type` through as
`$n::type`, bail above ~32 IN elements.

**Refined design:**
- **Power-of-two IN-arity padding** instead of a hard cap: normalize `IN ($1,$2,$3)` by
  padding to the next power of two, repeating the last parameter (`IN ($1,$2,$3,$3)`).
  IN has set semantics, so duplicate elements are provably result-neutral (holds for
  `NOT IN` and NULL elements too). Distinct plan-cache shapes collapse from one-per-arity
  to log₂(arity) — an ORM sweeping arities 1..200 produces ~8 cached plans, not 200.
  Keep a bail above 128 elements (padding cost + plan quality both degrade).
- **Cast whitelist, not blanket passthrough:** only rewrite `literal::T` → `$n::T` for
  `T ∈ {uuid, int2/4/8, bigint, text, varchar, date, timestamp, timestamptz, numeric,
  float4/8, boolean}` — the types whose `Cast{expr: Parameter}` unwrap is already verified
  in the index-probe path (`scan.rs::lookup_bound_value` handles Cast-over-Parameter).
  Unknown cast targets bail as today.
- **Oracle upgrade to match the wider shape space:** add a property-based corpus
  (randomized tables + predicates, raw vs normalized row-parity) alongside the fixed
  corpus, and specifically: `IN` with NULL element, `NOT IN`, duplicate elements,
  1/2/3/33/128-arity, `BETWEEN` inclusive bounds and reversed bounds, each whitelisted
  cast, and `POSITION('x' IN s)` (the non-predicate `IN` false-positive).
- Keep `query()` un-normalized (it remains the oracle's independent raw reference).

**Why better than v1:** the hard cap silently gives up on the most common ORM shape
(large preload lists); padding keeps them cached with bounded shape count and zero
semantic risk. The cast whitelist turns "medium blast radius" into "verified-path only."

**First step:** padding helper + keyword removal + whitelist, then extend the oracle
before wiring — the oracle must fail closed on any shape the lexer mis-handles.

**Code-verified key points (v2.1):**
- **Executor readiness is confirmed in source, not assumed.** The PK-IN fast path resolves
  parameters directly: `pk_in_list_value` matches `LogicalExpr::Parameter { index } =>
  self.parameters.get(index-1)` (`src/sql/executor/mod.rs:2317-2325`). The indexed range
  fast path's `Between` arm resolves BOTH bounds through `lookup_bound_value(low/high,
  parameters)` (`src/sql/executor/scan.rs:458-470`). So `IN ($1,$2,…)` keeps the PK-IN
  count/scan fast paths and `BETWEEN $1 AND $2` keeps the index range scan — no executor
  work needed, as designed.
- **Ride the plain-InList lowering, do NOT emit an array parameter.** The planner lowers
  literal-array `= ANY(…)` to a plain `InList`, while *parameter arrays* use a special
  runtime-expansion marker (`src/sql/planner.rs:~3548-3575`). Per-element `$n` params go
  through the ordinary `InList` path the fast paths understand; a single array param would
  route into the marker machinery and lose them.
- **Padding trigger discriminator:** only pad a parenthesized list following a predicate
  `IN` — i.e. the token pattern `IN` + ws + `(`. `POSITION('x' IN s)` has no `(` after its
  `IN`, so the discriminator excludes it structurally; still add the POSITION test to the
  oracle (and note the lexer's literals inside POSITION's parens WILL be parameterized,
  which is safe — only the list-shape padding must not trigger).
- **NULL/dup padding safety:** repeating the last element is result-neutral for `IN` and
  `NOT IN` including NULL elements (three-valued logic is per-element; duplicates add
  nothing). Encode this as oracle cases rather than a comment.

---

### 2. COPY → PG parity — **transient batch marker + background materialization** (L / medium)

**v1 proposal:** permanent `vrange:{table}:{first}:{last}:{ts}` markers replacing per-row
`v:`/`v_idx:` writes; lazy backfill on UPDATE/DELETE; permanent in-memory interval map.

**Refined design:** same critical-path win, but the marker is **transient**:
1. COPY's fast batch writes `data:` rows + ONE durable `vmeta:{table}:{first}:{last}:{ts}`
   record in the same WriteBatch (atomic). No per-row `v:`/`v_idx:` on the COPY path —
   the full measured win (~230–270 ms of the 423 ms @100k) stays.
2. A **background materializer** (same worker pattern as SMFI rebuilds) walks live markers
   at low priority, writes the standard per-row `v:`/`v_idx:` records batch-by-batch, then
   deletes the marker. The system **converges to today's on-disk format** — no permanent
   new MVCC concept for AS-OF, branches, GC, or backup/restore to understand forever.
3. While a marker is live (transient window): an in-memory per-table interval set (loaded
   from the `vmeta:` prefix at open — crash-safe resume) is consulted by AS-OF reads and
   UPDATE/DELETE. The consult is guarded by one process-wide atomic "any live markers"
   fast-out, so the **permanent tax on the hot paths is a single atomic load** once
   materialization drains. UPDATE/DELETE of a covered row synchronously materializes just
   that row first (the update path already reads the old row value; the marker carries the
   insert ts it needs for the `v:{t}:{row}:{revts}` key).
4. AS-OF semantics during the window: `T < ts` → row excluded (marker says so);
   `T ≥ ts`, no newer version → serve `data:` (insert version == current value).

**Fallback increment (lower risk, likely 60–75% of the win):** keep per-row `v_idx:`
(20-byte keys — cheap) and elide only the `v:` full-value duplicate; AS-OF derives the
insert version from `data:` when the `v_idx:` seek shows no newer version (that seek
already happens on every AS-OF read, so reads pay nothing extra); UPDATE/DELETE backfills
the old `v:` using the ts recovered from the row's existing `v_idx:` entry
(`v_idx:{t}:{row}:{rev_ts} → actual_ts`, verified `time_travel.rs:653-664`). No interval
map at all.

**Why better than v1:** v1's permanent interval map grows with every COPY forever and puts
a check on every future UPDATE/DELETE of every table. The transient design bounds the
novel semantics to a drain window, self-cleans, converges to the standard format, and
collapses the permanent cost to one atomic load. The fallback increment needs no marker
machinery at all.

**First step:** ship the fallback increment behind `HELIOS_COPY_ELIDE_V=1`, gated on
time-travel + branch + crash suites; then the transient-marker design as increment 2.
Before choosing, run one measurement with only the `v:` put removed to learn the
WAL-bytes vs memtable-entry split of the 230–270 ms — it decides whether increment 1
alone reaches parity.

**Code-verified key points (v2.1):**
- **⚠️ Columnar gate (invalidates the naive fallback for columnar tables).** In
  `insert_prepared_tuples_fast_batch`, `data:` stores `serialize_stored(&tuple)` — which
  replaces columnar slots with `Value::ColumnarRef` sentinels — while `v:` stores a
  separate LOGICAL serialization with real values (the in-code comment: *"Version history
  stays logical … AS OF reads must not see ColumnarRef"*, `src/storage/engine.rs`
  ~10390-10420). **Deriving the insert version from `data:` is therefore WRONG when
  `uses_columnar`** — gate any elision on `!uses_columnar` (COPY's fast path today serves
  Default/Columnar storage; elide only for the non-columnar case, which is every
  benchmark/real table).
- **Exact key formats (from the write site, don't re-derive):** `v:{table}:{row_id}:{ts}`
  (FORWARD ts suffix) → logical row bytes; `v_idx:{table}:{row_id}:{reverse_ts:020}` →
  **8-byte BE `actual_ts`**. The AS-OF reader only requires `value.len() >= 8`
  (`time_travel.rs:672`) — so an **elision flag can be a 9th byte appended to the v_idx
  value** without breaking existing readers or the on-disk format.
- **The AS-OF hook is exactly `read_at_snapshot_uncached`** (`time_travel.rs:647+`):
  v_idx seek → `get_version_by_exact_timestamp` → and, critically, an explicit
  no-versions fallback: *"If none exist at all, the row was written through a
  non-versioned path (fast insert) and should be visible to all snapshots."*
  Consequences: (a) the **full transient-marker design MUST intercept in that
  no-versions branch**, or COPY'd rows (having no `v_idx:` at all) become visible to ALL
  earlier snapshots by this rule; (b) for the fallback increment, the "am I the newest
  version" test is one cheap seek: the FIRST key under the `v_idx:{t}:{row}:` prefix in
  forward order is the newest overall (smallest reverse_ts) — compare it to the found key.
- **The backfill API already exists:** `pub fn write_version(&self, table, row_id,
  timestamp, value)` (`time_travel.rs:736`). UPDATE/DELETE backfill is a call to an
  existing tested function, not new machinery.
- **Snapshot cache coherence:** `read_at_snapshot` memoizes `(table,row,ts) → result`
  (`time_travel.rs:622-641`). Elision + backfill must flow through / invalidate this cache
  (a backfill that changes what a snapshot read returns must not leave a stale memo).

---

### 3. OLAP — activate the shipped columnar engine via a **derived cache with watermark + delta overlay** (L / medium)

**v1 proposal:** columnar side copies; relax the two planner gates; any-DML-marks-stale →
background full re-backfill; increment 0 = measure the existing kernels first.

**Refined design (increment 0 unchanged — measure before building):**
- Side data is explicitly a **derived cache**, keyed by a per-table **watermark**
  (max row-id/LSN covered) in the catalog: scans serve rows ≤ watermark from typed
  columnar batches and the tail (> watermark) via the row path, merged. A trickle of
  INSERTs just lags the watermark — **analytics never fall off a cliff**, unlike v1's
  whole-table staleness where one INSERT reverts the table to the row path until a full
  O(table) re-backfill completes.
- UPDATE/DELETE of covered rows: clear the row's presence bit in its batch (presence
  bitmaps shipped in b866669) and append the row-id to a small in-memory **delta list**;
  scans overlay delta rows via the row path. A background compactor folds deltas and
  advances the watermark when lag exceeds a threshold. v1's "any DML → stale" becomes
  "DML → O(1) bitmap clear + delta append."
- Population is **always background** (bulk-load hook enqueues, never inline) — resolving
  the cross-item tension with #2's COPY-parity path that the v1 synthesis flagged.
- Recovery for free: side data is derivable, so the v1 delta list can be memory-only —
  on crash, rebuild from a watermark re-scan (or re-run backfill). No new durability
  obligations.
- Gates: planner keys on catalog "columnar cache ready(watermark)" instead of
  `storage_mode == Columnar`; session kill switch `SET helios.columnar_scan = off`;
  adoption explicit at first (`ALTER TABLE … SET COLUMNAR CACHE ON`), auto-threshold
  behind a flag later.

**Why better than v1:** v1's invalidation model made the feature self-defeating on any
table that receives writes — exactly the HTAP case. Watermark + delta overlay is the
standard main-store/delta-store split, reuses the shipped presence bitmaps and grouped
batch writer, and keeps the OLTP write path at one bitmap-clear + list-append.

**Increment 0 (unchanged, mandatory):** half a day — load the 1M-row P1_5 suite into a
`STORAGE COLUMNAR` twin and measure vs row twin + SQLite. **< 5× → stop.**

**Code-verified key points (v2.1):**
- **Increment 0 must use only kernel-supported shapes** or it will falsely conclude "no
  win": `kernel_update_aggregate` (`src/storage/engine.rs:853`) implements Count
  (mask-count), Sum and Avg over typed Int/Float batches — enumerate the full supported
  set (incl. whether Min/Max and GROUP BY forms are batch-direct) from that match block
  BEFORE writing the increment-0 query list.
- **The side-copy write is a one-branch change, not a new writer.** The fast batch already
  appends grouped columnar writes to the same WriteBatch (`stats_write_lock()` +
  `group_columnar_row_values(schema, &row_refs)`, `engine.rs:~10480`). The only difference
  for side copies is in the `serialize_stored` closure (`engine.rs:~10390`): KEEP real
  values in `data:` (skip the `ColumnarRef` replacement) while still emitting the grouped
  batches. True-columnar behavior stays intact for explicitly-declared tables.
- **Gate relaxation sites confirmed:** `indices_are_columnar` (`scan.rs:135`, call sites
  :101/:119/:132) and `try_columnar_aggregate`'s early storage/columns check
  (`executor/mod.rs:~2450`). Both currently key on `ColumnStorageMode::Columnar` per
  column; the side-copy check replaces this with "table's columnar cache ready + watermark
  covers scan" from the catalog.
- **Point-read protection is structural:** with real values kept inline in `data:`, the
  `ColumnarStore::get` full-batch-decode path (`columnar.rs:932-946`, BATCH_SIZE=1024,
  uncached) is never on the OLTP read path for side-copy tables — the v4.0.0 read win
  cannot regress by construction.

---

### 4. Aggregate-over-Join pruning — **as an optimizer rule, not an executor special case** (M / low)

**v1 proposal:** in the executor's Aggregate arm, collect required columns and call
`compact_projected_join_inputs` (the machinery Project-over-Join already uses).

**Refined design:** implement the same column-requirement propagation as a **plan-time
projection-pushdown-through-join rule** (extend the existing `ProjectionPruningRule`):
insert pruned `Project` nodes under join inputs based on the union of columns required by
*any* ancestor — Aggregate (group-by keys + aggregate args + HAVING), Sort, Project,
Filter. The executor's existing Project-under-Join handling consumes it unchanged.
- **Why plan-time is better:** it composes with the (now A2-normalized) plan cache — the
  pruned plan is cached once and reused across literal variants; it benefits every
  consumer shape instead of one executor arm; and the cost-guarded rule acceptance
  (`new_cost <= old_cost`) plus wildcard bail keep it safe.
- Prioritize **hash-join build-side** pruning: build rows are materialized in the hash
  table, so pruning there cuts memory + build time, not just per-row clones.
- Keep v1's executor-arm version as the fallback if the rule's interaction surface with
  correlated subqueries proves noisy — both reuse the same
  `compact_projected_join_inputs` core, so the work is shared.

**First step:** the rule + pg35 join categories and the join/window/subquery hardening
suites as the parity gate; measure on a TEXT-heavy aggregate-over-join microbench (v1's
2–4× claim assumes wide TEXT rows — validate that assumption explicitly).

**Code-verified key points (v2.1):**
- **Path correction:** `ProjectionPruningRule` lives at `src/optimizer/rules.rs:494`
  (NOT `src/sql/optimizer/` — that path doesn't exist).
- **The reusable core is pure plan→plan and rule-callable as-is:**
  `compact_projected_join_inputs(left, right, join_condition, post_join_predicate,
  project_exprs) -> Option<(LogicalPlan, LogicalPlan)>` (`src/sql/executor/join.rs:1035`).
  From the optimizer rule, package the aggregate's requirements (group-by keys + aggregate
  args + HAVING columns) as the `project_exprs` argument — no signature change needed.
- **The required-column collector already exists and walks the right shapes:**
  `collect_expr_columns_by_table` handles `Between`/`InList`/`Cast`/`IsNull`/
  `ArraySubscript` (`scan.rs:1265-1290` family) and carries a `bail` flag for unresolvable
  shapes (wildcards/unknown columns) — reuse it and respect `bail` → old path, rather than
  writing a new walker.

---

## LATER (ranked) — with upgraded designs

### 5. Same-row conflicts — Option 2 **+ engine-internal statement retry (PG-equivalent RC)** (M / medium) — now PROMOTABLE
**v1 blocker:** dropping pessimistic locks makes ReadCommitted fail-fast with retriable
errors — a client-visible PG divergence needing a product decision.
**Refined design that dissolves the blocker:** PostgreSQL's own RC conflict behavior is
block-then-**re-evaluate** (EvalPlanQual): the loser waits, then re-checks its predicate
against the winner's committed row and proceeds. Replicate that at the engine level: on a
first-committer-wins conflict at RC, **retry the losing statement internally** against a
fresh snapshot (bounded, e.g. 3 attempts, then surface a serialization error). A plain
same-row UPDATE/DELETE then behaves exactly like PG — no client-visible error, no ORM
retry loops needed — while the 1 s worker-pinning futile spin and the DFS-deadlock/
timeout machinery go away. Keep the bounded pessimistic path only for `SELECT FOR
UPDATE`; RR/Serializable keep fail-fast (PG also errors there).
**Why better:** turns a product-semantics decision into an engineering task with
PG-matching behavior for the common case. Promote into the milestone if contended
multi-writer pain is current — the underlying stall is production-severity.

**Code-verified key points (v2.1):**
- **Type the conflict error FIRST.** The retry loop must catch exactly the
  first-committer-wins failure surfaced by `commit_with_timestamp` when
  `conflict_registry.validate_and_record` reports a conflicting key
  (`src/storage/transaction.rs:~653-677`, `src/storage/conflict.rs::validate_and_record`).
  Introduce a dedicated variant (e.g. `Error::WriteConflict { key }`) before building the
  retry so it can never trigger on unrelated failures — string-matching is how this goes
  wrong.
- **Retry placement:** the autocommit single-statement wrapper
  (`execute_with_implicit_transaction`, `src/lib.rs`) is the one choke point where the
  whole failed statement can be re-executed against a fresh snapshot with clean scope.
  Session/explicit transactions must still surface the error to the client (PG does too —
  EvalPlanQual re-evaluation is statement-scoped, not transaction-scoped).
- **Scope of lock removal:** only the Write acquire in `Transaction::put`
  (`transaction.rs:450`) is dropped; the Read-lock site (`transaction.rs:333`) feeding
  `SELECT FOR UPDATE`-style flows keeps the bounded pessimistic path.

### 6. Filtered vector kNN — **selectivity-adaptive 3-way strategy** (M / medium)
**v1:** switch the over-fetch escalation loop to traversal-time `search_filter`.
**Refined:** build the filter as a roaring bitmap over hnsw ids (via `reverse_mapping`),
read selectivity off the bitmap cardinality (free once built), then pick:
**high selectivity** (matching set ≲ 5–10k) → exact distance scan over just the matching
ids (often beats any graph traversal; the brute-force rescue machinery already exists);
**medium** → traversal-time filtered search (`hnsw_rs::search_filter`, shipped-but-unused,
or the in-house tested `PersistentVectorIndex::search_filtered`); **low** → plain kNN +
post-filter (today's fast case). This pgvector/Qdrant-style adaptive planner strictly
dominates any single strategy across the selectivity range. Keep brute force as the
correctness net.

**Code-verified key points (v2.1):**
- **The adaptive planner slots inside one existing method:** `search_with_filters(&self,
  index_name, query, k, filter: Option<&HashMap<String, serde_json::Value>>, namespace)`
  (`src/storage/vector_index.rs:701`) — it already short-circuits to plain `search` when
  filter+namespace are None, so "low selectivity → post-filter" and the entry-point wiring
  exist; only the high/medium arms are new.
- **hnsw_rs adapter shape confirmed:** `search_filter(&self, data, knbn, ef_arg,
  filter: Option<&dyn FilterT>)` (`hnsw_rs-0.3.3/src/hnsw.rs:1475`). `FilterT` is keyed on
  hnsw `DataId` — implement it over a roaring bitmap of hnsw ids built once per query by
  evaluating the metadata predicate and mapping row ids through `reverse_mapping`
  (row_id → hnsw_id, the same map the recall-rescue path uses).

### 7. ART point probe — **resolved-index handles pinned in cached plans** (S-M / low)
**v1:** DashMap registry + FastSelectSpec caching + tree-Arc-swap audit.
**Refined:** attach the resolved `SharedArtIndex` Arc (plus a DDL epoch stamp) to the
**cached normalized plan** — the A2 plan cache gives these plans long lives, and
`invalidate_plan_cache()` (verified `lib.rs:7860`) already clears them wholesale on DDL,
so invalidation is free. A probe becomes: epoch check (one atomic) → direct per-tree
access. The two process-global registry RwLock acquisitions + linear scan + String clone
vanish from the hot path without touching tree internals or an Arc-swap audit. Swap the
per-tree `std::sync::RwLock` (verified `art_manager.rs:21-27`) for `parking_lot` as a
free rider. v1's lock-free tree redesign is deferred until this cheap version is measured.

**Code-verified key points (v2.1):**
- **Do NOT embed the handle in plan nodes.** `LogicalPlan`/`LogicalExpr` derive
  `Serialize, Deserialize, PartialEq, Clone` (`src/sql/logical_plan.rs:38+`) — adding an
  `Arc<SharedArtIndex>` field would break serde and plan equality. Use a **side cache**
  instead: `ShardedLruCache<String /* normalized SQL */, ResolvedProbe { index:
  Weak<RwLock<AdaptiveRadixTree>>, epoch: u64 }>` cleared inside `invalidate_plan_cache()`
  right alongside the other spec caches (the pattern `hot_fast_select_spec` /
  `fast_param_insert_cache` already establishes this).
- **Hold `Weak`, not `Arc`:** a dropped/rebuilt index (TRUNCATE, REINDEX, branch swap)
  must not be kept alive or resurrected by the probe cache; `Weak::upgrade` failure → fall
  back to the registry lookup and re-pin.

### 8. mimalloc — **binary-only, feature-gated, RSS-measured** (S / low)
**v1:** 5-line global allocator swap + A/B.
**Refined:** gate as `--features mimalloc`, default-on for the **server binary only** —
not the lib/cdylib/Python wheel (embedders and PyO3 own their allocator policy). A/B with
the paired order-swapped harness on the indexed-read + COPY cells, and track **RSS**
alongside TPS (mimalloc trades memory for speed; edge deployments care). Try snmalloc as
a second candidate in the same harness before committing.

---

## Cross-cutting addition (new in v2)
Add the batch's target metrics to the perf gate so the wins can't silently erode: an
IN-list normalization cell, the COPY 100k cell, and (post-#3) a columnar aggregate cell
in `ci_perf_smoke.sh` / `bench-engines.sh` baselines. S effort, protects everything above.

## Sequencing
#1 first (days, near-zero risk, harness exists) → #2 fallback increment → #2 full →
#3 increment 0 (half a day, can run any time) → #3 v1 → #4 in parallel with #2/#3
(different files) → #5 when promoted. #7 naturally follows #1 (it piggybacks on the
normalized plan cache). #8 slots into any idle machine window.

## Dropped (unchanged from v1)
Row-cache enlargement (disk fast-select path skips cache fill — confirmed by two
independent findings); new vectorized operator work (the R3.x engine already exists).
