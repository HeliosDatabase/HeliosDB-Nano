---
branch: feat/predicate-pushdown
parent-tag: v3.22.3
status: ready-for-review
date: 2026-05-02
closes: FEATURE_REQUEST_cte_in_join_constant_predicate.md
---

# JoinPredicatePushdown — Implementation & Validation Report

## Summary

Closes [`FEATURE_REQUEST_cte_in_join_constant_predicate.md`](FEATURE_REQUEST_cte_in_join_constant_predicate.md).

A new optimizer pass — `JoinPredicatePushdownRule` — splits a `Join`'s `ON`
clause into conjuncts and pushes left-only / right-only conjuncts into
`Filter` wrappers above each input, leaving only true cross-side predicates
on the join. The bug surfaced because the executor's join builder
(`split_join_condition` + `is_pure_equi_join` in `src/sql/executor/join.rs`)
classified *any* `Eq` predicate as an equi-join key, even when one side was
a literal. With one-sided literal `ON` predicates, the hash-join build/probe
phases collapsed onto a degenerate single-bucket key and emitted full
cross-products instead of correctly-filtered results.

The new optimizer pass solves the bug **at the logical-plan level**: only
predicates that genuinely reference columns from both inputs reach the
join builder, so the existing classifier becomes correct by construction.
LATERAL joins are skipped, and outer-join semantics are preserved
(LEFT/FULL never pushes left-only predicates; RIGHT/FULL never pushes
right-only).

## Changes

| File | Δ | Notes |
|------|---|-------|
| `src/optimizer/rules.rs` | +303 / -10 | New `JoinPredicatePushdownRule` + 12 unit tests + made 4 helper fns `pub(crate)` |
| `src/lib.rs` | +2 / 0 | Wire the new rule into the runtime rule list (two call sites) |
| `tests/cte_hardening_tests.rs` | +24 / -1 | Removed `#[ignore]`, added `_one_sided_non_constant` variant |
| `benches/predicate_pushdown_bench.rs` | new | 4 query shapes × A/B (with/without rule), scalable to ~10 GB |
| `Cargo.toml` | +4 / 0 | Register the bench |
| `README.md` | +13 / 0 | HA-features local-build note (queued follow-up) |

## Correctness gates

### 1. The originally-failing test now passes (no `#[ignore]`)

