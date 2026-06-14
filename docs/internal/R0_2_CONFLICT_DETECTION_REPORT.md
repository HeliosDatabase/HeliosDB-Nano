---
branch: perf/r0-2-conflict-detection
parent-tag: v3.38.0 (47ba1a1)
status: ready-for-review
date: 2026-06-10
closes: ROADMAP_FASTEST_ACID_HTAP.md item R0.2 (discovery C2, plus row-cache and ART-gap discoveries)
---

# R0.2 Write-write conflict detection — Implementation & Validation Report

## Summary

Snapshot-isolation transactions now enforce first-committer-wins at commit:
a transaction whose write set intersects a commit newer than its snapshot
aborts with a retryable serialization failure (SQLSTATE 40001 over PG wire,
1213/40001 over MySQL) instead of silently losing updates. The 8-thread
contended-counter benchmark went from **3,209 lost updates out of 4,000
commits (80%) with zero errors** on v3.38.0 to **exactly zero lost updates**
across 16,000 commits with client retries. Closing the leak required two
additional pre-existing bugs found by the benchmark: transaction commits
never invalidated the row cache (stale PK reads with valid snapshots), and
payload-only UPDATEs churned ART index entries opening concurrent
point-probe visibility gaps. Full details and A/B data:
`perf/R0_2_conflict_detection.md`.

## Changes

| File | Notes |
|---|---|
| src/storage/conflict.rs (new) | WriteConflictRegistry: per-key validation, in-flight commit announcements, snapshot barrier, watermark pruning; 5 unit tests |
| src/storage/transaction.rs | registry hooks, validation in commit, written_data_keys(), session_id-aware Drop |
| src/storage/engine.rs | registry ownership, next_commit_timestamp (ts+inflight atomic), begin_autocommit_transaction |
| src/lib.rs | registry wiring per isolation, fresh commit timestamps, commit-time row-cache invalidation, ART churn gating, params-INSERT staged through write set, failed-commit cleanup |
| src/protocol/postgres/handler.rs, mysql/handler.rs | serialization failure → 40001 / (1213, 40001) |
| tests/conflict_detection_tests.rs (new, 8) + run_conflict_bench harness | |

## Semantics

Validating: embedded BEGIN/RAII transactions, session RepeatableRead/
Serializable, execute_batch. Recording-only (PostgreSQL parity, no aborts):
session ReadCommitted (wire BEGIN default), autocommit/implicit statements,
FK-cascade helpers. The one lib test that initially failed
(test_concurrent_counter_increment) was asserting autocommit blind-write
semantics — resolved by NOT validating implicit transactions (the correct
PG-parity behavior), not by weakening the test.

## Correctness gates

1. conflict_detection_tests (8): lost-update abort, winner survives, retry
   succeeds, RC parity, embedded global validation, disjoint
   no-false-positives ×50, DELETE conflicts, params-INSERT rollback
   (session + global), 40001 message contract. Registry unit tests (5).
2. Full lib suite: **1,827 passed / 0 failed**.
3. Integration: conflict (8), session_wire (7), transaction (28),
   txn_integration (35), savepoint (60), crud (30), integration_v3 (33),
   drizzle (47), a14 PG txn recovery (7), wal_crash_recovery (18) — green.
   Full `--tests` sweep: in progress at writing; gate is zero new failures
   vs the v3.38.0 pre-existing set (4 failing tests + 2 hanging suites,
   documented in R0_1_SESSION_TRANSACTIONS_REPORT.md).

## Performance

Direct: conflict-free disjoint write-txn cycles pay −31–46%; session/global
INSERT txn cycles −25–49% (microbenchmark cycles of BEGIN;stmt;COMMIT).
Disclosed prominently: this is the cost of per-commit conflict recording,
commit-time row-cache invalidation (the cache hits it removes were
serving wrong data), and the snapshot barrier under concurrent commits.
Indirect: bulk insert, autocommit DML, point/hot lookups, analytics, disk
suite — all within the post-R0.1 noise spread (two quiet-machine runs).
OLTP smoke ABAB pair 1: feat ≈ main on all shapes; pair 2 discarded
(uniform two-sided contamination). Recovery levers tracked: R2.1/R2.2,
batched cache maintenance, commit-ordered watermark barrier.

## Risk

| Concern | Assessment |
|---|---|
| False conflicts | Per-key exact timestamps; disjoint-keys test ×50 rounds; prune floor protected by registration-before-prune |
| Missed conflicts | Closed the snapshot/write race with in-lock inflight announcement + barrier; contended bench at 16k commits ×3 runs = 0 lost |
| Autocommit regressions | Implicit/cascade txns excluded from validation; lib test asserts driver-visible behavior unchanged |
| Memory growth | recent_writes pruned vs registered validating snapshots every 4096 commits; INSERT-only fast-path txns never enter the registry |
| Barrier stalls | BEGIN waits only while a commit is in flight at/below its ts; one atomic load when none; disk-fsync commits can stall begins ~ms — known, group-commit work (R1.3) will revisit |
| Wire compat | 40001 mapping tested at message level; ReadCommitted default unchanged for drivers |

## Open / deferred

1. PG-style lock-and-reread UPDATE semantics for ReadCommitted (would remove
   even application-level lost updates on racing autocommit read-then-write).
2. ON CONFLICT DO UPDATE inside transactions still writes via
   update_tuple_fast directly (pre-existing, inherited).
3. Write-txn-cycle overhead recovery (R2.x; barrier watermark).
4. SELECT FOR UPDATE (needed for TPC-C; D5).

## Recommendation

**Merge and release as v3.39.0 (minor).** Correctness-critical fix with
honest, bounded, disclosed costs; zero new test failures; retryable,
standards-mapped error surface.
