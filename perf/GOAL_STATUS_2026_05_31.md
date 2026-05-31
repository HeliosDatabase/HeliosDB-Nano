# Goal Status: Overall TPS vs SQLite/PostgreSQL/MySQL

Date: 2026-05-31
Branch: `codex-next-write-tps`
Nano commit: `74dd498 perf: compact direct-column top-n scans`

Objective: HeliosDB-Nano should have a few times better overall performance than PostgreSQL, MySQL, and SQLite.

## Current Evidence

The goal is not complete. On this host, Nano is far ahead of SQLite on durable autocommit writes, but SQLite is still faster on most in-memory and analytical workloads in the mirrored TPS suite.

PostgreSQL/MySQL were not measured in this pass because `psql` and `mysql` clients are not installed in this environment. Existing external scripts are present under `benches/external/`, but those engines still need a same-host service/client gate.

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

## Bottlenecks Confirmed

1. Default time-travel write amplification is still expensive.
   - TT-off `execute_many` reaches ~522k/s and beats SQLite mem bulk insert in this run.
   - TT-on bulk/param insert is roughly half that because each logical row also writes version history.

2. Analytics remain row-materialization dominated.
   - Top-N improved in `74dd498`, but SQLite is still ~2x faster on the mirrored in-memory Top-N.
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
   - `benches/external/pg_vs_helios.py` exists but requires a running PG-compatible backend and `psycopg`.
   - No MySQL mirror is currently present; either add one or run through the MySQL wire protocol with an equivalent script.

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
