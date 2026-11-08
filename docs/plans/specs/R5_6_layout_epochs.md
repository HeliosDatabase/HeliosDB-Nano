# R5.6 — layout epochs: a statement reads its catalog and its rows under one view

Parent: `docs/plans/specs/R5_statement_atomicity.md`, Part 1 decision 2 (wins over design §6, M5).
On the tree that holds R5.1-R5.4 and B4.

## Why

R5.1 serializes writers against DDL, but readers never wait. A reader can still load a table's
catalog (schema, missing values, index list) from one moment and its rows from another. Observed
on the merged tree (`$SCRATCH/g/mrev_scripts/standby/sb5_volatile.py`, re-check in
`$SCRATCH/g/mrev_scripts/recheck_d0562eee/`): during a volatile ADD COLUMN, physical-standby
readers see the new column NULL in every row (about 600 wrong reads in 5 rounds of 5000 rows,
time travel on and off alike, also through prepared statements and `sum(CASE …)`), and the primary
did it twice. A stable default shows 1 or 2. R2's windowed test (`tests/physical_ddl_window.rs`) is
the wider acceptance: torn, window, pair, broken and not-ready reads all 0 on every standby reader
and on the primary control.

## Decision 2, as it stands (do not re-litigate)

(a) Bump a per-table epoch in the write funnel (`WriteGate::write`) for ANY batch carrying
`meta:table:`, `bmeta:{level}:table:`, `meta:index:` or `meta:attmissing:` (plus the B4 keys that
change what a reader decodes: `bmeta:{L}:missing:{t}`, `meta:table_shape:`), on the primary; and in
the applier at the publication of a run containing such keys on the standby, splitting the run at
such a batch. (b) Load the epoch BEFORE the cache lookup; odd means a change in flight, treat as a
mismatch. (c) Record epochs in the branch resolver and on the PG extended-protocol pinned-plan
path. (d) The last validation sits immediately before the statement's first durable or externally
visible effect (commit, first DataRow); after it the statement never retries. (e) A statement's own
bumps are re-recorded into its read set. (f) Retry granularity is one statement; scripts and
batches do not open a read set; statements with non-transactional side effects (nextval, advisory
locks, CALL whose body ran DDL) are not retried (40001 instead). (g) On a mismatch: bounded backoff
retry up to 100 ms total, sleeping inside the offloaded statement and never on an async worker,
then 40001 "could not serialize access due to a concurrent schema change"; same on primary and
standby. (h) A Described statement whose result shape changes after re-plan fails 0A000 (R5.2's
check stays the mechanism). (i) Per-table epochs, never striped.

## Also

- Rows: a statement's row reads must come from the same moment as its validated catalog. On the
  standby, find out exactly how a reader pairs a new catalog with old rows (row cache, RocksDB
  snapshot pinned before the apply, a cached plan, the missing-values cache) and close that path;
  the epoch check is the guard, not a substitute for the cause.
- Cost: one atomic load per table per statement on the hot path; measure single-row reads and
  writes on the release build.

## Tests

`tests/layout_epochs.rs`: a reader loop beside each catalog-changing statement (volatile and stable
ADD COLUMN, DROP COLUMN, RENAME, CREATE/DROP INDEX, REFRESH) on the primary and on an embedded
physical standby, asserting no torn read; retry then 40001 under a DDL storm; no retry for a
statement with nextval; the pinned-plan path. Un-ignore the main and paced tests of
`tests/physical_ddl_window.rs` if they pass; otherwise report what they still see.

## As built

- Root cause of the standby reads (merged-tree review M3): a plan built before the column existed
  was used after the new schema was readable. The planner does not resolve an aggregate's argument
  (`count(u)` plans against `(id, a)` and is cached before it fails 42703 at execution). On the
  standby, `publish_meta_tables` made the new schema readable but the plan caches were cleared only
  by `publish_listener`, after `stage_rows` over every row of the run. The stale plan's scan
  re-resolved the new schema, decoded none of its columns (empty decode hints are vacuously
  columnar) and returned every row with `u` NULL; the result cache then replayed it. The store
  was never torn, and time travel played no part.
- `storage::layout_epochs`: one epoch per table name (low 16 bits count changes in flight), a
  thread-local read set per client statement (`StatementScope`; nested executions join it), plan
  stamps (`LayoutStamp`, collected while planning), `PublicationScope` (a schema statement's change
  ends when its publication does), and the retry driver `EmbeddedDatabase::run_layout_statement`.
- Writers: `WriteGate::write` / `put` / `delete` validate the read set before the first durable
  write and bump every table a key of `LAYOUT_PREFIXES` names (`meta:table_layout:` included). The
  physical applier marks a run's layout tables in flight before its write and ends the change after
  `publish_meta_tables`, `publish_index_deletes`, `drop_layout_caches` and the listener's
  `on_layout_change`, before any row is staged; a run is split at a layout batch.
- Readers: `Catalog::get_table_schema` / `main_table_schema` record the epoch before any cache.
  Cached plans (text, parameterized, normalized), fast-select specs and extended-protocol pins are
  served only while their stamp is current. The scan and the aggregate pushdowns refuse a plan
  whose table schema differs from the table's now (40001).
- SQL `PREPARE`: the statement's text is kept on both paths (`LogicalPlan::Prepare::source_sql` for
  the parser's), and an `EXECUTE` whose stored plan the scan refuses is planned again from it; a
  changed result type is refused 0A000, as PostgreSQL refuses it.
- Not retried: explicit transactions (40001 at once, the statement fails inside its boundary),
  statements that took a sequence value, an advisory lock or ran `CALL`, and statements past their
  first durable write. A sequence's high-water write is `outside_statement`.
- Kept separate from `storage::row_locks`' R5.4 row-change epochs (1024 striped slots).
