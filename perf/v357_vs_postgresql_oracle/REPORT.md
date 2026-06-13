# HeliosDB-Nano v3.57.0 vs PostgreSQL 18.4 vs Oracle 26ai

Date: 2026-06-13 UTC

## Scope

This report combines the saved `pg35_benchmark` and `ora35_benchmark` runs for
HeliosDB-Nano v3.57.0, PostgreSQL 18.4, and Oracle AI Database 26ai Free.

All timing values in the summary table are normalized to microseconds (`us`).
The raw logs print a mix of `us` and `ms`; `ms` values were converted by
multiplying by 1000.

The two comparisons were paired benchmark runs, not one simultaneous three-way
run. For that reason, the table keeps both Nano paired measurements:

- `Nano us (Oracle run)` is the Nano timing captured by `ora35_benchmark`.
- `Nano us (PG run)` is the Nano timing captured by `pg35_benchmark`.

## Inputs

- Nano version: v3.57.0
- PostgreSQL version: PostgreSQL 18.4 (`postgres:18.4-bookworm`)
- Oracle version: Oracle AI Database 26ai Free Release 23.26.2.0.0
- Oracle image: `container-registry.oracle.com/database/free:latest`
- Dataset: 200 customers, 50 products, 500 orders, 1000 order items, 20 categories
- Iterations: 20 per category

## Summary

- Oracle comparison: Nano wins 34, Oracle wins 0, N/A 1, total 35.
- PostgreSQL comparison: Nano wins 32, PostgreSQL wins 3, N/A 0, total 35.
- PostgreSQL wins were limited to 4-table JOIN, ORDER+LIMIT, and Prepared stmts.
- Oracle N/A is Prepared stmts because SQL-level `PREPARE` / `EXECUTE` /
  `DEALLOCATE` is PostgreSQL-specific; Oracle driver-side statement caching is
  not the same operation.

## Normalized Table

| # | Category | Nano us (Oracle run) | Oracle 26ai us | Nano vs Oracle | Nano us (PG run) | PostgreSQL 18.4 us | Nano vs PG |
|---:|---|---:|---:|---:|---:|---:|---:|
| 1 | CREATE TABLE | 144 | 202,170 | Nano 1405.86x | 156 | 20,180 | Nano 129.63x |
| 2 | CREATE INDEX | 399 | 130,750 | Nano 327.69x | 280 | 22,720 | Nano 81.25x |
| 3 | ALTER TABLE | 625 | 59,810 | Nano 95.68x | 659 | 20,030 | Nano 30.38x |
| 4 | DROP TABLE | 58.3 | 91,880 | Nano 1576.40x | 61.4 | 7,650 | Nano 124.63x |
| 5 | CREATE/DROP VIEW | 210 | 53,470 | Nano 254.45x | 243 | 17,440 | Nano 71.63x |
| 6 | REFRESH MATVIEW | 295 | 50,610 | Nano 171.37x | 293 | 9,600 | Nano 32.80x |
| 7 | TRUNCATE | 107 | 79,420 | Nano 741.11x | 104 | 53,700 | Nano 517.70x |
| 8 | INSERT single | 7.70 | 15,850 | Nano 2058.98x | 8.56 | 6,240 | Nano 729.12x |
| 9 | INSERT multi-row | 134 | 18,420 | Nano 137.67x | 137 | 11,630 | Nano 85.15x |
| 10 | INSERT..SELECT | 405 | 28,100 | Nano 69.33x | 385 | 6,170 | Nano 16.04x |
| 11 | UPDATE point | 25.5 | 16,650 | Nano 654.17x | 19.8 | 12,970 | Nano 655.55x |
| 12 | DELETE point | 3.91 | 14,670 | Nano 3748.18x | 4.74 | 6,970 | Nano 1470.09x |
| 13 | UPSERT | 95.4 | 16,560 | Nano 173.52x | 96.3 | 8,160 | Nano 84.76x |
| 14 | UPDATE+subquery | 218 | 18,070 | Nano 82.88x | 215 | 8,560 | Nano 39.82x |
| 15 | Point lookup | 4.25 | 1,240 | Nano 292.21x | 3.90 | 270 | Nano 69.38x |
| 16 | Full scan+filter | 25.6 | 390 | Nano 15.24x | 25.2 | 345 | Nano 13.66x |
| 17 | Aggregation | 0.64 | 330 | Nano 514.46x | 0.66 | 358 | Nano 541.53x |
| 18 | INNER JOIN | 284 | 1,330 | Nano 4.66x | 249 | 326 | Nano 1.31x |
| 19 | LEFT JOIN | 267 | 2,310 | Nano 8.67x | 239 | 409 | Nano 1.71x |
| 20 | 4-table JOIN | 836 | 3,400 | Nano 4.06x | 693 | 603 | PG 1.15x |
| 21 | Scalar subquery | 2.14 | 333 | Nano 155.47x | 2.50 | 421 | Nano 168.75x |
| 22 | EXISTS subquery | 0.34 | 355 | Nano 1034.39x | 0.33 | 524 | Nano 1582.52x |
| 23 | IN subquery | 5.90 | 585 | Nano 99.15x | 6.44 | 454 | Nano 70.53x |
| 24 | CTE | 24.9 | 948 | Nano 38.12x | 26.5 | 635 | Nano 23.99x |
| 25 | Recursive CTE | 2.67 | 338 | Nano 126.77x | 3.02 | 405 | Nano 133.86x |
| 26 | Window funcs | 22.5 | 457 | Nano 20.31x | 25.0 | 562 | Nano 22.54x |
| 27 | UNION | 8.69 | 320 | Nano 36.81x | 9.67 | 415 | Nano 42.96x |
| 28 | DISTINCT | 0.59 | 281 | Nano 473.85x | 0.66 | 317 | Nano 478.90x |
| 29 | ORDER+LIMIT | 379 | 1,940 | Nano 5.11x | 412 | 362 | PG 1.14x |
| 30 | CASE expr | 17.0 | 328 | Nano 19.32x | 14.9 | 336 | Nano 22.58x |
| 31 | LIKE/BETWEEN/IN | 12.1 | 323 | Nano 26.66x | 9.75 | 362 | Nano 37.08x |
| 32 | String ops | 13.9 | 366 | Nano 26.40x | 10.7 | 350 | Nano 32.76x |
| 33 | Transaction ctl | 0.41 | 13,320 | Nano 32340.04x | 0.39 | 15,970 | Nano 40640.94x |
| 34 | Prepared stmts | 642 | N/A | N/A | 725 | 683 | PG 1.06x |
| 35 | SET/SHOW/RESET | 5.89 | 658 | Nano 111.76x | 8.33 | 603 | Nano 72.35x |

