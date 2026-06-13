# v3.57.0 vs PostgreSQL 18.4 Final PG35 Report
Generated: 2026-06-13 UTC
Source: final `/home/gpc/HDB/Nano-r01` v3.57.0 shipping candidate with COUNT/point lookup fast paths, no-cache-fill, indexed nested-loop join, Opus correctness guards, and INLJ alias-stamped output schema fix.
Runs: `pg35_18_4_opusfix_r1.log` and `pg35_18_4_opusfix_r2.log`, 20 iterations per category. Values below are medians across both rounds.

## Scoreboard
| Round | Nano wins | PostgreSQL wins | Ties | N/A | Total |
|---|---:|---:|---:|---:|---:|
| r1 | 32 | 0 | 3 | 0 | 35 |
| r2 | 33 | 0 | 2 | 0 | 35 |
| median | 32 | 0 | 3 | 0 | 35 |

PostgreSQL median wins: none.

## 35 Categories
| # | Category | Nano v3.57.0 median | PostgreSQL 18.4 median | PG/Nano | Winner |
|---:|---|---:|---:|---:|---|
| 1 | CREATE TABLE | 149.50us | 16.50ms | 110.40x | Nano |
| 2 | CREATE INDEX | 278.00us | 17.49ms | 62.91x | Nano |
| 3 | ALTER TABLE | 634.00us | 14.07ms | 22.19x | Nano |
| 4 | DROP TABLE | 57.40us | 8.23ms | 143.38x | Nano |
| 5 | CREATE/DROP VIEW | 211.50us | 18.32ms | 86.60x | Nano |
| 6 | REFRESH MATVIEW | 292.50us | 8.96ms | 30.63x | Nano |
| 7 | TRUNCATE | 104.50us | 55.59ms | 531.91x | Nano |
| 8 | INSERT single | 6.94us | 6.16ms | 886.89x | Nano |
| 9 | INSERT multi-row | 157.50us | 7.66ms | 48.60x | Nano |
| 10 | INSERT..SELECT | 379.00us | 6.68ms | 17.63x | Nano |
| 11 | UPDATE point | 20.95us | 8.64ms | 412.41x | Nano |
| 12 | DELETE point | 3.80us | 7.54ms | 1981.60x | Nano |
| 13 | UPSERT | 93.00us | 10.83ms | 116.45x | Nano |
| 14 | UPDATE+subquery | 196.00us | 7.93ms | 40.46x | Nano |
| 15 | Point lookup | 3.92us | 289.00us | 73.72x | Nano |
| 16 | Full scan+filter | 31.95us | 360.00us | 11.27x | Nano |
| 17 | Aggregation | 0.79us | 352.00us | 445.57x | Nano |
| 18 | INNER JOIN | 277.50us | 323.50us | 1.17x | Nano |
| 19 | LEFT JOIN | 258.00us | 341.00us | 1.32x | Nano |
| 20 | 4-table JOIN | 603.50us | 605.50us | 1.00x | ~tie |
| 21 | Scalar subquery | 2.25us | 417.50us | 185.56x | Nano |
| 22 | EXISTS subquery | 0.31us | 531.50us | 1714.52x | Nano |
| 23 | IN subquery | 5.75us | 460.00us | 80.00x | Nano |
| 24 | CTE | 25.55us | 619.50us | 24.25x | Nano |
| 25 | Recursive CTE | 2.71us | 405.00us | 149.72x | Nano |
| 26 | Window funcs | 22.50us | 560.00us | 24.89x | Nano |
| 27 | UNION | 9.39us | 406.00us | 43.26x | Nano |
| 28 | DISTINCT | 0.58us | 312.00us | 537.93x | Nano |
| 29 | ORDER+LIMIT | 389.50us | 388.00us | 1.00x | ~tie |
| 30 | CASE expr | 17.90us | 368.00us | 20.56x | Nano |
| 31 | LIKE/BETWEEN/IN | 12.10us | 371.00us | 30.66x | Nano |
| 32 | String ops | 14.00us | 351.50us | 25.11x | Nano |
| 33 | Transaction ctl | 0.41us | 7.38ms | 18234.57x | Nano |
| 34 | Prepared stmts | 661.50us | 672.00us | 1.02x | ~tie |
| 35 | SET/SHOW/RESET | 7.27us | 575.50us | 79.16x | Nano |
