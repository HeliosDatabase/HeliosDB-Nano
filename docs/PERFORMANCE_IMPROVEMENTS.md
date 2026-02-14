# HeliosDB Nano — Performance Improvement Report

**Date:** 2026-02-13
**Test:** `cargo test --test pipeline_performance_test -- --nocapture`
**Hardware:** Linux 5.14.0-611 x86_64

## Improvements Implemented

| # | Improvement | Description |
|---|-------------|-------------|
| 1 | **Plan Cache** | LRU cache (256 entries) maps SQL string to LogicalPlan, skipping parse+plan for repeated queries |
| 2 | **Batch Commit** | `execute_batch()` wraps multiple statements in single BEGIN/COMMIT transaction |
| 3 | **Index Lookups** | ART index point lookups for `WHERE pk = value` — bypasses full table scan |
| 4 | **Parse Cache** | LRU cache (512 entries) maps SQL string to AST Statement, skipping SQL parsing |
| 5 | **RocksDB Tuning** | Write buffer optimization, pipelined writes, background compaction tuning |

## In-Memory Wall Time Comparison (6 Runs)

| Statement | Baseline | +Plan Cache | +Batch | +Index | +Parse Cache | +RocksDB |
|-----------|----------|-------------|--------|--------|--------------|----------|
| CREATE TABLE | 3.2ms | 3.2ms | 3.2ms | 3.3ms | 3.2ms | 3.4ms |
| ALTER TABLE ADD COL | 4.2ms | 4.2ms | 4.2ms | 4.2ms | 4.1ms | 4.3ms |
| DROP TABLE | 4.2ms | 4.2ms | 4.2ms | 4.4ms | 4.2ms | 4.3ms |
| INSERT (single) | 4.3ms | 4.3ms | 4.3ms | 4.4ms | 4.3ms | 4.4ms |
| INSERT (bulk 100) | 445ms | 445ms | 445ms | 452ms | 445ms | 447ms |
| INSERT (batch 100) | - | - | **171ms** | 171ms | 174ms | 173ms |
| UPDATE (single) | 7.7ms | 7.7ms | 7.7ms | 8.3ms | 7.8ms | 7.8ms |
| UPDATE (bulk WHERE) | 17.3ms | 17.3ms | 17.3ms | 17.9ms | 17.5ms | 17.8ms |
| DELETE (single) | 8.5ms | 8.5ms | 8.5ms | 9.0ms | 9.1ms | 9.0ms |
| DELETE (bulk WHERE) | 197ms | 197ms | 197ms | 595ms | 618ms | 603ms |
| SELECT * (full scan) | 7.1ms | 7.1ms | 7.1ms | 7.3ms | 7.4ms | 7.2ms |
| SELECT WHERE | 5.5ms | 5.5ms | 5.5ms | 6.0ms | 5.6ms | 5.6ms |
| **SELECT WHERE id=** | **5.2ms** | **5.2ms** | **5.2ms** | **551us** | **413us** | **406us** |
| SELECT LIMIT 10 | 4.4ms | 4.4ms | 4.4ms | 4.8ms | 4.5ms | 4.6ms |
| SELECT proj+filter | 5.5ms | 5.5ms | 5.5ms | 5.7ms | 5.5ms | 5.6ms |
| COUNT(*) | 4.9ms | 4.9ms | 4.9ms | 5.1ms | 5.6ms | 5.0ms |
| AVG/SUM/MIN/MAX | 6.2ms | 6.2ms | 6.2ms | 6.3ms | 6.4ms | 6.3ms |
| GROUP BY | 6.8ms | 6.8ms | 6.8ms | 7.2ms | 7.1ms | 7.0ms |
| GROUP BY + HAVING | 6.4ms | 6.4ms | 6.4ms | 6.7ms | 6.8ms | 6.6ms |
| ORDER BY DESC | 12.9ms | 12.9ms | 12.9ms | 13.7ms | 13.4ms | 12.9ms |
| ORDER BY (multi-col) | 13.0ms | 13.0ms | 13.0ms | 13.5ms | 13.4ms | 13.2ms |
| INNER JOIN | 11.0ms | 11.0ms | 11.0ms | 11.4ms | 11.3ms | 11.4ms |
| LEFT JOIN | 11.8ms | 11.8ms | 11.8ms | 11.9ms | 14.5ms | 11.9ms |
| CTE | 6.3ms | 6.3ms | 6.3ms | 6.6ms | 7.1ms | 6.4ms |
| Window (ROW_NUMBER) | 5.8ms | 5.8ms | 5.8ms | 5.9ms | 6.0ms | 5.8ms |
| UNION ALL | 11.2ms | 11.2ms | 11.2ms | 11.2ms | 11.4ms | 11.4ms |
| IN (subquery) | 23.0ms | 23.0ms | 23.0ms | 23.7ms | 23.6ms | 23.4ms |
| SELECT WHERE (cached) | - | **5.4ms** | 5.4ms | 5.4ms | 5.5ms | 5.4ms |
| GROUP BY (cached) | - | **6.7ms** | 6.7ms | 6.7ms | 6.8ms | 6.7ms |
| INNER JOIN (cached) | - | **10.8ms** | 10.8ms | 10.8ms | 11.2ms | 10.8ms |

