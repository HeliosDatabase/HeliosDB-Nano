# Executor Handbook — HeliosDB-Nano Next Perf Batch (for Opus 4.8)

## Context

HeliosDB-Nano just shipped **v4.0.0** (crates.io + GitHub, tag `v4.0.0`, `main`). The
2026-07 perf+stability campaign (M1–M5 + M2b) reversed the one workload PostgreSQL used to
win (indexed reads now 1.7–2.3× PG), made COPY atomic + 5.4× faster, and 32×'d
`nextval`-bound inserts. The next batch is specified in
`docs/plans/NEXT_PERF_BATCH_2026_07.md` (v2 refined designs). This handbook arms an executor
with the load-bearing anchors, invariants, traps, and gate recipes learned during the
campaign — everything not obvious from reading the roadmap alone. Every file:line below was
re-verified at HEAD (`main`, post-v4.0.0); line numbers drift, so each anchor is given as
**symbol + grep pattern + current line hint** — grep the symbol, don't trust the number.

Goal: each of the 8 roadmap items becomes its own gated milestone (branch → implement →
regression gate → scalability gate → PR → merge), exactly as M1–M2b were run.

---

## Operating context (read once)

- **Repo:** `/home/gpc/HDB/Nano`, branch `main`. One milestone = one branch
  (`perf/<name>` or `fix/<name>`) → PR → merge. Never commit straight to `main` except docs.
- **Frozen A/B baseline binary:** `perf/baseline_runs/bins/heliosdb-nano-baseline-main-68e814a`
  (v3.60.9, pre-campaign). This is the fixed reference for "vs baseline" scalability numbers.
  It is **gitignored** and must not be committed. ⚠️ **It HANGS on `ALTER TABLE … RENAME`**
  (the pre-M1 bug) — any bench harness that renames a fixture will wedge it; use the
  RENAME-free fixture path (`CREATE TABLE t50` + COPY + `CREATE INDEX`, no RENAME) when
  comparing against baseline.
- **Build times:** `cargo build --release --bin heliosdb-nano` ≈ 3–4 min cold. `cargo test
  --lib` ≈ 60 s. Do NOT cold-build the `perf` profile (`--profile perf`) unless already warm.
- **Host is shared and NOISY:** PostgreSQL's own numbers drift 5–40% run-to-run; a single
  bench run can show an apparent regression that reverses on an order-swapped retest. Port
  25433 is a shared `codex-pg184-bench` PG container — wait for it, never kill it. There are
  ~8 unrelated `heliosdb-nano` processes on the box — only kill servers you start.
- **Lib test count at HEAD:** 1997 pass / 0 fail / 3 ignored (the 3 ignored are pre-existing:
  an information_schema catalog test + two wire probes). Two KNOWN pre-existing failures that
  are NOT regressions and must be reconfirmed on baseline before dismissing:
  `fk_validation_modes::bulk_load_mode_setting_reaches_storage_engine` and the protocol
  suite's `helios_sessions` step 7 (dead-code schema, C17 deferred).

---

## Non-negotiable invariants (the hard-won rules)

1. **Differential-oracle discipline for any query-rewrite change.** If you change how a query
   is parsed/normalized/planned, you MUST prove `execute(raw) == execute(rewritten)`
   row-for-row over a corpus. The pattern exists: `src/sql/normalize.rs` mod `differential`
   (`raw_equals_normalized_over_corpus`) compares `db.query(raw)` vs
   `db.query_params(normalized, params)`. Extend that corpus; never hand-verify.
2. **Keep `query()` un-normalized.** It is the oracle's independent raw reference. Only
   `query_with_columns` (the wire path) carries normalization. Wiring `query()` would make the
   oracle vacuous (both sides would normalize).
3. **Wire-path rule.** Embedded lib tests miss `src/protocol/postgres/` (catalog, extended
   protocol, session settings). Any change touching protocol/planner/caches MUST also run the
   psycopg suite (`tests/protocol_tests/`) — the campaign repeatedly found wire-only bugs
   (e.g. `SET statement_timeout` silently dropped by the generic-SET branch).
4. **Bail to the raw path, never guess.** The normalizer and COPY fast path both return
   `None` on anything unproven-safe and fall back to the correct (slower) path. New fast paths
   follow this: gate hard, fall back on any doubt.
