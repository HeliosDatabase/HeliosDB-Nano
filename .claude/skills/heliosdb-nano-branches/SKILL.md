---
name: heliosdb-nano-branches
description: Database branching in HeliosDB-Nano — ephemeral fork-test-discard sandboxes. Create a branch from main or any other branch (a fork: what the parent does later does not show on it), make isolated changes, validate, then discard. The primary pattern for agent sandboxes, migration rehearsals, A/B experiments, and short-lived "what if" workspaces. MERGE BRANCH does move rows, but it is last-writer-wins with no conflict detection — prefer fork-test-discard for anything you cannot verify afterwards. Use this when the user says "branch", "fork the database", or wants to try a destructive change without affecting production data.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Database Branching — Fork-Test-Discard Sandboxes

**Verified against HeliosDB-Nano 4.42.0.** Syntax below matches the green test suite
(`tests/branch_sql_integration_tests.rs`, `tests/branch_merge_surface_tests.rs`,
`tests/branch_session_isolation.rs`, `tests/branch_write_isolation.rs`,
`tests/branch_schema_isolation.rs`, `tests/branch_fork_point.rs`).

Branches are ephemeral copy-on-write forks. The recommended lifecycle is
**fork → test → discard**: create a branch, run the risky work there, validate,
then `DROP` the branch and (if the change passed) re-apply the validated SQL to
`main`.

## A branch is a fork

A branch sees its parent as it was at its fork point: the `AS OF` anchor, or the creation
when there is none. What the parent does after that does not show on it: rows, `ADD COLUMN`,
`DROP TABLE`, a table it creates. What the branch does shows only on it.

`AS OF` is optional and defaults to `NOW`:

```sql
CREATE DATABASE BRANCH dev FROM main;                        -- the same as AS OF NOW
CREATE DATABASE BRANCH dev FROM main AS OF NOW;
CREATE DATABASE BRANCH rewind FROM main AS OF TIMESTAMP '2026-08-24 09:00:00';
```

How it works, for what it costs: the parent's write copies what a live child sees into the
child's own overlay, marked as a copy, in the same write. So reads stay what they were (an overlay
over the parent), and a branch pins no version history.

Per write of the level with live children, for each row and each of those children:

- **A row the child still reads from its parent:** two point reads to see it holds nothing, one for the
  pre-image, and two keys in the write's batch. Once per row and child: then the child holds it.
- **A row the child already holds:** at most two point reads. Every write of the row, every child: this does
  not get cheaper after the first touch. At 100 live branches of main an `UPDATE` by primary key was about 16
  times slower, first touch or not (1.4 times with one, 2.6 with ten; debug build, as ratios). **Keep few
  branches live.** Dropping them all restores the speed with none.
- **A table the child owns:** none, past one read of the table's record per batch.
- **Any other key** (indexes, versions, catalog): a prefix comparison.
- **A large batch** (`DROP TABLE`, a large transaction, over 64 rows): the copies are written first, in chunks
  of 10,000 rows or 64 MiB, so memory does not grow with the table. Writers of its tables wait until it is
  applied; writers of other tables do not.
- **No branch at all:** the hook is one atomic load per write. That is the hook only: with time travel on,
  `DELETE` writes a delete event per row, branch or no branch, and an autocommit `DELETE` by primary key a
  snapshot record too, so `AS OF` can return the row (about 15% fewer such `DELETE`s per second, debug build).

**A schema statement on main is wholly before a fork of main or wholly after it**, never across one, and so
are `CREATE` and `REFRESH MATERIALIZED VIEW` and a `CREATE SCHEMA` with elements: `CREATE BRANCH` waits for the ones
running (30 s, then `55006`), and new ones wait for it to register. So a branch's definition of a table and its rows
always come from the same side of the statement.

**A fork lands between writes, not between DML statements.** A transaction is one write, and so is a statement on
the simple protocol. On the extended protocol an autocommit `UPDATE`, `DELETE`, `INSERT … SELECT` or `TRUNCATE` of
many rows on main is written row by row, so a fork can land inside it, as main's own readers can see it half done.
Run it in a transaction if a branch must not.

