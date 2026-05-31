# Goal Status: Overall TPS vs SQLite/PostgreSQL/MySQL

Date: 2026-05-31
Branch: `codex-next-write-tps`
Nano commit: `bdd3ba5 perf: skip nondeterministic scan for uncached fast selects`

Objective: HeliosDB-Nano should have a few times better overall performance than PostgreSQL, MySQL, and SQLite.

## Current Evidence

The goal is not complete. On this host, Nano is far ahead of SQLite on durable autocommit writes and now beats same-host PostgreSQL/MariaDB Docker-client read/analytics smoke runs, but SQLite is still faster on most in-memory analytical workloads in the mirrored TPS suite.

Host `psql` / `mysql` clients are not installed, so `benches/external/docker_sql_tps_mirror.py` was added to drive `psql` / `mariadb` inside existing Docker containers without Python DB drivers.

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
| bulk_insert_users(txn) | 125,704 | 505,670 | SQLite 4.0x |
| autocommit_insert | 99,459 | 223,369 | SQLite 2.2x |
| point_lookup_pk | 280,066 | 323,920 | SQLite 1.2x |
| point_lookup_hot | 1,658,229 | 590,571 | Nano 2.8x |
| update_by_pk | 109,626 | 244,657 | SQLite 2.2x |
| delete_by_pk | 131,518 | 266,922 | SQLite 2.0x |
| filter_scan(age>50) | 171 | 381 | SQLite 2.2x |
| agg_count_sum_avg | 340 | 969 | SQLite 2.8x |
| group_by_status | 148 | 167 | SQLite 1.1x |
| join_users_orders | 53 | 201 | SQLite 3.8x |
| order_by_limit10 | 207 | 461 | SQLite 2.2x |

Parameterized Nano with time-travel disabled shows the write-path ceiling:

| Workload | Nano ops/s |
|---|---:|
| param_bulk_insert(txn) | 205,799 |
| param_execute_many_insert | 521,930 |
| param_autocommit_insert | 196,916 |
| param_point_lookup_pk | 353,796 |
| param_update_by_pk | 121,366 |
| param_delete_by_pk | 138,876 |

This means the in-memory write gap is heavily tied to default time-travel/MVCC version maintenance and per-row DML overhead, not just RocksDB.

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

## Same-Host PostgreSQL/MariaDB Docker-Client Comparison

This is a new external gate for the PostgreSQL/MySQL part of the goal. It uses the database clients inside existing containers, discards query output, and mirrors the read/analytics half of `tests/tps_workloads.rs` at `N=10000`, `M=2000`.

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

This improves the PostgreSQL/MySQL evidence gap, but it is not the final proof required by the goal: the comparison path is Docker-client based, while the Nano numbers above are embedded TPS harness numbers. It is still useful because the same SQL shapes now have same-host PG/MySQL guardrails.

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
