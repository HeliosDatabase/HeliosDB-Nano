# HeliosDB-Nano P0/P1/P2 performance work — consolidated report

**Branch:** `perf/p0-p1-p2` (worktree `/home/gpc/HDB/Nano-perf`, off `030948b` v3.33.1)
**Date:** 2026-05-29 · 32-core EPYC, 125 GiB RAM, XFS-on-`md` (fsync median ~11 ms)

This branch implements the P0/P1/P2 items from the TPS investigation. Each item
has a detailed comparison file in this directory. Work was split with a concurrent
**Codex** session that owned the **INSERT/bulk write path** (row-counter staging,
literal + parameterized fast inserts, multi-row VALUES); this branch owns
**DELETE/UPDATE, WAL, MVCC versioning, scans, row-cache, analytics**.

## Headline results

| Item | Change | Result | File |
|---|---|---|---|
| **baseline** | DELETE `WHERE pk=?` was a full keyspace scan (`get_referencing_fks`) | **118× faster** at 32k rows; now O(1) | (committed) |
| **P0#2** | autocommit UPDATE/DELETE drop per-statement WAL fsync (`append_nosync`) | DELETE **63→4,175/s** (disk, 43×); crash-recovery intact | [P0_2](P0_2_wal_durability.md) |
| **P0#1** | gate commit-time `v:`/`v_idx:` MVCC keys on `time_travel_enabled` | 3 keys/row → 1 when TT-off; +4% bulk insert; fixes the config | [P0_1](P0_1_conditional_versioning.md) |
| **P0#3** | UPDATE expr-RHS (`col+1`) fast path verified; DELETE status | UPDATE expr = 42µs (= literal, flat); DELETE O(1)+disk-fast | [P0_3](P0_3_delete_update_fastpaths.md) |
| **P0#4** | row-cache read path → shared lock + atomic counters | **1.58×** concurrent hot-lookup at 4 threads vs legacy | [P0_4](P0_4_lock_churn.md) |
| **P1#6** | parallel scan decode (rayon, order-preserving) | 1.1–1.4× on large scans/aggs (Amdahl-bound) | [P1_6](P1_6_parallel_scan.md) |
| **P1#7** | Top-N heap for `ORDER BY … LIMIT` | **already implemented** in baseline (`TopKOperator`) | — |
| **P2#8** | 7 storage prefix-seek fixes (was full-keyspace scans) | O(table) → O(1) for MV-refresh / DDL / trigger paths | [P2_8](P2_8_iterator_scan_audit.md) |
| **P1#5** | columnar scan operator | **design + rationale only** — multi-day exec-engine feature | [P1_5](P1_5_columnar_scan.md) |

## What each item delivered

- **P0#2 (biggest durable-write win)** — DELETE had no fast path, so every
  autocommit DELETE paid a per-statement fsync'd logical-WAL append on top of the
  RocksDB commit batch. Now appends *without* fsync by default
  (`storage.logical_wal_per_statement=false`), keeping recovery-replay +
  replication consistent. A naive *skip* broke crash recovery (replay resurrected
  deleted rows because INSERTs still log); `append_nosync` fixes that. Strict
  per-statement durability is opt-in.
- **P0#1** — the commit path wrote 3 keys/row unconditionally; `time_travel_enabled`
  only gated a *different* versioning path, so the documented "disable to reduce
  write overhead" didn't actually reduce commit writes. Now it does (default
  unchanged ⇒ time-travel untouched). The win is write *volume* (memtable/compaction/
  keyspace), not single-statement latency (the value isn't re-serialized).
- **P0#3** — `fast_eval_simple_expr` already makes `UPDATE … SET col = col+1`
  hit the fast path (verified 42µs, flat). DELETE is already O(1) + disk-fast; a
  dedicated `try_fast_delete` is scoped (see file) to avoid colliding with the
  active insert-path edits in `execute_in_transaction_inner`.
- **P0#4** — row-cache reads no longer take an exclusive lock; concurrent
  point-lookups scale better. The bench also pinpointed the *dominant* concurrency
  cap: `query()`'s 4 per-statement `Mutex`es (parse/plan/result caches +
  current_transaction) convoy at 16 threads — scoped as the larger follow-up.
