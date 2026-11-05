# R5.1 — per-table writer lock (first R5 implementation step)

Parent: docs/plans/specs/R5_statement_atomicity.md. Part 1 decisions 1, 13, 15 win. Why first: every
concurrency finding of the B3 reviews (g/b3_review5.txt #2, #3, #4; the ADD COLUMN backfill race; R5
design H1) is one defect: nothing keeps a writer out of a table while something rewrites it.

## The primitive
`src/storage/table_locks.rs` (new): per engine, table name -> a fair shared/exclusive lock, entries
created on demand and removed when idle. Shared = a statement or transaction that may write the table.
Exclusive = DDL or MERGE that reads or rewrites the table's rows or catalog records. Writer preference:
a waiting exclusive holds new shared requests back, so DDL cannot starve (the shared holders it waits
for are bounded by the timeout). Reentrant for the owner: an owner set per transaction or statement, so
a statement that already holds a table never waits for it again, and an upgrade (shared held, exclusive
wanted by the same owner, e.g. DDL in an explicit transaction that wrote the table) succeeds when it is
the only holder, else 55P03 at once with the reason. Waits happen off the async workers: statements run
under offload_statement already; assert it in test builds. `[session] lock_timeout_ms` (default 30000)
bounds every wait: 55P03 "could not obtain lock on table t" naming the holder kind. Readers never touch
it. When no exclusive is held or wanted anywhere, a shared acquire costs one atomic load plus a per-owner
set insert: measure it; the perf smoke must stay green. Temp tables (`pg_temp_*`) still take it, cheaply.
Physical standby: no writers, nothing to do. Branch sessions: the lock key is (level, table), level = the
statement's branch level (main = 1), so branches do not block main; B3's exclusive drain section stays.

## Who takes it
- Shared, from BEFORE the statement reads the rows it will change, until the end of the statement
  (autocommit) or the end of the transaction (explicit). Acquire at the statement level in both DML
  families (`execute_in_transaction_inner`, `execute_plan_with_params_inner`), from the plan's target
  table; and in every fast path that bypasses plans (`try_fast_insert*`, fast UPDATE/DELETE by PK, COPY,
  batch inserts, REST writes, MCP, vector-store and code-graph writes, MV refresh's own writes). FK cascades
  and triggers: acquire on the child or target table when reached (reentrant owner), in sorted order when
  known up front. A held shared lock survives a savepoint rollback, like PostgreSQL's row-table locks.
- Exclusive: every DDL arm that reads or rewrites rows or catalog records of the table: ALTER (all forms),
  DROP TABLE (partition children, sorted), RENAME, TRUNCATE, CREATE INDEX (backfill), DROP INDEX, CREATE/DROP
  TRIGGER, REFRESH of the MV's backing table; FK DDL also on the referenced or inbound table. Multi-table: sorted order.
  Released before R1's DdlWrite::wait_replicated.
- MERGE BRANCH: exclusive on every table of its plan at the target level, from before key validation through
  the last table's landing; a resumed merge takes it again and re-validates. A merge's budget (30 s) includes it.

## What this removes
`merged_writes`, `hold_merge_landing`, `validate_and_record_with_merge_floor`, `merge_conflict_floor`,
`exempt_from_merged_writes` (conflict.rs, transaction.rs): delete them; a writer now waits. The census re-checks
in the merge added for the same race (`refuse_open_writers`, the second check under the tree locks): an open
transaction holding the shared lock is now what the merge waits for, then 55P03 after the timeout, naming that a
transaction holds the table. The `READ COMMITTED` upgrade note goes. Keep what is still needed for other reasons, and say why.

## Detector
In test builds (debug_assertions or internal-tests) `WriteGate::write/put/delete`: a write of a `data:{t}:`, `v:{t}:`,
`bdata:{L}:{t}:` key on a thread whose statement holds no lock on (level, t) panics, naming the table and the
caller; release: a counter + WARN once per table. Allow-list only what cannot race: open-time paths, WAL replay,
the standby applier, version GC, background MV maintenance if proven. Run the whole default tier with it on: every hit is a missed path.

## Tests
New tests/table_write_lock.rs + the review scripts as tests (g/b3rev5_scripts/t_generic.py params, t_generic2.py del,
t_generic3.py, t_trunc.py, t_trunc2.py; the ignored w16, w17 in tests/branch_schema_isolation.rs lose their ignore):
during a merge and during an ALTER ADD COLUMN DEFAULT over 20k rows, each writer kind runs concurrently (both families:
single-row and multi-row INSERT, UPDATE by PK, UPDATE by non-key predicate, DELETE both ways, ON CONFLICT, INSERT...SELECT,
COPY, TRUNCATE, explicit transaction before and during) and afterwards rows, every index lookup, COUNT(*) fast path and a
restart agree, and every statement either succeeded or got a clean 55P03. Timeout: a held transaction makes ALTER and MERGE
fail with 55P03 after lock_timeout_ms (set small). No deadlock: transactions locking two tables in opposite orders with DDL
waiting: the timeout ends it. Starvation: DDL goes through under a continuous writer load. Reentrancy: DDL in an explicit
transaction that wrote the table. Unrelated tables never wait. Deterministic handshakes, no sleeps.

## Rules
No cargo, no git. Both executor families. No new unwrap/expect on non-test paths. STOP and report if statement-level
acquisition cannot see a write target on some path (say which) or if the lock would have to be held across an await.
Report: acquisition sites, what was removed, the detector's allow-list with the reason per entry, anything in this spec that turned out wrong.