## Raw Logs

- [Oracle 26ai full run](../v357_vs_oracle/ora35_full_iters20_20260613T120857Z.log)
- [PostgreSQL 18.4 full run](../v357_vs_postgresql/pg35_full_iters20_fixed_20260613T121206Z.log)

## Test Harnesses

- [Oracle benchmark test](../../tests/ora35_benchmark.rs)
- [Oracle python-oracledb helper](../../tests/support/ora35_client.py)
- [PostgreSQL benchmark test](../../tests/pg35_benchmark.rs)

## Reproduction

From `/home/gpc/HDB/Nano-r01`:

```bash
# Oracle 26ai Free
docker run -d --name ora26ai_bench_nano \
  -e ORACLE_PWD=oracle \
  -p 21521:1521 \
  container-registry.oracle.com/database/free:latest

# Wait until healthy
docker inspect --format '{{.State.Health.Status}}' ora26ai_bench_nano

# PostgreSQL 18.4
docker run -d --name codex-pg184-bench \
  -e POSTGRES_USER=bench \
  -e POSTGRES_PASSWORD=benchpass \
  -e POSTGRES_DB=benchdb \
  -p 25433:5432 \
  postgres:18.4-bookworm

# Build Oracle benchmark
cargo test --release --test ora35_benchmark --no-run

# Run Oracle benchmark
ORA35_ITERS=20 \
ORA35_DSN=127.0.0.1:21521/FREEPDB1 \
ORA35_USER=system \
ORA35_PASSWORD=oracle \
cargo test --release --test ora35_benchmark -- --nocapture --ignored

# Run PostgreSQL benchmark
PG35_ITERS=20 \
PG35_CONNSTR='host=127.0.0.1 port=25433 user=bench password=benchpass dbname=benchdb' \
PG35_PG_LABEL='POSTGRESQL 18.4' \
cargo test --release --test pg35_benchmark -- --nocapture --ignored
```

Oracle image reference:
<https://www.oracle.com/database/free/get-started/>

## Notes

- `ora35_benchmark` uses `tests/support/ora35_client.py`, which relies on
  `python-oracledb` thin mode and does not require Oracle Instant Client.
- The PostgreSQL benchmark uses the Rust `tokio-postgres` client.
- Existing compile warnings in the current tree are unrelated to this report:
  no-op clone warnings in `src/sql/executor/phase3.rs` and an unused assignment
  warning in `src/main.rs`.
