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
