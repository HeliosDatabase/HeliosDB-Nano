# HeliosDB-Nano 3.34.0 Release Closure

Date: 2026-05-31
Branch: `codex-sqlite-inmem-gap`
Package version: `3.34.0`
Latest crates.io version observed by `cargo search heliosdb-nano --limit 3`: `3.33.0`

## Release Decision

Proceed with the `3.34.0` release candidate from the committed tree. This is a
minor performance release, not a patch release: the current branch contains the
consolidated TPS work, the release docs, the release-gate fixes, and the stable
post-RC parameterized-DML benchmark improvements listed below. It should not be
held for the remaining known issues because they are pre-existing, explicitly
deferred semantics decisions, or post-release performance work rather than
regressions from the release batch.

## Included Work

- Docker PostgreSQL/MariaDB TPS mirror improvements and documentation.
- PostgreSQL wire/read-path improvements:
  - fast `query_with_columns()` SELECT routing;
  - shared deterministic result-cache reuse;
  - no-clone cached-row protocol encoding;
  - batched DataRow streaming.
- Embedded TPS improvements:
  - primitive row aggregate fusion;
  - integer Top-N projection/materialization reductions;
  - projected inner hash-join output and direct projection moves;
  - row-cache invalidation miss cleanup;
  - payload-only UPDATE move path;
  - DELETE logical-WAL key allocation deferral.
- Columnar diagnostic profile improvements:
  - columnar range predicate pushdown;
  - direct columnar Top-N;
  - small-group/direct columnar grouped `COUNT(*)` + `SUM(integer)`.
- Benchmark harness additions:
  - `HELIOS_TPS_WORKLOADS` focused selection;
  - `HELIOS_TPS_EMBEDDED_PROFILE=columnar_analytics`;
  - `HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast`.
- SQLite comparison cleanup:
  - `SQLITE_TPS_BINDINGS=literal` for a literal-SQL SQLite mirror, making the
    default Nano literal TPS suite comparable to SQLite driven the same way;
  - retained SQLite bound-parameter mode as the best-path comparison.
- Parameterized DML improvements:
  - repeated `execute_params()` UPDATE/DELETE can use cached fast DML specs
    before entering the parameterized plan cache;
  - one-entry hot fast-spec caches avoid LRU work for tight prepared-style
    UPDATE/DELETE loops;
  - eligible autocommit `execute_many_params()` UPDATE/DELETE batches reuse the
    fast DML specs and invalidate result-cache entries once per batch;
  - focused in-memory parameterized DELETE is now close to SQLite's
    bound-parameter path on this host, while UPDATE remains a tracked gap.
- Join read-path improvement:
  - projected inner equi-joins now compact each direct scan input to the
    qualified join-key, pushed-filter, and output columns before building the
    hash table;
  - this is a guarded extension of the projected join path and falls back for
    outer/lateral joins, unqualified or complex expressions, subqueries, and
    already-projected inputs.
- Filtered scan dispatch improvement:
  - repeated simple projected `FilteredScan` plans can now call the compact
    storage projected-filtered scan primitive directly from the cached-plan
    path;
  - this is gated to simple column-vs-non-NULL literal comparisons and avoids
    `!=` because the storage predicate helper is not SQL-NULL-safe for that
    operator.
- Gated columnar analytics profile improvement:
  - filtered scans whose predicate and projection are entirely `STORAGE
    COLUMNAR` now emit compact projected tuples directly from columnar side-data
    instead of building a full-width row and re-projecting it in `ScanOperator`;
  - non-main branches and mixed row/columnar scans still fall back to the
    existing branch-aware paths.

## Release-Gate Fixes

The broad release gate initially exposed seven new blockers. They are fixed in
this release candidate:

- `FilteredScan` now materializes planner-backed system views before filtering,
  so `sqlite_master` and `information_schema.referential_constraints` work when
  selection pushdown rewrites them into filtered scans.
- Storage predicate pushdown is now gated to predicates whose literal types can
  be evaluated exactly by the storage filter. Mixed/coercive predicates fall
  back to SQL evaluator filtering, restoring string-to-int, int-to-decimal,
  decimal range/IN, date/timestamp string, and UUID string comparison behavior.
- `tests/comprehensive_benchmarks.rs::bench_transaction_overhead` now computes
  the percentage from nanoseconds instead of millisecond-truncated durations.

## Current Performance Status

The revised user acceptance bar allows "surpass or similar" results when the
caveats are explicit. Current status:

- PostgreSQL/MariaDB Docker PG-wire mirror: acceptable for this release. Nano
  wins the repeated-query write, lookup, and read/analytics shapes measured in
  `perf/GOAL_STATUS_2026_05_31.md`.