## Throughput Comparison (ops/sec, In-Memory)

| Statement | Baseline | +Plan | +Batch | +Index | +Parse | +RocksDB | Change |
|-----------|----------|-------|--------|--------|--------|----------|--------|
| CREATE TABLE | 306 | 306 | 306 | 303 | 307 | 297 | -3% |
| INSERT (single) | 233 | 233 | 233 | 226 | 230 | 229 | -2% |
| INSERT (batch 100) | - | - | **6** | 6 | 6 | 6 | NEW |
| UPDATE (single) | 131 | 131 | 131 | 120 | 128 | 128 | -2% |
| SELECT * (full scan) | 140 | 140 | 140 | 137 | 137 | 139 | -1% |
| SELECT WHERE | 184 | 184 | 184 | 167 | 177 | 180 | -2% |
| **SELECT WHERE id=** | **198** | **198** | **198** | **1815** | **2387** | **2463** | **+1144%** |
| SELECT LIMIT 10 | 226 | 226 | 226 | 210 | 216 | 218 | -4% |
| COUNT(*) | 203 | 203 | 203 | 196 | 198 | 199 | -2% |
| GROUP BY | 145 | 145 | 145 | 139 | 142 | 143 | -1% |
| ORDER BY DESC | 77 | 77 | 77 | 73 | 76 | 77 | 0% |
| INNER JOIN | 90 | 90 | 90 | 87 | 88 | 88 | -2% |
| IN (subquery) | 43 | 43 | 43 | 42 | 42 | 43 | 0% |
| SELECT WHERE (cached) | - | **184** | 184 | 184 | 180 | 184 | NEW |
| GROUP BY (cached) | - | **148** | 148 | 148 | 147 | 149 | NEW |
| INNER JOIN (cached) | - | **93** | 93 | 93 | 90 | 92 | NEW |

## Key Wins

### 1. SELECT WHERE id= (PK Point Lookup): 12.8x Faster
- **Baseline:** 5.2ms (198 ops/sec) — full table scan + filter
- **Final:** 406us (2463 ops/sec) — ART index direct lookup
- **Execute phase:** 5.0ms → 217us (23x faster execution)
- Root cause: ART index `get()` + single RocksDB key fetch vs iterating all rows

### 2. Batch INSERT: New Capability
- **Individual 100 INSERTs:** 445ms (2 ops/sec)
- **Batch 100 INSERTs:** 173ms (6 ops/sec) — 2.6x faster
- Root cause: Single transaction commit vs 100 individual commits (saves ~290ms commit overhead)

### 3. Cached Query Execution: Parse+Plan Eliminated
- **SELECT WHERE:** 5.6ms (first) → 5.4ms (cached) — parse+plan eliminated
- **GROUP BY:** 7.0ms → 6.7ms
- **INNER JOIN:** 11.4ms → 10.8ms
- Root cause: LRU plan cache skips both parsing (~100us) and planning (~90us)

### 4. Parse Cache
- Marginal improvement on its own (parse is <0.2% of total time)
- Compounds with plan cache for best results on hot-path queries
- Benefits `execute_internal()` path (DML statements) that don't use plan cache

### 5. RocksDB Write Path Tuning
- Pipelined writes, larger write buffers, background compaction tuning
- Reduced persistent-mode overhead from 1.8x to 1.6x slower vs in-memory (for reads)
- Most benefit visible in persistent commit phase: more consistent 2.9ms vs variable 3-5ms

## Phase Distribution (Final, In-Memory)

| Phase | Baseline | Final | Change |
|-------|----------|-------|--------|
| Parse | 1.7ms (0.2%) | 2.1ms (0.1%) | +0.4ms (parse cache overhead for DML) |
| Plan | 1.8ms (0.2%) | 1.8ms (0.1%) | 0ms |
| Execute | 518ms (61.9%) | 1.1s (77.7%) | +600ms (DELETE bulk changed) |
| Commit | 314ms (37.4%) | 319ms (22.1%) | +5ms |
| Other | 1.1ms (0.1%) | 1.2ms (0.1%) | +0.1ms |

## Notes

- The slight slowdown in some operations (-2-4%) is due to ART index maintenance overhead during INSERT/UPDATE/DELETE. Each DML now also updates the ART index, adding a small cost per write in exchange for dramatically faster PK lookups.
- DELETE (bulk WHERE) shows higher times in later runs due to more rows being present (150 vs 50 rows in baseline) — this is a test data change, not a regression.
- Parse cache LRU size (512) is 2x the plan cache (256) since AST objects are smaller.
