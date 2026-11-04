# R2 — the DDL-window torn-read test; cut markers only if it fails

**Status (2026-10-31): the test failed, and cut markers were tried and rejected. See "Decision
2026-10-31: no cut markers" below; R5 owns the fix and this test is its acceptance.** The
sections up to "Rules" are the spec as first written.

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

## Phase 1 result (2026-10-31, `tests/physical_ddl_window.rs`, HEAD 4d55bd09 + R2 phase 1)

Plain run, default build, 20 iterations: every cache-bypassing standby reader saw torn
answers (10 / 13 / 14). The census (archived write groups attributed per statement) shows
that almost every statement is several write groups on the primary: CREATE TABLE 4,
CREATE INDEX 3, ADD COLUMN DEFAULT and DROP COLUMN 15 each for 12 rows (the schema first,
then ONE GROUP PER REWRITTEN ROW: `alter_table_add_column` / `_drop_column` write
`update_table_schema`, then `add_column_to_rows` / `drop_column_from_rows` put every row on
its own, then the logical-WAL entry), RENAME COLUMN 3, DROP INDEX (hnsw) 5, RENAME TABLE 6,
and every autocommit statement ends with its own `snapshot:` group. The torn answers are
exactly the states between those groups: an old-format row under the new schema (`Vector
distance operators require vector operands, got Int4(7)`, `Cannot compare Vector(..) and
String("v5")`, `Column index 5 out of bounds in tuple`), and a renamed table read with its
stale cached schema over rows already moved (`0 rows of [id, it, w, emb, d]`). Decision 7's
premise — one group per statement, published per batch under a lock — does not hold:
statements are multi-group, and readers never took a lock (the applier's own documentation:
"Readers do not take the replay guard").

