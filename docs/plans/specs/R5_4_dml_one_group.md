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
