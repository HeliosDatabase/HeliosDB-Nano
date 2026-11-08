# R5.4 — every DML statement is one write group, and no committed write is lost or hidden

Parent: `docs/plans/specs/R5_statement_atomicity.md`. Part 1 (decisions) wins over Part 3
(design). This step is design §8 F1 with decisions 6 (ceiling) and 10 (params implicit
transactions), on the tree that already holds R5.1 (per-table writer lock), R5.2 (row shape) and
B4 (fork point). It also takes the DML correctness defects the R5.1, R5.2 and B4 reviews found,
because they live in the same paths. Sprinter 925803b35113 (engine write grouping) is this step.

## Why

An autocommit multi-row statement on the params family (extended protocol: psycopg server-side
binding, JDBC, sqlx, node-postgres, every REST/BaaS write) writes row by row, several groups per
row. Readers on main, a standby, and a branch fork all see it half done, and a crash keeps half.
Fast single-row UPDATE/DELETE write no version, so a later transaction and `AS OF` miss them.

## Items (design §8 F1, re-checked against the code)

1. F1.1 INSERT … SELECT: stage under autocommit too, both families, its WAL ops in the commit
   group. A failure at row k leaves nothing.
2. Decision 10: the params family runs a plan in an implicit transaction only for multi-row
   statements and ON CONFLICT (Insert with more than one row, InsertSelect, Update, Delete,
   Truncate, CTAS, CREATE MATERIALIZED VIEW, CALL). The single-row extended-protocol hot path keeps
   one batch per row. Perf-gate it.
3. F1.2 The per-row side groups of `insert_tuple_versioned_with_schema`: one `WriteBatch` per row.
4. F1.3 Fast single-row INSERT, UPDATE, DELETE: the WAL op and HNSW order as the design says. And:
   the fast UPDATE and DELETE write a version and an event in the same batch whenever a version
   store exists for the row, so a later snapshot reads them (see Findings P2, P9, D4).
5. F1.4 Params ON CONFLICT DO UPDATE stages in the statement's transaction; params TRUNCATE uses the
   staged helper inside the implicit transaction.
6. F1.5 The `snapshot:` record rides the commit group.
7. F1.6 / decision 6: the ceiling `physical::MAX_BATCH_BYTES` is enforced once, in
   `WriteGate::write`, 54000, before anything is written. It sees B4's frozen copies.

## Findings that land here (each needs a test that fails before the fix)