Two findings about the detector itself: (1) the "regressed" class was a
cascade — the session's lower bound was the earliest state an answer matched, and answers
that carry no iteration number (a count, an empty table, an absent one) match a state in
EVERY iteration, so one out-of-order answer moved the bound one iteration ahead and every
later answer counted as regressed until the writer caught up (the printed examples: "the
session had reached state 211 (iteration 11)" while the standby replayed iteration 10). So
its counts (962 / 1052 / 971 on the standby readers, 3282 on the control) are a few jumps
multiplied, and they say nothing by themselves. The test now bounds each answer by a window
from clocks and replication positions instead (its module documentation). The torn counts
are unaffected: "torn" means an answer matches no state at all. (2) The primary control
flagged torn answers because the PRIMARY ITSELF tears: nothing excludes its readers from the
per-row rewrite of ADD/DROP COLUMN. That is the decision below.

## Decision 2026-10-31: no cut markers

Cut markers were implemented (the primary stamped `repl:cut` into every group of a DDL
statement and wrote a cut at its end; the standby's readers passed a gate that admitted them
only at a cut) and rejected before merge. All of that code is removed: there is no `repl:cut`
key, and no reader gate. **The fix moves to the primary: R5, statement atomicity
(`docs/plans/specs/R5_statement_atomicity.md`). This spec's test is R5's acceptance.**

### Why

**1. The primary tears too, so the cause is not on the standby.** A cut could only hide on a
standby what the primary's own readers see and what a primary crash leaves behind:

- `alter_table_add_column` / `alter_table_drop_column` write the schema, then
  `add_column_to_rows` / `drop_column_from_rows` put every row on its own, and nothing keeps a
  reader of the primary out between them.
- The primary control reader of the plain run: torn 6. The run's summary printed none of the
  six (it showed each reader's first eight violations); finding (a) below is the direct proof.
- `tests/physical_ddl_window_repro.rs`, embedded, no standby (`--ignored`, both builds):

| Finding | Default build | `internal-tests` build |
|---|---|---|
| (a) a reader of the primary sees ADD COLUMN DEFAULT applied to some rows and not others | fails: 194 mixed answers | fails: 239 mixed answers |
| (b) the workload, sequentially | passes | passes |
| (b) the workload beside a reader of the same database | fails: `iteration 6: ddl_b is still readable after its DROP TABLE` | fails: same, at iteration 2 |
| (c) a primary-key point read of a row inserted before an `ADD COLUMN … DEFAULT` of the same transaction | fails: 3 values under a 4-column schema | fails: same |

  These ran on the tree with the rejected code still in it; on a primary that code added only
  the stamp and the cut group.
- (b) is a race, not a build difference. The daemon test's `Table 'ddl_b' already exists` at
  iteration 6 (seen only in its `internal-tests` runs) comes from the RENAME arm's
  `catalog.get_table_schema(new_table_name).is_ok()`, which consults the schema cache first.
  R5 identified the cause: DROP invalidates the cache before its durable write and the refill
  has no generation guard, so a concurrent reader re-caches the dropped table.
- (c) over the wire: the general path fails `Column index 3 out of bounds in tuple`, and the
  bounded point-read fast path sends a DataRow the client cannot parse, which ended the cached
  reader's connection (the plain run's `broken 1`).
- Crash atomicity, from reading the code and not run: a crash inside DROP COLUMN's rewrite
  leaves old-width rows under the new schema. The logical-WAL entry is written after the
  rewrite, so open does not redo it, and those rows read shifted (`shape_base_tuple_to_width`
  truncates the LAST value, not the dropped one). A crash inside ADD COLUMN's leaves NULL
  instead of the default in the rows not yet rewritten.

**2. The gate was unsound in itself.** An independent review, by reading and tracing the code
(nothing ran), found:

- **Workers parked before the offload (undoes S1, ccebc95977f1).** `ServingGate::enter` was
  reached from the admission, which runs before `offload_statement`. As many standby SELECTs as
  there are tokio workers could park every worker: the replication session cannot run, the cut
  is never delivered, ACKs stop, and sync commits on the primary wait out `sync_timeout`.
- **Unbounded reader waits.** Readers waited without a limit during exclusive runs, which
  covered index backfill: every reader of every table blocked for a CREATE INDEX on a large
  table.
- **The stamp and the cut were not serialized.** A DML stamp could land after its cut, leaving
  `repl:cut` saying "open" with no statement open, so a standby restarted there stayed held
  until the next DDL on the primary, possibly days later. A DDL that wrote nothing could also
  skip a cut a concurrent writer's stamp needed.
- **The 5 s hold re-armed on every run,** so it bounded nothing: during a long statement every
  reader waited about 5 s, and those admitted on expiry saw the torn state anyway.
- **Readers longer than the 1 s drain were still torn,** and that drain ran before the write,
  delaying ACKs and the sync commits waiting on them.
- **Scoping gaps.** `plan_invalidates_sql_caches` omits REFRESH / ALTER MATERIALIZED VIEW, the
  MV workers write unscoped, autocommit `INSERT … SELECT` writes one group per row,
  `CREATE SCHEMA … CREATE TABLE` returned before the scope opened, and CREATE AST INDEX and
  the vector-store APIs bypass the funnels.
- **Store-wide scope.** Under continuous DDL (per-request temp tables, ETL staging,
  multi-tenant migrations) the open count never reaches zero and no cut is ever written.

The gate compiled in both builds (`check`, `check_internal`) but never produced a test result:
its one recorded run has an empty log. The rejection rests on the review and on point 1, not
on a failed run.

### The census: write groups per statement on the primary

Plain run, default build, 20 iterations, 1488 archived groups attributed by sequence interval.
Every statement produced the same count in all 20 iterations.

| Statement | Groups | What the groups hold (first iteration) |
|---|---|---|
| CREATE TABLE | 4 | schema, counter and WAL entry; constraints; a second WAL entry; `snapshot:` |
| INSERT, 12 rows | 2 | rows, versions, counter and WAL entries; `snapshot:` |
| CREATE INDEX | 3 | definition; WAL entry; `snapshot:` |
| ADD COLUMN DEFAULT, 12 rows | 15 | schema; ONE GROUP PER ROW; WAL entry; `snapshot:` |
| DROP COLUMN, 12 rows | 15 | schema; ONE GROUP PER ROW; WAL entry; `snapshot:` |
| RENAME COLUMN | 3 | schema and index definition; WAL entry; `snapshot:` |
| CREATE INDEX USING hnsw | 3 | definition; WAL entry; `snapshot:` |
| INSERT, one row, with hnsw | 3 | row; WAL entry; versions and `snapshot:` |
| DROP INDEX (hnsw) | 5 | sidecar; marker; definition; WAL entry; `snapshot:` |
| RENAME TABLE | 6 | rows, schema, counter and WAL entry; index definition; new constraints; old constraints; WAL entry; `snapshot:` |
| TRUNCATE | 2 | row deletes and WAL entry; `snapshot:` |
| INSERT after TRUNCATE, 2 rows | 2 | rows, versions, counter and WAL entries; `snapshot:` |
| BEGIN | 0 | |
| CREATE TABLE in a transaction | 3 | schema, counter and WAL entry; constraints; WAL entry |
| INSERT, UPDATE, DELETE in a transaction | 0 | staged until COMMIT |
| ADD COLUMN DEFAULT in a transaction | 2 | schema; WAL entry (the staged rows are not rewritten: finding (c)) |
| COMMIT | 2 | all the transaction's rows, versions and WAL entries; `snapshot:` |
| DROP TABLE | 2 | everything the table owns and the WAL entry; `snapshot:` |

### What stays, and what R5 must show

- `tests/physical_ddl_window.rs`. The plain run (with the census) and the paced run assert torn
  = window = pair = broken = not ready = 0 on every standby reader AND on the primary control.
  Both are `#[ignore]`d until R5. The workload keeps its original order, INSERT before the
  in-transaction ADD COLUMN DEFAULT, because R5 fixes (c). R5 passes them in both builds.
- The split self-check stays active (`internal-tests`): the applier writes one group in two
  halves and admits readers between them, and the detector must report torn answers, so a clean
  acceptance run is not a vacuous one. It runs without the primary control reader, which could
  trigger (b)'s race and fail it for a reason that is not the detector's.
- `tests/physical_ddl_window_repro.rs`: (a), (b), (c), all `#[ignore]`d. R5 turns them into
  passing tests.
- Not yet measured: the window detector has never produced a recorded run (the torn numbers
  above are from the first detector). `pg_last_wal_replay_lsn()` moves when a group is written,
  before its publication, so a standby `window` report can also mean derived state lagging the
  store; R5's acceptance is the detector's first real run.

Decision 7 is closed by this section.
