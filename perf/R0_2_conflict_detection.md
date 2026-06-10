# R0.2 — Write-write conflict detection (first-committer-wins)

**Branch:** `perf/r0-2-conflict-detection` (worktree `/home/gpc/HDB/Nano-r01`, off `47ba1a1` v3.38.0)
**Date:** 2026-06-10 · 32-core EPYC · `--profile perf`
**Baseline:** post-R0.1 numbers (`perf/R0_1_per_session_transactions.md`) + `perf/baseline_runs/conflict_baseline.txt`
**Raw runs:** `perf/r0_2_runs/` (`*_final*` = quiet-machine runs; earlier files in that dir were taken under
parallel-agent compile load and are kept only as contamination examples)

## The bug, measured

`run_conflict_bench` contended counter: 8 RepeatableRead sessions increment ONE row,
500 cycles each, retrying on error:

| | committed | retries | final value | lost updates |
|---|---:|---:|---:|---:|
| **v3.38.0 (baseline)** | 4,000 | 0 | **791** | **3,209 (80%)** |
| **R0.2** (2k cycles ×8) | 16,000 | 1,761 | **16,000** | **0** |

Baseline silently lost 80% of committed increments with zero errors. R0.2 aborts the
loser with `serialization failure … retry the transaction` (SQLSTATE 40001 on PG wire,
1213/40001 on MySQL); with client retries the counter is exact. Verified across three
quiet-machine runs.

## What was needed beyond the registry (each found by this bench)

1. **Registry v1 (global commit mutex) leaked ~1%** — a fresh transaction's snapshot
   timestamp could be allocated after a commit's timestamp but before its RocksDB write
   landed: stale read, clean validation. Fix: commit timestamps announce themselves
   in-flight **inside the engine timestamp lock** (`next_commit_timestamp`), and new
   snapshots wait at a barrier until no in-flight commit ≤ their timestamp. v2 also
   replaced the global mutex with per-key DashMap-entry validation (disjoint committers
   never serialize) with partial-record undo for multi-key losers.
2. **Row cache poisoning (pre-existing bug)** — transaction commits write RocksDB
   directly and never invalidated the row cache; the UPDATE arm's PK point-lookup could
   read a pre-commit value repopulated during the staging window and lose an update with
   a perfectly valid snapshot. Commits now invalidate written rows
   (`written_data_keys` → `invalidate_row_cache_for`).
3. **ART visibility gaps (pre-existing, exposed by aborts)** — eager
   delete+reinsert of identical index entries on payload-only UPDATEs (and their undo
   replays) let concurrent PK probes miss the row entirely (`zero_matches` in the bench).
   ART churn is now gated on `tuple_update_affects_indexes`.

## Semantics shipped

| Path | Validates? | Rationale |
|---|---|---|
| Embedded `BEGIN`/RAII transactions | **yes** | snapshot reads + blind commit was silent lost updates (C2) |
| Session RepeatableRead / Serializable | **yes** | SI contract; PG parity |
| Session ReadCommitted (wire BEGIN default) | no (records) | PostgreSQL RC does not raise serialization failures |
| Autocommit / implicit / FK-cascade statements | no (records) | PG parity; retryable errors on plain autocommit would break drivers (caught by `test_concurrent_counter_increment`) |
| `execute_batch` | yes | atomic multi-statement unit |

Also shipped: fresh commit timestamps on all commit paths (commit at BEGIN-ts recorded
wrong version order); parameterized-INSERT plan arm stages through the write set
(was a direct write that survived ROLLBACK) and passes the txn to FK validation;
failed session commits clean up like rollbacks.

## Performance

Direct (quiet machine, `conflict_final.txt` / `session_final.txt`):

| Workload | v3.38.0 | R0.2 | Δ |
|---|---:|---:|---|
| update_txn_cycle 1T disjoint | 32,191/s | 18,166/s | −44% |
| update_txn_cycle 4T disjoint | 72,815/s | 39,338/s | −46% |
| update_txn_cycle 16T disjoint | 112,054/s | 77,504/s | −31% |
| contended_counter 8T | 24,723/s (80% lost) | 15,824/s (0 lost) | correctness |
| session/global INSERT txn cycles | 45.6k/50.3k (1T) | 23.4k/37.1k | −25–49% |
| autocommit_insert w/ open session txn | 123,352/s | 105,962/s | −14% (noise band) |

The write-transaction-cycle overhead is real and disclosed: per-commit conflict
recording, commit-time row-cache invalidation (every cycle's read is now a cache
miss — the hits were the bug), and the snapshot barrier under concurrent commits.
The baseline was fast *because it was wrong*. Recovery levers: batched/keyed cache
maintenance, barrier refinement (commit-ordered watermark), R2.1/R2.2 lock work.

Indirect (mem suite, 2 runs vs post-R0.1 spread): bulk_insert 141–170k (R0.1:
141–172k) ✓; autocommit_insert 119–126k (vs 129k, edge of noise); update_by_pk
164–192k (R0.1 spread 168–203k) ✓; delete 242–284k (R0.1 258–313k) ✓; point/hot
lookups and all analytics flat ✓. Disk suite flat. OLTP smoke ABAB: pair 1
feat ≈ main on all six shapes; pair 2 discarded (uniform ~45% degradation on both
arms' read metrics — machine contamination).

## Validation

- `tests/conflict_detection_tests.rs` (8): lost-update abort + winner-survives +
  retry-succeeds; RC blind-write parity; embedded global validation; disjoint
  no-false-positives (50 rounds); DELETE conflicts; params-INSERT rollback in
  session + global txns; 40001 message contract.
- Registry unit tests (5): first-committer-wins, partial-record undo,
  record-without-validate, prune vs active snapshots, barrier waits.
- `run_conflict_bench` with per-cycle assertions (disjoint rows must all land).
- Full lib suite **1,827 green** (the one initially failing test asserted autocommit
  blind-write semantics — fixed by NOT validating implicit transactions, which is
  the correct PG-parity call, not by changing the test's assertion).
- Suites green: conflict (8), session_wire (7), transaction (28), txn_integration
  (35), savepoint (60), crud (30), integration_v3 (33), drizzle (47), a14 (7),
  wal_crash_recovery (18+2 ignored), time_travel_integration (0 — empty by default).