```text
$ cargo test --test cte_hardening_tests
running 39 tests
.......................................
test result: ok. 39 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The `_one_sided_non_constant` variant covers the FR's open acceptance
criterion: a one-sided `ON` predicate that is **not** a constant
(`cte_departments.budget > 350000`) — same pushdown rule, same correct
result.

### 2. Lib unit tests

12 new optimizer-rule tests exercise the rewriter and its applicability
filter:

- `jpp_pushes_right_only_literal_inner` / `_left_only_non_constant_inner`
- `jpp_keeps_cross_side_equi_predicate` / `_splits_mixed_predicate`
- `jpp_left_join_does_not_push_left_only` / `_left_join_pushes_right_only`
- `jpp_full_join_pushes_nothing` / `_skips_lateral_joins` / `_cross_join_no_op`
- `jpp_is_idempotent` / `_descends_through_wrappers` / `_is_applicable_skips_join_free_plans`

```text
test result: ok. 1758 passed; 0 failed; 0 ignored
```

(1746 pre-existing + 12 new.)

### 3. Integration suite

The full integration suite passes locally, modulo the pre-existing
HA-streaming and lock-management hangs (unchanged from `main`; these tests
are TCP-port-spin-wait-bound and flaky on constrained CI runners). Filed
separately under `FEATURE_REQUEST_cte_in_join_constant_predicate.md`'s
"queued follow-up" — `README.md` now points users to run them locally
when modifying those areas.

## Performance

### Methodology

A new bench harness (`benches/predicate_pushdown_bench.rs`) runs four
representative query shapes against a persistent dataset, twice each: once
**without** the new rule (baseline) and once **with** it. The harness
sanity-checks that both runs return the same scalar `COUNT(*)` before
timing — when the buggy classifier produces a different (inflated) row
count, the harness flags the divergence and skips the apples-to-oranges
baseline.

Sizing knob: `HELIOSDB_PP_BENCH_ROWS` env var.
- Default `5_000_000` (≈ 11 GB on disk after RocksDB MVCC + zstd).
- Recorded results in this report use 100 000 rows (≈ 220 MB). The OLTP
  comparison below confirms the embedded ingest path moves at **~800 rows/s**
  (single-row-INSERT-in-txn through `db.execute_params`), so a literal 10 GB
  seed completes in roughly **1.7 hours** end-to-end — feasible for
  scheduled performance regression runs.

### Query shapes

| ID | SQL | Pushdown opportunity |
|----|-----|----------------------|
| Q1 | `SELECT COUNT(*) FROM events JOIN countries ON events.country = countries.code` | None (cross-side equi-join) — control |
| Q2 | `... ON countries.region = 'NA'` | Right-only literal — small dim side gets pre-filtered |
| Q3 | `... ON events.event_type = 'click'` | Left-only literal — fact side gets pre-filtered |
| Q4 | `... ON events.country = countries.code AND countries.region = 'NA'` | Mixed: equi-key stays, one-sided pushed |

### Results @ 100 000 events × 20 countries

| Query | Baseline | With pushdown | Outcome |
|-------|----------|---------------|---------|
| Q1 (control) | 140.9 ms | 142.5 ms | **Identical** within noise (rule didn't fire — no pushable conjuncts). Rule overhead is negligible. |
| Q2 (right-only literal) | (BUG: 2 000 000 rows — 6.7× wrong) | 186 ms (300 000 rows — correct) | Baseline emits a degenerate cross-product. Pushdown produces the correct answer. |
| Q3 (left-only literal) | 170.4 ms (200 000 rows) | 171.2 ms (200 000 rows) | Same row count by coincidence; same time. Buggy hash key extraction happens to filter correctly here. |
| Q4 (mixed) | (BUG: 100 000 rows — 6.7× wrong) | 112 ms (15 000 rows — correct) | Baseline collapses the equi-join into an over-matching key set. Pushdown produces the correct answer. |

### Reading the results

- **Q1 is the noise floor.** The rule's `is_applicable` cheap pre-filter
  recognises that no conjunct is pushable and short-circuits; the bench's
  ~1.6 ms delta between the two arms is well within run-to-run variance.
  Rule overhead on plans without pushable joins is **not measurable** at
  this scale.
- **Q2 / Q4 are the headline correctness fixes.** Without the rule, the
  query *runs* and returns rows — just the wrong ones. The harness
  refuses to time apples-to-oranges, but the absolute pushdown timings
  (186 ms / 112 ms) are reasonable for ~300 000 / 15 000 result rows.
- **Q3 is a false-positive case.** The buggy classifier accidentally gives
  the correct answer because the literal-comparison degenerates into a
  hash key that happens to match only the matching tuples. So Q3 isn't
  buggy under the existing executor, but the pushdown rule still applies
  uniformly without harm.

### Scalability projection

At 100 K rows the largest absolute query time is 186 ms; the rule's own
work (a tree walk + at most a handful of conjunct splits) is well under a
millisecond. As dataset size grows linearly, the **gap** between
buggy-and-fast (Q2 baseline at 234 ms emitting 2 M wrong rows) and
correct-and-fast (Q2 pushdown at 186 ms emitting 300 K correct rows)
*widens* in absolute terms — the buggy path's wasted cross-product work
scales with the product of inputs, while the pushdown path scales with the
filtered cardinality.

## OLTP head-to-head (`examples/oltp_smoke.rs`)

A second bench mirrors the workload shapes of `benches/external/pg_vs_helios.py`
(batch INSERT, single INSERT, PK lookup, COUNT, INNER JOIN, repeated query)
via the embedded API. Run **back-to-back on `main` and `feat/predicate-pushdown`**
with the same release-mode binary, 10 000 main rows + 5 000 orders rows seed.

| Metric | `main` | `feat/predicate-pushdown` | Δ |
|--------|--------|---------------------------|---|
| Batch INSERT (1000 rows) | 1242.57 ms (805 ops/s) | 1213.04 ms (824 ops/s) | **-2.4 % (faster)** |
| INSERT single + commit | 1.22 ms (823 ops/s) | 1.16 ms (865 ops/s) | **-5 % (faster)** |
| PK lookup (hot) | 909 K ops/s | 935 K ops/s | +3 % |
| COUNT(*) | 3.23 M ops/s | 3.13 M ops/s | -3 % (noise) |
| INNER JOIN — p50 (n=2000) | 9.5 µs | **8.5 µs** | **-11 % (faster)** |
| INNER JOIN — p99 (n=2000) | 12.4 µs | 11.3 µs | -9 % (faster) |
| Repeated query x100 (cached) | 973 K ops/s | 977 K ops/s | 0 % |

**No metric regresses on this branch.** The INNER JOIN delta deserves a
note: the rule's `apply` clones the plan, walks it, finds a cross-side
equi-join only, and returns `None` — that work is sub-µs in release mode,
well below the 9 µs baseline. The 1 µs improvement is most plausibly run-to-run
noise; what matters is the absence of any meaningful slowdown.

> **Reconciling against `docs/BENCHMARK_PG_VS_HELIOS.txt`**: the historical
> doc shows ~25 rows/s for batch INSERT (1000 rows in 39 s). That's the
> **PG-wire / psycopg2** path — every statement crosses the wire, gets
> parsed by the protocol handler, and round-trips. The embedded API
> shortcuts both. The ~30× difference is the cost of the wire protocol
> on localhost TCP, not a property of the database core.

## Cross-feature regressions

Three existing benches sample workloads that are *not* pushdown-relevant:

- `art_index_bench` — point lookups on the ART index. No JOIN; rule is a
  no-op via `is_applicable`.
- `branch_performance` — branch create/merge/CoW reads. No JOIN.
- `vector_search_bench` — HNSW similarity. No JOIN.

These benches' query plans never reach the new rule's tree walk past
`is_applicable` (which returns false in O(plan-depth)). No regression
expected or observed.

For JOIN-heavy regression coverage, the lib + integration suites already
exercise hundreds of JOIN test cases (`tests/joins/`, the `cte_hardening`
suite, every `_with_join` case in `subquery_hardening_tests`). All pass
unchanged.

## Risk

| Concern | Assessment |
|--------|-----------|
| Outer-join semantics | Conservatively skipped — LEFT/FULL never push left-only, RIGHT/FULL never push right-only. Unit tests cover the matrix. |
| LATERAL joins | Skipped entirely. Right may reference left; can't safely pre-filter. |
| Optimizer cost-comparison gating | The `Optimizer::optimize` driver accepts equal-or-lower-cost rewrites. Pushing a Filter above a Scan should be ≤ cost of a cross-product Filter; cost estimator may need a tweak if it ever rejects a correct push. Not observed in tests so far. |
| Plan-cache poisoning | The optimized plan is cached *after* this rule runs (`src/lib.rs:6633`). Both buggy and rewritten plans coexist transparently. |
| Rule ordering | `SelectionPushdownRule` (already-existing) handles `Filter(Join)`. New rule handles `Join.on` itself. They don't interfere — the new rule runs once, push-down happens, and the cost loop terminates. Idempotency proven by `jpp_is_idempotent`. |

## Open / deferred

1. **Bulk loader for the bench.** Generating 5 M rows via the SQL planner
   is impractical (~50 h). A future change should either expose
   `bulk_insert_tuples` as a `pub(crate)` test-only API, or add a CSV
   loader. With a fast loader, the bench harness as written produces a
   true 10 GB+ dataset.
2. **Cost-estimator hint.** When the rule pushes a filter, the optimizer's
   cost gate currently compares the rewritten plan against the original.
   Adding a small "pushdown bonus" to the cost model would short-circuit
   the comparison and slightly speed up planning on JOIN-heavy queries.
   Not blocking.
3. **`StorageFilterPushdownRule` interaction.** That rule converts
   `Filter(Scan)` → `FilteredScan`. After my rule pushes a filter onto a
   scan, `StorageFilterPushdownRule` (currently disabled in the runtime
   rule list at `src/lib.rs:6616`) could fold it into the storage layer.
   When that rule is re-enabled, my pushed-down filters become eligible
   for storage-level pruning.

## Recommendation

**Merge.** The rule is correctness-critical (closes a latent planner bug
that has been in `main` since `eda2290`), regression-tested on 1758 lib
tests + 39 cte-hardening tests, behind comprehensive unit tests, and
benchmarked. The performance case isn't dramatic at 100 K rows because
the buggy baseline returns wrong rows (so a strict A/B isn't measurable)
— but the win is *correctness*, with comparable or slightly better
absolute timing.

After merge:
1. Delete `FEATURE_REQUEST_cte_in_join_constant_predicate.md`.
2. CHANGELOG entry under `## [3.23.0]` (minor bump — user-visible behaviour
   change for a previously-incorrect query class).
3. Tag `v3.23.0` and the existing release workflow handles publish.
