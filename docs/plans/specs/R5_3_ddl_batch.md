# R5.3 — every catalog DDL statement is one write group

Parent: `docs/plans/specs/R5_statement_atomicity.md`. Part 1 (decisions) wins over Part 3
(design). This step is decision 3 (explicit `DdlBatch`, detector only), decision 7 (hidden index
registration), decision 9 (`STATS_WRITE_LOCK`) and design §8 F2 and F7, on the tree that already
holds R5.1 (per-table writer lock), R5.2 (row shape, missing values) and B4 (fork point).

## Why

A DDL statement today writes several groups (CREATE TABLE 4, RENAME TABLE 6, CREATE INDEX, MV
refresh, CTAS, CREATE SCHEMA with elements). A crash between them leaves a half-made catalog, and
a reader, a standby, or a branch fork between them sees one. R2's windowed test
(`tests/physical_ddl_window.rs`) is this step's eventual acceptance, together with the later steps.

## What is already in place (do not redo)

- R1: a DDL's logical-WAL entry rides its own write group (`write_ddl_batch`, `DdlWrite`).
- R5.1: the per-table writer lock. DDL holds the table exclusive from its first read through its
  commit, a transaction's DDL until the transaction ends. This replaces design §3's "M2 fence".
  Lock order is in `src/storage/branch_freeze.rs` ("Lock order").
- R5.1 decision 11: generation-guarded schema-cache fills, publish after the write.
- R5.2: ADD COLUMN with a stable default is one group; a volatile rewrite is one group up to
  `max_atomic_rewrite_bytes`.
- B4: the freeze hook runs inside `WriteGate`; a DDL's frozen copies ride its batch.

## Items

1. `DdlBatch`. An explicit handle threaded through the catalog funnels that already stage
   (`stage_table_records_drop`, `stage_prefix_delete`, `write_ddl_batch`, the `materialize_*` and
   `stage_*` variants). A statement creates one at its start and commits it once at its end:
   one `WriteGate::write` carrying data, catalog records, WAL ops, and B4 frozen copies. Publication
   of in-memory state (caches, registries, ART/HNSW structures) happens after the write, in the
   order design §2.5 gives; a failed write publishes nothing.
2. The detector. During a DDL statement (a thread-local marker set by the `DdlBatch` owner), any
   `WriteGate` write that is neither the statement's commit nor on an allow-list with a reason
   panics in test builds and counts and warns once per site in release. Reuse R5.1's detector
   style (`#[track_caller]`, report after RocksDB's batch callback returns, never panic while
   unwinding).
3. Statements to convert (design §8 F2 is the inventory; re-check it against the code, which has
   moved): CREATE TABLE (with identity, partition links, constraints, incarnation), CTAS
   (populate through the InsertSelect arm in the statement's implicit transaction), CREATE SCHEMA
   with elements, sequences, CREATE and REFRESH MATERIALIZED VIEW (one transaction: delete every
   current row, insert every new row, the refreshed record; CONCURRENTLY and the auto-refresh
   worker use the same path), CREATE and DROP INDEX, TRUNCATE (ART clear moves to an on-commit
   hook), RENAME TABLE (including index-definition rewrites and trigger moves, see Findings),
   RENAME COLUMN, constraints (ADD/DROP UNIQUE, FK, CHECK, NOT NULL), views, functions, procedures,
   triggers, types, grants, roles, DROP TABLE (partition children in the same group), CREATE BRANCH.
4. Hidden index registration (decision 7). CREATE INDEX registers the new ART index HIDDEN: writers
   maintain it, lookups skip it; it becomes visible in the publication after the durable group.
   This closes the "0 rows during backfill" window.
5. Persistent HNSW keeps today's order (decision 8): its own batch before the definition; dangling
   ids are filtered. Do not fold it in unless proven simple.
6. `STATS_WRITE_LOCK` (decision 9) is process-global: never hold it across a statement; take it
   only at the final commit, or make it per table.
7. F7: the pg35 DDL profile (design §8 F7). Each converted statement must not regress its pg35
   category beyond ±3%; a WIN that flips to LOSS against v4.41.1 is a blocker.

## Findings from earlier reviews that land here

- RENAME TABLE: index-definition rewrites and trigger moves are separate writes after the rename's
  batch. A kill 14-30 ms in leaves the renamed table without its secondary indexes and trigger
  (pre-existing, also on 7300e365). Repro: `$SCRATCH/g/b4rev_scripts/t_renidx.py`. They go in the batch.
- REFRESH MATERIALIZED VIEW killed part way leaves the view partial (48 to 1018 of 2000 rows).
  A REFRESH of 1000 or more rows writes one snapshot record per row; only the last 1000 snapshots
  survive a restart. The one-transaction refresh fixes both; assert both.
- `REFRESH` and `DROP MATERIALIZED VIEW` do not parse on the params family (`execute_params`), on
  main too. Make them parse there, or say why not.
- `EXPLAIN ANALYZE TRUNCATE` inside a transaction is not rolled back.

## Constraints

- No backward compatibility shims for the on-disk catalog beyond what keyspace classification
  requires; every new key class is classified in `src/storage/keyspace.rs`.
- One group per statement must hold on the physical standby too: the applier sees one Data group.
- Branch DDL (B3) and branch forks (B4) must keep their guarantees; B4's fork fence wraps CREATE
  BRANCH unchanged.
- The statement batch counts against `physical::MAX_BATCH_BYTES` (decision 6 puts the ceiling in
  `WriteGate::write`; enforce it there once, 54000, if R5.4 has not already).

## Tests

New `tests/statement_atomicity_ddl.rs` with a helper `write_groups_since(db, seq)` over
`DB::get_updates_since`. For each converted statement, `<statement>_is_one_write_group` (both
families where the parser allows). Plus the crash tests of design §8 F2 (subprocess exit before and
after the group), the REFRESH reader loop, `rename_keeps_its_indexes_and_triggers_at_every_kill`,
`ddl_commit_failure_publishes_nothing`, and a detector unit test that proves it fires.

## Deliverable

Code, tests, docs (`docs/NANO_CONCURRENCY_LOCKING.md`, the schema skill), CHANGELOG text, and a
report naming every statement converted, every allow-listed write and why, and anything left.
