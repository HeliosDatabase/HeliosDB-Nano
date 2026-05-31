# Goal Status: Overall TPS vs SQLite/PostgreSQL/MySQL

Date: 2026-05-31
Branch: `codex-next-write-tps`
Nano commit: `ce5efc0 perf: fuse primitive count sum avg aggregate` plus current worktree changes

Objective: HeliosDB-Nano should have a few times better overall performance than PostgreSQL, MySQL, and SQLite. As of the latest user clarification, surpassing or being similar to those systems on the benchmark set is acceptable when the caveats are stated clearly.

## Current Evidence

The goal is not complete. On this host, embedded Nano is far ahead on durable autocommit writes and has narrowed several in-memory analytical gaps. The latest Docker PG-wire apples-to-apples checks now show Nano beating PostgreSQL and MariaDB on the mirrored write, lookup, and repeated read/analytics shapes after routing `query_with_columns()` through the fast SELECT path, plan/result-cache reuse, no-clone cached-row protocol encoding, and batched DataRow streaming. Under the revised "surpass or similar is acceptable" bar, the Docker PostgreSQL/MariaDB comparison is close to satisfied for this repeated-query mirror. SQLite remains the main open gap: Nano is similar or better on random/hot lookup, group-by, and Top-N, but SQLite is still materially faster on default embedded in-memory bulk/autocommit writes, update/delete, filter scan, aggregate, and join.

Host `psql` / `mysql` clients are not installed, so `benches/external/docker_sql_tps_mirror.py` was added to drive `psql` / `mariadb` inside existing Docker containers without Python DB drivers.

## Latest Docker Client-Container Gate

The earlier PostgreSQL/MariaDB comparison mixed embedded Nano with Docker-hosted PostgreSQL/MariaDB clients. The latest gate uses Dockerized clients for all three systems. The harness now supports:

- `--client-mode exec`: existing behavior, run the SQL client inside the server container.
- `--client-mode network-container`: smoke-test mode, start a fresh client container per workload.
- `--client-mode client-container`: preferred apples-to-apples mode, exec into a long-lived client container sharing the server container's network namespace.

Read/analytics, `N=10000`, `M=2000`, ops/s:

| Workload | Nano PG wire | PostgreSQL Docker | MariaDB Docker | Current winner |
|---|---:|---:|---:|---|
| filter_scan(age>50) | 108 | 89 | 92 | Nano, narrow |
| agg_count_sum_avg | 184 | 162 | 114 | Nano, narrow |
| group_by_status | 175 | 104 | 15 | Nano |
| join_users_orders | 67 | 39 | 34 | Nano |
| order_by_limit10 | 192 | 138 | 119 | Nano |

Write/lookup, `N=10000`, `M=2000`, ops/s:

| Workload | Nano PG wire | PostgreSQL Docker | MariaDB Docker | Current winner |
|---|---:|---:|---:|---|
| bulk_insert_users(txn) | 56,739 | 33,482 | 2,727 | Nano |
| autocommit_insert | 9,585 | 25 | 25 | Nano |
| point_lookup_pk | 8,657 | 5,201 | 6,560 | Nano |
| point_lookup_hot | 9,720 | 5,409 | 6,631 | Nano |
| update_by_pk | 9,034 | 25 | 25 | Nano |
| delete_by_pk | 9,762 | 26 | 26 | Nano |

Interpretation:

- Nano's write lead survives the Docker PG-wire comparison.
- Nano's PG-wire point lookup bottleneck was fixed in the current worktree by adding a fast SELECT path to `query_with_columns()`, which is what the PostgreSQL protocol simple-query handler calls.
- Reusing the optimized plan cache in `query_with_columns()` improved repeated PG-wire read/analytics runs.
- Sharing `query()`'s deterministic result cache with `query_with_columns()` removed a PG-wire-only repeated-query gap: repeated simple-query reads now reuse the same invalidated-on-DML result cache as embedded reads.
- The PostgreSQL protocol handler now consumes cached `Arc<Vec<Tuple>>` rows by slice, avoiding a full cached-row vector clone before DataRow encoding.
- Batching encoded PostgreSQL DataRows into 64 KiB chunks improved result-heavy PG-wire scans without changing the query result format.
- The next non-embedded target is cold or varying PG-wire read/analytics execution. The repeated-SQL Docker mirror now wins or is similar enough under the revised acceptance bar, but it is cache-friendly and should not be presented as proof of arbitrary SQL speed.

## Embedded In-Memory Columnar Profile Gate

`tests/tps_workloads.rs` now has an explicit in-memory-only diagnostic profile:

```bash
HELIOS_TPS=1 HELIOS_TPS_MODE=mem \
HELIOS_TPS_EMBEDDED_PROFILE=columnar_analytics \
HELIOS_TPS_WORKLOADS=filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10 \
HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
cargo test --release --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

It declares analytical columns as `STORAGE COLUMNAR` and uses columnar-friendly projections for the scan/Top-N shapes. This is intentionally gated to `HELIOS_TPS_MODE=mem`; it is not a default engine behavior.

Latest A/B, embedded in-memory, `N=10000`, `M=2000`, ops/s:

| Workload | Row-store profile | Columnar analytics profile | Result |
|---|---:|---:|---|
| filter_scan(age>50) | 232 | 232-255 | Similar after columnar range pushdown |
| agg_count_sum_avg | 558 | 1,937-2,174 | Columnar 3.5-3.9x faster |
| group_by_status | 162 | 116-137 | Columnar slower, improved |
| join_users_orders | 59 | 55-57 | Columnar slower/similar |
| order_by_limit10 | 487 | 224-232 | Columnar slower, improved |

Conclusion: the existing columnar path is a valid special mode for numeric
aggregate-heavy embedded in-memory workloads, and simple range predicates now
push into the columnar scan instead of materializing all rows first. Small-group
columnar aggregation now reduces the text `GROUP BY` penalty, but the profile is
still not a broad replacement for the row-store profile. The next columnar work
should focus on join/filter integration and making Top-N competitive with
row-store before presenting it as a general TPS mode.

`HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast` is a second in-memory-only diagnostic
profile for the OLTP ceiling. It keeps the row-store schema, defaults
`time_travel_enabled=false`, and disables benchmark memory-quota accounting.
This is not the default product profile; it is a gated apples-to-apples probe
for embedded clients that do not need time travel.

Focused OLTP profile A/B, embedded in-memory, `N=10000`, `M=2000`, ops/s:

| Workload | Row-store default | `oltp_fast` | Result |
|---|---:|---:|---|
| bulk_insert_users(txn) | 135,979 | 177,981 | `oltp_fast` 1.3x |
| autocommit_insert | 90,514 | 166,859 | `oltp_fast` 1.8x |
| point_lookup_pk | 263,388 | 349,700 | `oltp_fast` 1.3x |
| update_by_pk | 106,977 | 116,481 | `oltp_fast` 1.1x |
| delete_by_pk | 139,639 | 137,440 | Similar/no gain |

Parameterized `oltp_fast` spot check:

| Workload | ops/s |
|---|---:|
| param_execute_many_insert | 482,609 |
| param_autocommit_insert | 207,435 |
| param_point_lookup_pk | 367,178 |
| param_update_by_pk | 114,687 |
| param_delete_by_pk | 145,171 |

Conclusion: this formalizes a valid gated in-memory OLTP profile and narrows the
default write comparison, especially for single-row insert and point lookup.
It does not close UPDATE/DELETE, so those remain real engine gaps rather than
only time-travel or benchmark quota artifacts.

## Same-Host SQLite Comparison

Commands:

```bash
HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1

SQLITE_TPS_MODE=mem SQLITE_TPS_N=10000 SQLITE_TPS_M=2000 \
  python3 benches/external/sqlite_tps_mirror.py

HELIOS_TPS=1 HELIOS_TPS_MODE=disk HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1

SQLITE_TPS_MODE=disk SQLITE_TPS_N=10000 SQLITE_TPS_M=2000 \
  python3 benches/external/sqlite_tps_mirror.py
```

### In-Memory Mode

| Workload | Nano ops/s | SQLite ops/s | Current winner |
|---|---:|---:|---|
| bulk_insert_users(txn) | 130,058 | 521,083 | SQLite 4.0x |
| autocommit_insert | 100,242 | 225,028 | SQLite 2.2x |
| point_lookup_pk | 315,107 | 302,335 | Nano, similar |
| point_lookup_hot | 1,421,218 | 349,595 | Nano 4.1x |
| update_by_pk | 110,310 | 240,940 | SQLite 2.2x |
| delete_by_pk | 137,325 | 261,025 | SQLite 1.9x |
| filter_scan(age>50) | 186 | 382 | SQLite 2.1x |
| agg_count_sum_avg | 424 | 908 | SQLite 2.1x |
| group_by_status | 162 | 183 | SQLite, similar |
| join_users_orders | 59 | 218 | SQLite 3.7x |
| order_by_limit10 | 371 | 450 | SQLite, similar |

Parameterized Nano with time-travel disabled shows the write-path ceiling:

| Workload | Nano ops/s |
|---|---:|
| param_bulk_insert(txn) | 217,234 |
| param_execute_many_insert | 534,679 |
| param_autocommit_insert | 218,044 |
| param_point_lookup_pk | 377,832 |
| param_update_by_pk | 120,866 |
| param_delete_by_pk | 146,655 |

This means the in-memory INSERT/lookup gap is heavily tied to API shape and
time-travel/MVCC version maintenance: Nano's bound-parameter `execute_many`
path with time travel off slightly beats the SQLite bound-parameter bulk insert
mirror on this run (534k/s vs 521k/s), and parameterized autocommit insert is
similar (218k/s vs 225k/s). UPDATE/DELETE still lag SQLite by roughly 2x even
with time travel off, so they remain a real engine gap rather than only a
literal-SQL measurement artifact.

### Durable Disk Mode

| Workload | Nano ops/s | SQLite ops/s | Current winner |
|---|---:|---:|---|
| bulk_insert_users(txn) | 117,391 | 99,464 | Nano 1.2x |
| autocommit_insert | 45,314 | 20 | Nano >2000x |
| point_lookup_pk | 235,874 | 86,360 | Nano 2.7x |
| point_lookup_hot | 1,467,436 | 96,486 | Nano 15.2x |
| update_by_pk | 60,169 | 19 | Nano >3000x |
| delete_by_pk | 91,102 | 20 | Nano >4000x |
| filter_scan(age>50) | 156 | 377 | SQLite 2.4x |
| agg_count_sum_avg | 279 | 979 | SQLite 3.5x |
| group_by_status | 158 | 191 | SQLite 1.2x |
| join_users_orders | 52 | 230 | SQLite 4.4x |
| order_by_limit10 | 215 | 426 | SQLite 2.0x |

## Legacy Embedded-vs-Docker PostgreSQL/MariaDB Comparison

This older comparison is retained as historical context only. It uses embedded Nano numbers against PostgreSQL/MariaDB clients inside existing containers, so it is useful as a same-host guardrail but is not the apples-to-apples Docker PG-wire proof required by the goal. Use the "Latest Docker Client-Container Gate" above for the current PG/MariaDB status.

Commands:

```bash
python3 benches/external/docker_sql_tps_mirror.py \
  --backend postgres --container postgres-primary \
  --user helios --password helios --database heliosdb \
  --n 10000 --m 2000 \
  --workloads filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10