`CREATE BRANCH … AS OF` a past time rewrites nothing: it writes the rows that differ between the
anchor and now, once, at creation, in chunks. If it fails or crashes there is no such branch; the next
open deletes what it wrote. It needs `time_travel_enabled`, an anchor inside the retention window, and
main as the parent: a branch keeps no history. Definitions are the CURRENT ones: a column added since the
anchor shows on the branch, a table dropped since is gone, a table created since is there and empty.

**An anchor is refused (`0A000`, naming the table or view and why) when a row as it was cannot be read now.** A
version is positional and nothing rewrites it, and a rename or a `REFRESH` deletes it. So: an anchor before a `DROP
COLUMN`, `RENAME COLUMN`, `ALTER COLUMN … SET STORAGE` or `RENAME TO` of a table, or a `REFRESH` of a materialized
view; and a later anchor, if a row's last version at it is from before that statement and the row has changed
since (a row not changed since is what it is now). The statement records this in its own write,
`meta:table_shape:{t}`. A rename records it on the old name too, so a table that takes that name refuses an anchor
from before the rename, whatever becomes of the renamed table. A `DROP TABLE` takes the table's own record, and its
version history. **Such a statement run before 4.42.0 left no record**: nothing refuses an anchor before it, and its
rows read by position, wrongly.