5. **Cache admission is 2nd-run.** `cache_admits()` (lib.rs) only admits a query to the
   plan/result cache on its *second* sighting (a 1024-slot fingerprint filter). Tests that
   assert "query is cached after one run" are wrong post-campaign — run it twice. A
   normalized query caches under the *normalized* key, not the raw key.
6. **Fast-batch atomicity.** `insert_prepared_tuples_fast_batch` applies one `WriteBatch`
   with one `commit_ts` for the whole batch. Preserve all-or-nothing: validate before any
   write; a constraint failure rejects the whole batch.
7. **Time-travel is ON by default and every published benchmark runs it.** Version writes
   (`v:`/`v_idx:`) are the default. Do not "optimize" by assuming time-travel off.
8. **`parking_lot`, not `std::sync`, for serving-path mutexes.** std Mutex poisons on a
   panic and wedges every later statement (M1 C4 fixed `current_transaction`).
9. **Sequence durability is unconditional.** The high-water fsync fires even at
   `durable_commit=false` (no-duplicate-on-crash invariant). Don't gate it on durable_commit.
10. **Paired, order-swapped bench measurement.** Never call a cell eroded from one run. Run
    baseline vs candidate back-to-back, then swap order and repeat. Report all rounds. The
    effect must survive the swap.

---

## Gate & bench cookbook (exact commands)

**Per-milestone regression gate (must be clean):**
```
cargo test --locked --lib -- --test-threads=2         # expect 1997+/0 (+ your new tests)
cargo test --locked --doc -- --test-threads=2         # expect ~45/0
cargo test --test <touched-integration-suites>        # e.g. crud_tests, cte_hardening_tests, ...
benches/public/ci_perf_smoke.sh                        # 12/12, no workload >2.5× slower than ci_baseline.json
```
**Protocol (psycopg) suite — for any protocol/planner/cache change:**
```
./target/release/heliosdb-nano start --auth trust --http-port 0 --port 20000 --data-dir <fresh> &
# poll port 20000, then:
tests/protocol_tests/venv/bin/python tests/protocol_tests/test_postgres.py   # 6/7 (helios_sessions pre-existing)
tests/protocol_tests/venv/bin/python tests/protocol_tests/test_copy.py       # 7/7 must pass
# kill the server you started; remove the scratch data-dir
```
**pg35 benchmark-of-record (correctness + erosion across 35 SQL categories vs PG 18.4):**
```
PG35_ITERS=100 cargo test --release --test pg35_benchmark -- --nocapture --ignored
# needs the PG container on 25433; scoreboard must stay 35–0–0; compare vs
# perf/v358_program/pg35_category_history.json. NOTE: pg35 is timing-only (discards rows) —
# it is an erosion signal, NOT the correctness proof. Correctness = the differential oracle.
```
**Scalability A/B (paired, order-swapped):**
```
cd docs/benchmarks && ./bench-engines.sh baseline:<baseline-bin> <name>:<candidate-bin>
# sweeps SELECT 1, indexed-read (SELECT abalance FROM t50 WHERE aid=:rand, 50k rows),
# COPY 10k/50k/100k, DROP 100k, c∈{1,8,16,32,64}. If baseline hangs on the RENAME fixture
# step, kill -9 that server and use the RENAME-free retest harness (see prior gate logs
# in scratchpad: m5_paired_retest.sh). Env: DUR, CLIENTS, NANO_PORT0.
```
**Release (only when a version ships):** bump `version` in `Cargo.toml` AND
`bindings/python/Cargo.toml`; add a `## [X.Y.Z] - DATE` CHANGELOG entry (release.yml extracts
it for GitHub notes); `cargo check --workspace` to update `Cargo.lock`; run `--lib`, `--doc`,
`cargo publish --dry-run --locked` locally; ensure the tree is clean (gitignore scratch);
commit; `git tag -a vX.Y.Z`; `git push origin main && git push origin vX.Y.Z`. The tag push
triggers `release.yml` (verify-tag → tests → dry-run → publish → GH release). **Release gate
flakes** on dep-download or a vector-index test → `gh run rerun --failed <id>`, never re-tag.

---

## Per-item execution specs

### Item 1 — Normalizer widening (IN/BETWEEN/casts + arity padding) · S/low · DO FIRST
- **Files:** `src/sql/normalize.rs` (the whole change), `tests/normalization_differential.rs`
  + the in-file `differential` mod, `tests/query_normalization.rs`.
