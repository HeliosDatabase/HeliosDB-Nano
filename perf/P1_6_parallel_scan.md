# P1#6 — Intra-query parallelism (parallel scan decode)

**Branch:** `perf/p0-p1-p2`  ·  machine: 32-core EPYC, in-memory engine

## Change

`StorageEngine::scan_table_with_schema_opt` (the full/filtered-scan path) was a
single loop: iterate RocksDB → decrypt → bincode-decode → resolve column-storage
refs, one row at a time on one core. Restructured into two phases:

1. **Collect** raw `(key,value)` byte pairs from the RocksDB iterator (cheap memcpy).
2. **Decode** them via `rayon::par_iter` above a 4096-row threshold (serial below,
   to avoid thread-pool overhead). `par_iter` preserves order, so results are
   byte-identical to the serial path.

`HELIOS_SCAN_SERIAL=1` forces the serial path (A/B benchmarking + ops kill-switch).

## Benchmark (N = 300,000 rows, 6-col table, result cache defeated by varying SQL)

| workload | serial | parallel | speedup |
|---|---:|---:|---:|
| full_scan `SELECT *` (300k rows out) | 576.7 ms | 523.0 ms | 1.10× |
| filter_scan (150k rows out) | 420.0 ms | 341.7 ms | 1.23× |
| agg `SUM/AVG/MAX` | 376.5 ms | 268.4 ms | **1.40×** |
| group_by (8 groups) | 508.4 ms | 399.3 ms | 1.27× |

## Honest analysis — why only ~1.1–1.4×, not ~16×

Amdahl's law. Parallel decode only speeds the **decode slice** of the query. The
parts that stay serial dominate:

- **Phase-1 RocksDB iteration** is a single iterator (can't be split without
  key-range partitioning, which the non-zero-padded `data:{table}:{rowid}` key
  layout makes awkward).
- **The operator pipeline above the scan is single-threaded Volcano**: the
  `AggregateOperator` / `FilterOperator` / projection all run on one core after
  the scan returns `Vec<Tuple>`.
- **Row-oriented full materialization**: every query builds a full `Vec<Value>`
  per row even when it needs 1–2 columns.

This directly confirms `PROPOSAL_COLUMNAR_STORAGE.md`: the analytics gap vs SQLite
(6–89×) is a *row-store materialization* problem, not a decode-speed problem.
Parallel decode is a real, safe, free-ish win but **not** the lever that closes
the SQLite gap. The levers that would are **P1#5 columnar scan** (read only the
referenced columns) and **parallel aggregation** (rayon partial-aggregate + merge
in `AggregateOperator`) — see those items.

## Cost / risk

- Adds one `Vec<(Box<[u8]>,Box<[u8]>)>` allocation per scan (transient). Point
  lookups are unaffected (they use `get_row_by_pk`, not this path).
- Order-preserving, so no result-ordering change. Encrypted tables handled
  (decrypt happens inside the per-row decode closure).

## Validation

`aggregate_hardening_tests` (72), `crud_tests` (27) pass — identical results in
serial and parallel mode. (`null_semantics … default_value_on_omitted_column_known_limitation`
fails, but it is a **stale pre-existing** test asserting `NULL` for an omitted
`DEFAULT` column; the engine now correctly returns the default `42` — true in
serial mode too, i.e. independent of this change.)
