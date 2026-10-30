# Branch isolation — plan (Sprinter 6bc4a4040eb7, before 4.42.0)

## Why

`README.md` and `.claude/skills/heliosdb-nano-branches/SKILL.md` promise isolated,
copy-on-write, fork-test-discard sandboxes: "test a destructive change (a migration, an
experiment, an AI agent's writes) against real data, then discard the branch. `main` is
never touched." Measured on 4.41.1 and in a read-only survey of this branch (2026-10-30),
the engine does not deliver that:

1. The active branch is engine-wide (`StorageEngine.current_branch`): one connection's
   `USE BRANCH` moves every other connection.
2. Schema DDL on a branch changes main. `DROP TABLE` on a branch drops main's table, and
   `TRUNCATE` on a branch deletes main's rows (data loss).
3. PRIMARY KEY and UNIQUE are not enforced on a branch (branch rows are in no index; the
   text literal fast path checks nothing).
4. `INSERT … ON CONFLICT DO UPDATE` on a branch writes main's key; an explicit transaction
   on a branch reads main, not the branch; the params family's in-transaction DELETE and
   FK cascades remove `bdata:` without a `bdel:` tombstone, so the parent row stays
   visible on the branch.
5. A branch reads its parent LIVE: writes to main after the fork show through, and the
   `AS OF TIMESTAMP` anchor is stored but no read path uses it.
6. Dropping a branch reclaims nothing (GC never runs, and scans a binary key format the
   SQL path never writes); a dropped branch can still be `USE`d and written; its name
   cannot be reused, and `CREATE BRANCH IF NOT EXISTS` resurrects its rows.
7. REST `/v1/branches/:name/query` runs on main; MCP's branch switch is a global
   switch-and-restore, not atomic across concurrent requests.

Standing rule: build what the docs promise; honouring it must fail closed.

## Decisions

- **D1 — the active branch is session state.** Each PostgreSQL and MySQL connection, each
  embedded/REPL session, each REST request under `/v1/branches/:name/…` and each MCP call
  that names a branch has its own active branch. `USE BRANCH` changes only its own
  session. Nothing engine-wide selects a branch for a session.
- **D2 — writes on a branch change only that branch.** Every write path (both DML executor
  families and their fast paths, explicit transactions, `ON CONFLICT`, FK cascades,
  `TRUNCATE`, COPY) writes the branch's overlay; only `MERGE BRANCH … INTO` writes its
  target.
- **D3 — constraints hold on a branch.** PRIMARY KEY and UNIQUE are enforced over the rows
  the branch sees (its own rows plus the rows it inherits and has not changed or deleted).
- **D4 — schema DDL on a branch.** `CREATE TABLE`, `DROP TABLE` and `ALTER TABLE … ADD
  COLUMN` apply to the branch only (a branch catalog overlay). Every other schema
  statement on a branch is refused before it changes anything, naming the alternative
  (run it on the parent, or rehearse it in a dump-restored copy). `MERGE BRANCH` carries
  the branch's schema changes into the target when the target's definition of each
  affected table has not changed since the fork, and is refused otherwise (skill recipe
  4b). `TRUNCATE` on a branch empties the table on that branch only (PostgreSQL
  semantics; the skill's pitfall line is corrected).
- **D5 — a branch is a fork.** A branch sees its parent's data as it was at its fork
  point — the `AS OF` anchor, or the creation time when `AS OF` is omitted or `NOW` — and
  writes to the parent afterwards do not appear (skill recipe 2). Design in phase B4.
- **D6 — dropping reclaims.** Dropped and merged branches' rows, tombstones, versions and
  catalog entries are deleted using the key formats they were written with; a dropped
  branch cannot be `USE`d; its name can be reused.
- **D7 — docs.** `AS OF` is optional (default `NOW`); per-session branches; selecting a
  branch at connection startup; TRUNCATE semantics; what a branch refuses.

## Phases (each: spec → implementation → adversarial review → fix → gate → commit)

- **B1 — session-scoped branch** (`specs/B1_session_branch.md`): D1, the dropped-branch
  lookup of D6, every cache that assumed a global switch, REST/MCP request scope, branch
  selection at PostgreSQL startup, MCP branch drop.
- **B2 — writes and constraints on a branch** (`specs/B2_branch_writes.md`): D2, D3.
- **B3 — branch-scoped schema** (`specs/B3_branch_schema.md`): D4, catalog reclaim.
- **B4 — fork-point reads, reclaim, docs** (`specs/B4_branch_fork_point.md`): D5, D6, D7.

The long-statement stall (Sprinter ccebc95977f1) is fixed alongside, in
`specs/S1_statement_off_worker.md`.