- **Anchors (verified):** `is_predicate_bail_kw` (normalize.rs:333) currently bails on
  `["in","between","exists","any","all","some","values","array","case","select"]` — remove
  `"in"` and `"between"`, KEEP `"select"` (subquery guard) and the rest. The `::` cast bail is
  normalize.rs:167 (`b == b':' && bytes[i+1]==b':' && in_where`) — replace the blanket bail
  with a type-whitelist passthrough. `normalize_select_literals` entry is normalize.rs:33.
- **Mechanism:** the lexer already parameterizes any WHERE-region literal at any paren depth;
  only the keyword/`::` bails block IN/BETWEEN/casts. The executor already resolves
  `LogicalExpr::Parameter` for IN-lists and range bounds (verify
  `lookup_bound_value`/`indexed_equality_lookup` in `src/sql/executor/scan.rs`, and the
  Cast-over-Parameter unwrap already used by the M1 UUID probe work).
- **The refinement that matters:** **power-of-two IN-arity padding** (repeat the last param to
  pad `IN($1,$2,$3)`→`IN($1,$2,$3,$3)`; result-neutral by set semantics incl. NOT IN / NULL
  elements). Collapses cache shapes from one-per-arity to log₂(arity). Bail above 128
  elements. **Cast whitelist only** (uuid, int2/4/8, text/varchar, date, timestamp(tz),
  numeric, float4/8, boolean) — the verified index-probe-safe types.
- **Invariant/trap:** must NOT normalize `POSITION('x' IN s)` (the `IN` there is not a
  predicate) — the `select`-style keyword guard won't catch it; add a test and handle it.
  `IN (SELECT …)` must still bail (the retained `"select"` keyword does this). Extend the
  oracle FIRST (property-based + the specific shapes) and let it fail closed before wiring.
- **Gate:** differential oracle (expanded), `tests/query_normalization.rs`, psycopg parity,
  pg35 35–0–0, perf smoke flat. **Expected:** ~30–100% on IN-list/range/cast point reads.

### Item 2 — COPY → PostgreSQL parity (transient version marker) · L/medium
- **Files:** `src/storage/engine.rs` (`insert_prepared_tuples_fast_batch`, engine.rs:10330),
  time-travel read path `src/storage/time_travel.rs`, `src/lib.rs` `copy_bulk_insert`
  (lib.rs:6378).
- **Anchors (verified):** in `insert_prepared_tuples_fast_batch`, `commit_ts` is taken ONCE
  for the batch (`if time_travel_enabled { next_commit_timestamp }`, ~engine.rs:10374);
  `reverse_ts = u64::MAX - ts`; the per-row triple-put is `data:` + `v:{table}:` (full
  `logical_value`) + `v_idx:{table}:` (8-byte `ts.to_be_bytes()`) inside
  `if let (Some(ts), Some(reverse_ts)) = …` (~engine.rs:10405–10444). The `v_idx` key format
  is `v_idx:{table}:{row_id}:{reverse_ts:020} → actual_ts` (verified time_travel.rs:653-664).
- **THE EVIDENCE (measured this session, v4.0.0 wire COPY, exact bench workload):**
  time_travel ON → 368/337/425 ms (10k/50k/100k); OFF → 156/131/102 ms; PG ref 115–133 ms.
  So the `v:`/`v_idx:` writes are ~230–270 ms of the 423 ms — **effectively the entire
  remaining gap.** Everything else already fits inside PG-parity.
- **Design (refined, transient):** COPY writes `data:` + ONE durable
  `vmeta:{table}:{first}:{last}:{ts}` per batch (same WriteBatch); a background materializer
  converts markers to standard per-row `v:`/`v_idx:` and deletes them (converges to today's
  on-disk format — no permanent new MVCC concept). While a marker is live: an in-memory
  per-table interval set (loaded from `vmeta:` at open) is consulted by AS-OF +
  UPDATE/DELETE, guarded by ONE process-wide atomic "any live markers" fast-out (permanent
  hot-path tax = one atomic load once drained). UPDATE/DELETE of a covered row materializes
  that row first (update path already reads the old value).
