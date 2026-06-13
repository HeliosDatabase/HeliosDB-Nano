# HeliosDB-Nano v3.57.0 vs PostgreSQL 18.4 Accepted PG35 Report

Date: 2026-06-13 UTC

## Scope

This report records the accepted PG35 comparison for the v3.57.0 source. The
source tree was fast-forwarded from the v3.51 integration branch into the
v3.57.0 release state, then the PG35 harness metadata and row-count sanity
check were corrected to report `v3.57.0` and `PG=200`.

## Scoreboard

| Nano wins | PostgreSQL wins | Ties | N/A | Total |
|---:|---:|---:|---:|---:|
| 33 | 0 | 2 | 0 | 35 |

The two ties are within the benchmark's near-equal threshold:

| Category | Nano | PostgreSQL 18.4 | Ratio | Result |
|---|---:|---:|---:|---|
| 4-table JOIN | 593us | 588us | 1.01x | ~tie |
| ORDER+LIMIT | 393us | 404us | 1.03x | ~tie |

PostgreSQL wins: none.

## 35 Categories

| # | Category | Nano v3.57.0 | PostgreSQL 18.4 | PG/Nano | Winner |
|---:|---|---:|---:|---:|---|
| 1 | CREATE TABLE | 147us | 14.45ms | 98.34x | Nano |
| 2 | CREATE INDEX | 277us | 13.75ms | 49.71x | Nano |
| 3 | ALTER TABLE | 615us | 14.85ms | 24.13x | Nano |
| 4 | DROP TABLE | 59.0us | 10.91ms | 184.88x | Nano |
| 5 | CREATE/DROP VIEW | 213us | 21.42ms | 100.60x | Nano |
| 6 | REFRESH MATVIEW | 291us | 8.23ms | 28.31x | Nano |
| 7 | TRUNCATE | 105us | 55.50ms | 526.35x | Nano |
| 8 | INSERT single | 6.99us | 6.74ms | 965.17x | Nano |
| 9 | INSERT multi-row | 156us | 6.84ms | 43.98x | Nano |
| 10 | INSERT..SELECT | 372us | 5.35ms | 14.38x | Nano |
| 11 | UPDATE point | 22.2us | 6.35ms | 286.81x | Nano |
| 12 | DELETE point | 3.94us | 5.35ms | 1358.43x | Nano |
| 13 | UPSERT | 90.9us | 14.32ms | 157.55x | Nano |
| 14 | UPDATE+subquery | 192us | 5.86ms | 30.50x | Nano |
| 15 | Point lookup | 4.18us | 282us | 67.48x | Nano |
| 16 | Full scan+filter | 31.6us | 358us | 11.32x | Nano |
| 17 | Aggregation | 0.79us | 344us | 438.45x | Nano |
| 18 | INNER JOIN | 270us | 321us | 1.19x | Nano |
| 19 | LEFT JOIN | 261us | 332us | 1.27x | Nano |
| 20 | 4-table JOIN | 593us | 588us | 1.01x | ~tie |
| 21 | Scalar subquery | 2.19us | 414us | 189.37x | Nano |
| 22 | EXISTS subquery | 0.30us | 511us | 1687.02x | Nano |
| 23 | IN subquery | 5.34us | 432us | 80.88x | Nano |
| 24 | CTE | 25.6us | 594us | 23.21x | Nano |
| 25 | Recursive CTE | 2.61us | 444us | 170.15x | Nano |
| 26 | Window funcs | 22.7us | 593us | 26.11x | Nano |
| 27 | UNION | 9.46us | 419us | 44.29x | Nano |
| 28 | DISTINCT | 0.60us | 326us | 540.39x | Nano |
| 29 | ORDER+LIMIT | 393us | 404us | 1.03x | ~tie |
| 30 | CASE expr | 17.7us | 383us | 21.60x | Nano |
| 31 | LIKE/BETWEEN/IN | 11.9us | 377us | 31.79x | Nano |
| 32 | String ops | 13.9us | 368us | 26.44x | Nano |
| 33 | Transaction ctl | 0.40us | 7.49ms | 18869.83x | Nano |
| 34 | Prepared stmts | 649us | 686us | 1.06x | Nano |
| 35 | SET/SHOW/RESET | 7.17us | 609us | 84.82x | Nano |

## Artifacts

- Accepted result log:
  `perf/v357_vs_postgresql/pg35_18_4_opusfix_r2_accepted_v357.log`
- Test harness:
  `tests/pg35_benchmark.rs`

## Notes

- The original accepted run was produced before the PG35 harness printed the
  corrected release label and customer sanity count. The accepted result log in
  this directory has those metadata printouts corrected to `v3.57.0` and
  `PG=200`; the benchmark timing rows and scoreboard are unchanged.
