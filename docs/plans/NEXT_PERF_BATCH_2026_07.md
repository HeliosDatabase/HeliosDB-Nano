# HeliosDB-Nano — Next Perf Batch Roadmap (post-v4.0.0)

_Produced 2026-07-05 by a 7-way parallel subsystem analysis (Fable 5) + adversarial synthesis. All file:line anchors re-verified at HEAD._

All seven findings' load-bearing anchors verified at HEAD. Final roadmap follows.

# HeliosDB-Nano post-v4.0.0 — Ranked Next-Batch Perf Roadmap

All file:line anchors below re-verified at HEAD this session (not just trusted from the findings).

## NEXT MILESTONE (do now, in this order)

### 1. DO FIRST — IN/BETWEEN/cast widening of the literal normalizer (Read path)
- **Lever**: Remove `"in"`/`"between"` from `is_predicate_bail_kw` (src/sql/normalize.rs:333-338) and pass `::type` through as `$n::type` instead of bailing (normalize.rs:166-169). Cache key = normalized SQL, so IN-arity is naturally keyed.
- **Mechanism**: The lexer already parameterizes any WHERE-region literal regardless of paren depth; only the keyword bail blocks IN/BETWEEN and only the `::` bail blocks casts. Zero executor work needed — verified `pk_in_list_value` already resolves `LogicalExpr::Parameter` (src/sql/executor/mod.rs:2317-2324) and range bounds likewise. `IN (SELECT …)` still bails on the retained `"select"` keyword. ORM IN-list preloads (the most common unique-literal shape v1 misses) stop re-parsing/re-planning per statement.
- **Expected win**: Extends the v4.0.0 flagship read win (1.7-2.3x PG) to IN-list/range/cast point reads; ~30-100% on those shapes on the wire simple-query + embedded autocommit paths.
- **Effort/Risk**: S / low. Correctness is machine-checked by the existing differential oracle (normalize.rs:511+, tests/normalization_differential.rs) and the env kill switch is already wired.
- **First step**: Delete the two bail keywords + add cast passthrough; extend the oracle corpus with IN (incl. NULL-in-list, NOT IN), BETWEEN, `::uuid`/`::int` casts, and `POSITION('x' IN s)`; **add an arity cap (bail above ~32 elements)** so ORM 500-element IN lists don't pollute the plan cache — the finding omitted this.
- **Flag**: Impact honest per-shape but suite-level effect is smaller than the headline reads suggest (published benches are equality-dominated). Ship it anyway — it's days of work at near-zero risk.

