# Fable 5 Performance Items - Final v3.57.0 Disposition

Generated: 2026-06-13 UTC

Scope: final v3.37.0 vs v3.57.0 release comparison after Opus INLJ correctness
guards and the alias-stamped INLJ output-schema fix.

There are no blocking regressions for the v3.57.0 ship decision.

## Accepted Below-Baseline Rows

| Suite | Workload | v3.37 | v3.57 | Delta | Disposition |
|---|---:|---:|---:|---:|---|
| disk TPS | `filter_scan(age>50)` | 32.5 ops/s | 30.5 ops/s | -6.15% | Sub-8% noise |
| OLTP | `INNER JOIN mean` | 0.0115ms | 0.0121ms | -4.94% | Sub-8% noise |
| OLTP | `INNER JOIN p50` | 0.0113ms | 0.0118ms | -4.64% | Sub-8% noise |
| OLTP | `INNER JOIN p99` | 0.0146ms | 0.0176ms | -16.81% | User-accepted two-sample tail noise |
| param TPS | `param_execute_many_insert` | 227,875 ops/s | 224,455 ops/s | -1.50% | Sub-8% noise |
| param TPS | `param_execute_many_update` | 243,760 ops/s | 216,694 ops/s | -11.10% | User-accepted R0.2 conflict-validation cost plus two-sample noise; not introduced by final INLJ cycle |

## Fable 5 Follow-Up Notes

- Keep the `param_execute_many_update` row on the post-ship watch list, but it
  is explicitly not blocking v3.57.0.
- OLTP join p99 is a sub-0.02ms tail metric; p50/mean are near tie and PG35
  shows Nano faster or tied on all join categories.
- Raw per-round dumps are intentionally ignored; see `REPORT.md` and
  `opusfix_summary.tsv` for committed benchmark evidence.
