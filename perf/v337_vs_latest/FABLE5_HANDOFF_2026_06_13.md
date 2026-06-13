# Fable 5 Handoff - v3.57.0 Release Closure

Date: 2026-06-13 UTC

Workspace: `/home/gpc/HDB/Nano-r01` only. Protected worktree
`/home/gpc/HDB/Nano` must still be stash-protected before the fast-forward
release merge.

## Final Status

Ship is authorized by the user as `v3.57.0`.

The final source preserves the Fable 5 value from the v3.38-v3.50 line plus
the v3.51 big rocks, then adds the release-loop fast paths and INLJ correctness
guards validated on 2026-06-13.

## Included Work

- R3.4 typed batches and columnar sidecars for scan-heavy paths.
- R1.3-p2 group commit and lock-free commit barrier.
- R4.3 MVCC version garbage collection.
- R4.2/R4.4 durable ART/HNSW index snapshots and ordered range scans.
- COUNT/point-lookup fast paths and no-cache-fill behavior for disk random
  reads.
- Indexed nested-loop join for selective indexed equi-joins.
- INLJ correctness guards:
  - branch-active fallback;
  - active transaction / staged-write fallback;
  - type-equivalence fallback for cross-type equi-joins;
  - alias-stamped materialized output schema for predicates such as
    `LEFT JOIN ... WHERE p.id IS NULL`.

## Final Validation

| Check | Result |
|---|---:|
| `cargo build --release --tests` | PASS, warnings only |
| `cargo test --release --test transaction_integration_tests -- --test-threads=1` | 35 passed / 0 failed / 1 ignored |
| `cargo test --release --test crud_tests -- --test-threads=1` | 30 passed / 0 failed |
| join suites (`a5_uuid_join_coercion`, `a6_subquery_antijoin`, `join_hardening_tests`, `lateral_join_test`) | 60 passed / 0 failed |
| cross-type INLJ smoke (`INT` FK to `BIGINT` PK) | default INLJ count 3, `HELIOS_INLJ_OFF=1` count 3 |
| `cargo test --release --lib -- --test-threads=8` | 1896 passed / 0 failed / 3 ignored |
| version-retargeted lib check after `3.57.0` bump | 1896 passed / 0 failed / 3 ignored |
| `cargo update -p heliosdb-nano --offline` | lockfile updated to 3.57.0 |

## Benchmark Artifacts

- v3.37 comparison report: `perf/v337_vs_latest/REPORT.md`
- v3.37 compact metrics: `perf/v337_vs_latest/opusfix_summary.tsv`
- PostgreSQL comparison report: `perf/v351_vs_postgresql/REPORT.md`
- PostgreSQL compact metrics: `perf/v351_vs_postgresql/opusfix_summary.tsv`

Raw benchmark dumps (`*.txt`, `*.log`) are ignored to avoid bloating the
release commit. They remain on disk in this worktree for local inspection.

## Ship Decision Evidence

v3.37 A/B final rounds:

- `ORDER=AB bash /tmp/v337_compare.sh opusfix_r1`
- `ORDER=BA bash /tmp/v337_compare.sh opusfix_r2`

Final interpretation:

- 67 direct parsed metrics.
- 61 rows at-or-above v3.37.
- 6 rows below v3.37, all accepted as sub-threshold noise or explicitly waived.
- Blocking regressions: 0.

PostgreSQL 18.4 PG35 final rounds:

- `pg35_18_4_opusfix_r1.log`: Nano 32, PostgreSQL 0, ties 3.
- `pg35_18_4_opusfix_r2.log`: Nano 33, PostgreSQL 0, ties 2.
- Median: Nano 32, PostgreSQL 0, ties 3.

## Release Target

Commit the current source and selected reports on `integrate/v3.51.0`, then
fast-forward `/home/gpc/HDB/Nano` main and tag `v3.57.0` at the final commit.
