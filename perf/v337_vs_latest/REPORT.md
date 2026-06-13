# v3.37.0 vs v3.57.0 Final Performance Report
Generated: 2026-06-13 UTC
Workspace: `/home/gpc/HDB/Nano-r01` on `integrate/v3.51.0` at `7b281f7` plus the final committed release-candidate worktree changes.
Baseline: `/home/gpc/HDB/Nano-v337`.
Runs: `ORDER=AB bash /tmp/v337_compare.sh opusfix_r1` and `ORDER=BA bash /tmp/v337_compare.sh opusfix_r2`. Values are two-round medians.
Status: ship-authorized. Direct parsed metrics: 67. v3.57 wins/non-regresses on 61 direct rows; 6 rows are below v3.37; 2 are below by more than 8% and accepted by user as R0.2 conflict-validation cost plus two-sample noise, not introduced by the final INLJ correctness cycle.
Validation anchors: lib 1896/0, cross-type INLJ smoke row count `3 == 3`, targeted transaction/CRUD/join suites green, PG35 zero PostgreSQL wins.
Raw per-round benchmark dumps are intentionally not tracked; compact summary data is in `opusfix_summary.tsv`.

## Summary
| Metric set | Count |
|---|---:|
| Direct parsed metrics | 67 |
| v3.51 at-or-above v3.37 | 61 |
| Below v3.37 but within/noise accepted | 6 |
| Blocking regressions | 0 |

## Accepted Below-Baseline Rows
| Metric | v3.37 | v3.51 | Delta | Disposition |
|---|---:|---:|---:|---|
| `disk:filter_scan(age>50)` | 32.5 | 30.5 | -6.15% | Sub-8% noise/measurement variance |
| `oltp:INNER JOIN mean` | 0.0115ms | 0.0121ms | -4.94% | Sub-8% noise/measurement variance |
| `oltp:INNER JOIN p50` | 0.0113ms | 0.0118ms | -4.64% | Sub-8% noise/measurement variance |
| `oltp:INNER JOIN p99` | 0.0146ms | 0.0176ms | -16.81% | Accepted: R0.2 conflict-validation cost + two-sample/noise; not introduced this cycle |
| `param:param_execute_many_insert` | 227,875 | 224,455 | -1.50% | Sub-8% noise/measurement variance |
| `param:param_execute_many_update` | 243,760 | 216,694 | -11.10% | Accepted: R0.2 conflict-validation cost + two-sample/noise; not introduced this cycle |