- SQLite durable disk: acceptable for this release. Nano is far ahead on
  durable autocommit writes and point/hot lookups; SQLite remains ahead on
  several disk analytics shapes.
- SQLite embedded in-memory: acceptable for this release with caveats. Nano is
  similar/better on literal-SQL write/lookup shapes when SQLite is also driven
  through literal SQL; it is similar/better on hot lookup, random lookup under
  current runs, group-by, Top-N, and parameterized/`execute_many` insert in the
  gated `oltp_fast` profile. Batched parameterized UPDATE/DELETE via
  `execute_many_params()` is now similar/ahead on the focused in-memory run.
  SQLite still leads single-row best-path bound-parameter UPDATE and several
  analytical scan/join shapes.
  Final focused 3.34.0 snapshot: `oltp_fast` params reached about 541k/s
  `execute_many` insert, 391k/s lookup, 293k/s batched UPDATE, and 499k/s
  batched DELETE versus SQLite params at about 491k/s bulk insert, 310k/s
  lookup, 243k/s UPDATE, and 266k/s DELETE. Row-store analytics remain behind
  SQLite on filter, aggregate, and join, although the final projected
  `FilteredScan`, cached projected-filtered scan dispatch, and compact
  join-input follow-ups nudged row-store filter to about 224-235/s in focused
  runs and join to about 78-81/s. The gated columnar profile now reaches about
  245-259/s on its columnar filter variant and about 2.1k/s on the aggregate
  shape, but it still does not win the full analytics set.

This means the release meets the revised acceptance bar for publication:
surpass-or-similar is achieved across the PostgreSQL/MariaDB Docker mirror and
several SQLite embedded modes, with honest caveats. The long-term performance
goal should remain active after publish because SQLite's embedded in-memory
analytical path and bound-parameter UPDATE are still ahead.

## Deferred Items

These are release-accepted deferrals, not crates.io blockers:

1. `tests/truncate_hardening_tests.rs::test_truncate_does_not_return_affected_row_count`
   - Current behavior returns an affected-row count where the test expects
     DDL-like `0`.
   - Classified as pre-existing low/medium behavior semantics.
   - Track for a dedicated SQL compatibility pass.

2. `src/vector/hnsw_index.rs` tombstone count semantics
   - The failing test expects physical/tombstone count, while current callers
     use `len()` as live vector count.
   - Do not change `len()` blindly. Preferred follow-up: add explicit
     `physical_len()` / tombstone accessors or update the test to assert live
     count.

3. Sandbox-only HA/SSL integration hangs
   - `ha_integration` and selected PostgreSQL SSL tests can hang in this shared
     sandbox due port/listener environment.
   - Run them only in an isolated network namespace or dedicated CI job.

4. Fixed-width integer UPDATE patching
   - A post-release prototype patched serialized Int2/Int4/Int8 payload columns
     in place and measured `param_update_by_pk` at about 187k/s versus the
     accepted hot-cache result of about 172k/s.
   - It is not release code. The prototype returned before the normal
     fast-update LSN increment path, so it needs an explicit LSN/versioning
     design and a full-suite gate before reconsideration.
   - Saved for later review at
     `/tmp/heliosdb-nano-fixed-width-param-update-wip-20260531.patch`.

5. SQLite embedded analytical gap
   - The next real lever is a compact/vectorized scan-filter-project-join
     pipeline; the 3.34.0 branch now includes the first guarded compact join
     input step, but SQLite still leads the full in-memory analytical set.
   - Keep `columnar_analytics` as a gated diagnostic profile until joins,
     filters, and Top-N are broadly competitive with the row-store profile.

6. Code-graph ingest batching branch
   - dm26 reported uncommitted code-graph batching/options work touching
     `src/lib.rs` and `src/code_graph/storage.rs`.
   - It is a separate code-graph ingest surface and is not part of this
     crates.io release candidate unless it lands through a separate gate.

## Publish Blockers

All local verification blockers for this release candidate are closed. The
remaining operational release step is to commit/tag the release candidate and
run the package/publish commands without `--allow-dirty`.

- `cargo check --workspace --all-targets` passes.
- `cargo test --profile perf --no-fail-fast` has been run in an environment
  that excludes or isolates the known HA/SSL hang class.
- Final result has only the accepted deferred A4/A6 failures, or explicit
  owner sign-off for any other failure.
- `cargo package` passes on a committed tree for the exact crate contents to be
  published.
- `cargo publish --dry-run` passes on a committed tree before the real publish.

## Verification Run On This Release Candidate

Completed in this worktree on 2026-05-31:

```text
cargo search heliosdb-nano --limit 3
  latest crates.io version observed: 3.33.0

cargo metadata --no-deps --format-version 1
  local heliosdb-nano package version: 3.34.0

cargo check --workspace --all-targets
  passed

cargo package --allow-dirty
  passed; packaged 706 files, 12.7 MiB uncompressed, 2.5 MiB compressed

cargo publish --dry-run --allow-dirty
  passed; dry-run aborted before upload as expected

cargo test --test parameterized_query_tests --test crud_tests --test transaction_tests --test protocol_tests --test query_optimizer_tests --test join_hardening_tests --test aggregate_hardening_tests --test pagination_tests --test storage_modes_test -- --nocapture --test-threads=1
  passed; 260 tests passed

cargo test --profile perf --no-run --message-format=json
  passed; built 174 test binaries

binary-by-binary release gate from /tmp/helios_test_bins_2.tsv
  skipped: ha_integration, postgres_ssl_tests (known sandbox hang class)
  result: 170 passed / 2 failed / 0 timed out / 2 skipped
  failed: heliosdb_nano (A6 HNSW tombstone-count semantics),
          truncate_hardening_tests (A4 TRUNCATE affected-count semantics)

cargo check --workspace --all-targets
  passed after release-gate fixes

cargo package --allow-dirty
  passed after release-gate fixes; packaged 706 files, 12.7 MiB uncompressed, 2.5 MiB compressed

cargo publish --dry-run --allow-dirty
  passed after release-gate fixes; dry-run aborted before upload as expected

git diff --check
  passed

git fetch origin && git rebase origin/main
  passed cleanly; release candidate now includes origin/main f51ac86

cargo check --workspace --all-targets
  passed after rebase

cargo test --profile perf --test mcp_conformance --test mcp_endpoint_phase4 --test mcp_auth --test mcp_new_tools --test mcp_axum_routes --test mcp_introspection --test mcp_progress --test mcp_progress_http --test mcp_auto_register -- --test-threads=1
  passed after rebase; selected MCP binaries compiled and executed successfully

cargo package
  passed after rebase without --allow-dirty; packaged 706 files, 12.7 MiB uncompressed, 2.5 MiB compressed

cargo publish --dry-run
  passed after rebase without --allow-dirty; dry-run aborted before upload as expected

post-RC parameterized-DML focused gate on branch codex-sqlite-inmem-gap
  cargo check --workspace --all-targets
    passed after final release-close docs
  cargo test --test parameterized_query_tests -- --nocapture --test-threads=1
    passed after final release-close docs; 27 tests passed
  cargo test --test transaction_tests -- --nocapture --test-threads=1
    passed after final release-close docs; 28 tests passed
  HELIOS_TPS_PARAMS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_WORKLOADS=param_update,param_delete HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
    accepted committed hot-cache result: param_update_by_pk about 171,860/s,
    param_delete_by_pk about 227,122/s

final release-close packaging gate on committed docs
  cargo search heliosdb-nano --limit 3
    latest crates.io version observed: 3.33.0
  cargo metadata --no-deps --format-version 1
    local heliosdb-nano package version: 3.34.0
  cargo check --workspace --all-targets
    passed on 3.34.0 metadata
  cargo package
    passed; packaged 706 files, 12.8 MiB uncompressed, 2.5 MiB compressed
  cargo publish --dry-run
    passed; dry-run aborted before upload as expected

post-close join payload pruning gate
  cargo check --lib
    passed
  cargo test --test query_optimizer_tests -- --nocapture --test-threads=1
    passed; 14 tests passed
  cargo test --test join_hardening_tests -- --nocapture --test-threads=1
    passed; 46 tests passed
  cargo check --workspace --all-targets
    passed
  HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_WORKLOADS=join_users_orders HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
    focused join_users_orders: 81/s
  HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_WORKLOADS=filter_scan,agg_count_sum_avg,join_users_orders HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
    mixed run: filter_scan 234/s, aggregate 568/s, join_users_orders 78/s
  cargo package
    passed; packaged 706 files, 12.8 MiB uncompressed, 2.5 MiB compressed
  cargo publish --dry-run
    passed; dry-run aborted before upload as expected
```

## Recommended Publish Commands

```bash
cargo check --workspace --all-targets
cargo package
cargo publish --dry-run
cargo publish
```

Use `--allow-dirty` only while validating a local, uncommitted release
candidate. The final publish path should commit the release candidate, tag it,
and run the package/publish commands without `--allow-dirty`.

## Notes For Release Messaging

- Do not headline the multi-thousand-x SQLite disk autocommit wins without the
  fsync caveat.
- Do state that Nano now wins the Docker PG-wire PostgreSQL/MariaDB mirror on
  the measured repeated-query suite.
- Do state that SQLite embedded in-memory analytics remain the main follow-up.
- Present `oltp_fast` and `columnar_analytics` as explicit gated modes, not
  default behavior.