- **Fallback increment (do this first, ~60–75% of win, no marker machinery):** elide only the
  `v:` full-value duplicate; AS-OF derives the insert version from `data:` when the (already-
  performed) `v_idx:` seek shows no newer version; UPDATE/DELETE backfills the old `v:` using
  the ts recovered from the existing `v_idx:` entry.
- **Invariant/trap:** AS-OF `T < ts` must exclude the row; `T ≥ ts` no-newer-version must
  serve `data:`. Branches + GC + backup/restore must handle markers (or wait for drain).
  **Measure first:** one run with only the `v:` put removed tells you the WAL-bytes vs
  memtable-entry split and whether the fallback alone reaches parity.
- **Gate:** ALL time-travel + branch + `crash_recovery_e2e` + `wal_crash_recovery` suites;
  psycopg `test_copy.py`; paired COPY bench (target ≤170 ms @100k = PG parity). Kill switch
  `HELIOS_COPY_ELIDE_V=1`.

### Item 3 — OLAP: activate the shipped columnar engine (watermark + delta overlay) · L/medium
- **The surprise (verified):** `perf/P1_5_columnar_scan.md:3` says "NOT IMPLEMENTED" but the
  vectorized engine ALREADY SHIPPED (commits 884ae83/c3432b2/b866669/d602b17/ae00fd0/da60a5d:
  typed batches v2, zone-map pruning, presence bitmaps, batch-direct aggregation, rayon
  partial aggregation). It only activates for opt-in `STORAGE COLUMNAR` DDL, which zero real
  tables set. **Do NOT build a new operator.**
- **Anchors (verified):** the two planner gates that require `storage_mode == Columnar` —
  `indices_are_columnar` (scan.rs:135, called at scan.rs:101/119/132) and the columnar
  aggregate gate in `src/sql/executor/mod.rs` (`try_columnar_aggregate`, the all-columns bail
  ~mod.rs:2440). The grouped batch writer + point-get: `store_columnar_rows_grouped` and
  `ColumnarStore::get` (full-batch decode, `src/storage/columnar.rs`, BATCH_SIZE=1024).
  Columnar write hook is already inside `insert_prepared_tuples_fast_batch` (~engine.rs:10471,
  `stats_write_lock` + `group_columnar_row_values`). DML parity battery:
  `tests/columnar_adoptable_tests.rs`.
- **⚠️ INCREMENT 0 IS MANDATORY (half a day, no new code):** load the P1_5 1M-row suite into a
  `STORAGE COLUMNAR` twin table, measure vs the row twin and vs SQLite. The "1–3× of SQLite"
  claim is UNMEASURED (design-phase). **If the shipped kernels don't deliver ≥5×, STOP** —
  the whole lever's premise fails cheaply.
