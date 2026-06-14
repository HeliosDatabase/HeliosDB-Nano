---
branch: perf/r0-1-session-txns
parent-tag: v3.37.3 (98a868a)
status: ready-for-review
date: 2026-06-10
closes: ROADMAP_FASTEST_ACID_HTAP.md item R0.1 (+ discoveries C1-partial, ART-undo latent bug)
---

# R0.1 Per-session wire transactions — Implementation & Validation Report

## Summary

PG and MySQL wire connections each get an isolated database session; transaction
control and in-transaction statements run through the per-session transaction API
instead of the process-global `current_transaction` slot. This fixes the
cross-connection transaction bleed (any connection's statements executed inside a
foreign open transaction), adds read-your-writes to wire in-transaction SELECTs
(simple-query and extended-protocol), isolates eagerly-applied ART index
mutations per session (previously a latent phantom/stale-index bug), and — via an
atomic open-transaction counter replacing per-statement `DashMap::is_empty()`
shard sweeps — makes the autocommit DML fast-path gates cheaper than they were
on main. Detailed change list and A/B data: `perf/R0_1_per_session_transactions.md`;
baseline protocol: `perf/BASELINE_2026_06_10.md`.

## Changes

| File | Δ | Notes |
|---|---|---|
| src/lib.rs | +560/−70 | session entry points, session_txn threading, per-session ART undo, TT-aware gates, atomic txn counter |
| src/protocol/postgres/handler.rs (+_extended) | ~+90 | per-connection session, txn control via session API, Drop cleanup |
| src/protocol/mysql/handler.rs | ~+30 | same migration |
| src/storage/transaction.rs | +7 | `session_id()` accessor |
| src/session/manager.rs | +14 | quota-exempt `create_session_unchecked` for wire connections |
| tests/session_wire_transactions.rs | +190 (new) | 7-test correctness matrix |
| tests/tps_workloads.rs | +113 | `run_session_txn_bench` (HELIOS_SESSION_TXN=1) |

## Correctness gates

1. **Phase 2 — targeted matrix (new, all pass):** cross-session bleed;
   read-your-writes (simple + extended params); per-session ART undo isolation
   incl. committed-rows-survive-unrelated-global-rollback; 16-thread concurrent
   commit count; orphaned-connection rollback via destroy/Drop; RepeatableRead
   snapshot vs concurrent autocommit fast insert (guards the TT-aware gate).
2. **Phase 3 — lib suite:** 1,822 passed / 0 failed (`cargo test --lib`).
3. **Phase 3 — integration sweep:** full `--tests` target sweep (fail-fast pass
   through `a14…postgres_scram`, then individually `pq…zke`): 62/66 remaining
   targets green (~896 tests) + the 13 suites run pre-merge (282 tests).
   **Zero new failures.** Six anomalies, each reproduced bit-identically on
   clean main (98a868a) in a separate worktree, i.e. pre-existing:
   - `postgres_ssl_tests` — hangs (also found an 11-day-old orphaned instance
     of this binary from the deleted Nano-perf worktree; killed). Same class
     as the documented HA-streaming/lock-management environment flakes.
   - `pq_storage_integration_test` — exceeds 600 s in debug profile on this
     host (PQ k-means training), both sides.
   - `truncate_hardening_tests::test_truncate_does_not_return_affected_row_count`,
     `v334_a11…::a11_do_update_where_reads_excluded_and_existing_row`,
     `vector_store_api::{vector_delete_removes_from_unfiltered_search,
     vector_upsert_replaces_single_visible_record}` — fail identically on main
     (4 tests; likely related to the in-flight ISSUE-08 work planned on main).

## Performance

### Phase 4 — targeted bench (`run_session_txn_bench`, mem, N=10k/M=2k)

