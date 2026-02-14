# HeliosDB Nano — Pipeline Performance Baseline Report

**Date:** 2026-02-13
**Test:** `cargo test --test pipeline_performance_test -- --nocapture`
**Hardware:** Linux 5.14.0-611 x86_64

## Phase Distribution Summary

### In-Memory Mode
| Phase | Time | % of Total |
|-------|------|------------|
| Parse | 1.7ms | 0.2% |
| Plan | 1.8ms | 0.2% |
| Execute | 518ms | 61.9% |
| Commit | 314ms | 37.4% |
| Other | 1.1ms | 0.1% |

### Persistent (RocksDB) Mode
| Phase | Time | % of Total |
|-------|------|------------|
| Parse | 1.9ms | 0.2% |
| Plan | 1.9ms | 0.2% |
| Execute | 759ms | 70.2% |
| Commit | 316ms | 29.2% |
| Other | 1.1ms | 0.1% |

## Baseline Throughput (In-Memory)

| Statement | Wall Time | Parse | Plan | Execute | Commit | Rows | ops/sec |
|-----------|-----------|-------|------|---------|--------|------|---------|
| CREATE TABLE | 3.2ms | 65us | 45us | 2.8ms | 280us | 0 | 306 |
| ALTER TABLE ADD COL | 4.2ms | 67us | 44us | 817us | 3.2ms | 0 | 240 |
| DROP TABLE | 4.2ms | 63us | 39us | 1.1ms | 3.0ms | 0 | 236 |
| INSERT (single) | 4.3ms | 61us | 38us | 1.2ms | 3.0ms | 1 | 233 |
| INSERT (bulk 100) | 445ms | - | - | - | - | 100 | 2 |
| UPDATE (single) | 7.7ms | 57us | 48us | 4.3ms | 3.3ms | 1 | 131 |
| UPDATE (bulk WHERE) | 17.3ms | 55us | 39us | 12.3ms | 4.9ms | 180 | 58 |
| DELETE (single) | 8.5ms | 52us | 42us | 5.0ms | 3.3ms | 1 | 118 |
| DELETE (bulk WHERE) | 197ms | 53us | 40us | 194ms | 3.1ms | 50 | 5 |
| SELECT * (full scan) | 7.1ms | 59us | 85us | 6.8ms | 0 | 1000 | 140 |
| SELECT WHERE | 5.5ms | 72us | 91us | 5.3ms | 0 | 180 | 184 |
| SELECT WHERE id= | 5.2ms | 55us | 79us | 5.0ms | 0 | 1 | 198 |
| SELECT LIMIT 10 | 4.4ms | 67us | 79us | 4.3ms | 0 | 10 | 226 |
| SELECT projection+filter | 5.5ms | 75us | 87us | 5.3ms | 0 | 334 | 183 |
| COUNT(*) | 4.9ms | 73us | 96us | 4.7ms | 0 | 1 | 203 |
| AVG/SUM/MIN/MAX | 6.2ms | 134us | 104us | 6.0ms | 0 | 1 | 161 |
| GROUP BY | 6.8ms | 124us | 95us | 6.6ms | 0 | 50 | 145 |
| GROUP BY + HAVING | 6.4ms | 112us | 96us | 6.2ms | 0 | 50 | 155 |
| ORDER BY DESC | 12.9ms | 73us | 93us | 12.8ms | 0 | 1000 | 77 |
| ORDER BY (multi-col) | 13.0ms | 104us | 101us | 12.8ms | 0 | 1000 | 75 |
| INNER JOIN | 11.0ms | 152us | 147us | 10.7ms | 0 | 150 | 90 |
| LEFT JOIN | 11.8ms | 127us | 155us | 11.5ms | 0 | 150 | 85 |
| CTE | 6.3ms | 152us | 107us | 6.0ms | 0 | 1 | 158 |
| Window (ROW_NUMBER) | 5.8ms | 129us | 91us | 5.5ms | 0 | 100 | 175 |
| UNION ALL | 11.2ms | 128us | 146us | 10.9ms | 0 | 1000 | 90 |
| IN (subquery) | 23.0ms | 130us | 155us | 22.7ms | 0 | 200 | 43 |

## Key Bottlenecks

1. **Execution dominates reads** (95-99% of wall time for SELECT)
2. **Commit dominates writes in persistent mode** (44-69% for DDL/DML)
3. **Operator BUILD 4x slower than EXEC** (117ms build vs 29ms exec)
4. **Bulk UPDATE has 6.4x disk penalty** (17ms in-memory vs 110ms persistent)
5. **IN (subquery) is slowest query** (23ms — subquery materialization)

## Improvements Implemented

All 5 planned improvements have been implemented. See [PERFORMANCE_IMPROVEMENTS.md](PERFORMANCE_IMPROVEMENTS.md) for full results.

1. **Plan/operator caching** — LRU cache (256 entries) for SQL → LogicalPlan
2. **Batch commit** — `execute_batch()` wraps N statements in single transaction
3. **Index-based point lookups** — ART index for WHERE pk=value (12.8x faster)
4. **SQL parse cache** — LRU cache (512 entries) for SQL → AST Statement
5. **RocksDB write path optimization** — pipelined writes, buffer tuning