- P12 SEVERE, pre-existing: lost updates. Three clients running `UPDATE t SET n = n + 1 WHERE id = 1`
  over the extended protocol: 576 acked, the counter ended at 426 (466 on 4.41-era code). Under
  READ COMMITTED, PostgreSQL re-reads the row and never loses an increment. Requirement: no
  acknowledged update is lost, on either family, autocommit and in transactions; a write-write
  conflict either waits and re-reads (preferred, PostgreSQL's behaviour) or fails 40001, never
  succeeds silently over another commit. Repro: the counter in `$SCRATCH/g/r52rev_scripts/t2_race.py` (review text: `$SCRATCH/g/r52_review.txt` P12).
- P2 / P9 / D4, pre-existing: after an autocommit PK-fast UPDATE, a later transaction's scan and
  `AS OF` read the old value. Repro: `$SCRATCH/g/r52rev_scripts/t_txnread.py`, `t_tt.py`.
- P8, pre-existing: ON CONFLICT against a row the transaction staged itself: XX000 "existing row
  not found in storage" on the params family; RETURNING says 2 while the row ends at 7. CREATE
  UNIQUE INDEX misses own staged rows (R5.2 now builds over them; re-check).
  Repro: `$SCRATCH/g/r51rev_scripts/rr.py`.
- HIGH, pre-existing: text-family upsert beside a DELETE of the same keys leaves duplicate primary
  keys (519 rows over 20 ids), surviving a restart. Repro: `$SCRATCH/g/r51rev_scripts/dupcheck.py`.
- Pre-existing: an autocommit key move beside an upsert occasionally leaves a UNIQUE lookup empty
  (about 1 in 800). Repro: `$SCRATCH/g/r51rev_scripts/stress.py` with `NODDL=1`.
- Pre-existing: 150 of 167 deleted rows came back without index entries in the per-row family.
  Repro: `$SCRATCH/g/r51rev_scripts/ghost2.py PORT 500`.
- P11 SEVERE, pre-existing: the extended protocol does not see temp tables. A psycopg INSERT into a
  temp table gets 42P01; with a permanent table of the same name, the row goes into the permanent
  table. Repro: `$SCRATCH/g/r52rev_scripts/t2_temp.py`. Fix the name resolution on the params family so a
  temp table shadows a permanent one exactly as on the simple protocol.
- B4 F7: a branch fork lands inside one autocommit multi-row UPDATE, DELETE, INSERT … SELECT or
  TRUNCATE on the extended protocol. One group per statement removes it; delete the docs caveat
  (README, branches skill) once the test shows it.
- R5.2 docs: "a crash leaves one or the other" for volatile defaults waits for the crash-replay
  step; do not claim it here.

## Constraints

- Invariants of design §8 F1 (ART claims and undo, census, W2.5 watermark before the batch is
  visible, the commit's row-cache fence, HNSW undo, logical WAL atomic with data, COPY `vmeta:`
  markers, branch DML via B2 staging, one Data group per statement on the standby).
- R5.1's lock is taken at the statement; a new implicit transaction must bind to it so the lock is
  held through the commit, and the detector must stay silent.
- `Transaction::commit_with_timestamp` has one owner in this step: this one.
- Perf: pg35 ABBA against v4.41.1, DML categories within ±3% or explained, no WIN→LOSS flips;
  INSERT … SELECT throughput before and after (groups per row from about 7 to about 0.001);
  single-row extended-protocol INSERT/UPDATE throughput unchanged within noise.

## Tests

New `tests/statement_atomicity_dml.rs` with `write_groups_since(db, seq)`; the test list of design
§8 F1; plus one test per finding above, on both families and on the wire where the finding was on
the wire; a three-client lost-update test that asserts the final counter equals the acked count.

## Deliverable

Code, tests, docs, CHANGELOG text, perf numbers, and a report of every path converted and every
finding closed, with anything left open stated plainly.

## As built (what turned out different)

Code: `src/storage/row_locks.rs` (new), `Transaction::{lock_row, commit, commit_with_timestamp}`,
`StorageEngine::{write_fast_row_change, insert_tuple_versioned_with_schema, insert_tuple_fast}`,
`WriteGate::write`, the DML arms of both families in `src/lib.rs`. Tests: `tests/statement_atomicity_dml.rs`.

**P12 is a row lock, not a retry.** Every writer of an existing row (UPDATE and DELETE on both families, the PK
fast paths, both `ON CONFLICT DO UPDATE` legs) locks the row for its owner (a transaction's id, or a fresh owner for
a fast-path write) before it computes the new image, and holds it until its write is visible: a transaction to its
commit's write, a fast path to its one group. A second writer waits (off the async workers, bounded by
`lock_timeout`, `statement_timeout` and a cancel; a wait closing a cycle is 40P01 at once) and then changes the row
as committed under the lock. Re-reading every row costs a point read per row, so a statement loads its table's
change epoch before it reads its rows, every writer bumps it after its write is visible and before it lets its
locks go, and a row is re-read only if the epoch moved. A re-read row is evaluated again (WHERE, RLS, assignments);
gone, it is skipped. A validating transaction (RR, SERIALIZABLE, the embedded handle's) gets 40001 instead of a
moved row. R5.1's table lock is taken first and stays the outer lock (`branch_freeze`'s "Lock order", 2).

**Commit timestamps.** An autocommit transaction committed at its snapshot's timestamp, which can be below the commit
of the transaction it waited for; its version then sorted under the older one and a later snapshot read the older
value. A commit that holds row locks takes a fresh timestamp, announced in flight as `next_commit_timestamp` does.
The fast UPDATE and DELETE take one too (they had none, or an unannounced one).

**ON CONFLICT DO UPDATE against a row deleted meanwhile is 40001.** PostgreSQL re-runs the speculative insert;
here the arms' structure (one `match` per proposed row) made that a larger change than this step. It is a clean,
retryable refusal and never a duplicate key.

**Params-family WAL.** The params family's DML arms stage and log nothing per row, so its implicit transactions
would have committed rows with no logical-WAL entry (design §11 pre-existing defect 3). The commit derives the
entries from what it staged (`Transaction::log_staged_writes_at_commit`), into the same group; the text family's
autocommit `INSERT … SELECT` uses the same mechanism now that it stages.

