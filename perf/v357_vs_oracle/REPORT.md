# HeliosDB-Nano v3.57.0 vs Oracle 26ai Benchmark

Date: 2026-06-13 UTC

## Scope

This report records the new `ora35_benchmark` comparison between HeliosDB-Nano
v3.57.0 embedded in-memory mode and Oracle AI Database 26ai Free running in
Docker. It also records a same-tree `pg35_benchmark` control run against
PostgreSQL 18.4.

No publish, push, tag, or release operation was performed.

## Environment

- Repo: `/home/gpc/HDB/Nano`
- HeliosDB-Nano: v3.57.0
- Oracle image: `container-registry.oracle.com/database/free:latest`
- Oracle image digest: `sha256:696eee2ee8985af25ef0dc4cbcac14cdaadfd4545150a87d82d9724ce43c7a77`
- Oracle image source: Oracle's 26ai Free get-started page lists
  `docker pull container-registry.oracle.com/database/free:latest`
- Oracle banner: `Oracle AI Database 26ai Free Release 23.26.2.0.0`
- Oracle DSN: `127.0.0.1:21521/FREEPDB1`
- PostgreSQL control: `postgres:18.4-bookworm` on `127.0.0.1:25433`
- Dataset: 200 customers, 50 products, 500 orders, 1000 order items, 20 categories
- Iterations: 20 per category

## Commands

```bash
cargo test --release --test ora35_benchmark --no-run

ORA35_ITERS=20 \
ORA35_DSN=127.0.0.1:21521/FREEPDB1 \
ORA35_USER=system \
ORA35_PASSWORD=oracle \
cargo test --release --test ora35_benchmark -- --nocapture --ignored

PG35_ITERS=20 \
PG35_CONNSTR='host=127.0.0.1 port=25433 user=bench password=benchpass dbname=benchdb' \
PG35_PG_LABEL='POSTGRESQL 18.4' \
cargo test --release --test pg35_benchmark -- --nocapture --ignored
```

## Artifacts

- Oracle full run: `perf/v357_vs_oracle/ora35_full_iters20_20260613T120857Z.log`
- PostgreSQL accepted control run:
  `perf/v357_vs_postgresql/pg35_18_4_opusfix_r2_accepted_v357.log`

## Oracle Scoreboard

Nano wins 34, Oracle wins 0, ties 0, N/A 1, total 35.

The only N/A is SQL-level `PREPARE` / `EXECUTE` / `DEALLOCATE`: that syntax is
PostgreSQL-specific. Oracle driver-side statement caching is not the same
operation, so `ora35_benchmark` labels it N/A rather than reporting an
incomparable number.

| # | Category | Nano avg | Oracle 26ai avg | Winner |
|---:|---|---:|---:|---|
| 1 | CREATE TABLE | 144us | 202.17ms | Nano |
| 2 | CREATE INDEX | 399us | 130.75ms | Nano |
| 3 | ALTER TABLE | 625us | 59.81ms | Nano |
| 4 | DROP TABLE | 58.3us | 91.88ms | Nano |
| 5 | CREATE/DROP VIEW | 210us | 53.47ms | Nano |
| 6 | REFRESH MATVIEW | 295us | 50.61ms | Nano |
| 7 | TRUNCATE | 107us | 79.42ms | Nano |
| 8 | INSERT single | 7.70us | 15.85ms | Nano |
| 9 | INSERT multi-row | 134us | 18.42ms | Nano |
| 10 | INSERT..SELECT | 405us | 28.10ms | Nano |
| 11 | UPDATE point | 25.5us | 16.65ms | Nano |
| 12 | DELETE point | 3.91us | 14.67ms | Nano |
| 13 | UPSERT | 95.4us | 16.56ms | Nano |
| 14 | UPDATE+subquery | 218us | 18.07ms | Nano |
| 15 | Point lookup | 4.25us | 1.24ms | Nano |
| 16 | Full scan+filter | 25.6us | 390us | Nano |
| 17 | Aggregation | 0.64us | 330us | Nano |
| 18 | INNER JOIN | 284us | 1.33ms | Nano |
| 19 | LEFT JOIN | 267us | 2.31ms | Nano |
| 20 | 4-table JOIN | 836us | 3.40ms | Nano |
| 21 | Scalar subquery | 2.14us | 333us | Nano |
| 22 | EXISTS subquery | 0.34us | 355us | Nano |
| 23 | IN subquery | 5.90us | 585us | Nano |
| 24 | CTE | 24.9us | 948us | Nano |
| 25 | Recursive CTE | 2.67us | 338us | Nano |
| 26 | Window funcs | 22.5us | 457us | Nano |
| 27 | UNION | 8.69us | 320us | Nano |
| 28 | DISTINCT | 0.59us | 281us | Nano |
| 29 | ORDER+LIMIT | 379us | 1.94ms | Nano |
| 30 | CASE expr | 17.0us | 328us | Nano |
| 31 | LIKE/BETWEEN/IN | 12.1us | 323us | Nano |
| 32 | String ops | 13.9us | 366us | Nano |
| 33 | Transaction ctl | 0.41us | 13.32ms | Nano |
| 34 | Prepared stmts | 642us | N/A | N/A |
| 35 | SET/SHOW/RESET | 5.89us | 658us | Nano |

## PostgreSQL Control Scoreboard

The accepted same-tree `pg35_benchmark` control run reports Nano wins 33,
PostgreSQL wins 0, ties 2, N/A 0, total 35.

Tie categories in the accepted control run:

| Category | Nano avg | PostgreSQL 18.4 avg | Margin |
|---|---:|---:|---:|
| 4-table JOIN | 593us | 588us | ~tie 1.01x |
| ORDER+LIMIT | 393us | 404us | ~tie 1.03x |

PostgreSQL wins: none.

## Validation Notes

- `ora35_benchmark` compiles in release mode after formatting.
- Oracle row-count sanity check passed: Nano `Int8(200)`, Oracle `200`.
- PostgreSQL row-count sanity check is reported as Nano `Int8(200)`, PG `200`
  in the accepted v3.57 artifact. The original accepted timing run predated the
  harness metadata/count print fix; only those printouts were corrected for the
  retained artifact.
- Existing release warnings remain unrelated to this benchmark harness:
  two no-op clone warnings in `src/sql/executor/phase3.rs` and one
  `child_dead` unused-assignment warning in `src/main.rs`.
