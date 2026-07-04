# Group A — Indexed-read hot path

**Target:** T1 — indexed point-read (simple protocol, literal predicates): c=1 5.3k → ≥8k
(past PG's 6.8k), c=32 48k → ≥85k (past PG's 71.8k @ c=32); SELECT 1 and pg35 must not erode.
**Branch:** `perf/read-path-normalization` · **Risk:** medium (new normalization path with
kill switch + conservative bail-outs) · **Effort:** M-L

## Diagnosis (analysis agent MEASURED at HEAD @ 68e814a; psycopg2 + pstack, release build)

The pgbench shape (`SELECT abalance FROM t50 WHERE aid = <literal>`, no PK on table,
secondary btree, fresh literal per query) misses everything by construction:

- `try_fast_select` needs `SELECT *` (`lib.rs:9162`) AND single-column PK (`lib.rs:9101,9109`) → full pipeline every statement.
- parse/plan/result caches keyed on raw SQL text (`lib.rs:425-429`) → 0% hits, but the
  engine still **inserts into all three every statement**: AST deep clone (`lib.rs:10276-10285`),
  plan deep clone (`lib.rs:13326`), result clone + **global `hot_result_cache_entry.write()`**
  (`lib.rs:1398-1404`), plus eviction drops under shard locks — pure churn.
- Row cache capped at 10k entries (`row_cache.rs:145`) vs 50k-row working set → ~80% miss;
  each fill takes the global RwLock **exclusive** + a second `stats.write()` lock
  (`row_cache.rs:307,314`).
- Optimizer clones per node per rule per pass (`optimizer/mod.rs:162,337-338`); executor
  probe does 2 RocksDB existence gets per stmt (`scan.rs:290-291`), rebuilds schema per
  stmt (`scan.rs:317`); ART registry lookup = linear scan under global RwLock ×2
  (`art_manager.rs:868-891`).

**Measured:** slow pipeline ≈ 75µs/stmt at c=1 (vs 18µs fast path); plateaus ~57k @ c=32
while the existing PK fast path hits 169k and protocol floor 213k (psycopg2 numbers —
pgbench ratios match). pstack at c=32: top waits = glibc **malloc arena locks** fed by the
clone/evict churn; ART/RocksDB are NOT the cap. The v3.33 "4-Mutex convoy" is REFUTED at
HEAD (fixed by R2.1 sharded LRUs + `global_txn_active` fast-out).

## Execution split (decided during implementation)

Group A ships in **two milestones** matching the plan's own "ship first" / "headline"
structure, so the low-risk churn-stop isn't gated against the beat-PG target it can't
reach alone, and the high-risk normalization gets its own focused gate:

- **M2a (`perf/read-path-normalization`, A1 + A5 + parse guard)** — DONE, gating.
  Cache-admission filter, shared cold optimizer, Arc-once plan insert, parse-cache size
  guard. Gate criterion: **no regression anywhere (incl. pg35, SELECT 1, COPY, DROP) +
  measurable c≥16 indexed-read improvement** (A1's actual goal is removing malloc-arena
  churn at concurrency, not the c=1 straight-line cost — that needs A2).
- **M2b (next milestone, A2 + A3 + A4)** — the headline: token-level literal
  normalization → parameterized plan cache (design below), row-cache capacity/stats/keys,
  ART registry map. Gate criterion: the full group-A target (c=1 ≥+50% & >PG; c=32
  ≥+75% & >PG). A2 is the risky piece (touches every wire SELECT) — own branch, full
  bail-out matrix, pg35 + psycopg regression.

## Changes (agent's ranked list, implementation order)

