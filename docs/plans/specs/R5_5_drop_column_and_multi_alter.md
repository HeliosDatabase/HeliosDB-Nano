# R5.5 — DROP COLUMN in one group, multi-clause ALTER in one group, and AS OF across a shape change

Parent: `docs/plans/specs/R5_statement_atomicity.md`, Part 1 decision 5 (DROP COLUMN and any
row-rewriting ALTER: schema, all rows and constraint/columnar cleanup in ONE group up to the cap,
54000 above it; M4 deferred) and design §8 F3 (multi-op composition). On the tree that holds
R5.1-R5.4 and B4 (R5.3's `DdlBatch` is the vehicle).

## Why

- `ALTER TABLE … DROP COLUMN` writes the schema and then rewrites rows in further groups: a crash or
  a reader between them sees a schema without the column over rows that still have it, and kill -9
  shifts values (`tests/row_shape.rs` ignored crash tests: drop_middle, readd_drop).
- `AGENTS.md` promises "Multi-op `ALTER TABLE` is atomic per statement". Today each clause is its own
  group: a crash or a later clause's refusal keeps the earlier clauses (R5.2 pre-validates its own
  refusals, but `ADD c DEFAULT 1 CHECK (c > 5)` still leaves c, and DROP COLUMN clauses aren't
  pre-validated). Deliver the promise.
- Main's own `AS OF` before a DROP COLUMN, RENAME COLUMN, RENAME TO or REFRESH mis-shapes: a dropped
  column's value shows under a later column's name (R5.2 review P3: `DROP secret; ADD pub` shows
  'secret' under pub in a transaction and in AS OF). B4 records these changes
  (`meta:table_shape:{t}`, `storage.catalog().main_table_shape_change(table)`, `describe(table)`,
  per-row `snapshot_manager().row_events_around(table, row, ts)`; see
  `StorageEngine::anchor_version_is_current_shape`).

## Items

1. DROP COLUMN: schema, every row's rewrite with a version of each (as R5.2's volatile rewrite
   does), the column's constraints, indexes, columnar side data and missing value, in ONE group up to
   `[storage] max_atomic_rewrite_bytes` (with B4's frozen-copy factor), 54000 above it naming the
   alternative. Any other row-rewriting ALTER (SET STORAGE, ALTER COLUMN TYPE if supported) the same.
2. Multi-clause ALTER: one `DdlBatch` for the whole statement; every clause pre-validated before the
   first write; one group; one publication. A refusal by any clause leaves nothing.
3. Main's `AS OF` (and a transaction snapshot) older than a recorded shape change: read the row
   version under the definition it was written with, if the definition at that time can be
   reconstructed from what is recorded; otherwise refuse 0A000 naming the table and the change, the
   way B4 refuses an anchor. Never show a value under the wrong column. Rows whose newest version
   predates the change and that changed since are the hard case; follow B4's rule.
4. Un-ignore `a_crash_after_drop_column_keeps_the_other_columns_values` and
   `a_crash_after_a_drop_and_readd_keeps_the_last_definition` only if they now pass WITHOUT the
   crash-replay step; otherwise leave them for R5.7 and say why.

## Tests

One-group tests for DROP COLUMN and for multi-clause ALTER (both families), a refusal in clause 3
leaving clauses 1-2 unwritten, a reader loop during DROP COLUMN never seeing a shifted value, AS OF
before DROP COLUMN / RENAME COLUMN / RENAME TO / REFRESH either right or refused (never shifted), on
main and in a REPEATABLE READ transaction begun before the change.