| Direct workload | Baseline | After | Δ |
|---|---:|---:|---|
| session_txn_cycle(1T) | 18,364 txn/s | 45,601 | +148% |
| session_txn_cycle(4T) | 36,155 txn/s | 90,261 | +150% |
| session_txn_cycle(16T) | 49,390 txn/s | 161,770 | +228% |
| autocommit insert w/ open session txn | 12,908 ops/s | 123,352 | +856% |
| global_txn_cycle(1T) | 48,335 txn/s | 50,324 | +4% |

Both arms verified to produce identical logical results (commit counts asserted
in the test matrix). 16 concurrent isolated wire transactions is a new
capability — previously a second wire BEGIN errored or folded.

### Phase 5 — cross-feature regression

- `art_index_bench` (the area adjacent to the ART-undo change): 30 paired
  measurements main-vs-branch, deltas scattered ±7% in both directions — noise,
  no systematic regression.
- Embedded TPS suite + cache-concurrency (perf/R0_1_per_session_transactions.md):
  reads/analytics within noise; autocommit DML faster (+29–112%, atomic counter).
- `branch_performance` — bench harness panics identically on clean main under
  `--quick` ("Branch 'bench_0' already exists", counter-reset bug); branching
  covered instead by branch_storage/branch_data_isolation integration tests
  (green). `conflict_detection_bench` requires non-default `sync-experimental`.

### Phase 6 — OLTP head-to-head (oltp_smoke, release, ABAB ×2)

| Workload | main (2 runs) | branch (2 runs) | Verdict |
|---|---|---|---|
| Batch INSERT (1000 rows) | 159,958 / 151,647 ops/s | 228,800 / 213,201 | **+42%** |
| INSERT single + commit | 66,190 / 63,902 | 85,529 / 81,613 | **+28%** |
| PK lookup (hot) | 1.25M / 1.11M | 1.20M / 1.07M | noise |
| COUNT(*) | 1.22M / 1.06M | 1.16M / 1.10M | noise |
| INNER JOIN p50 (n=2000) | 12.4 / 11.6 µs | 12.7 / 12.4 µs | noise (≤4%, p99 equal) |
| Repeated cached query | 1.21M / 1.33M | 1.31M / 1.30M | noise |

## Risk

| Concern | Assessment |
|---|---|
| Snapshot isolation vs relaxed INSERT fast-path gate | Gate requires time-travel versioning on; regression test asserts RepeatableRead snapshots filter concurrent fast inserts. UPDATE/DELETE gates unchanged (strict). |
| ART undo routing | Per-session logs replayed/cleared on rollback/commit/destroy; global path byte-identical. Dedicated tests incl. cross-log isolation. |
| Embedded API behavior change | None — `begin()/commit()/rollback()` and the global slot untouched; REST/MCP/REPL are autocommit-only/single-session. |
| Wire session leaks | `destroy_session` rolls back open txns; called from both handlers' `Drop`. Quota-exempt creation bounded by `max_connections` semaphore. |
| `execute_plan_with_params` signature threading | `None` = legacy global-slot lookup, byte-equivalent; `Some` only from new session entry points. Plan-level txn-control with a session txn now errors instead of corrupting the global slot. |
| Session SAVEPOINT | Rejected with explicit error (previously global-only anyway); follow-up. |
| Complex params INSERT bypasses write set | Pre-existing on the global path; inherited unchanged. Tracked for R0.2. |

## Open / deferred

1. R0.1b — delete the embedded global slot (compat shim) once downstream users migrate.
2. Session SAVEPOINT support.
3. R0.2 — write-write conflict detection on the default path; fix the complex
   params-INSERT write-set bypass.
4. MySQL COM_STMT_EXECUTE parameter decoding (roadmap C6/R5.W4).
5. Pre-existing failures found during validation (4 tests + 2 hanging suites,
   all reproduced on main) need their own triage; `postgres_ssl_tests` and
   `pq_storage_integration_test` should join the documented `--skip` flake list.

## Recommendation

**Merge.** Fast-forward from 98a868a; zero new test failures; direct targets met
or exceeded; indirect categories within noise or improved; correctness fixes are
covered by a dedicated regression matrix.
