# B3 — schema DDL on a branch

Plan: `docs/plans/BRANCH_ISOLATION.md` (D4). Builds on B1 (session binding) and B2 (the
branch view). Sprinter 6bc4a4040eb7 item 1 (DROP TABLE on a branch drops main's table).

## Promise being delivered

The branches skill, Recipe 1 (migration rehearsal): `USE BRANCH r; ALTER TABLE orders ADD
COLUMN currency TEXT NOT NULL DEFAULT 'USD'; UPDATE …; SELECT …` on the branch, then either
4a — drop the branch and run the same `ADD COLUMN` on main (today it fails: the column is
already on main) — or 4b — `MERGE BRANCH r INTO main` and see the column and the values on
main. "A/B test a new schema." README: "main is never touched."

## Facts (survey 2026-10-30; approximate lines)

- The catalog is global: `meta:table:{t}` (storage/catalog.rs ~4371), `meta:index:`,
  `table_constraints:{t}`, `constraint:{t}:{n}`, identity, partition and trigger keys;
  in-memory `schema_cache: DashMap<String, Schema>` keyed by table name (engine.rs ~2102);
  `Catalog::get_table_schema` (~1586) is the single resolver — its temp-table check (~1609)
  is a precedent for session-private relations.
- Rows are positional bincode `Tuple { values }` (types.rs ~553) shaped to the schema width
  by `values.resize` (engine.rs ~8572); there are no column ids.
- DDL on a branch today: CREATE TABLE and DROP TABLE are global (DROP purges main's
  `data:{t}:`, catalog.rs ~1885, and orphans the branch's rows); ADD COLUMN writes the
  global schema and rewrites main's rows (`add_column_to_rows`, engine.rs ~9737); DROP
  COLUMN rewrites `data:` only (~9863). Refused on a branch today: CREATE INDEX (ddl.rs
  ~66), DROP INDEX (~685), ADD PRIMARY KEY (lib.rs ~7693, ~15055), ADD UNIQUE (~7710,
  ~14734). The shared DDL funnel for both executor families is
  `plan_invalidates_sql_caches` (lib.rs ~2324; called ~8333 and ~19203); pre-plan DDL
  entry points also exist (`execute_create_schema_with_elements` ~8200, MV pre-parse, the
  `execute_alter_table_multi` pre-pass ~7595).
- `tests/w3_5_alter_snapshot_characterization.rs` ~167-250 PINS that a branch sees main's
  ADD COLUMN backfill; this phase (and B4) deliberately changes that — update the test to
  the new rule and say so in its comment.

## State after B1 and B2 (committed: 7c307781, 3de9ac66)

- The session's branch is a per-statement snapshot (`STATEMENT_BRANCH`); a statement on a
  branch holds that branch's drain; transactions on a branch work.
- Every row write and read on a branch goes through `src/storage/branch_view.rs`
  (`BranchView`: `get`, `scan`, `rows_with_key`, `put`/`delete`/`insert_checked`/`truncate`,
  transaction-aware variants; nearest chain level wins). Uniqueness is main's ART probe plus
  the overlay. The COMMITTED overlay is cached across statements keyed by each chain level's
  write generation plus the table width and `schema_generation` — once a branch has its own
  definition of a table, that cache key must include the BRANCH catalog's generation for that
  table too (and an isolated table must never consult main's ART for candidates).
- Refused on a branch with 0A000 today (keep refused in B3 unless this spec says otherwise):
  CREATE / REFRESH MATERIALIZED VIEW, lock-free ingestion, direct bulk load, the main-only
  fast write funnels. CREATE/DROP INDEX, ADD PRIMARY KEY, ADD UNIQUE were already refused.
- `bump_write_generation` must be called for every level a schema change rewrites rows of
  (ADD COLUMN's materialization).

## What to build

1. **Branch catalog overlay.** Keys `bmeta:{bid}:table:{t}` holding either a table
   definition (the `Schema` plus its constraints, in whatever shape the main catalog keeps
   them — keep it one record per table if possible) and an `isolated` flag, or a
   tombstone. Resolution through the session's chain: the NEAREST level with a `bmeta`
   entry for `t` decides (definition → that definition; tombstone → no such table); none →
   main's catalog. Every catalog reader goes through it: `get_table_schema`, `table_exists`,
   `list_tables`, `information_schema` / `pg_catalog` / `sqlite_master` / `\d` / PRAGMA
   introspection, the planner, and both executor families. Bypass `schema_cache` (and any
   other name-keyed schema cache) when the session is on a branch.
2. **`isolated` tables.** When `t` is isolated on level `L`, the branch view (B2) reads
   `t`'s rows from `L` and its descendants only — never from `L`'s ancestors or main.
3. **DDL on a branch** (the session is bound to branch `b`):
   - `CREATE TABLE t` (t not visible on `b`): writes `bmeta:{b}:table:t` (isolated); rows
     go to `b`'s overlay through the B2 view. Constraints are enforced by the B2 view
     (`rows_with_key` falls back to overlay scans — there is no ART for a branch-created
     table, which is fine at sandbox scale). `CREATE TABLE IF NOT EXISTS` and `CREATE
     TABLE … AS SELECT` likewise.
   - `DROP TABLE t` (visible on `b`): writes a tombstone `bmeta:{b}:table:t` and deletes
     `b`'s own `bdata`/`bdel` for `t`. Main and other branches untouched.
     `DROP TABLE IF EXISTS` likewise.
   - `ALTER TABLE t ADD COLUMN …` (single or multi-op where every op is ADD COLUMN):
     compute the new definition exactly as main would (defaults, NOT NULL, generated
     values), materialize every row `b` sees into `bdata:{b}:t:*` under the new definition
     (applying the column default/backfill rule main uses), and write `bmeta:{b}:table:t`
     with `isolated = true`. O(rows). Constraint and default semantics identical to main.
   - Every OTHER schema statement on a branch — DROP COLUMN, RENAME (table or column),
     ALTER COLUMN TYPE / SET DEFAULT / NOT NULL, constraints, indexes, views, materialized
     views, sequences, functions/procedures, triggers, schemas, `TRUNCATE` is NOT schema
     (B2 owns it) — is REFUSED before it changes anything: SQLSTATE `0A000`, message
     "<statement> is not supported on a branch (on branch b): run it on the parent, or
     rehearse it in a copy of the database (`heliosdb-nano dump` / `restore`)". Both
     executor families and every pre-plan DDL entry point.
4. **MERGE BRANCH with schema changes** (skill Recipe 4b). For each table the source
   branch defines in its own `bmeta` (relative to the target):
   - the TARGET's definition of `t` must be unchanged since the source forked (record
     enough in the branch's `bmeta` at fork/first-DDL time — e.g. the target's definition
     fingerprint — to tell); if any affected table changed on the target, refuse the
     whole merge before writing anything: SQLSTATE `55000`, naming the tables;
   - created on the branch → create it on the target (same definition), then the rows;
   - dropped on the branch → drop it on the target (its rows go with it);
   - ADD COLUMN on the branch → apply the same ADD COLUMN on the target (same backfill
     rule), then the rows (the branch's materialized rows win, as rows do today).
   Rows of tables without schema changes merge exactly as today. The merge stays
   last-writer-wins for rows (documented). All of it in one atomic write if the existing
   merge is atomic; otherwise refuse-or-complete must not leave a half-merged schema.
5. **Reclaim.** `DROP BRANCH` deletes `bmeta:{bid}:*` along with everything else (B4 wires
   the reclaim; here just make sure nothing else references a dropped branch's `bmeta`).
6. **Main's DDL while a branch exists** is B4's job (freezing the parent's definition into
   live children); in B3 a branch keeps reading main's CURRENT definition for tables it
   has not changed itself — do not try to freeze here.

## Tests (write them first)

New `tests/branch_schema_isolation.rs`, embedded text path and params/extended protocol:
- Recipe 1 verbatim on a seeded `orders` table: on the branch the column exists and the
  UPDATE applies; main has no such column (information_schema + SELECT *); 4a — after
  `DROP BRANCH`, the same `ADD COLUMN` on main succeeds; 4b — after `MERGE … INTO main`,
  main has the column and the branch's values.
- CREATE TABLE on a branch: invisible on main and on a sibling branch (catalog AND
  introspection); PK/UNIQUE enforced on it; dropped with the branch; if main later creates
  a table with the same name, the branch still sees only its own rows (isolated).
- DROP TABLE on a branch: main's table and rows intact; the branch no longer sees it;
  `SELECT` on the branch → `42P01`; recreating it on the branch starts empty.
- ADD COLUMN with a DEFAULT and NOT NULL on a populated table: every inherited row shows
  the default on the branch; main unchanged; inserts on the branch must supply/receive the
  new column.
- Refusals: DROP COLUMN, RENAME, CREATE INDEX, ADD CONSTRAINT, CREATE VIEW, CREATE SEQUENCE,
  CREATE FUNCTION on a branch → `0A000`, and the catalog is unchanged afterwards.
- MERGE refused (`55000`) when main changed an affected table's definition after the fork;
  nothing written.
- Two branches each `ADD COLUMN` different columns to the same table: each sees only its
  own.
- Branch-of-branch: a child of `b` sees `b`'s new column and table; a DROP TABLE in the
  child does not affect `b`.
- `w3_5_alter_snapshot_characterization.rs`: updated to the new rule.

## Rules

Do not run cargo, git or any build. Both executor families. No new `unwrap`/`expect` on
non-test paths. Report: files touched, the `bmeta` record format, every catalog reader
routed through the resolver, every DDL entry point and what it now does on a branch, the
merge algorithm, and anything in this spec that turned out wrong.

## Fix pass (decisions; sprinter 6bc4a4040eb7)

The first B3 tree passed its gate and a review found two high findings and several
smaller ones; three more reviews of the fixes found more, the last on a live server. This
section records what the code does now and why, after all four. It overrides the sections
above where they differ.

### MERGE: copies are not changes

`bfrz:{L}:{t}:{r}` (empty value) says "level L's entry for row r is a copy, not a change
made on L". A branch `ADD COLUMN` writes it for every row the level did not already hold
(asked of the level's own keys, never of the tuple's `branch_id`). Any write of the row
on the level clears it, in the same batch or transaction. MERGE copies only images and
tombstones without a marker; into a branch target, a merged row's marker is deleted,
because it is now the target's own change. Without this, MERGE reverted every row main
changed after the branch's `ADD COLUMN`.

### MERGE: refuse before, finish after

There are two phases and nothing in between.

1. **Validate, changing nothing.** Refusals leave the source Active and mergeable:
   - definition changed on the target (fingerprint): 55000, for a table the branch changed
     the schema of, and for one it only changed rows of (below);
   - table recreated on the target (incarnation, amendment A): 55000;
   - table gone from the target (dropped or renamed): 55000;
   - rows of a table created on the target after the fork, or of any table when the branch has
     no fork record at all (amendment A): 55000, naming what to do instead;
   - rows of a definition an unmerged ancestor owns: 55000 (an ancestor already merged into
     the target's lineage is carried, and refuses nothing; one merged into a branch since dropped
     can never reach the target, and is told so rather than to "merge that branch first");
   - a PRIMARY KEY / UNIQUE key the target already holds (amendment B): 23505;
   - a table whose changed rows would not fit in one write group: 54000;
   - a key an in-flight statement on main claimed and has not written: 55006;
   - open staged writes on the source, or on any table of the merge on the target, main or
     branch: 55006;
   - a schema change into a branch with other live descendants: 0A000;
   - main as the source, or a branch into itself: refused outright.
2. **Write the intent, synced, then roll forward.** `branch:merge:{source}` is `BMI1` +
   version + sealed bincode of `MergeIntent`. The source is marked Merged after it. An
   error from here on leaves the intent in place and says "run it again to finish it"
   (55006). Rerunning the MERGE finishes it, and so does the next open
   (`finish_pending_branch_merges`, after `rebuild_all_indexes` in both constructors,
   skipped on a physical standby, and a failure never fails the open).

Roll forward, never back: rolling back would need the pre-image of every dropped table and
of every row an `ADD COLUMN` rewrote in place, which the engine does not keep.

Every step is idempotent, decided by what the target holds now:

| Step | Already ran when | Applies when | Otherwise |
|---|---|---|---|
| `DropOnMain` | no such table | definition is `before` | refuses |
| `CreateOnMain` | main's incarnation is the definition's own, or (a WAL replay re-created it) another id on the definition's columns; re-runs the idempotent constraint writes | no such table, or definition is `before` (drops it first) | refuses |
| `AddColumnsOnMain` | columns are `base ++ all added` (still re-runs the backfill) | columns are `base ++ a prefix of added` | refuses |
| `PutOnBranch` | the level holds exactly the step's record bytes (the record is written last) | definition is `before` | refuses |

Three differences from the design:

- There is no `after` fingerprint. A created table keeps its definition's incarnation on main
  (`create_table_with_incarnation`), which identifies "this create already ran" exactly,
  including a crash between its schema and its constraint records; a fingerprint could not
  tell that partial state from a foreign table.
- A step does not compare the incarnation. The plan did, under the exclusion, and the target's
  DDL has been held off the table since. A different id at a resume can only be a WAL replay at
  open minting one for the same table (a DROP + CREATE inside the checkpoint window), which must
  not leave the merge unfinishable.
- A resume runs the key validation again, the open-writers refusal, and the live-descendants
  refusal, on the tables whose rows have not landed: main's DML stays open during a pending merge.
  An error there leaves the merge pending and changes nothing further.

`backfill_added_columns` is the idempotent sibling of `add_column_to_rows`: it extends only
rows narrower than the new width, with the same W2.5 / W3.2 fences. All main-side catalog
writes of a merge are in `apply_merge_step_to_main`; its drop is main's drop (partition
children cascade, pushdown structures go, the live trigger registry is told).

After a failed row phase, the tables that landed are whole, in main's indexes and in main's history,
and a branch target's write generations still move: the rerun may be a long way off, and main serves
meanwhile.

### What an unfinished merge holds

While an intent exists:

- The target's DDL on the merge's tables, rows-only tables included, is refused 55006. A branch
  target that inherits a table from main is exposed to main's DDL on it, so that is refused too
  (`MergeIntent.main_tables`, a shared hold: several merges may depend on one main table, but
  none may write it meanwhile).
- The refusal is in the catalog funnels (`StorageEngine::main_catalog_write`, with a bypass for
  the merge's own thread), not only at the plan gate, so `db.query`'s cold path, `DROP SCHEMA …
  CASCADE`, restore and the protocol adapter are covered. A restore on a store with an intent is
  refused outright.
- A schema statement on main holds the tables it names exclusively for its whole duration (spec R5.1,
  `main_ddl_statement_guard` -> `enter_main_statement`, which replaced a per-table count): it reads the
  exclusions, takes the locks, and reads them again. A merge registers first and then takes the same
  locks, so it waits for a statement already past its check, and only for statements on ITS tables.
  The plan is re-read after that, and what gets held is the FINAL plan's table list. A statement naming
  no table holds nothing.
- **Past that check a statement is never refused again** (third fix pass). What it holds, the catalog
  funnels under it do not look at again, because the merge is waiting for it and a refusal half way tears
  what it already did: `ALTER COLUMN … SET STORAGE` rewrites its rows before it writes the schema, and
  `CREATE INDEX` builds its tree before it records the definition. A table it takes itself, on a route
  with no statement guard (a direct catalog call, restore, the protocol adapter), is refused at the
  funnel. The merge then plans against the new state.
- Into a branch target, main's statements on the tables it inherits are waited for without taking anything
  (`wait_for_exclusive_holders`): a branch's merge does not block main's writers.
- **One budget.** A MERGE statement waits at most `BRANCH_DRAIN_TIMEOUT` (30 s) in all: closing its
  source, its section on a branch target and every wait for a table's lock share one deadline.
- `DROP TABLE` and `DROP SCHEMA … CASCADE` name the partition children they take, transitively, so
  they are refused up front when a merge holds a child, never half way through. A merged drop or
  re-create lists them among its tables too: held, and in the report of an ABORT.
- A new branch with a parent that is either end of an unfinished merge is refused 55006: it would read
  through a level part way through being rewritten, and a resume would rewrite it under the child.
- A second merge that needs one of the tables is refused 55006 (`BRANCH_MERGE_BUSY_MARKER`,
  told to retry, not to "run it again": it has nothing to finish).
- A merge of a branch another intent writes into, or into a branch with an intent of its own, is
  refused 55006: it would leave that intent unfinishable. DROP BRANCH of either end, and CREATE
  BRANCH over the source's name, are refused in `BranchManager`, so every surface is covered.

### The way out: `MERGE BRANCH x ABORT`

A merge is rolled forward whenever it can be, but not every intent CAN finish: a step can find
a state it refuses, after an operator's hand on the store, or a key conflict nobody will resolve.
Without an exit that state would hold its tables and both ends forever.

`MERGE [DATABASE] BRANCH x ABORT` puts the source back to Active, deletes the intent, moves what
serves the rows by generation (main's schema generation, or a branch target's write generations),
releases the exclusion LAST, and returns one row per table that may be partially changed, partition
children of a merged drop included; the same list goes to the log at WARN. It rebuilds nothing: on main a
table's rows landed whole or not at all, with their keys. It undoes nothing: the engine keeps no pre-image
to undo with. Merging the source again is validated from scratch, and lands every table again, over
whatever main did since; rows that landed are the merge's own, so it usually finishes, and is refused if
the target changed underneath. A physical standby refuses it up front with the standard standby refusal:
abandoning a merge is its primary's call.

Answered by the branch statement gate (`classify_branch_statement`), for the same reason as `USE
BRANCH`: the gate is in both executor families, the query path and the wire listeners, and the
params family has no grammar for it. So it works from the embedded API, both PostgreSQL protocols
(rows on simple, log only on extended), MySQL and the REPL, off the async worker on both wires, since
it can wait for the statements on the branch. No grammar change to `MERGE BRANCH …
INTO`. Metadata is written Active before the intent is deleted: a crash between the two leaves an
intent on an Active source, which the next open finishes, instead of a Merged source with nothing
to finish it.

### Amendment A: table incarnations

MERGE must refuse `DROP TABLE t` + an identical `CREATE TABLE t` on main (plan D4: the
target's definition changed since the fork). The fingerprint cannot see that.

- `meta:table_incarnation:{t}`: a **random u64**, not a counter, so no counter key has to
  stay durable across a crash or a replica. Written in CREATE TABLE's batch (schema, row
  counter and id are now one batch), deleted in DROP's one-group purge, moved by RENAME in
  its batch (every rename: relation, MV swap, WAL replay). Classified
  `KeyClass::MetaTableIncarnation`; it replicates as catalog, and no standby state derives
  from it.
- Tables with no key get one at the first writable open, in one batch
  (`Catalog::backfill_table_incarnations`), never on a standby.
- **Compared separately from the fingerprint, not an input to it.** A merged CREATE gives
  the target the definition's id, and an `ADD COLUMN` keeps the base's; keeping the two
  apart means each answers one question.
- `BranchTableDefinition.incarnation`: fresh for a branch-created table, the base's for
  ADD COLUMN, main's for main's. `BranchTableRecord.parent_incarnation`: the parent's at the
  level's first entry for the table.
- **One fork record per branch**: `bmeta:{id}:fork` = `BMF1` + version + bincode of
  `table -> ForkEntry { incarnation, fingerprint, columns }` for every table the parent sees,
  written in the branch's creation batch (`BranchManager::create_branch_with`). So the check
  covers a branch that changed only rows, and a first DDL that ran after main recreated the table.
- **A table with no incarnation is UNKNOWN, never different** (`incarnations_differ`): that
  only happens after a failed backfill, and the backfill that later succeeds must not turn into
  a false refusal. A branch's `ADD COLUMN` of such a table mints an id for its own definition,
  because the id also salts the key index.

The checks: a table with a record needs `target now == record.parent_incarnation`, and
`F(t) == record.parent_incarnation` when the fork record has `t`. A table with only rows
needs `target now == F(t)` when the fork record has `t`.

**A rows-only merge across a definition change is refused too** (second review, H2): a table the
branch changed rows of must have, on the target, the fingerprint it had at the fork, or the merge
is refused 55000 naming the table and what changed (the columns then and now, or "its constraints
or unique indexes"). Fail closed, no row shaping: rows are copied by position, so `DROP COLUMN` on
main used to land `(1,10,21)` under a two-column schema and main then read `b = 10`, and `ADD
COLUMN … NOT NULL` landed NULL. The same holds for a table with a record: `F(t)` must be the
definition the branch first changed. A table gone from the target says so, instead of "dropped and
created again" (it may have been renamed).

**Nothing to compare fails closed** (third fix pass; the same `(1,10,21)` recurs otherwise):

- A table with no fork entry that the branch changed rows of was created on the target after the fork.
  It is read live until B4 makes it invisible to the branch. MERGE refuses 55000, saying it was created
  on the target after the branch forked, and to copy the rows by hand or recreate the branch.
- A branch with no fork record at all was created by an earlier release. MERGE refuses 55000 for any
  table it changed rows of, saying its fork point is not recorded, and to recreate the branch or copy
  its rows. A branch that changed no rows has nothing to place, and merges.

Residual, safe direction: a WAL replay that re-runs DROP + CREATE mints a new id, so a MERGE
across it is refused rather than mis-merged.

### Amendment B: MERGE never duplicates a key

Validated before the intent, with the source Active. For every image the merge will put on
the target (own images, minus copies and tombstones), each rule's key is probed on the
target, ignoring holders the merge itself deletes or replaces by row id, plus a check among
the merged rows. A table the branch created replaces the target's, so only the merged rows
can collide. Key positions come by column name from the source's definition, so a wider
source row is read correctly. The refusal is 23505 naming the table, the constraint and the
key. The row phase's claims are what arbitrates after that.

### A table lands as one main transaction (fourth fix pass)

The row phase used to be a write path of its own beside main's, and each property it lacked was
a defect of its own: no versions and no version index (`AS OF`, and a transaction begun after the
merge read the pre-merge row), no logical WAL (a crash replayed older entries over a finished merge),
no replication wait, no conflict detection (a COMMIT silently put a pre-merge row back), no columnar
store (`SELECT v` read NULL), no atomicity between a table's keys and its rows. So it is gone. Into
main, each table's rows land as **one ordinary transaction**
(`StorageEngine::land_merged_table_on_main`): `begin_transaction`, `txn.put` of `data:{t}:{r}` for
images, `txn.delete` for deletions, row ids preserved, `next_commit_timestamp` and
`commit_with_timestamp`, what a main multi-row UPDATE in a transaction runs. From that commit come
versions and the version index, the copy-marker and elided-version materialization, the logical WAL in
the same batch, the physical write group, the replication wait, the conflict registry, the row cache
fence, the watermark and the snapshot record. A reader or a crash sees a table's merge wholly or not at
all, and a merge's memory is the size of the changed rows of its largest table.

A table is the unit, not the merge: two tables land in two commits, and a merge stopped between them
has landed the first. A merge into a branch is unchanged: a level has no versions and no logical WAL,
and the merge runs on it alone, in its exclusive section, with per-row batches.

**Size.** One write group is never split, and above `physical::MAX_BATCH_BYTES` it could be neither
served nor archived. What a table's merge would stage (key and value of each changed row, three times
over: `data:`, `v:`, the WAL record) is compared with that limit before the intent and again at a
resume, and refused 54000 with nothing changed, naming the table and the alternative.

**Columnar.** The rows go through what a transaction's INSERT, UPDATE and DELETE go through:
`stage_columnar_rows_grouped_in_transaction` (presence marked for a new row, not for an update) and
`stage_columnar_deletes_grouped_in_transaction`, in the same write set, so the column store commits with
the rows, on tables the merge created and on existing ones. A dictionary or content-addressed value is
inline in the row, as a transaction's UPDATE leaves it, and every reader resolves it. No case had to be
refused.

**Keys.** `ArtIndexManager::claim_merged_table_keys`, before the commit, all of the table's key
changes in one critical section per tree, under its write lock (one tree lock at a time, rule 3;
enforcing trees first, since only they can refuse). In each: look for another writer (below); refuse a key
held by a row that is not giving it up in this merge (55006, the intent stays, nothing of the table
landed, the trees before it put back); then move the keys that pass between merged rows and enter the
free ones. A swap, a chain and a key a deletion frees are not special cases, and at no moment is a key
free for someone else to take. Dense-int statistics count what enters and leaves, not what moves. Keys only
an old image holds stay with that row until the commit succeeded (`MergedKeysClaim::finish`), so every key
always has an owner, and when the commit fails, dropping the claim puts everything back and cannot fail
for lack of a key. Between the commit and `finish` a lookup by such a key finds the row with its new
image, the window a transaction's UPDATE has on its other side. The transaction's census slots are held
across all of it, as a transactional UPDATE's are.

**Reruns.** The commit also writes `MergeIntent::landed`, in the same batch. A rerun, and the open-time
finish, leave a landed table alone: its rows are main's now, and main may have changed them since. Those
changes are not the merge's to undo, and validation does not hold them against it. ABORT forgets the list:
merging the source again starts over, last-writer-wins.

**Row counters.** A table the merge created is writable on main from its create step, also while the merge
is pending, so `CreateOnMain` raises and flushes its counter to the source's largest changed row id before
any row lands, on a rerun too. Every table's commit stages its counter with its rows.

### Writers on main during a merge

Spec R5.1 (`R5_1_writer_lock.md`). A merge holds the per-table writer lock (`storage::table_locks`) exclusively on
every table of its plan, on the target level, from before its key validation through the last table's landing; a resumed
merge takes it again and validates again under it. A writer holds it shared from before it reads the rows it will
change until the end of its statement or transaction. So no writer runs beside a landing: it waits, and then it reads
what landed. The wait is bounded by `[session] lock_timeout_ms` and the merge's own 30 s budget includes it: past that,
55P03, naming what holds the table, with nothing changed.

- **An open transaction with staged writes** of a merge table holds the lock until it ends, so the merge waits for it,
  whichever level: the target's on those tables, the source's on any table it has staged rows of. A merge inside the
  very transaction that wrote the table is refused at once (55P03): it cannot end while its own statement waits.
- **An autocommit writer** finishes first, or waits for the merge and then gets its ordinary answer: an `INSERT` of a key
  the merge took gets 23505.
- **A statement that waited reads after what it waited for.** Its transaction's snapshot was taken before the lock, so an
  autocommit or READ COMMITTED transaction takes a new one when a schema statement or a merge has ended since (the
  schema generation says so). A transaction that validates keeps its snapshot, and first-committer-wins refuses its
  COMMIT with a serialization failure (40001): REPEATABLE READ, and the embedded global transaction.

What stays from before the lock, because it is not about writers:

- `claim_merged_table_keys` still moves a table's keys in one critical section per tree, so a swap, a chain and a key
  freed by a deletion are one step, and a failed commit puts everything back. Its refusal of a key held by a row outside the
  merge (`HeldBy`) and validation's `main_key_held_by_an_uncommitted_row` can no longer be reached by a SQL writer;
  they stay as fail-safes, cost nothing, and the tests reach them by taking a key in the tree directly.
- The exclusion (`MERGE_EXCLUSIONS`), rebuilt from the intent at open: a pending merge must refuse DDL with no lock held
  by anybody, also after a restart. A pending merge leaves main's DML open, as before.
- The census slots held across the landing: they keep a `COUNT` off the index while its keys settle. That is about
  readers, who take no lock.

### What stays open

**Closed:**

- A duplicate key from any writer, on any path.
- A transaction's COMMIT overwriting a landed row, at any isolation level: it waited, and read the landed row, or it gets
  40001 from first-committer-wins.
- A rollback restoring a key the merge took: the merge waits for the transaction.
- A table's landing visible in part, or half replayed after a crash.
- **The params family's multi-row UPDATE and DELETE** (`update_tuples_branch_aware`, `delete_tuples_branch_aware`), the
  default path of psycopg, JDBC, sqlx and node-postgres, and of the REST routes: the statement holds the table's lock from
  before it reads its rows. The review's numbers (686 rows back to their pre-merge image, 1920 deleted rows back) are now
  tests (`a_multi_row_update_during_a_merge_keeps_what_landed`, `a_multi_row_delete_during_a_merge_stays_deleted`).
- **TRUNCATE**: it waits for the merge, and can no longer fail at commit with its indexes cleared, since nothing is left to
  make an autocommit commit fail.
- **A key-moving statement that ended in 40001**: there is no 40001, and no undo over a merge's keys.
- **The primary-key fast paths on the very row a merge lands**: they lock before they read the row.
- **`ADD COLUMN`'s backfill beside an autocommit UPDATE**, on main and through a merge: they exclude each other.
  `an_add_column_backfill_survives_autocommit_updates` is no longer ignored.
- **A false 40001** for a READ COMMITTED transaction spanning a merge: the floor is gone.

**Still open:**

- **`ADD COLUMN`'s backfill after a crash** is lost, on main and through a merge, with `SELECT x … WHERE id = 3` failing with
  `Column index 2 out of bounds in tuple`, unless the merge was still pending, because then the step reruns. R5's
  `meta:attmissing:` does away with the rewrite (decision 4).
- **Crash replay (R5 decision 14).** Pre-existing, on main with no branch, after an earlier clean close left a checkpoint. An
  autocommit PK UPDATE or DELETE of a row whose multi-row INSERT is above the checkpoint is undone by the replay, and that
  now includes a PK UPDATE made after a merge, which falls back to the merged image (the merged rows themselves survive: their
  WAL records ride their commit). DROP + CREATE puts the old rows into the new table, and CREATE + DROP brings the table
  back, through a merge as on main. Decision 14 stops the replay re-applying entries at open.

Into a branch target the same lock applies at the target's level, inside the exclusive section the merge already ran in.

### Merge into a branch target

The target ends with the source's definition and its own prior view under it: its view is
materialized first (copies marked), then the source's own rows land. The merged record is
computed at plan time and stored in the step, so a rerun writes the same bytes.

### Concurrency

- `BranchDrains::exclusive`: every branch DDL that is not a no-op runs on the branch alone, CTAS
  through its population (a no-op such as `CREATE TABLE IF NOT EXISTS` takes no section, and the
  question is asked again once the section is held). Other statements finish first, new ones
  wait (bounded, then 55006); a second section is refused at once, so two cannot wait on each
  other's shared hold. EVERY merge into a branch takes the same section, not only a
  schema-changing one. CREATE BRANCH of a child takes a shared hold of the parent first, so it
  waits.
- `PENDING` registry per (level, table) of transactions with staged writes: the DDL refuses
  55006. A merge waits for them on the table's writer lock instead (R5.1), on any table of its
  source (a COMMIT there would land inside the validation window, since the source is still Active)
  and on the merge's tables of a branch target; `PENDING` names the source's tables it has to wait
  for. ROLLBACK TO SAVEPOINT keeps the entry (conservative).
- Preflight order, after the no-op checks: own staged writes, live descendants (0A000; walked
  through merged branches, which stay in their children's chains), other transactions' staged
  writes (55006), a pending merge into this level (55006).

### Scale

Isolated tables have a durable per-level key index,
`bkey:{L}:{hex t}:{hex set}:{hex digest}:{row}`, one entry per visible own row per key set
(PRIMARY KEY / UNIQUE rules and FOREIGN KEY column sets). `rows_with_key` on a covered set
costs O(levels + candidates), verifies each candidate through `get`, and loads no overlay.
The entry holds a SHA-256 digest of the encoded key, truncated to 128 bits and salted with
the definition's incarnation id, because RocksDB keys are not sealed at rest and a key value
in one would have broken the at-rest promise. The salt is random and sealed in the catalog
record; the levels that share a definition share it.
Writes keep it in the same batch or transaction, and a merged row carries its entries in its own
batch (`apply_merged_row`; there is no rebuild afterwards, which left the target answering "no such
key" for rows already on it). `BranchView::materialize` streams main's rows and writes the copies
10,000 per batch (or 256 MiB, whichever comes first); the rows the level CHANGED (held and not marked)
go in the last batch, with the record. So a crash between batches leaves only marked copies under the
old definition, which MERGE ignores and a retry rewrites, and a retry does not grow the last batch: the
copies of a failed attempt stay marked and chunked. A row the level CHANGED is never wider than its
definition: MERGE copies it by position, and the next main `ADD COLUMN` would have read the stale
backfill value. The last batch cannot be split, so it is built first, and if it would not fit in one
write group (`physical::MAX_BATCH_BYTES`, which can be neither served nor archived above that) the
statement is refused 54000 before anything is written, naming the table and the alternative (merge or
copy those rows in smaller steps). The branch reverse-FK
answer is cached per (branch, table), validated by the schema generation. These keys are
not "row writes" to the census (`note_written_key`), so an open branch transaction does not
demote main's index fast paths.

### Other

`SELECT … LIMIT n` on a branch reads the branch: the executor's LIMIT pushdown walked main's `data:` keys
and stands down there now (an overlay's rows were missing, and what the branch deleted was back).
`CREATE [DATABASE] BRANCH x PARENT p` is the same clause as `FROM p`; it was accepted and ignored. REST data
routes read the catalog inside the request's branch scope; the insert route reports the
row id the storage insert returned, from the counter the SQL insert uses (it used to burn another one
of the durable counter, which ignores the scope); a branch reports no `row_count`, since statistics
describe main's rows. A schema statement on any query route (`db.query`, the result-set routes, a
session transaction's, the params read route, the RAII `Transaction` query) applies to the branch like
any other (it reached the executor's backstop and got a wrong 0A000), and on main it holds its tables and is
checked, and kept out of the plan cache, whose hit path would skip that. `DROP TABLE` on a branch takes
partition children with it. The open-time finish asks
the write gate, so a fenced primary does not tear down indexes trying. Code-graph DDL
refuses on a branch. `pg_indexes` and `\d` hide main's manual and vector indexes of a table
the branch overrides. The fingerprint canonicalizes constraint order and includes unique
column sets. A tombstone of a table the parent never had is not a change. Deferred: stable
table OIDs (`pg_table_oid` is positional on main too).

### What B4 inherits

- `bfrz:` is shared: same clearing and MERGE rules. The ADD COLUMN copy and B4's frozen
  pre-image are the same thing to everything that reads the marker.
- The reclaim prefixes gain `bkey:{id}:`.
- Reclaim at open runs AFTER `finish_pending_branch_merges` and skips sources with an intent.
- A frozen `bmeta` record's `parent_fingerprint` is the frozen definition's fingerprint
  (`None` for a frozen tombstone), and its `parent_incarnation` is that definition's
  incarnation (`None` for a tombstone).
- Lift the live-descendants DDL refusal, and the one on a schema-changing MERGE into a branch,
  once the freeze hook covers the parent's `bmeta:` and `bdata:` writes. The walk goes through
  merged branches: the hook must too.
- Lift the "created on the target after the fork" refusal of amendment A: with fork-point reads such
  a table is not visible to the branch at all, so it cannot have changed rows of it. The refusal for a
  branch with no fork record stays: such a branch has no fork point to read from either.
- MERGE into main lands each table as one main transaction, so what it reads and writes is what any main
  commit does. A deleted row is absent from `AS OF` on main at every point (`scan_table_at_snapshot` starts
  from the current rows); B4's delete events change that for a merged deletion as for main's own.
- `ForkEntry` is the fork point's natural home for what B4 needs per table. `bmeta:{id}:fork` is
  under `bmeta:{id}:`, so it goes with the reclaim.
- A merged branch stays in its children's chains (M4, M5): reclaim of a Merged branch with live
  descendants must wait for them, or freeze first.
- A `MERGE BRANCH x ABORT` puts a source back to Active with its overlay intact: the reclaim of a
  Merged branch must re-check the state, and skip while an intent exists.
- R5 will replace `AddColumnsOnMain`'s backfill with `meta:attmissing:`. Keep
  `backfill_added_columns` small and `apply_merge_step_to_main` the only main-side writer.
