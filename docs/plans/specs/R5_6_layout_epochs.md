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