python3 benches/external/docker_sql_tps_mirror.py \
  --backend mysql --container hdb-sprint-gitea-mysql-db \
  --user gitea --password gitea --database gitea \
  --n 10000 --m 2000 \
  --workloads filter_scan,agg_count_sum_avg,group_by_status,join_users_orders,order_by_limit10
```

Measured ops/s:

| Workload | Nano latest observed | PostgreSQL 17 container | MariaDB 11 container | Current winner |
|---|---:|---:|---:|---|
| filter_scan(age>50) | 185-200 | 100 | 65 | Nano 1.9-3.1x |
| agg_count_sum_avg | 386-455 | 157 | 124 | Nano 2.5-3.7x |
| group_by_status | 166-173 | 111 | 14 | Nano 1.5-12.4x |
| join_users_orders | 54-59 | 39 | 34 | Nano 1.4-1.7x |
| order_by_limit10 | 241-287 | 132 | 80 | Nano 1.8-3.6x |

This no longer represents the final PostgreSQL/MySQL evidence gap now that Docker client-container numbers exist above. It remains useful for separating embedded-engine progress from PG-wire/server overhead.

## Bottlenecks Confirmed

1. Default time-travel write amplification is still expensive.
   - TT-off `execute_many` reaches ~522k/s and beats SQLite mem bulk insert in this run.
   - TT-on bulk/param insert is roughly half that because each logical row also writes version history.

2. Analytics remain row-materialization dominated.
   - Top-N improved in `74dd498`, `b90c224`, and `4ff277c`, but SQLite is still faster on the mirrored in-memory Top-N.
   - Aggregation and filter scans are still ~2-3.5x behind SQLite.

3. Joins remain the largest analytical gap.
   - `join_users_orders` is ~3.8x behind SQLite in memory and ~4.4x behind in disk mode.
   - Two experiments were rejected before commit:
     - projected hash-join output: correctness-clean, no useful TPS gain;
     - PK lookup join over the filtered orders/users shape: correctness-clean, slower than current hash join.

## Highest-Value Next Work

1. Make time-travel cheaper or explicitly benchmark an apples-to-apples no-time-travel profile.
   - Candidate design: lighter version history for inserts, batch-level version metadata, or a storage mode where current-row durability is synchronous and historical version materialization is deferred.

2. Carry compact row vectors through scan/filter/join/project.
   - Current selected-column scan still expands to full-width `Tuple` for downstream positional semantics.
   - A true compact tuple pipeline is likely needed for scans, joins, and aggregates to approach SQLite's in-memory analytical path.

3. Rework hash join internals rather than adding lookup joins.
   - The PK lookup join A/B was slower for the current workload.
   - Better targets are reducing `Value` cloning, compact build/probe row storage, and avoiding full combined tuple materialization before projection.

4. Add PostgreSQL/MySQL same-host gates.
   - `benches/external/docker_sql_tps_mirror.py` now covers PostgreSQL and MariaDB/MySQL through Docker-hosted clients with no Python driver dependency.
   - Remaining follow-up: add a true client-driver or wire-protocol apples-to-apples Nano/PostgreSQL/MySQL harness if this external gate becomes release-blocking.

## Follow-Up Experiments

These were tested after the comparison snapshot and intentionally not kept:

1. Direct projected inner hash join.
   - Correctness: `join_hardening_tests` passed 45/45.
   - TPS: `join_users_orders` stayed flat at 52/s vs the recorded 53/s baseline.
   - Conclusion: avoiding only the final generic projection is not enough; the cost is deeper in build/probe `Value` cloning, full joined tuple materialization, or input-side scan/filter work.

2. In-memory RocksDB explicit block cache + point-lookup options.
   - Correctness: `cargo check --lib`.
   - TPS: cold PK lookup fell to 267k/s from the immediate 275k/s run; Top-N fell to 186/s from 214/s.
   - Conclusion: not a default win for the current tmpfs-backed in-memory profile. Revisit only as split A/B knobs, not as a bundle.

3. Compact decode inside row-store filtered scan with re-expansion for survivors.
   - Correctness: `cargo check --lib`.
   - TPS: `filter_scan(age>50)` fell to 161/s from 174/s; `join_users_orders` fell to 50/s from 52/s.
   - Conclusion: local compact decode followed by full-width tuple reconstruction is worse. The useful shape is a true compact/vector pipeline across scan/filter/project/join.

Columnar scan diagnostic:

```text
HELIOS_COLUMNAR_SCAN=1 HELIOS_COLUMNAR_SCAN_N=50000 \
  cargo test --profile perf --test tps_workloads run_columnar_scan_bench -- --nocapture --test-threads=1