- **Design (refined, if increment 0 passes):** side data is a DERIVED CACHE keyed by a
  per-table watermark (max row-id/LSN covered). Scans serve ≤watermark from columnar batches,
  the tail via the row path, merged — a trickle of INSERTs just lags the watermark (NO
  whole-table staleness cliff, unlike v1). UPDATE/DELETE = O(1) presence-bit clear + append to
  an in-memory delta list overlaid on scans; a background compactor folds deltas + advances
  the watermark. Population always background (never inline — resolves the tension with #2).
  Delta list can be memory-only (side data is derivable; rebuild on crash).
- **Invariant/trap:** naive default-flip is WRONG — columnar storage replaces row values with
  `ColumnarRef` sentinels and a point-read then decodes a full 1024-row batch (uncached),
  which would regress the just-won OLTP point-read lead. Keep row blobs INLINE (side copies).
- **Gate:** the columnar DML-parity battery is for TRUE columnar, not side-copy coherence —
  add side-copy coherence tests (INSERT lag, UPDATE/DELETE overlay, crash rebuild). pg35.

### Item 4 — Aggregate-over-Join column pruning (optimizer rule) · M/low
- **Files:** `src/sql/optimizer/rules.rs` (`ProjectionPruningRule`), `src/sql/executor/join.rs`
  (`compact_projected_join_inputs` ~join.rs:1035, collector ~join.rs:827), `src/sql/executor/mod.rs`
  (the Aggregate arm `plan_to_operator(input)` ~mod.rs:3521, currently unpruned).
- **Design (refined):** implement as a plan-time projection-pushdown-through-join RULE (extend
  `ProjectionPruningRule`) so it composes with the A2 normalized plan cache (pruned once,
  reused across literals) instead of an executor special-case. Union the columns required by
  any ancestor (Aggregate group-by + agg args + HAVING, Sort, Project, Filter). Prioritize
  hash-join build-side pruning (cuts hash-table memory + build time).
- **Invariant/trap:** cost-guarded rule acceptance (`new_cost <= old_cost`) + wildcard bail
  (any unresolved `*` → old path). Watch correlated-subquery interaction — keep the
  executor-arm version (same `compact_projected_join_inputs` core) as fallback.
- **Gate:** pg35 join categories + join/window/subquery hardening suites. TEXT-heavy
  aggregate-over-join microbench (the 2–4× assumes wide TEXT rows — validate). **This hedges
  #3:** if columnar increment 0 disappoints, join-shaped analytics still improve.

### Item 5 — Same-row conflicts: Option 2 + engine-internal retry (PG RC-equivalent) · M/medium · PROMOTABLE
- **Files:** `src/storage/transaction.rs` (`acquire_lock` for Write, transaction.rs:450),
  `src/storage/lock_manager.rs` (`try_acquire_lock` lm.rs:279, `detect_deadlock` lm.rs:357,
  `NANO_LOCK_TIMEOUT_MS` lm.rs:187), the optimistic registry `src/storage/conflict.rs`,
  `docs/NANO_CONCURRENCY_LOCKING.md` (the design doc, Option 2 at :55-81).
- **The blocker-dissolving refinement:** v1 stalled because dropping pessimistic locks makes
  ReadCommitted fail-fast (client-visible PG divergence). But PG's RC is block-then-RE-EVALUATE
  (EvalPlanQual). Replicate at the engine level: on first-committer-wins conflict at RC,
  **retry the losing statement internally** against a fresh snapshot (bounded, ~3 attempts,
  then serialization error). Plain UPDATE/DELETE then behaves exactly like PG — no ORM retry
  loop needed — and the 1 s worker-pinning spin + DFS-deadlock machinery go away. Keep bounded
  pessimistic path only for `SELECT FOR UPDATE`. RR/Serializable keep fail-fast (PG errors too).
- **Invariant/trap:** the doc's prescribed tests — no whole-server stall, one-winner/one-loser.
  Statement retry must re-bind params + re-take snapshot cleanly and be side-effect-safe
  (autocommit single-statement scope). **Gate:** pg35 + tps + protocol; the doc's stall tests.

### Item 6 — Filtered vector kNN (selectivity-adaptive 3-way) · M/medium
- **Files/anchors (verified):** `src/storage/vector_index.rs` `search_with_filters`
  (vi.rs:701, currently O(N) scan), `src/vector/persistent.rs` `search_filtered` (persistent.rs:955,
  tested but unreachable), the escalation/brute-force loop in `src/mcp/` or graph_rag (mod.rs:1127-1207
  per v1), `hnsw_rs` 0.3.3 ships `search_filter` (hnsw.rs:1475, UNUSED).
- **Design:** build the filter as a roaring bitmap over hnsw ids (via `reverse_mapping`);
  read selectivity off cardinality; pick — high (≲5–10k matches) → exact distance scan over
  matches; medium → traversal-time `search_filter`/`search_filtered`; low → plain kNN +
  post-filter. Keep brute force as the correctness net. **Later** (off the PG/SQLite headline
  tables but strategically important for agentic-DB positioning). Expect 10–100× on selective
  filtered kNN. **Gate:** vector correctness/recall suites; the brute-force fallback is the net.

### Item 7 — ART point probe: pin resolved handles in cached plans · S-M/low
- **Files/anchors (verified):** `src/storage/art_manager.rs` (per-probe: 2 process-global
  RwLock acquisitions + linear scan + String clone in `find_column_index`/`index_get_all`
  ~art_manager.rs:868-937; per-tree `std::sync::RwLock` art_manager.rs:21-27), the A2 plan
  cache + `invalidate_plan_cache` (lib.rs:7860, already clears plans wholesale on DDL).
- **Design (refined):** attach the resolved `SharedArtIndex` Arc + a DDL epoch stamp to the
  cached normalized plan. Probe = epoch check (one atomic) → direct per-tree access; the
  global registry locks vanish. Invalidation is FREE (`invalidate_plan_cache` already fires on
  DDL). Swap per-tree `std::sync::RwLock` → `parking_lot::RwLock` as a free rider. Defer the
  lock-free tree redesign until this cheap version is measured. **Naturally follows #1** (it
  piggybacks on the normalized plan cache). +25–60% is extrapolated, not measured.

