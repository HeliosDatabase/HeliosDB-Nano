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
- `Nano us (PG accepted run)` is the Nano timing captured by the accepted
  `pg35_benchmark` r2 result.

## Inputs

- Nano version: v3.57.0
- PostgreSQL version: PostgreSQL 18.4 (`postgres:18.4-bookworm`)
- Oracle version: Oracle AI Database 26ai Free Release 23.26.2.0.0
- Oracle image: `container-registry.oracle.com/database/free:latest`
- Dataset: 200 customers, 50 products, 500 orders, 1000 order items, 20 categories
- Iterations: 20 per category

## Summary

- Oracle comparison: Nano wins 34, Oracle wins 0, N/A 1, total 35.
- PostgreSQL comparison: Nano wins 33, PostgreSQL wins 0, ties 2, N/A 0,
  total 35.
- PostgreSQL wins: none. The two ties are 4-table JOIN and ORDER+LIMIT.
- Oracle N/A is Prepared stmts because SQL-level `PREPARE` / `EXECUTE` /
  `DEALLOCATE` is PostgreSQL-specific; Oracle driver-side statement caching is
  not the same operation.

## Normalized Table

| # | Category | Nano us (Oracle run) | Oracle 26ai us | Nano vs Oracle | Nano us (PG accepted run) | PostgreSQL 18.4 us | Nano vs PG |
|---:|---|---:|---:|---:|---:|---:|---:|
| 1 | CREATE TABLE | 144 | 202,170 | Nano 1405.86x | 147 | 14,450 | Nano 98.34x |
| 2 | CREATE INDEX | 399 | 130,750 | Nano 327.69x | 277 | 13,750 | Nano 49.71x |
| 3 | ALTER TABLE | 625 | 59,810 | Nano 95.68x | 615 | 14,850 | Nano 24.13x |
| 4 | DROP TABLE | 58.3 | 91,880 | Nano 1576.40x | 59.0 | 10,910 | Nano 184.88x |
| 5 | CREATE/DROP VIEW | 210 | 53,470 | Nano 254.45x | 213 | 21,420 | Nano 100.60x |
| 6 | REFRESH MATVIEW | 295 | 50,610 | Nano 171.37x | 291 | 8,230 | Nano 28.31x |
| 7 | TRUNCATE | 107 | 79,420 | Nano 741.11x | 105 | 55,500 | Nano 526.35x |
| 8 | INSERT single | 7.70 | 15,850 | Nano 2058.98x | 6.99 | 6,740 | Nano 965.17x |
| 9 | INSERT multi-row | 134 | 18,420 | Nano 137.67x | 156 | 6,840 | Nano 43.98x |
| 10 | INSERT..SELECT | 405 | 28,100 | Nano 69.33x | 372 | 5,350 | Nano 14.38x |
| 11 | UPDATE point | 25.5 | 16,650 | Nano 654.17x | 22.2 | 6,350 | Nano 286.81x |
| 12 | DELETE point | 3.91 | 14,670 | Nano 3748.18x | 3.94 | 5,350 | Nano 1358.43x |
| 13 | UPSERT | 95.4 | 16,560 | Nano 173.52x | 90.9 | 14,320 | Nano 157.55x |
| 14 | UPDATE+subquery | 218 | 18,070 | Nano 82.88x | 192 | 5,860 | Nano 30.50x |
| 15 | Point lookup | 4.25 | 1,240 | Nano 292.21x | 4.18 | 282 | Nano 67.48x |
| 16 | Full scan+filter | 25.6 | 390 | Nano 15.24x | 31.6 | 358 | Nano 11.32x |
| 17 | Aggregation | 0.64 | 330 | Nano 514.46x | 0.79 | 344 | Nano 438.45x |
| 18 | INNER JOIN | 284 | 1,330 | Nano 4.66x | 270 | 321 | Nano 1.19x |
| 19 | LEFT JOIN | 267 | 2,310 | Nano 8.67x | 261 | 332 | Nano 1.27x |
| 20 | 4-table JOIN | 836 | 3,400 | Nano 4.06x | 593 | 588 | ~tie 1.01x |
| 21 | Scalar subquery | 2.14 | 333 | Nano 155.47x | 2.19 | 414 | Nano 189.37x |
| 22 | EXISTS subquery | 0.34 | 355 | Nano 1034.39x | 0.30 | 511 | Nano 1687.02x |
| 23 | IN subquery | 5.90 | 585 | Nano 99.15x | 5.34 | 432 | Nano 80.88x |
| 24 | CTE | 24.9 | 948 | Nano 38.12x | 25.6 | 594 | Nano 23.21x |
| 25 | Recursive CTE | 2.67 | 338 | Nano 126.77x | 2.61 | 444 | Nano 170.15x |
| 26 | Window funcs | 22.5 | 457 | Nano 20.31x | 22.7 | 593 | Nano 26.11x |
| 27 | UNION | 8.69 | 320 | Nano 36.81x | 9.46 | 419 | Nano 44.29x |
| 28 | DISTINCT | 0.59 | 281 | Nano 473.85x | 0.60 | 326 | Nano 540.39x |
| 29 | ORDER+LIMIT | 379 | 1,940 | Nano 5.11x | 393 | 404 | ~tie 1.03x |
| 30 | CASE expr | 17.0 | 328 | Nano 19.32x | 17.7 | 383 | Nano 21.60x |
| 31 | LIKE/BETWEEN/IN | 12.1 | 323 | Nano 26.66x | 11.9 | 377 | Nano 31.79x |
| 32 | String ops | 13.9 | 366 | Nano 26.40x | 13.9 | 368 | Nano 26.44x |
| 33 | Transaction ctl | 0.41 | 13,320 | Nano 32340.04x | 0.40 | 7,490 | Nano 18869.83x |
| 34 | Prepared stmts | 642 | N/A | N/A | 649 | 686 | Nano 1.06x |
| 35 | SET/SHOW/RESET | 5.89 | 658 | Nano 111.76x | 7.17 | 609 | Nano 84.82x |

## Raw Logs

- [Oracle 26ai full run](../v357_vs_oracle/ora35_full_iters20_20260613T120857Z.log)
- [PostgreSQL 18.4 accepted run](../v357_vs_postgresql/pg35_18_4_opusfix_r2_accepted_v357.log)
- [PostgreSQL 18.4 accepted report](../v357_vs_postgresql/REPORT.md)

## Test Harnesses

- [Oracle benchmark test](../../tests/ora35_benchmark.rs)
- [Oracle python-oracledb helper](../../tests/support/ora35_client.py)
- [PostgreSQL benchmark test](../../tests/pg35_benchmark.rs)

## Reproduction

From `/home/gpc/HDB/Nano`:

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
