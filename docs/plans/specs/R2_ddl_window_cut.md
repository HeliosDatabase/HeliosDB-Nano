# R2 — the DDL-window torn-read test; cut markers only if it fails

Plan decision 7 (`docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3 "Consistent cut"): one
`PhysicalBatch` = one write group, and the standby publishes per batch under a lock, so a
reader sees exactly a primary crash-recovery state at that sequence; cut markers (`repl:cut`,
`docs/plans/REPLICATION_4_41_1_DESIGN.md` §3.1) were deferred until a DDL-window test shows a
torn read. Write that test; act on its result.

## The test (`tests/physical_ddl_window.rs`, daemon pair via tests/support/physical_pair.rs)

- Reader side: several concurrent sessions on the STANDBY run a fixed set of queries in a
  tight loop for the whole test (SELECT * / COUNT(*) / an index-backed point read / a kNN
  read / `information_schema.columns` for the tables under churn), recording every answer.
- Writer side, on the PRIMARY, in a loop of N iterations (N sized so the test runs ~30-60 s
  in a debug build): CREATE TABLE + INSERT rows + CREATE INDEX; ALTER TABLE ADD COLUMN with a
  DEFAULT on a populated table; ALTER TABLE DROP COLUMN; RENAME COLUMN / RENAME TABLE; CREATE
  and DROP of an HNSW index; TRUNCATE; DROP TABLE; a multi-statement explicit transaction
  mixing DDL and DML. First establish (log it) how many write groups each of these produces
  on the primary — a statement that produces more than one group is the candidate for a torn
  read.
- A torn read is any standby answer that matches NO state the primary passed through: a
  table visible with rows but no columns (or the reverse), rows of a width that matches
  neither the old nor the new schema, an index-backed read that disagrees with a full scan of
  the same table, an internal error other than the "relation does not exist" / "column does
  not exist" family, a panic or a poisoned lock, `COUNT(*)` disagreeing with `SELECT *` in the
  same snapshot. The assertions must be on these invariants (not on timing).
- Make it fail on purpose first: temporarily verify the detector catches a synthetic torn
  state (e.g. a test-only applier hook that publishes between the two halves of a
  multi-group statement, under `internal-tests`), so the test is not vacuous.

## If the test finds torn reads

Implement the cut marker: the primary writes `repl:cut` (sequence of the last group of a
statement/transaction) in the statement's LAST write group; the standby's applier publishes
derived state and admits readers only at cut boundaries (between cuts it applies but readers
keep the previous published state); a `PhysicalBatch` without any cut (e.g. a long
statement) is bounded by a maximum delay after which the standby reports `lag` accordingly.
Spec the details in this file before coding them (add a section), keep the wire format
compatible within this release (no protocol version bump unless needed — say so if it is).

## If it does not

Record the evidence (groups per DDL, iterations, reads checked) in the plan's decision 7 and
close it: cut markers are not needed with one-group-per-batch publication.

## Rules

Do not run cargo, git or any build. No new `unwrap`/`expect` on non-test paths. Report the
groups-per-statement census, the detector's self-check, and the result.