### 2. COPY at PG parity — per-batch version-range marker replacing per-row MVCC writes
- **Lever**: In `insert_prepared_tuples_fast_batch`, replace the N× (`v:` full-value duplicate + `v_idx:` padded key) puts (verified triple-put loop, src/storage/engine.rs:10404-10448) with ONE `vrange:{table}:{first}:{last}:{ts}` record — valid because the whole batch shares one `commit_ts` (engine.rs:10374-10378) and contiguous row ids. AS-OF treats "covered by marker, no newer v_idx:" as "insert version = current `data:` value"; UPDATE/DELETE lazily backfill a real `v:` from the old row (both paths already read it for ART maintenance).
- **Expected win**: 423ms → ~140-170ms on the 100k-row COPY bench = **parity with PostgreSQL (115-133ms) at default settings**, closing the last headline write-path loss. The finding's live A/B (time-travel on: 368-425ms; off: 102-156ms) is the strongest evidence in the whole set — it isolates the version writes as effectively the *entire* remaining gap.
- **Effort/Risk**: L / medium. Risk is time-travel/branch-anchor correctness and scope creep: the lazy backfill puts a marker-check on every future UPDATE/DELETE of bulk-loaded tables (needs an in-memory per-table interval map loaded at open — the finding understates this permanent cost, though it's O(log ranges)).
- **First step**: Scope v1 to the COPY fast path only, behind `HELIOS_COPY_VRANGE` kill switch; gate on the full time-travel + branch + AS-OF suites per the merge-validation methodology. Fallback increment if backfill risk bites: drop only the `v:` full-value duplicate (derive insert version from `data:` when no newer version exists), keep `v_idx:` — roughly half the win, much smaller blast radius.

### 3. Analytics/OLAP — activate the already-shipped columnar engine via side copies (STAGED, gated)
- **Lever**: The P1#5 "real fix" **already exists and is unreachable** — verified: `perf/P1_5_columnar_scan.md` says "NOT IMPLEMENTED" but commits 884ae83/ae00fd0/da60a5d/d602b17 shipped typed batches v2, batch-direct aggregation, and rayon partial aggregation; the only blockers are two planner gates requiring `storage_mode == Columnar` (src/sql/executor/scan.rs:135-141; src/sql/executor/mod.rs:2440-2448), which zero real tables set. Build columnar SIDE COPIES for default tables (row blobs stay inline — point reads untouched) and relax the gates to "side data present + fresh".
- **Expected win**: The 6.2-26.8x SQLite deficits (1M-row suite) compress toward 1-3x — the single biggest remaining gap in the product.
- **Effort/Risk**: L / medium, **but only under the staged plan below**.
- **Adversarial take (this is the item that demands it)**: (a) The "1-3x of SQLite" number is **unmeasured** — P1_5's estimates were design-phase. (b) Side-copy *freshness under DML* is the hard 20%: tests/columnar_adoptable_tests.rs validates true-columnar DML parity, NOT side-copy coherence. (c) Naive default-flip is correctly ruled out — verified `ColumnarRef` sentinel (engine.rs:10387-10402) + uncached full-batch point-get (columnar.rs:932-946) would regress the just-won OLTP lead. (d) **Cross-item tension the finding missed**: inline populate during COPY adds typed-batch writes to the exact path item #2 is driving to PG parity — populate must be async/background, not inline.
- **First step — increment 0 (half a day, no new code)**: with the existing release binary, load the P1_5 1M-row suite into an explicit `STORAGE COLUMNAR` twin table and measure vs the row twin and vs SQLite. **If the shipped kernels don't deliver ≥5x, stop — the whole lever's premise fails cheaply.** If they do: v1 = bulk-load/background-backfill population + any-DML-marks-stale invalidation (stale → row path; background re-backfill), behind a flag. Load-then-analyze coverage matches both the benchmark shape and real OLAP usage; coherent dual-write is explicitly out of scope for v1.

### 4. Aggregate-over-Join column pruning
- **Lever**: The Aggregate arm builds its join input unpruned (verified: src/sql/executor/mod.rs:3521 `plan_to_operator(input)` raw) while Project-over-Join already prunes via `compact_projected_join_inputs` (join.rs:1035) — extend the same shipped machinery (collector at join.rs:827 already handles aggregate nodes) to GROUP BY/HAVING-over-join.
- **Expected win**: 2-4x on aggregate-over-join with TEXT-bearing schemas (pg35: 12 values/~6 Strings cloned per joined row to consume 2-3); attacks the join-shaped slice of the OLAP gap.
- **Effort/Risk**: M / low — reuses validated machinery, wildcard cases already bail to the old path.
- **Why in-milestone**: It hedges item #3 — if columnar increment 0 disappoints, join-shaped analytics still improve; and it's independent code (executor only, no storage). Flag: 2-4x assumes TEXT-heavy rows; expect less on narrow int tables.
- **First step**: In the Aggregate arm, collect required columns from group_by + aggr args + HAVING, call `compact_projected_join_inputs`, bail-to-old-path on any unresolved/wildcard column; gate with pg35 + join-suite parity.

**Sequencing constraints**: #1 is independent, land immediately. #2 before #3 (both touch `insert_prepared_tuples_fast_batch`, engine.rs:10374-10516; the columnar populate hook at 10471 must be rebased on the vrange change). #4 fully parallel with everything.

## LATER (ranked)

5. **LockManager Option 2 — drop pessimistic row locks for session-txn DML** (M/medium). Verified: repo's own doc recommends it with gdb-proven whole-server 1s stalls (docs/NANO_CONCURRENCY_LOCKING.md:26-81), redundant lock at transaction.rs:448-453, optimistic registry already wired. Order-of-magnitude win on contended multi-writer. **Held out of the milestone for one reason the finding under-flags**: at ReadCommitted, PostgreSQL blocks-then-proceeds and never returns serialization errors for a plain same-row UPDATE — Option 2 makes RC fail-fast with retriable errors, a client-visible PG-compat divergence (ORMs without retry loops). Needs a product decision first: ship for RR/Serializable only (keep bounded pessimistic wait at RC), or ship for all with documented semantics. Promote to the milestone immediately if contended multi-writer workloads are hurting users today — the stall is production-severity, not just a benchmark number.
6. **Filtered vector kNN via traversal-time predicates** (M/medium). Fully verified: escalation loop with brute-force O(N) bailouts (mod.rs:1127-1207), `hnsw_rs` 0.3.3/0.3.4 both ship unused `search_filter` (registry hnsw.rs:1475), in-house `PersistentVectorIndex::search_filtered` tested but unreachable (persistent.rs:955), `search_with_filters` is an O(N) scan (vector_index.rs:701). 10-100x on selective filtered kNN — the dominant RAG shape. Strategically important (agentic-DB positioning) but absent from the PG/SQLite headline tables, hence "later". Keep the brute-force fallback as the correctness net.
7. **Lock-free ART point probe** (M/medium). Verified: 2 process-global RwLock acquisitions + String clone + per-tree lock per probe (art_manager.rs:925-937). Extends an already-won lead rather than closing a gap; the +25-60% is extrapolated from analogues, not measured — do it when the point-read saturation number matters for marketing again. The FastSelectSpec-caching step is the right design; the tree-Arc-swap audit (recovery/TRUNCATE/branch) is the real work.
8. **mimalloc global allocator** (S/low). Cross-cutting percent-level win cited by two findings' evidence (glibc arena contention); cheap A/B, slot it into any milestone with spare capacity.

**Dropped/demoted from findings**: row-cache enlargement (both findings that examined it independently concluded the disk fast-select path skips cache fill — consistent, so correctly demoted to runner-up status); new vectorized operator work (would duplicate the verified-existing R3.x engine).

## Why #1 is first
The normalize widening is the only S-effort/low-risk item in the set, its safety harness (differential oracle + kill switch) already exists, its executor prerequisites are verified in-tree, and it compounds the campaign's flagship win on the workload shape (ORM IN-list reads) users hit most. It ships in days while #2/#3 — the two items that actually move headline gaps (COPY parity vs PG, OLAP vs SQLite) — run their longer validation gates.

## Evidence-quality notes (over/understatements found during verification)
- **Strongest**: COPY finding (live A/B isolating the entire remaining gap). **Weakest impact claim**: columnar "1-3x of SQLite" (unmeasured — hence mandatory increment 0).
- Columnar finding understates side-copy freshness risk and misses the COPY-parity tension (fixed via async backfill above).
- COPY finding understates the permanent marker-check cost on future UPDATE/DELETE of bulk-loaded tables.
- LockManager finding's "medium risk" hides a PG RC-semantics divergence (block-and-wait → fail-fast) that is a product decision, not an engineering one.
- Normalize finding omits the IN-arity cache-explosion cap; ART finding's +25-60% is analogy-based, not measured.
- All file:line anchors from all seven findings that I spot-checked were accurate — no fabricated evidence detected.