## Direct Metric Table
| Metric | Unit | v3.37 median | v3.57 median | Delta | Status |
|---|---|---:|---:|---:|---|
| `cache:1 threads` | lookups/s | 1.05M | 1.09M | +4.60% | green |
| `cache:16 threads` | lookups/s | 924,396 | 2.16M | +133.61% | green |
| `cache:4 threads` | lookups/s | 914,978 | 1.28M | +39.79% | green |
| `colscan:agg_no_filter columnar` | us/query | 4.22ms | 1.16ms | +264.58% | green |
| `colscan:agg_no_filter row` | us/query | 16.19ms | 2.04ms | +694.59% | green |
| `colscan:agg_sum_avg columnar` | us/query | 10.60ms | 1.23ms | +761.47% | green |
| `colscan:agg_sum_avg row` | us/query | 16.66ms | 2.66ms | +527.04% | green |
| `colscan:count_distinct_a columnar` | us/query | 6.67ms | 3.39ms | +96.90% | green |
| `colscan:count_distinct_a row` | us/query | 16.23ms | 16.11ms | +0.78% | green |
| `colscan:count_star_filter columnar` | us/query | 2.70ms | 524.45us | +414.29% | green |
| `colscan:count_star_filter row` | us/query | 13.58ms | 1.69ms | +703.98% | green |
| `colscan:filter_eq_e columnar` | us/query | 19.62ms | 4.72ms | +315.60% | green |
| `colscan:filter_eq_e row` | us/query | 20.42ms | 17.49ms | +16.76% | green |
| `colscan:filter_scan columnar` | us/query | 35.07ms | 17.69ms | +98.30% | green |
| `colscan:filter_scan row` | us/query | 38.22ms | 32.67ms | +16.99% | green |
| `colscan:group_by_e columnar` | us/query | 23.84ms | 1.34ms | +1677.16% | green |
| `colscan:group_by_e row` | us/query | 17.44ms | 2.41ms | +625.22% | green |
| `disk:agg_count_sum_avg` | ops/s | 85.0 | 90.5 | +6.47% | green |
| `disk:autocommit_insert` | ops/s | 41,934 | 47,654 | +13.64% | green |
| `disk:bulk_insert_users(txn)` | ops/s | 132,457 | 163,582 | +23.50% | green |
| `disk:delete_by_pk` | ops/s | 92,986 | 138,448 | +48.89% | green |
| `disk:filter_scan(age>50)` | ops/s | 32.5 | 30.5 | -6.15% | green |
| `disk:group_by_status` | ops/s | 52.0 | 490 | +843.27% | green |
| `disk:join_users_orders` | ops/s | 14.0 | 15.0 | +7.14% | green |
| `disk:order_by_limit10` | ops/s | 32.0 | 91.5 | +185.94% | green |
| `disk:point_lookup_hot` | ops/s | 1.42M | 2.83M | +99.68% | green |
| `disk:point_lookup_pk` | ops/s | 225,874 | 233,718 | +3.47% | green |
| `disk:update_by_pk` | ops/s | 56,737 | 71,324 | +25.71% | green |
| `mem:agg_count_sum_avg` | ops/s | 85.0 | 95.0 | +11.76% | green |
| `mem:autocommit_insert` | ops/s | 96,199 | 112,972 | +17.44% | green |
| `mem:bulk_insert_users(txn)` | ops/s | 135,758 | 161,912 | +19.27% | green |
| `mem:delete_by_pk` | ops/s | 142,223 | 247,482 | +74.01% | green |
| `mem:filter_scan(age>50)` | ops/s | 32.5 | 37.0 | +13.85% | green |
| `mem:group_by_status` | ops/s | 49.0 | 465 | +848.98% | green |
| `mem:join_users_orders` | ops/s | 15.0 | 15.5 | +3.33% | green |
| `mem:order_by_limit10` | ops/s | 32.0 | 92.0 | +187.50% | green |
| `mem:point_lookup_hot` | ops/s | 1.43M | 2.84M | +98.77% | green |
| `mem:point_lookup_pk` | ops/s | 279,108 | 291,062 | +4.28% | green |
| `mem:update_by_pk` | ops/s | 108,400 | 168,992 | +55.90% | green |
| `oltp:Batch INSERT (1000 rows)` | ops/s | 162,982 | 213,796 | +31.18% | green |
| `oltp:COUNT(*) (median of 5)` | ops/s | 1.19M | 2.29M | +92.81% | green |
| `oltp:INNER JOIN mean` | ms | 0.0115 | 0.0121 | -4.94% | green |
| `oltp:INNER JOIN p50` | ms | 0.0113 | 0.0118 | -4.64% | green |
| `oltp:INNER JOIN p99` | ms | 0.0146 | 0.0176 | -16.81% | accepted/noise |
| `oltp:INSERT single + commit (median)` | ops/s | 69,108 | 107,097 | +54.97% | green |
| `oltp:PK lookup (hot, median of 100)` | ops/s | 1.20M | 2.56M | +113.31% | green |
| `oltp:Repeated query x100 (cached)` | ops/s | 1.41M | 2.78M | +96.70% | green |
| `param:param_autocommit_insert` | ops/s | 98,530 | 151,594 | +53.86% | green |
| `param:param_bulk_insert(txn)` | ops/s | 153,664 | 200,360 | +30.39% | green |
| `param:param_delete_by_pk` | ops/s | 234,910 | 408,998 | +74.11% | green |
| `param:param_execute_many_delete` | ops/s | 441,787 | 466,635 | +5.62% | green |
| `param:param_execute_many_insert` | ops/s | 227,875 | 224,455 | -1.50% | green |
| `param:param_execute_many_update` | ops/s | 243,760 | 216,694 | -11.10% | accepted/noise |
| `param:param_point_lookup_pk` | ops/s | 351,552 | 368,258 | +4.75% | green |
| `param:param_update_by_pk` | ops/s | 168,860 | 216,236 | +28.06% | green |
| `scan:agg_sum_avg` | us/query | 78.06ms | 8.83ms | +783.94% | green |
| `scan:count_distinct_expr(range)` | us/query | 128.64ms | 179.50us | +71565.91% | green |
| `scan:count_distinct_pk(range)` | us/query | 190.20us | 142.45us | +33.52% | green |
| `scan:count_pk(id IN)` | us/query | 146.30us | 5.60us | +2512.50% | green |
| `scan:count_star(all)` | us/query | 222.65us | 14.80us | +1404.39% | green |
| `scan:count_star(id IN)` | us/query | 153.00us | 87.70us | +74.46% | green |
| `scan:count_star(id range)` | us/query | 149.25us | 4.55us | +3180.22% | green |
| `scan:count_star(id=r)` | us/query | 139.20us | 4.55us | +2959.34% | green |
| `scan:count_star(id>=r)` | us/query | 133.40us | 5.40us | +2370.37% | green |
| `scan:filter_scan(d>50000)` | us/query | 148.94ms | 145.43ms | +2.41% | green |
| `scan:full_scan(SELECT *)` | us/query | 329.48ms | 305.20ms | +7.96% | green |
| `scan:group_by_e` | us/query | 84.60ms | 8.92ms | +847.95% | green |
