# P1#5 — Columnar scan operator (design + status)

**Branch:** `perf/p0-p1-p2`  ·  **Status: NOT IMPLEMENTED this round — design + rationale below.**

## Why this is the key analytics lever (evidence)

HeliosDB-Nano is a row store: every scan reads each full row blob from
`data:{table}:{rowid}`, bincode-decodes the **whole** `Vec<Value>`, and the
operator pipeline runs row-at-a-time. The repo's own `PROPOSAL_COLUMNAR_STORAGE.md`
measured the gap vs SQLite on a 1 M-row table:

| query | Nano | SQLite | gap |
|---|---:|---:|---:|
| `COUNT(DISTINCT session_id)` | 725 ms | 27 ms | 26.8× |
| `WHERE type='user'` | 852 ms | 76 ms | 11.2× |
| `GROUP BY … SUM` | 902 ms | 145 ms | 6.2× |

P1#6 (this branch) added **parallel decode** and measured only **1.1–1.4×** — a
direct confirmation that decode speed is *not* the bottleneck; **reading and
materializing every column of every row is.** SQLite answers `COUNT(DISTINCT
session_id)` in 27 ms because it reads *only the `session_id` column*, contiguously.
Closing the 6–89× analytics gap requires reading only the referenced columns —
i.e. a columnar scan. No amount of parallelism or decode tuning substitutes for it.

## What already exists in the tree

- `src/storage/columnar.rs` — `ColumnarStore` (keys `col:{table}:{column}:{batch}`),
  **not wired into the executor**.
- `ColumnStorageMode::Columnar` per-column storage mode (opt-in; resolved
  row-wise today in `scan_table_with_schema_opt`).
- `src/storage/columnar_zone_summary.rs`, `zone_map.rs`, `simd_filter.rs`,
  `parallel_filter.rs` (the last is instantiated but **never called**).
- `prefix_decode` (decode only the first *k* columns) — a partial projection win
  already wired via `compute_scan_prefix_hint`, but it still reads the full row
  blob and can't skip a leading column.

## Design (phased)

**Phase 1 — `ColumnarScanOperator`** (the 80% win):
1. Planner detects a single-table scan whose referenced columns are all stored
   `Columnar` (or transparently project-pushed), with no wildcard.
2. The operator reads only the `col:{table}:{c}:*` batches for the referenced
   columns, zips them by `row_id` into narrow tuples (only the needed columns,
   the rest `Null`), and feeds the existing Filter/Aggregate operators.
3. Fall back to the row scan if any referenced column is non-columnar.
4. Combine with P1#6: decode/scan each column batch in parallel (rayon).

**Phase 2 — vectorized aggregation**: run `SUM`/`COUNT`/`GROUP BY` over Arrow
column batches (the `arrow` crate is already a dependency) with SIMD, instead of
row-at-a-time `Value` evaluation.

**Phase 3 — columnar by default** for analytical tables (background row→column
conversion), so the win doesn't require opt-in `STORAGE COLUMNAR`.

## Expected impact

Phase 1 alone should bring single-column aggregates / filters within ~2–3× of
SQLite (from 6–27×) by eliminating the read+decode of unreferenced columns; Phase 2
should reach parity or better on `SUM`/`GROUP BY`. This is the change that turns
"a few× *slower* on analytics" into "competitive/faster".

## Why deferred this round

This is an execution-engine feature (new physical operator + planner integration +
column-batch I/O + a storage-population path), realistically multi-day, and it
must not destabilize the OLTP write path that the concurrent session is actively
changing. It is sequenced after the write-path work lands. The P1#6 parallel-decode
groundwork (raw-collect + parallel map) is directly reusable for parallel column-batch
decode.
