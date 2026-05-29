# Pre-existing test-failure triage

**Created:** 2026-05-29 · **Branch/commit:** `main` @ `dddaed1` (after the `perf/p0-p1-p2` merge)

## What this is

Running the full test suite (`cargo test --profile perf --no-fail-fast`) on `main`
surfaces **18 failing tests across 10 binaries** (out of 164 passing binaries).
**All of them pre-date the perf/P0-P1-P2 work** — every one was verified to fail
identically on the untouched baseline `030948b` (the v3.33.1 release content), or
is an environment/test-data artifact. The perf work and its verification fixes
introduced **zero** new failures.

These are inherited from the `030948b` (v3.33.1) content that the merge brought
into `main`; this doc triages them so they can be fixed/closed separately.

**Categorization method:** each suspect binary was rebuilt and run at `030948b`
(my changes absent — confirmed: 0 occurrences of `broadcast_after_append` /
`versioning_enabled` / `[profile.perf]`). Baseline line numbers differ from
`main` only by the `cargo fmt` baseline commit's whitespace shifts.

---

## Group A — likely real engine bugs / feature gaps (priority: triage & fix)

### A1. Correlated subqueries / EXISTS return wrong results (8 tests) — **HIGH**
`tests/subquery_hardening_tests.rs`: `test_correlated_exists_with_additional_filter`,
`test_correlated_subquery_comparison`, `test_correlated_scalar_subquery_in_select`,
`test_subquery_in_having`, `test_exists_anti_join_with_condition`,
`test_exists_correlated_join_pattern`, `test_exists_with_select_star`,
`test_not_exists_anti_join_pattern` (panics ~ :506/:526).
Non-correlated EXISTS/IN variants in the same file **pass**. Pattern: the executor
mishandles **correlated** subqueries (those referencing the outer row). Likely the
biggest functional gap here. Fails on baseline.

### A2. `information_schema.referential_constraints` view (3 tests) — **MED**
`tests/information_schema_completion.rs`: `referential_constraints_view_returns_zero_rows_for_no_fks`
(:78), `referential_constraints_view_exposes_real_fk_metadata` (:98),
`existing_views_still_work` (:230). The info-schema FK-metadata view does not
return the expected rows. (Independent of `get_referencing_fks`, which is exercised
correctly by the passing FK cascade/restrict suites.) Fails on baseline.

### A3. CTE with `EXISTS` returns too few rows — **MED**
`tests/cte_hardening_tests.rs::test_cte_with_exists` (:526) — `assert rows.len() >= 2`.
Related to A1 (EXISTS handling inside a CTE). Fails on baseline.

### A4. `TRUNCATE` returns an affected-row count — **LOW/MED**
`tests/truncate_hardening_tests.rs::test_truncate_does_not_return_affected_row_count`
(:792). TRUNCATE should report 0 affected rows (DDL-like), but returns a count.
Fails on baseline.

### A5. No read-your-own-writes inside an explicit transaction — **MED**
`tests/savepoint_hardening_tests.rs::test_query_within_explicit_transaction_no_ryow`
(:498). A `SELECT` in an explicit `BEGIN` block doesn't observe the txn's own
uncommitted writes as the test expects. Fails on baseline. (Note: this is the
RYOW/MVCC read path — unrelated to the P0#1 read-side gate, which only changes
behavior when `time_travel_enabled=false`; this fails with the default TT-on.)

### A6. HNSW vector index tombstone count — **MED**
lib: `vector::hnsw_index::tests::test_vector_count_tracking` (`src/vector/hnsw_index.rs:618`)
— "L2 index length should remain 3 (tombstone), left: 2, right: 3". After a vector
delete, the L2 index length drops to 2 instead of keeping the tombstone at 3.
Possible real bug in HNSW delete/tombstone bookkeeping. Fails on baseline.

---

## Group B — stale test (engine is correct; update the test)

### B1. `DEFAULT` on omitted column "known limitation" is fixed — **update test**
`tests/null_semantics_hardening_tests.rs::test_default_value_on_omitted_column_known_limitation`
(:1091) — asserts the omitted-`DEFAULT` column yields `Null`, but the engine now
**correctly** applies the default (`Int4(42)`). The "known limitation" was fixed; the
test still asserts the old buggy behavior. **Action:** update the assertion to expect
the default value (and rename away from "known_limitation").

---

## Group C — flaky / environment-dependent (CI hygiene)

### C1. Auth timing-attack assertion is timing-fragile — **flaky**
`tests/postgres_scram_auth_tests.rs::test_auth_manager_timing_attack_resistance`
(:279) — `assert time_*.as_micros() > 0`. On a fast/idle machine the measured op
rounds to 0 µs and the assertion fails. **Action:** measure in nanoseconds, or assert
relative timing rather than `> 0`.

### C2. PQ index test provides insufficient training data — **test-data fix**
`tests/pq_storage_integration_test.rs::test_memory_efficiency_comparison` (:221) —
`PQ training failed: Insufficient training data: got 1000 samples, need at least 1920`.
The test feeds 1000 vectors but PQ training requires ≥1920. **Action:** raise the
test's sample count to ≥1920 (or assert the training-error path intentionally).

### C3. `ha_integration` hangs in this sandbox — **environment**
`tests/ha_integration.rs` — hangs (does not complete within 120 s) on **both** `main`
and baseline; it's an HA/network integration test that binds ports / waits on
connections that aren't available here (a `127.0.0.1:5432`/`:54320` listener is
already present in this environment). Not a code regression — a no-standby WAL
broadcast is a no-op. **Action:** run in an isolated network namespace / dedicated
ports in CI, or gate behind a feature/`#[ignore]` for sandboxed runs.
(`tests/postgres_ssl_tests.rs::test_ssl_mode_disable_rejects_ssl_request` similarly
hangs and was skipped during the full-suite run — same class.)

---

## Summary

| group | count | nature | priority |
|---|---|---|---|
| A (real engine bugs) | 14 | correlated subquery/EXISTS (8+1), info-schema views (3), TRUNCATE count, txn RYOW, HNSW tombstone | triage & fix |
| B (stale test) | 1 | default-value test asserts old behavior | update test |
| C (flaky/env) | 2 (+SSL) | auth timing, PQ training data, HA/SSL network | CI hygiene |

None block the perf work (all pre-existing in v3.33.1). Group A1 (correlated
subqueries) is the most impactful functional gap.