- **P1#6** — parallel decode is a real but Amdahl-limited win; it *proves* the
  analytics gap is row-materialization (P1#5), not decode speed.
- **P2#8** — eliminated 7 `IteratorMode::Start` full-keyspace scans (the same class
  as the baseline 118× DELETE bug) on MV-refresh/DDL/trigger paths.

## Validation

All changes are default-feature green: lib **1,772 pass** (the only failure,
`vector::hnsw_index::test_vector_count_tracking`, is pre-existing and unrelated —
it fails on the untouched baseline too). Targeted suites pass: crash_recovery_e2e
(4), wal_crash_recovery (35), transaction (27), transaction_integration (18),
crud (27), datatype (27), aggregate_hardening (72), materialized_view (18+33),
trigger (9), row_cache (5). The repo's `internal-tests` time-travel suites are
bit-rotted against current structs (pre-existing) and could not be run.

## Reproduce

```bash
# per-workload TPS (regime ∈ mem|disk|disk_group|disk_nowal)
HELIOS_TPS=1 HELIOS_TPS_MODE=disk HELIOS_TPS_N=5000 HELIOS_TPS_M=400 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
# A/B toggles: HELIOS_LOGICAL_WAL=1 (P0#2), HELIOS_NO_TT=1 (P0#1),
#              HELIOS_SCAN_SERIAL=1 (P1#6), HELIOS_ROWCACHE_LEGACY=1 (P0#4)
HELIOS_SCAN=1  cargo test --profile perf --test tps_workloads run_scan_bench -- --nocapture --test-threads=1
HELIOS_CACHE_CONC=1 cargo test --profile perf --test tps_workloads run_cache_concurrency_bench -- --nocapture --test-threads=1
```

## Toward the goal (a few× faster than PG/MySQL/SQLite)

- **Reads**: already there — point lookups beat SQLite (ART index + caches).
- **Durable writes**: DELETE/UPDATE were the gap; P0#2 + the baseline DELETE fix
  + Codex's insert work bring autocommit DML to ~4–22k/s on this slow-fsync disk
  (fsync-bound for *all* engines here; group-commit is the lever for concurrency).
- **Analytics (the remaining gap)**: still 6–89× slower than SQLite. Parallel
  decode (P1#6) only moves it ~1.4× — the real fix is **P1#5 columnar scan +
  vectorized aggregation**, the single highest-value remaining piece.
- **Concurrency**: row-cache fixed; the per-query `Mutex` convoy (P0#4 follow-up)
  is the next ceiling.

## Merge plan / coordination

Worktree branch off `030948b`; one fmt baseline commit (`f12aeb0`) neutralizes the
repo's non-rustfmt-clean state. Expected merge overlaps with the concurrent
insert-path work — all in **different functions**, so low-conflict:

| file | their edits (insert path) | my edits |
|---|---|---|
| `transaction.rs` | `commit_with_timestamp` `counter:*` special-case | `commit_with_timestamp` `if versioning_enabled` wrapper (P0#1); `Transaction` field + `set_versioning_enabled` |
| `storage/engine.rs` | `insert_tuples_fast_batch`, `insert_tuple_fast` + row-counter staging | `scan_table_with_schema_opt` (P1#6), `log_data_{update,delete}_nosync` (P0#2), `begin_transaction` versioning (P0#1), `logical_wal_per_statement()` accessor |
| `lib.rs` | INSERT arms + `try_fast_insert*` routing in `execute_in_transaction_inner` | UPDATE/DELETE arms WAL gates (P0#2) |
| `config.rs` | — | `logical_wal_per_statement` field (P0#2) |
| `row_cache.rs` | — (untouched) | shared-lock read path + atomic counters (P0#4) |

`transaction.rs::commit_with_timestamp` is the only function both touch; the
`counter:*` arm and the version-key wrapper are independent regions of the
write-set loop and should auto-merge or trivially resolve. The scoped follow-ups
(`try_fast_delete`, query-path Mutex sharding, columnar scan) are deliberately
sequenced **after** the insert-path work merges, to avoid churn in the files it is
actively editing.