Two consequences to expect. **One table's change refuses the branch as a whole**, whichever table you care
about: a branch holds every table at its anchor or none. And **it goes on for later anchors**: until each row last
versioned before the statement is written again (a transaction's `UPDATE` of it is enough), an anchor is refused
once such a row has changed after it. `AS OF NOW` always works, and so does a time after the last such change.

**What `AS OF` can see.** Version history, and two things are not in it. A row deleted before 4.42.0 has no
delete event: it is absent at every time, as it always was. An autocommit `UPDATE` on main by primary key, or
of several rows on the extended protocol, records no version `AS OF` can find, so `AS OF` shows such a row as
of its last versioned write (a transaction's `UPDATE` is versioned). This is the same for `SELECT … AS OF` and for a branch anchored there.
`SELECT … AS OF` on a session bound to a branch is refused (`0A000`): only main has history.

**A branch created by an earlier release** has no recorded fork point. It reads what its parent held when
4.42.0 first opened the store, and from then on it is a fork. `AS OF` it was given is not honoured.
`MERGE` refuses it for any table it changed rows of (`55000`). Recreate it to get a real one.

## Per session

The branch is session state. `USE BRANCH x` moves that session only; so do `SET heliosdb.branch = 'x'` and
the PostgreSQL startup forms `dbname=<db>@<branch>` and `options=-c heliosdb.branch=<branch>`. REST
`/v1/branches/:name/…` and an MCP call that names a branch run on it for that request. A physical standby
takes all of them: binding a session writes nothing. **HeliosProxy's `hba` and tenant rules do not match
`db@branch` yet: do not rely on it through the proxy.**

## What holds on a branch

- PRIMARY KEY, UNIQUE and FOREIGN KEY hold over the rows the branch sees: its own and what it inherits.
- `TRUNCATE` empties the table on that branch only.
- Sequences are shared: `nextval` on a branch moves main's.
- Transactions work: `BEGIN … COMMIT` on a branch is atomic there. Its reads are READ COMMITTED.
- `CREATE TABLE`, `CREATE TABLE … AS`, `DROP TABLE` and `ALTER TABLE … ADD COLUMN` apply to the branch only.
  Every other schema statement is refused (`0A000`) before it changes anything, naming the alternative:
  run it on the parent, or rehearse it in a `dump` / `restore` copy.

## What is not forked: views, materialized views, functions, procedures

All branches share them, as they share sequences. What main does to one shows on every branch at once,
whatever its fork point:

```sql
CREATE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql;
CREATE BRANCH dev;
CREATE OR REPLACE FUNCTION dbl(x INT) RETURNS INT AS $$ SELECT $1 * 3 $$ LANGUAGE sql;
USE BRANCH dev;
SELECT dbl(21);    -- 63, not 42
```

After `DROP VIEW v` on main, `SELECT * FROM v` on a branch fails (`42P01`) though the tables it read are all
there. `CREATE OR REPLACE VIEW` changes what it returns there, and a view main creates appears there. A view's body
reads the session's branch. A materialized view's rows are a table's rows and are forked: `REFRESH` on main does not
change what a branch reads from it, but `DROP MATERIALIZED VIEW` on main removes it there. A branch session cannot
change any of these: `CREATE`, `REPLACE`, `DROP` and `REFRESH` of them on a branch are refused (`0A000`), so a branch
cannot change them for main either.

## When to use
- Give an LLM agent a sandboxed copy-on-write workspace (the primary use).
- Rehearse a migration on production data without touching production.
- A/B test a new schema or query plan.
- Compare aggregates across "real" vs. "what if".

## MERGE BRANCH — what is actually true

`MERGE BRANCH dev INTO main` **works and moves rows.** Pinned by
`tests/branch_merge_surface_tests.rs`, which asserts row *content* on the target after
a merge, plus branch deletions carrying over, rows unique to each side surviving, and
large diffs applying.

**But conflicts are not detected.** Per `CHANGELOG.md` [4.16.0] *"Known limitations"*:

> Branch merging is last-writer-wins. Conflicts are not detected and merge strategies
> are not implemented.

Consequences:
- If both `main` and the branch changed the same row, **you get no warning** — one
  value silently wins.
- `MERGE BRANCH … WITH (conflict_resolution = 'branch_wins' | 'target_wins')` now
  **errors with *not implemented***. It previously returned success while doing
  nothing. Remove the option to get last-writer-wins, which is what you were
  already getting.
- `WITH (delete_branch_after = true)` **is** honoured and works. On a branch with a child, merged or not,
  it is refused (`2BP01`, naming the child) before anything is merged, as the drop would be.

A merge carries what the branch itself changed. A row it only copied, because its parent changed it,
is no change: the target keeps its own. So a row the target changed after the fork stays the target's
unless the branch changed that same row, and then the branch's wins. The branch's schema changes go too,
unless the target's definition moved (below). When the merge has finished, the source's keys are deleted,
once no live branch reads through it.

**Guidance:** agent runs should always discard, never merge: an agent cannot verify a last-writer-wins
outcome. For anything else, check what the target did since the fork, or fork-test-discard and re-run the
validated SQL on the target.

### What MERGE refuses, and what it finishes

Two writers of the same row are still last-writer-wins, as above. These are not: each
is refused **before anything changes**, the source stays `Active`, and it can be merged
once the cause is gone.

| SQLSTATE | Refused when |
|---|---|
| `55000` | The target changed the definition of a table the branch changed (columns, constraints, a new UNIQUE index) |
| `55000` | The target **dropped and recreated** a table since the fork, even with an identical definition, and the branch changed its schema or only its rows |
| `55000` | The target changed the definition of, renamed or dropped a table since the fork, and the branch changed its rows: they were written for the old definition and are copied by position. The message names the table and what changed |
| `55000` | The branch was **created by an earlier release**, so its fork point is not recorded, and it changed rows. Recreate it from the current target, or copy its rows. A branch that changed no rows still merges |
| `55000` | The branch wrote rows of a table whose definition comes from an ancestor branch that is not merged into the target, or that merged into a branch since dropped |
| `23505` | A row the merge would put on the target has a PRIMARY KEY / UNIQUE key another target row holds. The message names the table, the constraint and the key |
| `55006` | An open transaction has staged writes on the source branch, or of one of the merge's tables on the target, `main` included: retry after it commits or rolls back |
| `55006` | A statement on `main` has claimed a key a merged row holds and has not written its row yet: retry |
| `55006` | Another merge has not finished and holds one of the tables, or a schema statement on one of the merge's tables is still running after the statement's 30 s budget: retry |
| `54000` | A merge or `ADD COLUMN` on a branch carries more changed rows of a table than one atomic write can hold. Nothing was changed. The message names the table: copy those rows in smaller transactions, or change fewer per branch |

`MERGE BRANCH main INTO x`, and a branch into itself, are refused outright.

On `main`, each table's changes land as **one ordinary transaction**: versions, logical WAL, replication
wait, conflict detection and the column store all come from its commit. A reader or a crash sees a table's
merge wholly or not at all, and `AS OF` sees it like any commit. Tables land one after another, and the merge
holds a table's changed rows in memory.

The rows a branch `ADD COLUMN` copied are not changes either: a row `main` updated or deleted
after the branch's `ADD COLUMN` keeps `main`'s change, and `main` reads the same default for it: a
merge adds the column to `main` by missing value, one atomic write that rewrites no row (a volatile
default rewrites main's rows in that write, up to `[storage] max_atomic_rewrite_bytes`, and the
copies main's other live branches take of those rows count against it too). What such a column can
refuse is asked before the merge changes anything: `54000` above the cap, and `23502` for a
`NOT NULL` column with no value if a row of main would stay without one (a row the branch deleted
or changed will not).

**A merge that stopped part way is unfinished, not undone.** After a failed write or a crash the
error says `run it again to finish it` (SQLSTATE `55006`). Run the same `MERGE BRANCH` again; the
next writable open also tries. Reads and DML on the target keep working meanwhile. Refused with
`55006` until it is finished: DDL on its tables on the target (and on main, for tables a branch
target inherits; `DROP TABLE` of a parent whose partition child it holds is refused whole), `DROP
BRANCH` of either end, `CREATE BRANCH` over the source's name or with either end as the parent, a
merge of its target, and a restore. The rerun validates keys again, on the tables that have not landed,
so a key main wrote meanwhile refuses it (`23505`) until that row is gone. A table that landed is left
alone: what main changed in it since stays.

A schema statement on main already running when the merge arrives completes: the merge waits for it,
then plans against the new state (and is refused `55000` if the definition moved). A statement on
another table holds nothing up.

**It does not always finish.** A step can find a state it refuses. Then abandon it:

```sql
MERGE BRANCH dev ABORT;   -- returns one row per table that may be partially changed
```

The source is `Active` again and everything the merge held is released. **What the merge already
did to the target stays**: nothing keeps a pre-image to undo it with, so inspect the tables it
names (they also go to the log at WARN). On `main` a table landed whole or not at all, so there is
nothing to repair. Merging the branch again is validated from scratch and lands every table again: rows
that landed are the merge's own, so it usually finishes; if the target changed underneath, expect a refusal
and recreate the branch from the current target. A physical standby refuses `ABORT`: that is its primary's
call.

A merge does not duplicate a key, and writers do not race it. It holds each table it changes exclusively,
as `ALTER TABLE` does: a statement writing such a table waits for it and then runs against what landed (an
`INSERT` of a merged key gets its ordinary `23505`), and the merge waits for the writers already there,
including an open transaction. Readers never wait. `[session] lock_timeout_ms` (default 30000) or a session's
`SET lock_timeout` bounds every wait: after it, `55P03` naming what holds the table, with nothing changed; retry it. A `REPEATABLE READ`
transaction keeps its snapshot, and if the merge changed a row it writes, its `COMMIT` gets a serialization
failure (`40001`): retry it.

### History note — two stale warnings you may encounter

Older copies of this skill, and `README.md` at HEAD, say `MERGE BRANCH` *"merges
nothing and reports success"* or that its conflict detection is *"unreliable."*
**Both are wrong**, and [4.16.0] retracts them explicitly:

> Correction to the filed issue: #72 was recorded as "MERGE BRANCH is a silent no-op,
> reports completed with 0 conflicts and merges 0 rows". That is wrong. Measured, the
> SQL path merges correctly and always did.

The "merges nothing" evidence came from tests driving `BranchTransaction` — an API
with no production callers, whose on-disk key encoding the real merge implementation
does not read. Those tests were removed in [4.16.0].

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create | SQL | `CREATE DATABASE BRANCH dev FROM main AS OF NOW;` |
| create (idempotent) | SQL | `CREATE DATABASE BRANCH IF NOT EXISTS dev FROM main AS OF NOW;` |
| create from past | SQL | `CREATE DATABASE BRANCH rehearsal FROM main AS OF TIMESTAMP '2026-08-24 12:00:00';` |
| switch | SQL / REPL | `USE BRANCH dev;` / `\use dev` |
| current | REPL | `\show branch` |
| list | SQL / REPL | `SHOW BRANCHES;` · `SELECT * FROM pg_database_branches();` / `\branches` |
| merge | SQL | `MERGE BRANCH dev INTO main;` — see the MERGE section |
| merge + cleanup | SQL | `MERGE BRANCH dev INTO main WITH (delete_branch_after = true);` |
| drop | SQL | `DROP DATABASE BRANCH dev;` (`DROP BRANCH dev;` also parses) |
| library: create | Rust | `db.create_branch("dev")?` |
| library: switch | Rust | `db.switch_branch("dev")?` |
| library: merge | Rust | `db.merge_branch("dev")?` (merges source into current) |
| library: drop | Rust | `db.drop_branch("dev")?` |
| library: list | Rust | `db.list_branches()?` |

`CREATE BRANCH` and `CREATE DATABASE BRANCH` are both accepted, as are the `DROP` /
`MERGE` / `USE` short and long spellings (`src/sql/parser.rs:419-443`).
**`LIST BRANCHES` is not in the grammar** — use `SHOW BRANCHES`.

## Recipes

### Recipe 1: Migration rehearsal
```sql
-- 1. Branch off main
CREATE DATABASE BRANCH migration_v3_to_v4 FROM main AS OF NOW;

USE BRANCH migration_v3_to_v4;

-- 2. Run the migration
ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD';
UPDATE orders SET currency = 'EUR' WHERE customer_country = 'DE';

-- 3. Validate
SELECT currency, COUNT(*) FROM orders GROUP BY currency;

-- 4a. Recommended: discard the rehearsal branch and re-run the validated
--     migration SQL directly on main. Deterministic, no merge semantics involved.
USE BRANCH main;
DROP DATABASE BRANCH migration_v3_to_v4;
ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD';
UPDATE orders SET currency = 'EUR' WHERE customer_country = 'DE';

-- 4b. Alternative: MERGE it back. Carries the column and the branch's own rows; refused
--     if main changed the table's definition since the fork. Rows both changed are
--     last-writer-wins.
-- USE BRANCH main;
-- MERGE BRANCH migration_v3_to_v4 INTO main WITH (delete_branch_after = true);
-- SELECT currency, COUNT(*) FROM orders GROUP BY currency;

-- 4c. Sad path: discard
USE BRANCH main;
DROP DATABASE BRANCH migration_v3_to_v4;
```

### Recipe 2: Time-travel branch (rehearse against earlier state)
```sql
CREATE DATABASE BRANCH rewind FROM main
  AS OF TIMESTAMP '2026-08-24 09:00:00';

USE BRANCH rewind;
-- the branch sees data as it was at 09:00; live writes to main don't appear here
```
Requires time-travel, which is **off under the `fast` and `fast_ingest` profiles** and
on under `safe`, `balanced` and `agent`. See *A branch is a fork* for what `AS OF` can see.

### Recipe 3: Embedded library
```rust
let db = EmbeddedDatabase::new("./mydata")?;

db.create_branch("dev")?;
db.switch_branch("dev")?;

db.execute("INSERT INTO posts (title) VALUES ('experimental')")?;

db.switch_branch("main")?;
db.merge_branch("dev")?;          // merges "dev" into the now-active "main"
db.drop_branch("dev")?;
```
The public helpers emitted unparseable SQL until v4.16.0 (`merge_branch` omitted
`INTO`; `list_branches` emitted `LIST BRANCHES`). On ≥4.16.0 they are pinned by
`list_branches_helper_emits_parseable_sql` and friends.

### Recipe 4: Per-agent sandboxes
```sql
-- Each agent run starts with its own ephemeral branch:
CREATE DATABASE BRANCH agent_run_42 FROM main AS OF NOW;
USE BRANCH agent_run_42;
-- … agent does whatever it needs …
USE BRANCH main;
DROP DATABASE BRANCH agent_run_42;       -- always discard at end
```
This is the recommended pattern for letting an LLM execute SQL against a real DB
without write risk to `main` — and the primary reason branches exist. Agent runs
should fork, test, and discard; **they should not merge**, because an agent cannot
verify a last-writer-wins outcome.

### Recipe 5: A/B experiment routing (`ha-ab-testing` feature)
With `--features ha-ab-testing`, branches can be wired to traffic-split experiment
rules. See `docs/guides/ha_cluster_tutorial.md` and the `heliosdb-nano-server` skill.

### Recipe 6: Selective branch replication (`ha-branch-replication` feature)
Branches can be marked for selective sync to specific remote replicas — useful for
staging or jurisdiction-pinned data. See `docs/guides/ha_cluster_tutorial.md`.

## Pitfalls
- **Merge does not detect conflicts.** A row both the target and the branch changed is
  last-writer-wins, silently, the branch being last. Verify after any merge where the target may
  have moved, or prefer fork-test-discard.
- **`conflict_resolution` is rejected**, not ignored — the statement errors on ≥4.16.0.
- **Merge is "source into current"** for the library API. `db.merge_branch("dev")`
  merges `dev` into whatever branch you are on. Switch first, then merge.
- **Branches are per-DB-instance.** Without `ha-tier2` / `ha-branch-replication`, a
  branch created on one node is local to that node.
- **Branches are not free while they live.** A row its parent changes costs a copy on every live
  child that still reads it, and a table its parent drops is a copy of every row. Drop branches you no
  longer need: `DROP BRANCH` deletes the branch's keys, in batches; a crash on the way is finished by the
  next open. The name is free at once.
- **Long-lived branches diverge.** The longer a branch lives, the more likely an
  undetected merge conflict, and the more it holds. Treat branches as short-lived units of work.
- **Schema statements on a branch run alone.** `CREATE TABLE`, `CREATE TABLE … AS`,
  `DROP TABLE` and `ALTER TABLE … ADD COLUMN` wait for other statements on the branch
  (up to 30 s, then `55006`), and new statements wait for them. They are refused with
  `55006` while another open transaction has staged writes of the table: commit or roll
  it back and retry.
- **A schema statement on a branch with live descendants costs a copy.** A child reads through its
  parent, so it first takes its own copy of the table: every row it sees, in chunks, on its own. O(rows). It is
  refused `55006` when the child cannot be alone in time or has staged writes of the table. A merged child stays
  in its own children's chains, so a live grandchild counts.
- **DROP / MERGE wait for running statements.** A `DROP BRANCH` or `MERGE BRANCH`
  waits (up to 30 s) for the statements running on that branch, then takes it out
  of service; a statement that starts on the branch meanwhile is refused at once
  with SQLSTATE `55006` ("is being dropped or merged; retry"), and a session bound
  to a branch that is gone gets `3D000` until it `USE`s another branch.
- **The REST merge route** (`POST /v1/branches/:name/merge`) accepts only
  `strategy` `auto` (default) or `theirs`; `manual` and `ours` are refused with 422,
  because the merge is last-writer-wins with the source winning.

### Fixed in v4.18.0 — do not work around these any more
- **A branch that ever had a child could not be dropped.** The parent's children list
  was never pruned, so `DROP BRANCH` reported *has N child branch(es)* permanently,
  with no workaround. Fixed.
- **Merged branches vanished from the catalog.** `pg_database_branches()`,
  `pg_branch_stats()`, `pg_branches()` and `SHOW BRANCHES` filtered to `Active`, so
  merge history was unreachable. These four now report `Active` **and** `Merged`
  (`Dropped` stays hidden). **This changes row counts** — anything counting rows from
  those views now sees merged branches too. Operational listings (branch
  resolution, REST `/branches`, MCP) are deliberately still `Active`-only.

## Doc drift — sources that disagree with this file

| Source | Says | Status |
|---|---|---|
| Marketing repo Nano kit (v3.58.0) | "MERGE BRANCH is unreliable today" | Stale; also 3 majors behind |

When these disagree with `CHANGELOG.md` and the test suite, the changelog and tests win.

## See also
- `heliosdb-nano-time-travel` — point-in-time queries; required for `AS OF TIMESTAMP` forks.
- `heliosdb-nano-transactions` — an alternative isolation surface for multi-step work.
- `tests/branch_sql_integration_tests.rs`, `tests/branch_merge_surface_tests.rs` — the behavior matrix.
- `docs/guides/ha_cluster_tutorial.md` — multi-node branching.