### A1 — Stop guaranteed-miss cache churn + handler preamble trims (S, ship first)
- 2-touch admission (per-shard u64 fingerprint slot: cache only a raw-SQL key seen twice)
  for `cache_query_result` (`lib.rs:1398-1404`), `parse_cache.put` (`lib.rs:10284`),
  `plan_cache.put` (`lib.rs:13326`). Kills the global exclusive write + 3 deep clones +
  evictions per statement. (Subsumes Group B's B3 size guard.)
- Preamble: memchr `;` before statement split (`handler.rs:564→2284`); prefix-only
  uppercase in DO-block sniff (`handler.rs:2398`); allocation-free pre-scans in
  empty-projection rewrite (`handler.rs:2420-2425`) and catalog interception
  (`catalog.rs:36`); remove duplicated non-determinism check + result-cache probe
  (`handler.rs:1046` vs `lib.rs:13280-13283`).

### A2 — Token-level literal normalization → parameterized plan cache (XL, the headline)
Cheap byte-lexer (no AST) at `query_with_columns` (`lib.rs:13254`, after fast-select
attempt) + `query()`/`query_params` twins:
- v1 scope: statements starting with SELECT; normalize number/'string' literals **after
  top-level WHERE only** (output schema provably unaffected — sidesteps the
  `SELECT 1 AS x` display hazard that killed the AST attempt). Replace literals with `$n`,
  collect `Value` params. Bail out (raw path) on: comments, dollar-quotes, `E'…'`,
  semicolons, anything surprising.
- On normalized-key plan-cache hit: execute via existing parameterized executor
  (`Executor::with_parameters`; `lookup_bound_value` resolves `LogicalExpr::Parameter`,
  `scan.rs:800`). On miss: parse normalized text once, plan with Parameters, insert, execute.
- Invalidation: existing DDL hook `invalidate_plan_cache` (`lib.rs:7650-7667`) +
  sharded-LRU epoch. Do NOT insert into result cache on this path.
- Why this beats the reverted AST auto-param: that ran *after* full parse (paid what it
  saved); the lexer is ~100-300ns and a hit skips parse+plan+optimizer+both cache puts.
- Kill switch: `NANO_DISABLE_QUERY_NORMALIZATION=1` env + config flag.

### A3 — Row cache: config capacity (default ↑), atomic put-stats, interned table ids (S)
`row_cache.rs:145` (10k → config, default 100k or byte budget), `row_cache.rs:314-322`
stats → atomics (read side already atomic per P0#4), `RowCacheKey` String → interned id
(`row_cache.rs:36`). (Sharding itself + commit-invalidation batching stays in Group D3.)

### A4 — ART registry probe: `DashMap<(table,col) → index>` maintained on create/drop (S)
Replaces linear scan ×2 under global RwLock (`art_manager.rs:868-891`).

### A5 — Executor probe de-warting (S-M)
- `view_exists`/`table_exists` 2 RocksDB gets → schema-cache DashMap + in-memory view set
  (`scan.rs:290-291`).
- `schema_with_source` per-stmt rebuild → stamp `Arc<Schema>` at plan time
  (`scan.rs:317`, `logical_plan.rs:168`).
- Optimizer: `LazyLock` the 5 stateless boxed rules; in-place mutate + dirty flag instead
  of clone-per-node + deep `!=` (`optimizer/mod.rs:162,337-338`, `lib.rs:13313-13324`).

### A6 (optional, flag-gated) — mimalloc for the server binary
pstack shows glibc arena waits; A1/A2 delete most of the allocations — measure after, add
only if c≥32 still arena-bound.

## Expected (agent estimates, to validate at gate)
A1+A2: c=1 ~10-11k, c=32 100-140k. +A3/A4/A5: c=64 in the 120-170k band (floor 245k).

## Gate (campaign §Milestone gate) — group-specific criteria
- Indexed-read sweep vs baseline binary: c=1 ≥ +50% AND > PG-same-run; c=32 ≥ +75% AND
  > PG-same-run c=32; monotone non-degrading to c=64.
- SELECT 1 sweep, COPY, DROP within ±5% of baseline.
- pg35 benchmark: no category erosion (esp. Prepared stmts, point lookups).
- Full regression battery incl. psycopg protocol suite (normalization touches the simple-
  query path for every client!): correctness of `SELECT 'a''b'`, unicode literals, negative
  numbers, `WHERE x = -1`, floats, NULL, casts (`::uuid`), LIMIT/OFFSET literals (must NOT
  normalize those in v1 unless provably safe), multi-condition WHERE, parentheses.
- Normalization-specific: kill-switch A/B parity test (same results with/without).

## Rollback
Kill switch env/config disables A2 at runtime; A1/A3-A5 are independent small commits —
revert individually if the gate localizes a regression.