**What stays as it was.** Decision 8: persistent HNSW keeps today's order relative to the row (after the row's
group in the UPDATE, DELETE and plan-path INSERT funnels, before it in the fast INSERT's batched branch). The fast
INSERT's every-64-rows counter flush stays its own (reader-invisible) group. A side-storage table's columnar values
are still written by `transform_tuple_for_column_storage` before the row's group, and a fast DELETE's columnar
nulling after it. The tenant-context exclusion of `INSERT … SELECT` staging was #100's rollout caution and is gone.

**Left open.** Branch DML takes no row lock (branch writes go through the branch view; lost updates there are not
addressed). FK cascade child writes, `Transaction::{update_tuples, delete_tuples}` (storage API) and
`update_tuples_branch_aware` / `delete_tuples_branch_aware` (no main SQL statement reaches them since decision 10)
lock nothing. A cycle through a table-lock upgrade and a row lock is not detected; the timeouts end it. The fork
test of B4 F7 is the one-group test: a fork lands between write groups (B4).

### Review fix pass (r534 review, 2026-11-07)

- **F1, the row cache.** A writer reads the row from storage (`PkLookupCache::Bypass`: the write-path PK lookups and
  the text family's PK candidate), and every invalidation of a row-cache shard (`invalidate`, `invalidate_table`,
  `clear`, the generation-fenced one) moves the shard's count under its lock; a reader's fill carries the count it read
  before its storage read (`RowCache::fill_ticket` / `put_if_ticket`) and is refused if it moved. Before, a point
  reader refilled the old image after a write's invalidation, and the next PK increment trusted it under an unmoved
  epoch.
- **F3, key holds.** A transaction's eager removal of a PRIMARY KEY / UNIQUE key (DELETE, key-moving UPDATE, the
  upsert's moved key, a cascade) records a hold in the index entry under the tree's write lock
  (`ArtIndexManager::on_delete_holding`); a claim by another transaction is refused with the hold
  (`reserve_insert_tuple_as`, `on_insert_tuple_collect_index_values_as`) and waits for the row's lock
  (`StorageEngine::wait_out_key_hold`), a writer's PK probe that misses finds the held row (`pk_hold`), and an upsert
  waits out a held arbiter key (`held_key_for`). The restore of a rollback lifts each hold with its key; a commit lets
  them all go before its row locks (`release_holds_of`, an `after_commit` hook). A hold whose owner no longer holds
  its row is stale and lifted by the claimant. Not covered: a key-moving UPDATE *into* a held value (its own
  uniqueness probe sees the key free), `MERGE BRANCH`'s key claims, TRUNCATE (its exclusive table lock keeps writers
  out anyway).
- **F4, cascades.** `ON DELETE CASCADE` / `SET NULL` lock each child (`lock_cascade_children`) and use it as
  committed (a moved child is re-checked against the parent's key). `ON UPDATE` actions remain unenforced
  altogether (not a lock matter: there is no `ON UPDATE` code path).
- **F5, branches.** Row locks and change epochs are keyed by branch level and row (`RowKey::on_level`,
  `table_epoch_on_level`); a commit moves the epochs of the (level, table) pairs it staged; the four DML arms and both
  branch upsert legs lock and re-read through the branch view.
- **F6, speculative insertion.** `upsert_conflict_settled` runs on both families: the statement holds the value of
  each arbiter key until its transaction ends (`RowKey::unique_value`), takes its verdict again, and for DO UPDATE
  locks the conflicting row and waits for it to be committed; a row deleted meanwhile becomes an insert. Replaces the
  20 × 1 ms look and the 40001.
- **F9** `ROLLBACK TO SAVEPOINT` (and a failed statement's implicit one) releases the row locks taken after it, after
  the index undo. **F12** a row or table lock wait's `statement_timeout` is counted from the statement's start
  (`STATEMENT_STARTED`, set by the outermost `SessionScopedStateGuard`). **F13** `Transaction::put` /
  `put_insert_fast` / `delete` count staged bytes and refuse (54000) once the staged `data:` rows times the copies the
  group will hold (row, version, logical-WAL op) pass the ceiling; the gate's exact check stays. Effective limit with
  the defaults: about 1.33 GiB of row data per statement or transaction.
- **F10** stays open (documented): table-lock and row-lock owners are different objects, so there is no cheap joint
  wait graph.