### Item 8 — mimalloc (binary-only, RSS-tracked) · S/low
- **Design:** gate `--features mimalloc`, default-on for the SERVER BINARY only (not
  lib/cdylib/PyO3 wheel — embedders own allocator policy). A/B on indexed-read + COPY cells
  with the paired harness; track RSS alongside TPS (mimalloc trades memory for speed). Try
  snmalloc as a 2nd candidate in the same harness. Motivated by pstack showing glibc arena
  contention. Slot into any idle machine window.

---

## Cross-cutting (new in v2) — protect the wins
Add the batch's target metrics to the perf-gate baselines so they can't silently erode: an
IN-list normalization cell, the COPY 100k cell, and (post-#3) a columnar aggregate cell in
`benches/public/ci_perf_smoke.sh` / `docs/benchmarks/bench-engines.sh` + `ci_baseline.json`.

## Sequencing
#1 first (days, near-zero risk, harness exists). #2 before #3 (both touch
`insert_prepared_tuples_fast_batch`; the columnar populate hook must rebase on the vrange
change). #3 increment 0 can run any time (half a day). #4 fully parallel with #2/#3 (executor
only). #7 follows #1. #5 promote if multi-writer pain is current. #8 any idle window.

## Symbol/file index (grep these, don't trust line numbers)
| Concern | Symbol | File |
|---|---|---|
| Literal normalizer | `normalize_select_literals`, `is_predicate_bail_kw` | src/sql/normalize.rs |
| Differential oracle | mod `differential`, `raw_equals_normalized_over_corpus` | src/sql/normalize.rs |
| Normalization wire hook | `try_normalized_query_with_columns`, `query_normalization_enabled` | src/lib.rs |
| COPY fast batch | `insert_prepared_tuples_fast_batch` | src/storage/engine.rs |
| COPY entry + gates | `copy_bulk_insert`, `prepare_fast_insert_batch`, `validate_fast_insert_batch` | src/lib.rs |
| Version index format | `v_idx:{table}:{row_id}:{reverse_ts}` | src/storage/time_travel.rs |
| Columnar gates | `indices_are_columnar`, `try_columnar_aggregate` | src/sql/executor/scan.rs, mod.rs |
| Columnar store | `store_columnar_rows_grouped`, `ColumnarStore::get` | src/storage/engine.rs, columnar.rs |
| Join pruning | `compact_projected_join_inputs`, `ProjectionPruningRule` | src/sql/executor/join.rs, optimizer/rules.rs |
| Lock manager | `acquire_lock`, `try_acquire_lock`, `detect_deadlock` | src/storage/transaction.rs, lock_manager.rs |
| Optimistic conflict | conflict registry | src/storage/conflict.rs |
| Filtered kNN | `search_with_filters`, `search_filtered`, `search_filter` (hnsw_rs) | vector_index.rs, persistent.rs |
| ART probe | `find_column_index`, `SharedArtIndex`, `invalidate_plan_cache` | art_manager.rs, lib.rs |
| Cache admission | `cache_admits`, `cold_optimizer` | src/lib.rs |
| Plan/roadmap | v2 designs | docs/plans/NEXT_PERF_BATCH_2026_07.md |

## Verification (how to prove each milestone end-to-end)
1. Add the change behind a kill switch where one exists (normalization, COPY-elide).
2. Regression gate (lib + doc + touched integration + perf smoke) — clean vs the counts above.
3. For query-rewrite changes: extend + run the differential oracle (0 mismatches) BEFORE wiring.
4. Protocol psycopg suite (test_postgres.py 6/7, test_copy.py 7/7) for any wire-affecting change.
5. pg35 35–0–0 (erosion + broad correctness signal).
6. Scalability A/B: build a candidate binary, run `bench-engines.sh baseline:… cand:…`
   PAIRED + order-swapped ≥2 rounds; the target win must survive the swap; SELECT 1 / other
   cells flat. Kill-switch A/B to isolate the new lever's contribution.
7. PR with both gate reports in the body; merge to `main`.
