# Proposal: columnar scan path for analytical queries (sqlite-parity lever)

Status: proposal · Motivated by the issue #1 benchmark · Builds on existing
`ColumnStorageMode::Columnar` + `src/storage/columnar.rs` + `src/storage/columnar_zone_summary.rs`

## The problem, measured

The v3.33.0 PyO3 binding made Python access in-process (1.5–4.7× over PG-wire), but Nano
is still 6–89× slower than sqlite on the dashboard's hot-path aggregates over a 448k-row
table:

| query | Nano in-proc | sqlite |
|---|---|---|
| `COUNT(*)` | 153 ms | 1.7 ms |
| `COUNT(DISTINCT session_id)` | 725 ms | 27 ms |
| `WHERE type='user'` | 852 ms | 76 ms |
| `GROUP BY project_slug, SUM(input_tokens)` | 902 ms | 145 ms |

A projection-aware **prefix decode** (v3.33.0) — decode only the leading columns a query
references — bought only ~25% on `COUNT(DISTINCT)`. That measurement is the key result:
**decode is not the bottleneck.** Nano is a row store. Every scan iterates `data:{table}:*`,
reads each full row blob from RocksDB, and materializes a full `Vec<Value>` per row — even
when the query needs one column. sqlite answers `COUNT(DISTINCT session_id)` in 27 ms
because it reads *only the session_id column*, contiguously. Closing the gap is a
storage-layout problem, not a decode or access-mode problem.

## What already exists

Nano already ships columnar building blocks — they're just not on the scan path:

- **`ColumnStorageMode::Columnar`** — a per-column opt-in. Values live in
  `src/storage/columnar.rs` under keys `col:{table}:{column}:{batch_id}` (batched), with a
  `ColumnarRef` placeholder left in the row blob.
- **`ColumnarStore::get(table, column, row_id)`** — point lookup, used today only to
  *resolve* a `ColumnarRef` while scanning the row.
- **`columnar_zone_summary.rs`** — per-batch zone summaries (min/max), the basis for
  predicate skipping.

The gap: there is **no columnar-native scan**. Even for a Columnar column, reads go through
the row scan (`scan_table_with_schema`) and touch the row blob; the column store is only a
resolution side-channel. So the columnar layout exists but the win (reading just the column)
is unrealized.

## Proposal

A **columnar scan operator** that, for a single-table query whose referenced columns are all
columnar, reads only those columns' batches directly — never opening a row blob.

1. **Columnar scan path.** Reuse the existing needed-column analysis (the
   `compute_scan_prefix_hint` walker in `src/sql/executor/scan.rs` — generalize it from a
   prefix length to the exact referenced-column *set*). When every referenced column of a
   single table is Columnar (or can be promoted), build a `ColumnarScanOperator` that streams
   `col:{table}:{column}:*` batches for just those columns and zips them by row_id into narrow
   tuples. `COUNT(DISTINCT session_id)` then reads one column's batches (~MBs) instead of 448k
   full rows.

2. **Zone-map predicate pushdown.** For `WHERE col <op> literal`, consult
   `columnar_zone_summary` per batch and skip batches whose min/max can't match — turning
   `WHERE type='user'` into a scan of only the qualifying batches.

3. **Maintained aggregates (stretch).** Keep an incrementally-updated row count (and
   optionally per-group `COUNT`/`SUM`) so `COUNT(*)` is O(1) rather than a 448k-key walk
   (Nano's `COUNT(*)` fast-path is still 153 ms today because it iterates keys).

## Integration & safety

- **Opt-in, then automatic.** Phase 1 targets columns already declared Columnar. Phase 2 can
  auto-promote hot scan columns (the optimizer already has the referenced-column set).
- **Correctness fallback.** Exactly like the prefix-decode work: any query the analysis isn't
  certain about (wildcard, subquery, join, a non-columnar referenced column, multi-table)
  falls back to the existing row scan. Columnar and row paths must return identical tuples —
  enforced by a differential test (same query, both paths, assert equal) over the suite.
- **Write path.** Columnar batches are already written for Columnar columns; the cost is the
  batch flush cadence and keeping zone summaries current. Quantify on the OLTP benchmark before
  widening the default.

## Phasing

- **P1** — `ColumnarScanOperator` for projected columnar columns (no predicate); generalize
  the column-set analysis; differential correctness test. Expected: `COUNT(DISTINCT col)` and
  single-column aggregates drop toward sqlite range.
- **P2** — zone-map batch skipping for `WHERE`/range predicates.
- **P3** — maintained `COUNT(*)`/grouped aggregates for O(1) answers.
- **P4** — auto-promotion of hot columns + benchmark-gated default, head-to-head vs sqlite and
  vs the row path (no regression on OLTP).

## Expected outcome

Reading only the referenced column(s) removes the per-row blob read + full-tuple
materialization that dominates today, which is precisely the difference between Nano's 725 ms
and sqlite's 27 ms on `COUNT(DISTINCT session_id)`. This is the lever that makes the embedded
(and PG-wire) analytical path sqlite-competitive — the PyO3 binding then delivers that speed
in-process to Python, finishing issue #1's story.
