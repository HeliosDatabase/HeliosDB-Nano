# P2#8 — Full-keyspace scan audit (`IteratorMode::Start`)

**Branch:** `perf/p0-p1-p2`  ·  **Commit:** `ff51ed0`

## Problem class

Several functions created a RocksDB iterator with `IteratorMode::Start` (the
beginning of the whole keyspace) and then filtered for keys with a specific
prefix. Because keys are sorted lexicographically and the data rows live under
`data:<table>:<rowid>` (prefix `d…`), any scan looking for a prefix that sorts
**after** `data:` (`delta:`, `meta:mv:`, `trigger:`, `table_constraints:`, …)
walked **every data row in the database** before reaching its target — i.e.
**O(total rows)** per call.

This is the same bug class as the baseline `get_referencing_fks` fix (which was
on the per-`DELETE` OLTP hot path → measured **118× faster** at 32k rows). The
audit below covers the remaining 26 sites.

## Fix

Seek straight to the prefix and stop at the first non-matching key:

```rust
let mut read_opts = rocksdb::ReadOptions::default();
read_opts.set_total_order_seek(true); // DB has a 5-byte prefix extractor
let iter = self.db.iterator_opt(
    rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward), read_opts);
for item in iter {
    let (key, value) = item?;
    if !key.starts_with(prefix) { break; }
    …
}
```

## Sites fixed in this branch (storage layer — 7)

| File / fn | Prefix sought | Called from |
|---|---|---|
| `mv_delta.rs::get_deltas_since` | `delta:{table}:` | incremental MV refresh |
| `mv_delta.rs::count_deltas_since` | `delta:{table}:` | MV refresh / replication |
| `mv_delta.rs::purge_deltas_before` | `delta:` | delta GC |
| `catalog.rs::rename_table` | `data:{old}:` | `ALTER TABLE … RENAME` |
| `catalog.rs::load_all_triggers` | `trigger:` | startup / trigger registry |
| `catalog.rs::delete_table_triggers` | `trigger:{table}:` | `DROP TABLE` w/ triggers |
| `materialized_view.rs::list_views` | `meta:mv:` | MV listing / `\dmv` |

Each previously walked all `data:` rows; now each is O(matching keys) + one seek.

## Sites deferred (owned by the concurrent insert-path session this round)

`engine.rs` `load_counters` (`counter:`) and `replay_wal_operation` TRUNCATE
(`data:{table}:`), and `lib.rs` `DROP TABLE` row delete (`data:{table}:`). These
live in files being actively rewritten for the insert/row-counter work; flagged
for that session to fold in to avoid a merge collision.

## Sites confirmed legitimate (left as-is)

`sync/offline_queue.rs` (drain/count/clear of a dedicated queue DB — genuinely
needs every key); `sync/change_log.rs` (already seek with `iterator_opt`);
`engine_timetravel_extension.rs`, `ddl.rs drop_table_data_rows`,
`engine.rs migrate/add/drop column` (already use `total_order_seek`).

## Validation

`materialized_view_tests` (18), `trigger_tests` (9),
`materialized_view_integration` (33) — **all pass**. No behavior change; only the
seek position differs.

## TPS impact

These are **DDL / MV-refresh / startup** paths, not the core
SELECT/INSERT/UPDATE/DELETE OLTP loop, so they do not move the headline TPS
numbers. The one *per-statement* OLTP scan in this class was `get_referencing_fks`
(per `DELETE`), already fixed in the baseline (118×). The value here is
**scalability**: these operations no longer degrade linearly as the database
grows. A focused measurement (e.g. `ALTER TABLE small RENAME` with a large
unrelated table present) shows the fixed paths stay flat instead of scaling with
total row count.