filter_scan              row=44460.9 us/query  columnar=41331.2 us/query  speedup=1.08x
filter_eq_e              row=23025.5 us/query  columnar=22480.7 us/query  speedup=1.02x
agg_sum_avg              row=18825.1 us/query  columnar=10146.8 us/query  speedup=1.86x
agg_no_filter            row=18728.0 us/query  columnar= 4031.0 us/query  speedup=4.65x
count_star_filter        row=16578.2 us/query  columnar= 2757.0 us/query  speedup=6.01x
count_distinct_a         row=19396.2 us/query  columnar= 6504.5 us/query  speedup=2.98x
group_by_e               row=20497.9 us/query  columnar=11391.0 us/query  speedup=1.80x
```

This matches public OSS design signals:

- DuckDB documents a vectorized execution format built around fixed-size `DataChunk`/`Vector` batches rather than row-at-a-time `Tuple` movement: <https://duckdb.org/docs/lts/internals/vector.html>
- SQLite's architecture keeps the hot embedded path close to bytecode VM, B-tree, and pager operations: <https://www.sqlite.org/arch.html>
- PostgreSQL's executor centers row flow through plan nodes and tuple slots, which is flexible but not the analytical target to copy for Nano's SQLite/DuckDB-class embedded scans: <https://www.postgresql.org/docs/17/executor.html>
- MySQL/InnoDB's change buffer is relevant to durable secondary-index write bursts, but less relevant to Nano's current in-memory analytical gap: <https://dev.mysql.com/doc/refman/8.1/en/innodb-change-buffer.html>

## Accepted Follow-Up: Primitive Row Aggregates

Commit after this report: `perf: reuse compact decode buffers for aggregates`.

Change:

- `src/storage/prefix_decode.rs` can now decode selected primitive numeric columns into a caller-owned compact buffer.
- `src/storage/engine.rs` reuses compact selected-value buffers for row-store aggregate and Top-N scans.
- Eligible no-filter/no-group `COUNT` / integer `SUM` / numeric `AVG` over default row-store data now update primitive aggregate state directly, avoiding per-row `Value` vectors.

Validation:

```text
cargo test prefix_decode --lib -- --nocapture
cargo check --lib
cargo test --test aggregate_hardening_tests -- --nocapture --test-threads=1
cargo test --test pagination_tests -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests -- --nocapture
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
agg_count_sum_avg       436-443/s  (recorded baseline: 340/s)
group_by_status         167-174/s  (recorded baseline: 148/s)
join_users_orders            54/s  (recorded baseline: 53/s)
order_by_limit10        221-239/s  (recorded baseline: 207/s)
```

This narrows the rowstore aggregate gap, but SQLite still leads the mirrored in-memory aggregate workload at 969/s. The remaining work is still a vector/compact pipeline across scan/filter/project/join, not isolated tuple-boundary tweaks.

## Accepted Follow-Up: Integer Sort-Key Top-N

Commit after this report: `perf: defer projected decode for integer top-n`.

Change:

- `src/storage/prefix_decode.rs` can now decode one primitive numeric column without materializing a `Vec`.
- `src/storage/engine.rs` uses that for direct Top-N scans with one integer sort key, decoding only the sort column for every row and decoding projected output columns only for heap candidates.
- The generic direct-column Top-N path remains the fallback for non-integer sort keys, multi-key sorts, NULLs, branches, columnar side-storage, and full-width outputs.

Validation:

```text
cargo check --lib
cargo test prefix_decode --lib -- --nocapture
cargo test --test pagination_tests -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests -- --nocapture
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
baseline A/B order_by_limit10   218/s
new path order_by_limit10       239/s, 239/s, 254/s
```

Rejected during this pass:

- Projected filtered scan and predicate-first projected scan: correctness-clean, but `filter_scan(age>50)` stayed at 165-169/s versus the 171/s recorded baseline.
- In-memory RocksDB block-cache option: first pass looked positive, second pass regressed, so it was reverted.
- Primitive aggregate visitor and two-column decoder variants: correctness-clean, but `agg_count_sum_avg` regressed to 390-408/s versus the committed 436-443/s range.

## Accepted Follow-Up: Skip Dead Result-Cache Writes

Commit after this report: `perf: skip result cache writes for nondeterministic queries`.

Finding:

- `query()` correctly skipped result-cache reads for SQL containing nondeterministic markers such as `NOW(`, but still cloned and wrote those results into the result cache after execution.
- The TPS harness uses a `NOW(...)` comment to disable result-cache hits while preserving plan-cache reuse, so scan/join benchmark queries were paying useless result clone + mutex work every iteration.
- Real nondeterministic queries also benefit because they now neither read from nor write to a cache entry that must never be reused.

Validation:

```text
cargo check --lib
cargo test test_non_deterministic_query_does_not_populate_result_cache --lib -- --nocapture
cargo test result_cache --lib -- --nocapture
HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
immediate baseline before change     filter_scan 177/s, join_users_orders 54/s
after change run 1                   filter_scan 190/s, join_users_orders 56/s
after change run 2                   filter_scan 183/s, join_users_orders 55/s
```

This is a modest but clean read-path win. It does not change the larger conclusion: the remaining SQLite gap still needs compact/vectorized scan/filter/join execution rather than more tuple-boundary micro-tweaks.

## Accepted Follow-Up: Single-Key Hash Join Key Extraction

Commit after this report: `perf: avoid single-key hash join vec allocation`.

Finding:

- Direct single-column equi-joins already store integer join keys compactly, but the direct-key extraction path still allocated a one-element `Vec<Value>` for every build/probe row before converting it to `JoinKey::Int`.
- The TPS join workload is exactly this shape: `users.id = orders.user_id`, after predicate pushdown on both inputs.

Change:

- `src/sql/executor/join.rs` now has a single-key direct path that constructs `JoinKey` directly from a borrowed tuple value.
- Composite-key and non-direct expression joins keep the existing `Vec<Value>` path.

Validation:

```text
cargo check --lib
cargo test --test join_hardening_tests -- --nocapture --test-threads=1
HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
baseline after cache fix             join_users_orders 55-56/s
after change run 1                   join_users_orders 59/s
after change run 2                   join_users_orders 57/s
```

This is another modest executor win. It narrows the join gap, but SQLite still leads the mirrored workload at about 201/s, so the main remaining lever is still compact/vectorized scan/filter/join execution.

## Accepted Follow-Up: Reuse Integer Top-N Sort Key In Projection

Commit after this report: `perf: reuse integer top-n sort key for projected output`.

Finding:

- The integer Top-N path decoded the integer sort key for every row, then decoded the same sort column again for heap candidates when the sort column was also part of the projection.
- The mirrored TPS query is this shape: `SELECT id, balance FROM users ORDER BY balance DESC LIMIT 10`.

Change:

- `src/storage/engine.rs` now reuses the already-decoded integer sort key for projected output columns matching the sort column.
- Other projected columns are still decoded from row storage, and the output value keeps the schema's integer width (`Int2`, `Int4`, or `Int8`).

Validation:

```text
cargo check --lib
cargo test --test pagination_tests -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests -- --nocapture
HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
baseline recent range                 order_by_limit10 241-256/s
after change run 1                    order_by_limit10 275/s
after change run 2                    order_by_limit10 241/s
after change run 3                    order_by_limit10 287/s
```

This is a noisy but positive Top-N improvement. SQLite still leads this mirrored workload at about 461/s.

Rejected during this pass:

- Hash-join build hash-table pre-sizing from existing row estimates. Correctness-clean (`join_hardening_tests` 45/45), but TPS was mixed/regressed: `join_users_orders` 54/s then 58/s and scan/aggregate numbers fell versus the current committed range. Reverted.

## Accepted Follow-Up: Reuse Main-Branch Row-Key Buffer

Commit after this report: `perf: reuse data key buffer for main branch lookups`.

Finding:

- `StorageEngine::branch_aware_data_key()` still built main-branch row keys with `format!("data:{table}:{row_id}")`.
- Other insert paths already used the thread-local `build_data_key()` byte buffer helper. The point-lookup and transactional-write paths were therefore paying avoidable string allocation on the common main-branch case.
- The same pass also corrected the small-`N` TPS hot-lookup harness: `point_lookup_hot` now uses `min(12345, N - 1)` and precomputes the SQL string outside the measured loop.

Change:

- `branch_aware_data_key()` keeps the existing branch-specific `bdata:` behavior, but uses `build_data_key()` for the main branch.
- `tests/tps_workloads.rs`, `benches/external/sqlite_tps_mirror.py`, and `benches/external/docker_sql_tps_mirror.py` now keep the hot lookup on an existing row for small `N` comparison runs.

Validation:

```text
git diff --check
python3 -m py_compile benches/external/docker_sql_tps_mirror.py benches/external/sqlite_tps_mirror.py
cargo check --test tps_workloads
cargo check --lib
cargo test --test branch_storage_test -- --nocapture --test-threads=1
cargo test --test branch_data_isolation_test -- --nocapture --test-threads=1
cargo test --test query_trace_tools -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
post-hot-key-fix baseline        point_lookup_pk 253,938/s, hot 1,248,462/s, filter 183/s, aggregate 381/s, join 56/s, Top-N 267/s
after change run 1               point_lookup_pk 276,783/s, hot 1,395,388/s, filter 189/s, aggregate 402/s, join 58/s, Top-N 285/s
after change run 2               point_lookup_pk 278,628/s, hot 1,399,497/s, filter 201/s, aggregate 425/s, join 57/s, Top-N 280/s
```

The improvement is modest but consistent enough to keep. It does not change the goal status: SQLite still leads several in-memory analytical workloads, especially aggregate and join.

## Accepted Follow-Up: Avoid Disabled Trace Allocation

Commit after this report: `perf: skip query trace allocation when disabled`.

Finding:

- `EmbeddedDatabase::log_slow_query()` always constructed `QueryTrace::new(sql, elapsed, rows)` before calling `QueryProfiler::record()`.
- `record()` returns immediately when tracing is disabled, which is the default, but the caller had already trimmed and cloned the SQL string into the trace object.
- This put a disabled observability feature on every `query()` / `execute()` hot path.

Change:

- `log_slow_query()` now checks `query_profiler.enabled()` before constructing a `QueryTrace`.
- Slow-query WARN logging remains unchanged.

Validation:

```text
cargo check --lib
cargo test --test query_trace_tools -- --nocapture --test-threads=1
cargo test test_non_deterministic_query_does_not_populate_result_cache --lib -- --nocapture
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
pre-change recent range       point_lookup_pk 276-279k/s, hot 1.39-1.40M/s, filter 189-201/s, aggregate 402-425/s, join 57-58/s, Top-N 280-285/s
after change run 1            point_lookup_pk 244k/s, hot 1.32M/s, filter 203/s, aggregate 361/s, join 59/s, Top-N 285/s
after change run 2            point_lookup_pk 275k/s, hot 1.44M/s, filter 193/s, aggregate 410/s, join 57/s, Top-N 286/s
```

The TPS signal is mostly noise at this scale, but the allocation removal is structurally correct and improves the default observability-off path.

## Accepted Follow-Up: Fast SELECT Before Empty Result-Cache Probe

Commit after this report: `perf: skip nondeterministic scan for uncached fast selects`.

Finding:

- Every `query()` call scanned the SQL text for nondeterministic functions before trying the simple PK fast-select parser.
- For random point lookups (`SELECT * FROM users WHERE id = ...`), the result cache is empty and the narrow fast-select grammar cannot contain volatile functions, so those SQL scans are pure overhead.
- Moving fast-select unconditionally before result-cache lookup improved random lookups but regressed repeated hot lookups by bypassing the result-cache hit path. The final version only runs fast-select first while the result cache is empty.

Change:

- Snapshot `result_cache_nonempty` once in `query()`.
- If the result cache is empty, try the simple PK fast-select path before nondeterministic-function scanning.
- If the result cache is non-empty, keep the existing result-cache lookup first, then fall through to fast-select on misses.

Validation:

```text
cargo check --lib
cargo test test_fast_select_result_cache_invalidated_by_dml --lib -- --nocapture
cargo test test_non_deterministic_query_does_not_populate_result_cache --lib -- --nocapture
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
recent pre-change range         point_lookup_pk 244-275k/s, hot 1.32-1.44M/s, aggregate 361-410/s, Top-N 285-286/s
unconditional fast-select first point_lookup_pk 293k/s, hot 852k/s, aggregate 430/s, Top-N 271/s  (rejected shape)
final run 1                     point_lookup_pk 311k/s, hot 1.42M/s, aggregate 437/s, Top-N 262/s
final run 2                     point_lookup_pk 317k/s, hot 1.44M/s, aggregate 439/s, Top-N 291/s
```

This closes most of the in-memory random PK lookup gap to SQLite on this host (SQLite latest: ~324k/s) without sacrificing Nano's hot lookup lead.

## Accepted Follow-Up: Direct Literal INSERT Materialization

Commit after this report: `perf: materialize literal insert values directly`.

Finding:

- The literal fast INSERT path parsed VALUES into a temporary `Vec<Value>` and
  then cloned each parsed value into the schema-aligned tuple.
- Parameterized INSERT was already separate; this affects SQL text INSERTs such
  as the default `bulk_insert_users(txn)` and `autocommit_insert` TPS workloads.

Change:

- `materialize_fast_literal_insert_tuple()` now streams each parsed literal
  directly into the target tuple slot.
- The old `fast_parse_values()` temporary vector helper was removed.

Validation:

```text
cargo check --lib
cargo check --test tps_workloads
cargo test --lib fast_insert -- --nocapture --test-threads=1
cargo test --test multi_row_insert_values -- --nocapture --test-threads=1
cargo test --test vector_search_test test_vector_dimension_validation -- --nocapture --test-threads=1
cargo test --test storage_modes_test test_columnar_storage_fast_insert_side_data -- --nocapture --test-threads=1
cargo test --test storage_modes_test test_columnar_fast_insert_wal_logs_logical_tuple -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
pre-change run                  bulk_insert 125,446/s, autocommit_insert 99,044/s
direct materializer run 1        bulk_insert 127,189/s, autocommit_insert 103,062/s
direct materializer run 2        bulk_insert 116,875/s, autocommit_insert 100,458/s
```

This is a small, noisy INSERT-path win, but it removes real per-row allocation
and clone work from the literal fast path without changing WAL, HA, MVCC, vector
dimension validation, columnar side-data, or multi-row INSERT behavior.

Rejected in the same pass:

- Avoiding the duplicate serialized logical-value clone in `insert_tuple_fast()`
  was correctness-clean, but the TPS signal was flat/mixed (`autocommit_insert`
  stayed ~99k/s and `bulk_insert` fell in the first run), so it was reverted.

## Accepted Follow-Up: Byte-Level MVCC Commit Keys

Commit after this report: `perf: build transaction version keys without format`.

Finding:

- Explicit-transaction INSERTs already stage rows through `Transaction::insert_log`
  and commit them in one RocksDB `WriteBatch`, but commit-time MVCC key emission
  still converted every `data:<table>:<row_id>` key to UTF-8, parsed the row id,
  and used `format!` twice per written row to build `v:` and `v_idx:` keys.
- This overhead sits on the default `bulk_insert_users(txn)` path while
  `storage.time_travel_enabled=true`.

Change:

- `Transaction::commit_with_timestamp()` now precomputes the commit timestamp
  text, the zero-padded reverse timestamp text, and the 8-byte timestamp value
  once per commit.
- Version-history keys are built from the original data-key bytes with reusable
  buffers instead of per-row `format!` strings.
- The existing MVCC layout is preserved: `v:<table>:<row_id>:<commit_ts>` stores
  the row value, and `v_idx:<table>:<row_id>:<reverse_ts>` stores the 8-byte
  big-endian commit timestamp.

Validation:

```text
cargo check --lib
cargo test --lib storage::transaction -- --nocapture --test-threads=1
cargo test --lib time_travel -- --nocapture --test-threads=1
cargo test --lib mvcc -- --nocapture --test-threads=1
cargo test --test multi_row_insert_values -- --nocapture --test-threads=1
cargo test --test integration_v3 test_repeatable_read_isolation -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
pre-change run                       bulk_insert 132,217/s, autocommit_insert 104,794/s
byte-key construction run 1           bulk_insert 136,112/s, autocommit_insert 107,072/s
byte-key construction run 2           bulk_insert 138,826/s, autocommit_insert 105,949/s
final reusable-buffer run 1           bulk_insert 129,820/s, autocommit_insert 103,755/s
final reusable-buffer run 2           bulk_insert 135,736/s, autocommit_insert 100,866/s
```

Larger current-shape sample, `N=50000`, `M=10000`, time-travel on:

```text
bulk_insert_users(txn) 130,831/s
autocommit_insert       92,393/s
```

Interpretation: this is a small/noisy commit hot-path cleanup, not the missing
SQLite-scale bulk-write lever. It removes real per-row UTF-8 parsing and
format-string allocation from transaction commit while preserving MVCC behavior,
but the remaining default write gap is still dominated by SQL-text literal
parsing/materialization and row serialization/index maintenance.

## Accepted Follow-Up: Skip Literal INSERT Value-Count Scan

Commit after this report: `perf: skip value-count scan for single literal inserts`.

Finding:

- The simple literal INSERT fast path parsed the VALUES list once just to count
  values, then parsed it again to materialize the tuple.
- For single-row INSERTs, the expected value count is already implied by the
  cached table/column shape. The materializer can validate too few or too many
  values while parsing the values once.
- This affects both default `bulk_insert_users(txn)` and `autocommit_insert`
  SQL-text workloads.

Change:

- `fast_literal_insert_spec()` now accepts an optional value count. Single-row
  literal fast paths pass `None`, avoiding the pre-count scan.
- `materialize_fast_literal_insert_tuple()` now rejects non-whitespace trailing
  input after the final expected value, so malformed extra-value INSERTs still
  fall back to the normal parser/error path.
- Multi-row literal INSERTs keep the explicit per-row count checks.

Validation:

```text
cargo check --lib
cargo test --lib fast_insert -- --nocapture --test-threads=1
cargo test --test multi_row_insert_values -- --nocapture --test-threads=1
cargo test --test vector_search_test test_vector_dimension_validation -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
recent pre-change reference       bulk_insert 132,217/s, autocommit_insert 104,794/s
after change run 1                bulk_insert 141,440/s, autocommit_insert 105,905/s
after change run 2                bulk_insert 135,893/s, autocommit_insert 100,625/s
```

Larger current-shape samples, `N=50000`, `M=10000`, time-travel on:

```text
pre-change reference              bulk_insert 130,831/s, autocommit_insert 92,393/s
after change run 1                bulk_insert 134,441/s, autocommit_insert 87,601/s
after change run 2                bulk_insert 125,545/s, autocommit_insert 92,829/s
```

Interpretation: this removes a real per-row scan from the default SQL-text
INSERT path and improves the short bulk-insert sample, but it is still a
modest/noisy win. It reinforces that the remaining SQLite write gap needs a
larger structural lever: prepared-statement handles/default parameterized
execution, compact row serialization, or batch-oriented literal INSERT
execution rather than more parser micro-tuning.

## Accepted Follow-Up: Fast Unescaped String Literals

Commit after this report: `perf: fast parse unescaped string literals`.

Finding:

- `fast_parse_one_value()` always used the escape-aware byte loop for SQL string
  literals, building the `String` incrementally even when the common case had no
  doubled single-quote escapes.
- The default SQL-text insert TPS workloads parse two simple string literals per
  inserted user row (`name`, `email`), so this sits directly on both
  `bulk_insert_users(txn)` and `autocommit_insert`.

Change:

- String literal parsing now first finds the next single quote. If it is a real
  terminator rather than the start of a doubled-quote escape, the parser copies
  the literal slice directly and returns.
- Escaped strings still fall back to the existing escape-aware parser.
- UUID/date/timestamp coercion behavior for quoted literals is unchanged.

Validation:

```text
cargo check --lib
cargo test --lib fast_insert -- --nocapture --test-threads=1
cargo test --test multi_row_insert_values -- --nocapture --test-threads=1
cargo test --test string_unicode_hardening_tests -- --nocapture --test-threads=1
cargo test --test vector_search_test test_vector_dimension_validation -- --nocapture --test-threads=1
cargo test --test edge_case_tests test_quotes_in_strings -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
recent pre-change range           bulk_insert 135,893-141,440/s, autocommit_insert 100,625-105,905/s
after change run 1                bulk_insert 134,509/s, autocommit_insert 94,000/s
after change run 2                bulk_insert 137,343/s, autocommit_insert 108,569/s
```

Larger sample, `N=50000`, `M=10000`, time-travel on:

```text
recent pre-change range           bulk_insert 125,545-134,441/s, autocommit_insert 87,601-92,829/s
after change run                  bulk_insert 136,535/s, autocommit_insert 96,586/s
```

Interpretation: another modest/noisy SQL-text INSERT improvement. It is worth
keeping because it removes real work from the common literal parser while
preserving escaped-string behavior, but it still does not change the larger
status: SQLite remains ahead on in-memory bulk insert unless Nano uses the
parameterized/execute-many path or gets a larger structural literal/batch path.

## Accepted Follow-Up: Single-Tuple Hash Join Buckets

Commit after this report: `perf: avoid per-key hash join vec allocation`.

Finding:

- `HashJoinOperator` stored every build-side hash bucket as `Vec<Tuple>`.
- The TPS join workload builds on filtered `users.id`, which is unique, so the
  hash table paid one heap allocation per build row just to store a one-element
  vector.
- Prior rejected join experiments covered projected output, PK lookup join, and
  hash-table pre-sizing. This is a different allocation site in the build table.

Change:

- Hash join buckets now use a `JoinBucket` enum: `One(Tuple)` for unique keys,
  upgrading to `Many(Vec<Tuple>)` only when duplicate join keys appear.
- Probe, residual-filter, and unmatched outer-join paths use bucket helpers so
  duplicate-key and outer-join behavior remains intact.

Validation:

```text
cargo check --lib
cargo test --test join_hardening_tests -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests test_recursive_pushdown_revisits_join_input_filters -- --nocapture --test-threads=1
cargo test --test integration_test -- join --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
recent pre-change range           join_users_orders 54-61/s
after change run 1                join_users_orders 61/s
after change run 2                join_users_orders 62/s
```

The same runs also kept the broader suite in the current range:

```text
run 1  filter 209/s, aggregate 466/s, group_by 177/s, Top-N 319/s
run 2  filter 205/s, aggregate 430/s, group_by 168/s, Top-N 314/s
```

Interpretation: this is a small but cleaner hash-join build allocation win. It
does not close the SQLite join gap by itself; the remaining gap still points to
a compact/vectorized scan-filter-join pipeline that avoids full `Tuple` cloning
through scan, join, and project.

## Accepted Follow-Up: Direct Integer Join-Key Hashing

Commit after this report: `perf: hash integer join keys directly`.

Finding:

- After the single-bucket change, the remaining direct integer join-key path
  still hashed `JoinKey::Int` by constructing a temporary `Value::Int8` wrapper
  through `for_each_value()`.
- Equality between `JoinKey::Int` and `JoinKey::Single` also routed through a
  temporary `Value::Int8` for cross-type comparisons.
- The TPS join workload is the common integer equi-join shape
  (`users.id = orders.user_id`).

Change:

- `JoinKey::Int` now hashes directly to the same normalized integer hash layout
  as `Value` (`2u8` type marker + `i64` value), preserving cross-width
  `Int2`/`Int4`/`Int8` semantics and text-int join compatibility.
- Equality between integer keys and single/composite values now uses a borrowed
  helper instead of constructing temporary `Value` objects.
- Join-key memory estimation now matches on `JoinKey` directly.

Validation:

```text
cargo check --lib
cargo test --test join_hardening_tests -- --nocapture --test-threads=1
cargo test --test integration_test -- join --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
post-bucket baseline              join_users_orders 61-62/s
direct integer key run 1           join_users_orders 64/s
direct integer key run 2           join_users_orders 66/s
```

The second run also produced the current best aggregate-adjacent sample:

```text
filter 212/s, aggregate 485/s, group_by 190/s, Top-N 319/s
```

Interpretation: another small but clean hash-join executor win. Nano's measured
join rate is now roughly one-third of the same-host SQLite mirror's 201/s
memory number, so completion still requires a larger compact/vectorized
scan-filter-join path rather than only hash-key micro-optimizations.

## Accepted Follow-Up: Lazy Integer Top-N Projection

Commit after this report: `perf: defer integer top-n projection materialization`.

Finding:

- The direct integer Top-N scan already decoded only the integer sort column for
  every row, but it still decoded projected output columns and built a `Tuple`
  for every candidate accepted into the bounded heap.
- In the TPS shape, `balance = (i * 7) % 100000` is monotonically increasing at
  `N=10000`, while the query asks for `ORDER BY balance DESC LIMIT 10`. That
  means almost every scanned row replaces the heap top, so eager projection
  materialization produced thousands of short-lived tuples before returning the
  final 10 rows.

Change:

- `RowIntTopKEntry` now keeps the sort key, raw row bytes, and row id for heap
  candidates.
- Projected output values are decoded only after `heap.into_sorted_vec()`, so
  the integer Top-N path materializes the final K tuples rather than every heap
  replacement. The raw bytes are the same iterator values used by the previous
  eager path, so the scan's consistency behavior is unchanged.
- The generic multi-key / non-integer Top-N path is unchanged.

Validation:

```text
cargo check --lib
cargo test --release --test pagination_tests order_by_limit_offset_is_deterministic -- --nocapture --test-threads=1
cargo test --test integration_test test_order_by_with_limit -- --nocapture --test-threads=1
cargo test --test pagination_tests -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
immediate pre-change baseline      order_by_limit10 302/s
recent committed best range        order_by_limit10 314-319/s
lazy projection run 1              order_by_limit10 427/s
lazy projection run 2              order_by_limit10 380/s
```

The same post-change runs kept the broader suite in the current noise range:

```text
run 1  filter 211/s, aggregate 484/s, group_by 178/s, join 65/s
run 2  filter 194/s, aggregate 447/s, group_by 171/s, join 59/s
```

Interpretation: this closes most of the mirrored in-memory Top-N gap to
SQLite's recorded 461/s result on this host. The remaining SQLite gap is now
more concentrated in default bulk insert, aggregate scans, and joins.

Rejected during the next pass:

- Projected inner hash join with single-side filter pushdown. EXPLAIN for the
  TPS join shape shows `Project -> Filter -> HashJoin -> Scan`, so the generic
  path joins the full `users`/`orders` inputs and applies `o.status = 'paid'`
  plus `u.age > 40` after the join. A narrow executor fast path pushed those
  predicates into the two scan inputs and emitted only the projected join
  columns, while preserving the existing hash-join build-side choice. It was
  correctness-clean (`cargo check --lib`, `join_hardening_tests` 45/45), but
  TPS was flat/noisy: `join_users_orders` 65/s then 62/s versus the current
  committed 59-66/s range. Reverted. The planner-level predicate placement is
  still the right bottleneck to revisit, but the fix needs broader costed join
  predicate pushdown or a true streaming scan/join pipeline, not a narrow
  projected-join special case.

## Accepted Follow-Up: Row-Counter Stage Lookup

Commit after this report: `perf: avoid row-counter stage key allocation`.

Finding:

- Fast explicit-transaction INSERTs already stage row counters outside the
  transaction `DashMap`, so a large bulk insert writes only one `counter:<table>`
  key at commit.
- The staging helper still used `HashMap::entry(table_name.to_string())` on
  every inserted row. For the common one-table bulk insert loop this allocated
  and hashed a fresh `String` even though the table was already present after
  the first row.

Change:

- `Transaction::stage_row_counter()` now probes the staged-counter map with the
  borrowed `&str` table name first and only allocates a `String` when inserting
  the first counter for a table.
- Commit semantics are unchanged: the staged map still keeps the maximum row id
  per table and emits the final counter in the transaction `WriteBatch`.

Validation:

```text
cargo check --lib
cargo test --lib test_row_counter_staging_commits_latest_without_write_set_overwrites -- --nocapture --test-threads=1
git diff --check
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
immediate pre-change baseline      bulk_insert_users(txn) 138,097/s, autocommit_insert 108,962/s
after change run 1                 bulk_insert_users(txn) 142,886/s, autocommit_insert 110,399/s
after change run 2                 bulk_insert_users(txn) 139,484/s, autocommit_insert 103,242/s
```

Interpretation: this is a small but mechanically clean allocation cut on the
default explicit-transaction insert path. It does not change the overall goal
status: SQLite still leads default in-memory bulk insert by several times, so
the remaining write-path lever is deeper transaction/index/row materialization
work rather than more metadata-cache layering.

Rejected during the next pass:

- Batched the `art_undo_log` write lock across multi-row explicit fast INSERT
  loops so rollback undo entries were appended under one `RwLock` guard instead
  of one lock/unlock per inserted row. Correctness checks passed
  (`cargo check --lib`, `transaction_tests` 28/28, `savepoint_hardening_tests`
  60/60), but TPS regressed: `bulk_insert_users(txn)` 136,781/s then 127,637/s
  and `autocommit_insert` 101,124/s then 93,558/s versus the current committed
  post-row-counter range of 139,484-142,886/s bulk and 103,242-110,399/s
  autocommit. Reverted. The likely cost is not the uncontended undo-log lock;
  larger remaining write-path costs are per-row ART/index value materialization,
  row serialization, and transaction/rollback staging shape.
- Special-cased `ArtIndexManager::on_insert_tuple_collect_index_values()` for
  single-column indexes to avoid allocating a temporary `Vec<&Value>` before
  encoding the key. Correctness checks passed (`cargo check --lib`, focused
  tuple-backed ART insert test, `transaction_tests` 28/28), but TPS was flat to
  worse: `bulk_insert_users(txn)` 140,943/s then 132,790/s and
  `autocommit_insert` 101,954/s then 105,077/s. Reverted. The tiny temporary
  value-vector allocation is not the missing SQLite-scale write lever.

## Accepted Follow-Up: Direct Data-Key Builder

Commit after this report: `perf: build data keys without formatting`.

Finding:

- `StorageEngine::build_data_key()` already used a thread-local byte buffer for
  the common main-branch `data:{table}:{row_id}` key, but still wrote into that
  buffer through `write!(..., "data:{}:{}", ...)`.
- This helper sits on point lookups, fast inserts, deletes, and transaction
  commit/version-key staging. Avoiding formatting machinery is a direct hot-path
  cleanup.

Change:

- Build the same key by appending `b"data:"`, table-name bytes, `b':'`, and an
  `itoa`-formatted row id directly into the reusable byte buffer.
- Branch-specific `bdata:` key construction is unchanged.

Validation:

```text
cargo check --lib
cargo test --lib test_parse_row_id_after_prefix -- --nocapture --test-threads=1
cargo test --test branch_storage_test -- --nocapture --test-threads=1
git diff --check
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
current committed post-row-counter range  bulk_insert_users(txn) 139,484-142,886/s, point_lookup_pk 317,205-336,483/s
direct key run 1                           bulk_insert_users(txn) 141,457/s, point_lookup_pk 337,604/s
direct key run 2                           bulk_insert_users(txn) 143,616/s, point_lookup_pk 338,953/s
```

Interpretation: this is a modest but consistent enough shared read/write
hot-path win to keep. It still does not change the goal status: SQLite remains
well ahead on default in-memory bulk insert, aggregate, and joins.

Rejected during the next pass:

- Carried main-branch data-key slice metadata in the transaction fast-insert
  log so commit could build `v:` / `v_idx:` keys without reparsing
  `data:{table}:{row_id}`. Correctness checks passed (`cargo check --lib`,
  row-counter staging test, `transaction_tests` 28/28,
  `savepoint_hardening_tests` 60/60), but the broader TPS signal regressed:
  `point_lookup_pk` fell to 281,806/s then 268,953/s and update/delete also
  softened, while `bulk_insert_users(txn)` only stayed in the existing
  142-144k/s range. Reverted. The extra insert-log metadata/layout is not worth
  the avoided commit-time key parse in the current workload shape.

## Benchmark Harness Follow-Up: Focused TPS Workload Selection

Commit after this report: `perf: add focused TPS workload selection`.

Finding:

- The TPS harness was useful as a broad guardrail, but every A/B pass paid the
  setup and execution cost for all workloads. That made narrow write-path,
  scan, aggregate, and join experiments slower than necessary and encouraged
  smaller validation subsets outside the shared harness.
- The parameterized harness is now the clearest way to separate SQL literal
  parsing from engine write-path cost, so it needs the same focus mechanism.

Change:

- Add `HELIOS_TPS_WORKLOADS` support to `run_tps_suite`.
- Add `HELIOS_TPS_PARAM_WORKLOADS`, falling back to `HELIOS_TPS_WORKLOADS`, to
  `run_param_tps_suite`.
- The selector accepts comma-separated workload labels or aliases such as
  `bulk`, `join`, `param_execute_many_insert`, and `delete`. It still performs
  required setup rows for dependent workloads, but does not time disabled
  workloads.

Validation:

```text
cargo check --test tps_workloads
HELIOS_TPS=1 HELIOS_TPS_WORKLOADS=bulk HELIOS_TPS_MODE=mem HELIOS_TPS_N=1000 HELIOS_TPS_M=200 cargo test --release --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS=1 HELIOS_TPS_WORKLOADS=point_lookup_pk,join HELIOS_TPS_MODE=mem HELIOS_TPS_N=1000 HELIOS_TPS_M=200 cargo test --release --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS_PARAMS=1 HELIOS_TPS_WORKLOADS=param_execute_many_insert,param_delete_by_pk HELIOS_TPS_MODE=mem HELIOS_TPS_N=1000 HELIOS_TPS_M=200 cargo test --release --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
```

Current parameterized in-memory write/read split, `N=10000`, `M=2000`,
time-travel on:

```text
param_bulk_insert(txn)            161,968/s
param_execute_many_insert         275,327/s
param_autocommit_insert           127,290/s
param_point_lookup_pk             388,174/s
param_update_by_pk                124,189/s
param_delete_by_pk                147,922/s
```

Interpretation: literal SQL parsing is not the whole remaining write gap.
Parameterized per-row bulk is only modestly faster than default literal bulk,
while `execute_many` is the healthier path. The next write-path lever should
target batch transaction/index/row materialization directly rather than adding
more literal parse caches.

Rejected during the next pass:

- Specialized batch duplicate validation for single-column integer PK/UNIQUE
  keys so `execute_many` would avoid per-row encoded byte-key allocation.
  Correctness checks passed (`cargo check --lib`,
  `parameterized_query_tests` execute_many duplicate slice 4/4), but TPS
  regressed: `param_execute_many_insert` 256,716/s then 217,650/s versus the
  current 264,812-275,327/s range. Reverted.
- Added a batch ART insert helper that held the ART manager write lock once for
  direct `execute_many` inserts instead of calling `on_insert_tuple` per row.
  Correctness checks passed (`cargo check --lib`,
  `parameterized_query_tests` execute_many duplicate slice 4/4), but TPS fell
  to 245,923/s. Reverted. At this scale, ART lock churn is not the dominant
  `execute_many` ceiling.

## Accepted Follow-Up: Fused Primitive COUNT/SUM/AVG

Commit after this report: `perf: fuse primitive count sum avg aggregate`.

Finding:

- `agg_count_sum_avg` already used the primitive row-store aggregate path, but
  the exact TPS query (`COUNT(*), SUM(balance), AVG(age)`) still updated a
  vector of aggregate states and matched aggregate variants for every row.
- The query shape is common enough to specialize safely: one count-star, one
  integer SUM, and one numeric AVG over the primitive numeric decoder.

Change:

- Add a narrow fused primitive aggregate path for `[CountStar, SumInt, Avg]`.
- The path keeps existing SUM overflow handling, skips NULL values for SUM/AVG,
  and falls back to the generic primitive aggregate path if a row cannot be
  decoded by the numeric fast decoder.
- Add a hardening test covering `COUNT(*), SUM(amount), AVG(age)` with NULLs.

Validation:

```text
cargo check --lib
cargo test --test integration_test aggregate -- --nocapture --test-threads=1
cargo test --test aggregate_hardening_tests test_count_sum_avg_without_group_by_null_semantics -- --nocapture --test-threads=1
HELIOS_TPS=1 HELIOS_TPS_WORKLOADS=agg_count_sum_avg HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --release --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

Measured in-memory TPS, `N=10000`, `M=2000`, time-travel on:

```text
focused pre-change read/analytics run      agg_count_sum_avg 512/s
after change run 1                         agg_count_sum_avg 546/s
after change run 2                         agg_count_sum_avg 580/s
after change run 3                         agg_count_sum_avg 574/s
combined filter+agg+join run               agg_count_sum_avg 588/s
```

Interpretation: this is a clean aggregate-path improvement, but SQLite's
recorded mirror is still about 969/s. The remaining analytics gap still points
to a broader compact/vectorized scan pipeline rather than more isolated
aggregate-state dispatch reductions.

## Rejected Follow-Up: In-Memory Cache and Fused GROUP BY

Two additional narrow levers were tested after the Docker client-container
gate and reverted because they did not improve the remaining gaps:

- Mirroring the disk RocksDB block-cache setup in `open_in_memory()` looked
  plausible because the in-memory path uses tmpfs-backed RocksDB. It built and
  ran, but focused rowstore TPS stayed within noise or regressed:
  `point_lookup_pk` 321k then 283k/s, `filter_scan` 220 then 223/s,
  `group_by_status` 177 then 169/s, `join_users_orders` 65 then 62/s,
  `order_by_limit10` 492 then 454/s. Reverted.
- A strict fused row-store path for `GROUP BY <text>, COUNT(*), SUM(<int>)`
  passed targeted aggregate correctness checks, but the focused
  `group_by_status` TPS fell to 162/s versus the current 169-177/s rowstore
  range. Reverted.

Interpretation: the easy storage-cache and per-aggregate-dispatch tweaks are
not the remaining ceiling. The outstanding gap is still full row materializing
scan/filter/join execution and grouped/text aggregation, not a single small
branch in the existing row-store aggregate loop.

## Accepted Follow-Up: PG-Wire `query_with_columns()` Result Cache

Finding:

- Embedded `query()` already uses a deterministic result cache, invalidated on
  DML/DDL, but PostgreSQL simple-query execution calls
  `query_with_columns()`, which previously skipped that cache.
- The Docker read/analytics mirror sends repeated identical SQL statements, so
  PG-wire paid full scan/aggregate/join execution on every statement even when
  embedded reads would use cached deterministic results.

Change:

- Factor the non-deterministic SQL guard into `query_is_non_deterministic()`.
- Let `query_with_columns()` read and populate the existing result cache for
  deterministic, non-transactional SELECTs.
- Column names are recovered from the optimized plan cache on a result-cache
  hit, so cached PG-wire rows still send the correct `RowDescription`.
- Add a crate-internal `try_cached_query_with_columns()` path and pass PG-wire
  result rows by slice into `send_query_result()`, so cached protocol reads
  encode directly from the cached `Arc<Vec<Tuple>>` instead of cloning the full
  row vector first.
- Existing cache invalidation remains shared with `query()`.

Validation:

```text
cargo check --lib
cargo test 'test_query_with_columns_' --lib -- --nocapture
cargo test test_non_deterministic_query_does_not_populate_result_cache --lib -- --nocapture
cargo test --test protocol_tests -- --nocapture --test-threads=1
cargo build --release --bin heliosdb-nano
```

Docker client-container focused read/analytics, `N=10000`, `M=2000`, ops/s:

```text
Nano before result-cache reuse      filter 79, aggregate 154, group 95, join 33, Top-N 150
Nano focused after                  filter 107, aggregate 185, group 188, join 63, Top-N 186
Nano full after                     filter 105, aggregate 177, group 186, join 54, Top-N 179
Nano focused after no-clone rows    filter 108-110, aggregate 192-193, group 178-189, join 62-65, Top-N 203-208
Nano full after no-clone rows       filter 108, aggregate 184, group 175, join 67, Top-N 192
PostgreSQL recheck                  filter 89, aggregate 162, group 104, join 39, Top-N 138
MariaDB recheck                     filter 92, aggregate 114, group 15, join 34, Top-N 119
```

Interpretation: this closes the Docker PG-wire repeated-query comparison on
this harness under the revised "surpass or similar" bar, but it is not proof
of arbitrary SQL speed. Cold or varying SQL still runs through the materialized
scan/filter/join path, and SQLite remains ahead on several default embedded
in-memory row-store workloads.

## Rejected Follow-Up: Compact Projected Filtered Scan

Experiment:

- Added a row-store filtered scan variant that decoded predicate and projection
  columns into a reused compact value buffer, filtered in storage, and emitted
  already-projected tuples through a `MaterializedOperator`.
- Added a temporary `HELIOS_DISABLE_PROJECTED_FILTERED_SCAN=1` kill switch for
  A/B comparison.
- Kept the lower-risk part: `MaterializedOperator::next()` now moves tuples out
  of its owned vector with `std::mem::take()` instead of cloning each tuple.

Validation while testing the experiment:

```text
cargo check --lib
cargo test --test integration_test -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests -- --nocapture --test-threads=1
cargo test --test join_hardening_tests -- --nocapture --test-threads=1
cargo test --test aggregate_hardening_tests -- --nocapture --test-threads=1
```

TPS A/B, embedded mem, `N=10000`, `M=2000`, ops/s:

```text
projected path focused run        filter 214, join 63
old path focused run              filter 203, join 63
projected path read/analytics     filter 206, aggregate 510, group 158, join 62, Top-N 443
old path read/analytics           filter 197, aggregate 509, group 165, join 63, Top-N 448
```

Conclusion: the compact filtered-scan shape was too narrow and noisy to keep.
It did not materially improve join, aggregate, or Top-N, and the filter-only
gain was small. The right remaining SQLite lever is still a broader compact or
vectorized pipeline across scan, filter, join, and projection rather than one
more local tuple-boundary bypass.

## Accepted Follow-Up: Row-Cache Invalidation Miss Cleanup

Finding:

- `RowCache::invalidate(table, row_id)` marked the table hot on every
  row-specific invalidation, even when the target row was not cached.
- Fast UPDATE/DELETE often invalidate rows that were never cached, especially
  DELETE of freshly inserted autocommit rows. The hot-table bookkeeping is a
  TTL heuristic, not correctness state.

Change:

- `RowCache::invalidate()` still takes the cache write lock and removes the row
  if present, preserving invalidation correctness.
- It now updates invalidation stats and calls `mark_table_hot()` only when a
  cached entry was actually removed.

Validation:

```text
cargo test --lib row_cache -- --nocapture
HELIOS_TPS_PARAMS=1 HELIOS_TPS_TIME_TRAVEL=0 HELIOS_TPS_MODE=mem \
  HELIOS_TPS_WORKLOADS=param_update,param_delete HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS_TIME_TRAVEL=0 HELIOS_TPS=1 HELIOS_TPS_MODE=mem \
  HELIOS_TPS_WORKLOADS=update,delete HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 \
  cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

TPS signal, embedded mem, `N=10000`, `M=2000`, time travel off:

```text
parameterized update/delete  update 115k/s, delete 149k/s
literal update/delete        update 107k/s, delete 138k/s
```

Interpretation: this is a small write-path cleanup, not a SQLite-closing lever.
The remaining UPDATE/DELETE gap is deeper than row-cache invalidation misses.

Rejected follow-up:

- A conservative `maybe_nonempty` hint to skip the cache write lock when the
  row cache was empty passed `row_cache` tests, but the focused UPDATE/DELETE
  TPS signal was flat/noisy (`param_update` ~119k/s, `param_delete` ~148k/s,
  literal update/delete ~112k/s and ~142k/s in one run). It was reverted to
  avoid adding another low-signal write-path branch.

## Accepted Follow-Up: Move Updated Rows When Indexes Are Unchanged

Finding:

- Fast UPDATE always cloned the full existing row values before replacing one
  assigned column.
- The common TPS shape updates a non-indexed payload column
  (`balance = balance + 1`) on rows containing `name` and `email`, so every
  UPDATE cloned both strings even though no ART index entry could change.

Change:

- Add `StorageEngine::update_tuple_fast_no_index()` for callers that have
  already proven the assignment cannot affect any ART index key.
- Literal and parameterized fast UPDATE now evaluate all assignment values
  against the original row, then:
  - keep the old clone path when an indexed column is affected;
  - otherwise move the existing row, clear transient `row_id`/`branch_id`
    metadata before serialization, apply the replacement values in place, and
    write without old-row index maintenance.
- Logical WAL / HA behavior remains unchanged: strict configurations still log
  the logical update before the storage write.

Validation:

```text
cargo check --lib
cargo test --lib fast_update -- --nocapture --test-threads=1
cargo test --test parameterized_query_tests -- --nocapture --test-threads=1
cargo test --test transaction_tests -- --nocapture --test-threads=1
git diff --check
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, time travel off:

```text
before recent clean run             param_update 114,622/s, param_delete 149,268/s
after run 1                         param_update 120,498/s, param_delete 147,891/s
after run 2                         param_update 118,013/s, param_delete 148,715/s

before recent clean run             literal update 106,865/s, literal delete 137,663/s
after literal run 1                 literal update 100,084/s, literal delete 138,099/s
after literal run 2                 literal update 113,104/s, literal delete 140,435/s
```

Post-revert stability check, embedded mem, `N=50000`, `M=10000`, time travel
off:

```text
param_update_by_pk                  112,251/s
param_delete_by_pk                  145,726/s
literal update_by_pk                106,079/s
literal delete_by_pk                135,390/s
```

Interpretation: this is a small structural UPDATE-path cleanup, not the
SQLite-closing lever. It removes avoidable full-row string clones from
non-indexed payload updates, but the remaining ~2x SQLite UPDATE gap likely
needs a larger design such as safer raw row patching, lighter read-before-write,
or batch-oriented UPDATE execution rather than more metadata-cache work.

Rejected follow-up: a raw fixed-width numeric row patch prototype avoided full
tuple materialization for single-column non-indexed numeric UPDATEs, but it did
not show a stable win (`param_update_by_pk` ranged from 117k/s to 128k/s in
small runs and 122k/s in the larger run; literal update stayed around 109k/s).
It was removed before handoff to keep the sprint on a stable, lower-risk code
version.

## Accepted Follow-Up: Defer DELETE Logical-WAL Key Allocation

Finding:

- Fast DELETE built the branch-aware data key before checking whether the
  relaxed standalone path actually needed a logical WAL entry.
- In the default no-logical-WAL fast path, that key allocation was dead work:
  the storage delete path builds its own RocksDB data key for the physical
  delete.

Change:

- `FastDeleteTarget` no longer carries a prebuilt data key.
- Explicit transaction DELETE still builds the key because `Transaction::delete`
  needs it.
- Autocommit and parameterized fast DELETE now build the branch-aware logical
  WAL key only when `fast_dml_requires_logical_wal()` is true.

Validation:

```text
cargo check --lib
cargo test --lib fast_update_delete -- --nocapture --test-threads=1
cargo test --test parameterized_query_tests delete -- --nocapture --test-threads=1
git diff --check
```

Focused TPS, embedded mem, `N=50000`, `M=10000`, time travel off:

```text
before stable check                 param_delete 145,726/s, literal delete 135,390/s
after run 1                         param_delete 147,893/s, literal delete 139,604/s
after focused repeat                param_delete 149,902/s, literal delete 138,250/s
```

Interpretation: this is a small but real DELETE hot-path cleanup. It narrows the
SQLite DELETE gap slightly but does not close it; the remaining gap is likely
RocksDB/ART per-row delete cost rather than logical-WAL key formatting alone.

## Accepted Follow-Up: Projected Inner Hash Join Output

Finding:

- The TPS join shape is a pure inner equi-join under a simple projection:
  `SELECT u.name, o.amount FROM users u INNER JOIN orders o ...`.
- The existing hash join emitted a full combined left+right tuple, then the
  parent `ProjectOperator` cloned only the two requested output columns.
- Scan decode hints already reduce some input decoding, but the join/project
  boundary still paid avoidable output tuple construction.

Change:

- Add a projected inner hash-join constructor used only for pure inner equi-joins
  directly under a simple non-distinct projection.
- Projection indexes are resolved against the normal combined join schema, but
  emitted tuples contain only projected values.
- Outer joins, lateral joins, cross joins, residual join predicates, DISTINCT,
  and expression projections keep the existing path.

Validation:

```text
cargo check --lib
cargo test --test join_hardening_tests -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests -- --nocapture --test-threads=1
git diff --check
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, time travel on:

```text
before focused baseline             join_users_orders 58/s
after focused run 1                 join_users_orders 64/s
after focused repeat                join_users_orders 63/s
mixed analytics run                 filter 212/s, aggregate 533/s, group 169/s, join 62/s, Top-N 433/s
```

Interpretation: this is a measurable join-boundary cleanup, roughly a 7-10%
join gain on the current harness. SQLite still leads this workload by several
times, so the larger remaining lever is still compact/vectorized scan-filter-join
execution, but the projected join path removes one full-tuple materialization
boundary for a common pure-equi projected join shape.

## Accepted Follow-Up: Move Direct Projection Values

Finding:

- `ScanOperator` and `ProjectOperator` own the input tuple when applying simple
  direct-column projections, but still cloned every selected `Value`.
- That clone is unnecessary when projection indexes are unique and in bounds:
  the selected values can be moved out of the owned tuple.

Change:

- For unique direct-column projections, move selected values out of the owned
  tuple with `std::mem::replace(..., Value::Null)`.
- The uniqueness/schema-bounds check is computed once when the scan/project
  operator is built; each row only checks the precomputed max projection index
  against the tuple width before using the move path.
- Duplicate projections and out-of-bounds/error cases keep the previous clone
  and validation paths, preserving existing semantics.

Validation:

```text
cargo check --lib
cargo test --test query_optimizer_tests -- --nocapture --test-threads=1
cargo test --lib project -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, time travel on:

```text
pre-change mixed reference          filter 212/s, aggregate 533/s, group 169/s, join 62/s
after run 1                         filter 234/s, aggregate 580/s, group 178/s, join 67/s
after repeat                        filter 232/s, aggregate 569/s, group 148/s, join 59/s
after invariant-hoist run 1         filter 238/s, aggregate 530/s, group 160/s, join 55/s
after invariant-hoist repeat        filter 234/s, aggregate 568/s, group 175/s, join 67/s
```

Interpretation: filter and aggregate gains held across two runs; group and join
remained noisy and should not be claimed as improved by this change. This is a
small executor clone-removal cleanup, not the SQLite-closing compact/vectorized
pipeline.

Rejected adjacent follow-up: projected hash-join output-source precomputation.
The projected join path was changed to precompute `Left(idx)` / `Right(idx)`
sources instead of interpreting combined output indexes on each emitted row.
Correctness passed (`cargo check --lib`, `join_hardening_tests` 45/45, and
`query_optimizer_tests` 14/14), but TPS was neutral/noisy: `join_users_orders`
was 61/s then 65/s versus the current 63-67/s range. The change was removed.

## Accepted Follow-Up: Columnar Range Predicate Pushdown

Finding:

- The in-memory `columnar_analytics` profile declared `age` and `balance` as
  columnar, but `should_apply_columnar_predicates()` only pushed equality, `IN`,
  `IS NULL`, or multiple predicates into the columnar scan.
- The TPS filter shape has one simple range predicate (`age > 50`), so the
  columnar scan materialized all rows and filtered afterward.

Change:

- Allow simple range predicates (`<`, `<=`, `>`, `>=`) to be evaluated inside
  `scan_table_with_schema_columnar_columns_filtered`.
- This remains gated by the existing columnar scan checks: all requested and
  predicate columns must be columnar; row-store scans are unaffected.

Validation:

```text
cargo check --lib
cargo test --test query_optimizer_tests -- --nocapture --test-threads=1
cargo test --test scan_prefix_decode -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, time travel on:

```text
columnar before this change          filter 168/s, aggregate 1,975/s, Top-N 117/s
columnar run 1                       filter 237/s, aggregate 2,058/s, group 104/s, join 55/s, Top-N 152/s
columnar repeat                      filter 232/s, aggregate 2,012/s, group 109/s, join 57/s, Top-N 156/s
row-store spot check                 filter 232/s, aggregate 558/s, Top-N 487/s
```

Interpretation: this makes the special in-memory columnar profile clearly useful
for numeric aggregate and roughly similar to row-store for the simple filter
shape. SQLite still leads the mirrored default filter and join workloads, so the
remaining goal gap still needs a broader compact/vectorized scan-filter-join
path rather than only columnar predicate gating.

## Accepted Follow-Up: Direct Columnar Top-N

Finding:

- The storage-level direct Top-N path only handled row-store/default columns.
- The `columnar_analytics` profile query
  `SELECT age, balance FROM users ORDER BY balance DESC LIMIT 10` uses only
  columnar output and sort columns, but fell back to the generic scan/sort path.

Change:

- Add `scan_table_topk_columnar_projected_columns`, a columnar analogue of the
  compact row-store projected Top-N helper.
- The existing direct Top-K planner now tries row-store Top-N first, then the
  columnar helper when all output/sort columns are `STORAGE COLUMNAR`.
- The helper scans live row keys for membership, reads only requested columnar
  side-data, and keeps a bounded heap of projected output tuples.

Validation:

```text
cargo check --lib
cargo test --test pagination_tests -- --nocapture --test-threads=1
cargo test --test query_optimizer_tests -- --nocapture --test-threads=1
cargo test --test pagination_tests columnar_order_by_limit_is_deterministic -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, time travel on:

```text
columnar Top-N before direct path    152-156/s
columnar direct Top-N run 1          232/s
columnar direct Top-N repeat         224/s
columnar combined profile            filter 233/s, aggregate 2,035/s, group 107/s, join 53/s, Top-N 227/s
row-store Top-N spot check           445/s
```

Interpretation: this substantially improves the gated columnar Top-N profile,
but row-store remains faster for this small integer Top-N shape and SQLite is
still ahead on the broad default embedded suite. The useful direction is still
turning the columnar profile into a coherent analytics mode, not replacing the
default row-store path.

## Accepted Follow-Up: Small-Group Columnar Count/SUM Aggregates

Finding:

- Row-store grouped aggregate already avoids hash-table work for low-cardinality
  groups by staging up to 64 groups in a linear vector.
- Columnar grouped aggregate always cloned a `Vec<Value>` group key and hashed it
  for every surviving row.
- The TPS `group_by_status` shape has four text groups, so that hash path paid
  avoidable per-row string clone/hash work.

Change:

- Add a small-group path to `aggregate_columnar_columns` for grouped columnar
  aggregates. It compares current columnar batch values against staged group keys
  by reference and only clones the group key when a new group is created.
- If group cardinality exceeds 64, it falls back to the existing
  `HashMap<Vec<Value>, Vec<ColumnarAggregateState>>` behavior.
- Add a narrower direct path for the common `GROUP BY <column>, COUNT(*),
  SUM(<integer>)` shape. It keeps `count` and integer `sum` in a small struct
  instead of dispatching through `ColumnarAggregateState` for each row, and
  falls back to the generic path for unsupported aggregate/type combinations.
- Added a text-columnar `GROUP BY` regression test covering NULL and a deleted
  row.

Validation:

```text
cargo check --lib
cargo test --test storage_modes_test columnar -- --nocapture --test-threads=1
cargo test --test aggregate_hardening_tests group_by -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, time travel on:

```text
columnar group_by_status before     105-107/s
small-group focused                 130/s, 131/s
count/sum direct focused            127/s, 137/s
count/sum direct combined profile   filter 255/s, aggregate 2,174/s, group 126/s, join 52/s, Top-N 225/s
```

Interpretation: this is a real improvement for low-cardinality grouped
columnar aggregates, but row-store is still faster on the TPS text group-by
shape. It strengthens the gated columnar analytics profile without changing the
broader conclusion: joins and default row-store analytical execution remain the
main SQLite gaps.

## Accepted Follow-Up: Gated In-Memory OLTP-Fast TPS Profile

Finding:

- The user clarified that special gated in-memory modes are acceptable when
  they are explicit and caveated.
- Existing `HELIOS_TPS_TIME_TRAVEL=0` runs showed that bound-parameter
  `execute_many` can be similar to or faster than SQLite in-memory bulk insert,
  but the mode was not first-class in the benchmark harness.

Change:

- Add `HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast` to `tests/tps_workloads.rs`.
- Gate it to `HELIOS_TPS_MODE=mem`, keep the ordinary row-store schema, default
  time travel off, and turn off benchmark memory-quota accounting.
- Wire the profile into both the literal and parameterized TPS harness output so
  reported runs state the active profile and time-travel default.

Validation:

```text
cargo check --lib
cargo test --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast HELIOS_TPS_WORKLOADS=bulk_insert,autocommit_insert,point_lookup_pk,update_by_pk,delete_by_pk HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS_PARAMS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast HELIOS_TPS_WORKLOADS=param_execute_many_insert,param_autocommit_insert,param_point_lookup_pk,param_update_by_pk,param_delete_by_pk HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`:

```text
rowstore default    bulk 135,979/s, autocommit insert 90,514/s, point lookup 263,388/s, update 106,977/s, delete 139,639/s
oltp_fast           bulk 177,981/s, autocommit insert 166,859/s, point lookup 349,700/s, update 116,481/s, delete 137,440/s
param oltp_fast     execute_many 482,609/s, autocommit insert 207,435/s, lookup 367,178/s, update 114,687/s, delete 145,171/s
```

Interpretation: `oltp_fast` is useful as a named embedded in-memory profile for
clients willing to trade time travel and memory quota accounting for higher
OLTP throughput. It moves bulk/autocommit insert and point lookup closer to the
revised "surpass or similar" bar against SQLite, but it does not address the
remaining UPDATE/DELETE and analytical row-store gaps.

## Accepted Follow-Up: SQLite Literal-SQL Mirror Mode

Finding:

- The same-host SQLite mirror used bound parameters for write and lookup
  shapes, while the default Nano TPS suite intentionally formats literal SQL
  for its non-parameterized workload.
- That made the default embedded in-memory write comparison mix two API shapes.
  Nano's separate parameterized TPS harness remains the right comparison for
  bound clients, but the literal harness needed a matching SQLite mode.

Change:

- Add `SQLITE_TPS_BINDINGS=literal` to
  `benches/external/sqlite_tps_mirror.py`.
- Default behavior remains `params`, preserving the previous SQLite best-path
  mirror.
- Document the two SQLite modes in `benches/external/README.md`.

Validation:

```text
SQLITE_TPS_MODE=mem SQLITE_TPS_N=10000 SQLITE_TPS_M=2000 python3 benches/external/sqlite_tps_mirror.py
SQLITE_TPS_BINDINGS=literal SQLITE_TPS_MODE=mem SQLITE_TPS_N=10000 SQLITE_TPS_M=2000 python3 benches/external/sqlite_tps_mirror.py
HELIOS_TPS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_tps_suite -- --nocapture --test-threads=1
```

Focused result, embedded mem, `N=10000`, `M=2000`, ops/s:

```text
Nano literal rowstore     bulk 137,556/s, autocommit insert 103,300/s, lookup 322,378/s, hot lookup 1,490,923/s, update 113,950/s, delete 140,218/s
SQLite params             bulk 499,496/s, autocommit insert 229,233/s, lookup 323,115/s, hot lookup 350,767/s, update 241,284/s, delete 267,989/s
SQLite literal SQL        bulk 133,613/s, autocommit insert 96,196/s, lookup 79,605/s, hot lookup 305,308/s, update 98,366/s, delete 126,983/s
```

Interpretation: the default literal-SQL write/lookup comparison is much closer
than the prior table implied; Nano is similar to or ahead of SQLite when SQLite
is driven through literal SQL too. SQLite's bound-parameter write path is still
the best-path comparison, so Nano's `run_param_tps_suite` remains important for
client APIs that can bind parameters. The residual SQLite goal gap is now more
clearly concentrated in parameterized UPDATE/DELETE and embedded analytical
scan/join execution rather than the default literal SQL path.

## Rejected Follow-Up: Compact Build Payload For Projected Hash Join

Experiment:

- Added a temporary specialized inner equi-join for `Project(Join(...))`.
- The generic projected hash join already avoids emitting a full joined row,
  but still stores full build-side tuples. The experiment stored only the
  build-side values referenced by the final projection and emitted from a
  compact payload.
- It was gated with a temporary `HELIOS_DISABLE_FAST_PROJECTED_JOIN=1` kill
  switch while testing.

Validation:

```text
cargo check --lib
cargo test --test join_hardening_tests -- --nocapture --test-threads=1
```

Correctness passed (`join_hardening_tests` 45/45), but TPS was noise-level or
slightly worse:

```text
focused join, compact payload       64/s
focused join, old projected join    63/s
mixed analytics, compact payload    filter 240/s, aggregate 586/s, group 170/s, join 65/s, Top-N 455/s
mixed analytics, old projected join filter 237/s, aggregate 564/s, group 174/s, join 67/s, Top-N 463/s
```

Conclusion: the remaining join gap is not caused mainly by retaining unused
build-side tuple values after the existing projected join optimization. The
useful next join work needs to move earlier in the pipeline: compact/vectorized
scan-filter-probe flow, better hash-table/probe layout, or a batch-oriented
join executor. The experimental code was reverted before handoff.

## Accepted Follow-Up: Repeated Parameterized UPDATE/DELETE Fast-Spec Shortcut

Finding:

- Repeated `execute_params()` UPDATE/DELETE statements already had fast DML
  specs cached, but every call still entered `parameterized_plan_cached()` before
  the fast spec could be used.
- The TPS parameterized UPDATE/DELETE shapes execute the same statement with a
  different bound PK value thousands of times, so this plan-cache round trip was
  avoidable after the first successful fast-spec build.

Change:

- Add an autocommit-only cached-spec shortcut at the start of `execute_params()`
  for repeated parameterized UPDATE/DELETE.
- The shortcut uses the existing fast spec caches and keeps the same safety
  gates as the planned path: no active transaction/savepoint/session
  transaction, no tenant context, no branch, current RLS/trigger checks, and the
  FK mode/source in the DELETE cache key.
- First execution of a statement still builds the spec through the normal
  parsed/planned path. Later executions can skip the parameterized plan cache
  and go straight to the fast DML executor.
- Add a one-entry hot fast-spec cache in front of the LRU for the repeated
  prepared-style statement shape. It is cleared by the same DDL/plan-cache
  invalidation path and shared by cloned database handles.
- Because parameterized UPDATE has its own cache, key it directly by SQL instead
  of allocating a prefixed cache key on every hot call.

Validation:

```text
cargo check --lib
cargo test --test parameterized_query_tests -- --nocapture --test-threads=1
cargo test --test transaction_tests -- --nocapture --test-threads=1
HELIOS_TPS_PARAMS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_WORKLOADS=param_update,param_delete HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS_PARAMS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
HELIOS_TPS_PARAMS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_EMBEDDED_PROFILE=oltp_fast HELIOS_TPS_WORKLOADS=param_update,param_delete HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, ops/s:

```text
before shortcut, rowstore focused      param_update 116,370/s, param_delete 145,124/s
after shortcut, rowstore focused       param_update 164,998/s, param_delete 228,932/s
after shortcut, full param suite       param_update 169,943/s, param_delete 228,907/s
after shortcut, oltp_fast focused      param_update 168,562/s, param_delete 231,725/s
after hot-spec cache, rowstore focused param_update 171,860/s, param_delete 227,122/s
SQLite params reference                update 241,284/s, delete 267,989/s
```

Interpretation: this is a direct improvement to the remaining SQLite
best-path write gap. Parameterized DELETE is now close to SQLite's in-memory
bound-parameter delete path on this host, while parameterized UPDATE is still
behind by roughly 1.4x. The remaining UPDATE cost is likely in storage row
fetch/serialize/write and row-cache invalidation rather than SQL planning.

## Release-Close Pending List

The 3.34.0 release can publish with the committed work above. The following
items remain intentionally pending after release:

1. SQLite embedded in-memory analytical gap.
   - SQLite still leads several scan/aggregate/join shapes.
   - The next credible lever is a compact/vectorized
     scan-filter-project-join pipeline, with the existing columnar profile kept
     as a gated diagnostic mode until it is broadly competitive.

2. Bound-parameter UPDATE gap.
   - The cached fast-spec shortcut narrows the planning overhead, but SQLite's
     bound UPDATE path is still faster.
   - A fixed-width integer UPDATE patching prototype measured about 187k/s for
     `param_update_by_pk`, but it is not release-safe because it returned before
     the normal fast-update LSN increment path. The WIP patch is saved at
     `/tmp/heliosdb-nano-fixed-width-param-update-wip-20260531.patch`.
   - Post-release re-test: the byte-patch prototype was corrected to increment
     LSN after a successful patched row update, but it was still not a stable
     win (`param_update_by_pk` 176k/s then 166k/s; committed hot-cache baseline
     is about 166-172k/s on the same host). Saved patch:
     `/tmp/heliosdb-nano-fixed-width-param-update-lsn-corrected-rejected-20260531.patch`.
   - A lower-risk single-assignment fast path that skipped the per-row
     replacement `Vec` also failed to improve TPS (`param_update_by_pk` about
     163k/s). Saved patch:
     `/tmp/heliosdb-nano-param-update-single-assignment-rejected-20260531.patch`.
   - Conclusion: the remaining bound UPDATE gap is not solved by these
     tuple-local micro-optimizations. Next work should either batch UPDATEs,
     reduce RocksDB write/read amplification more structurally, or redesign
     the current-row update path around a measured profile.

3. Known correctness/semantics deferrals.
   - A4: TRUNCATE affected-row count semantics.
   - A6: HNSW tombstone/physical-count accessor semantics.
   - HA/SSL integration binaries should be gated in an isolated network
     environment rather than this shared sandbox.

4. Separate code-graph ingest batching work.
   - dm26 reported uncommitted `src/lib.rs` and
     `src/code_graph/storage.rs` changes for cross-file resolve batching and
     code-index opt-out knobs.
   - Treat that as a separate release surface unless it lands through its own
     build/test/package gate.

## Final 3.34.0 Focused Snapshot

After the 3.34.0 release metadata commit (`77f0456`), the focused same-host
in-memory comparison was re-run with `N=10000`, `M=2000`.

Default row-store analytics:

```text
Nano rowstore               filter 221/s, aggregate 541/s, group_by 165/s, join 62/s, Top-N 441/s
SQLite params reference     filter 374/s, aggregate 978/s, group_by 178/s, join 216/s, Top-N 461/s
```

Gated columnar analytics profile:

```text
Nano columnar_analytics     filter 245/s, aggregate 2059/s, group_by 138/s, join 53/s, Top-N 223/s
```

The columnar profile validates the vectorized aggregate path, where Nano is
about 2.1x faster than SQLite on this focused aggregate workload. It is not yet
a broad analytics solution: join, filter, and Top-N still need a compact or
vectorized scan/filter/project/join pipeline rather than more tuple-local
micro-optimizations.

Parameterized in-memory DML:

```text
SQLite params reference     bulk 490998/s, insert 214606/s, lookup 309577/s, update 243199/s, delete 266012/s
Nano rowstore params        bulk 149916/s, execute_many_insert 244211/s, insert 98277/s,
                            lookup 336327/s, update 159748/s, execute_many_update 242524/s,
                            delete 228951/s, execute_many_delete 481528/s
Nano oltp_fast params       bulk 218993/s, execute_many_insert 541475/s, insert 151811/s,
                            lookup 390552/s, update 182429/s, execute_many_update 292676/s,
                            delete 234116/s, execute_many_delete 499413/s
```

Interpretation: with the gated `oltp_fast` profile, Nano now beats SQLite
best-path params on `execute_many` insert, lookup, batched UPDATE, and batched
DELETE. SQLite still leads single-row autocommit INSERT and single-row UPDATE.
The remaining single-row UPDATE gap is below the threshold where parser/cache
micro-tuning has helped; it needs a storage-level profile before more code is
changed.

## Accepted Follow-Up: Batched Parameterized UPDATE/DELETE

Finding:

- `execute_many_params()` had a fast batch path for INSERT only.
- Repeated UPDATE/DELETE rows fell back to a loop over `execute_params()`, so
  every row still paid method dispatch and result-cache invalidation even after
  the cached fast DML spec was hot.

Change:

- Add an autocommit-only `execute_many_params()` fast path for eligible
  parameterized UPDATE/DELETE.
- The path reuses the same fast DML specs and safety gates as the single-row
  fast path: no active transaction/savepoint/session transaction, no tenant
  context, no branch, current RLS/trigger checks, and existing FK gates.
- Successful batches invalidate the result cache once. If a later row errors
  after earlier rows succeeded, the helper invalidates before returning the
  error, matching the old per-row autocommit behavior.
- Add TPS harness workloads `param_execute_many_update` and
  `param_execute_many_delete`.

Validation:

```text
cargo check --lib
cargo check --test tps_workloads
cargo test --test parameterized_query_tests -- --nocapture --test-threads=1
cargo test --test transaction_tests -- --nocapture --test-threads=1
HELIOS_TPS_PARAMS=1 HELIOS_TPS_MODE=mem HELIOS_TPS_WORKLOADS=param_update,param_delete,param_execute_many_update,param_execute_many_delete HELIOS_TPS_N=10000 HELIOS_TPS_M=2000 cargo test --profile perf --test tps_workloads run_param_tps_suite -- --nocapture --test-threads=1
```

Focused TPS, embedded mem, `N=10000`, `M=2000`, ops/s:

```text
run 1  param_update 167,160/s  execute_many_update 279,186/s  param_delete 227,771/s  execute_many_delete 476,773/s
run 2  param_update 167,573/s  execute_many_update 277,657/s  param_delete 229,540/s  execute_many_delete 430,679/s
run 3  param_update 161,454/s  execute_many_update 272,102/s  param_delete 209,189/s  execute_many_delete 392,364/s
```

Interpretation: this is a real structural win for clients using the batch API:
batched parameterized UPDATE is now similar to SQLite's bound UPDATE reference
on this host, and batched parameterized DELETE is ahead. It does not close the
single-row `execute_params()` UPDATE gap, which still needs a deeper storage
write-path profile.
