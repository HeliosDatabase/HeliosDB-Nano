---
name: heliosdb-nano-branches
description: Database branching in HeliosDB-Nano — ephemeral fork-test-discard sandboxes. Create a branch from main (or any other branch, anchored with a required `AS OF` clause), make isolated changes, validate, then discard. The primary pattern for agent sandboxes, migration rehearsals, A/B experiments, and short-lived "what if" workspaces. MERGE BRANCH does move rows, but it is last-writer-wins with no conflict detection — prefer fork-test-discard for anything you cannot verify afterwards. Use this when the user says "branch", "fork the database", or wants to try a destructive change without affecting production data.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Database Branching — Fork-Test-Discard Sandboxes

**Verified against HeliosDB-Nano v4.18.1 (2026-08-25).** Syntax below matches the
green test suite (`tests/branch_sql_integration_tests.rs`,
`tests/branch_merge_surface_tests.rs`), not the README — see *Doc drift* at the bottom.

Branches are ephemeral copy-on-write forks. The recommended lifecycle is
**fork → test → discard**: create a branch, run the risky work there, validate,
then `DROP` the branch and (if the change passed) re-apply the validated SQL to
`main`.

## ⚠️ `AS OF` is required on every CREATE

`CREATE BRANCH` / `CREATE DATABASE BRANCH` **must** carry an `AS OF` clause. Without
one the parser rejects the statement:

```
CREATE BRANCH requires AS OF clause
```

(`src/sql/parser.rs:3490`. A test pins this: *"CREATE BRANCH must still error"*.)

Use `AS OF NOW` for a fork of current state — that is the form every integration test
uses — or `AS OF TIMESTAMP '…'` to fork from a past point.

```sql
CREATE DATABASE BRANCH dev FROM main AS OF NOW;              -- correct
CREATE DATABASE BRANCH dev FROM main;                        -- ERRORS
```

**The README's headline snippet omits `AS OF` and does not run.** Do not copy it.

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
- `WITH (delete_branch_after = true)` **is** honoured and works.

**Guidance:** merge is safe when the target has not moved since the fork — a
short-lived branch off `main` where nothing else writes to `main`. When you cannot
guarantee that, fork-test-discard and re-run the validated SQL on the target instead.
Agent runs should always discard, never merge.

### What MERGE refuses, and what it finishes

Two writers of the same row are still last-writer-wins, as above. These are not: each
is refused **before anything changes**, the source stays `Active`, and it can be merged
once the cause is gone.

| SQLSTATE | Refused when |
|---|---|
| `55000` | The target changed the definition of a table the branch changed (columns, constraints, a new UNIQUE index) |
| `55000` | The target **dropped and recreated** a table since the fork, even with an identical definition, and the branch changed its schema or only its rows |
| `55000` | The target changed the definition of, renamed or dropped a table since the fork, and the branch changed its rows: they were written for the old definition and are copied by position. The message names the table and what changed |
| `55000` | The branch changed rows of a table **created on the target after it forked**: nothing records the definition they were written for. Copy those rows by hand, or recreate the branch |
| `55000` | The branch was **created by an earlier release**, so its fork point is not recorded, and it changed rows. Recreate it from the current target, or copy its rows. A branch that changed no rows still merges |
| `55000` | The branch wrote rows of a table whose definition comes from an ancestor branch that is not merged into the target, or that merged into a branch since dropped |
| `23505` | A row the merge would put on the target has a PRIMARY KEY / UNIQUE key another target row holds. The message names the table, the constraint and the key |
| `55006` | An open transaction has staged writes on the source branch, or of one of the merge's tables on the target, `main` included: retry after it commits or rolls back |
| `55006` | A statement on `main` has claimed a key a merged row holds and has not written its row yet: retry |
| `55006` | Another merge has not finished and holds one of the tables, or a schema statement on one of the merge's tables is still running after the statement's 30 s budget: retry |
| `54000` | A merge or `ADD COLUMN` on a branch carries more changed rows of a table than one atomic write can hold. Nothing was changed. The message names the table: copy those rows in smaller transactions, or change fewer per branch |
| `0A000` | A schema change merged into a branch that has other live descendants, through merged branches too |

`MERGE BRANCH main INTO x`, and a branch into itself, are refused outright.

On `main`, each table's changes land as **one ordinary transaction**: versions, logical WAL, replication
wait, conflict detection and the column store all come from its commit. A reader or a crash sees a table's
merge wholly or not at all, and `AS OF` sees it like any commit. Tables land one after another, and the merge
holds a table's changed rows in memory.

A merge carries what the branch itself changed. The rows a branch `ADD COLUMN` copied are
not changes: a row `main` updated or deleted after the branch's `ADD COLUMN` keeps `main`'s
change, and `main` reads the same default for it: a merge adds the column to `main` by missing
value, one atomic write that rewrites no row (a volatile default rewrites main's rows in that
write, up to `[storage] max_atomic_rewrite_bytes`). What such a column can refuse is asked before
the merge changes anything: `54000` above the cap, and `23502` for a `NOT NULL` column with no
value if a row of main would stay without one (a row the branch deleted or changed will not).

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

A merge does not duplicate a key. A statement on `main` that takes a merged key first stops the merge
(`55006`, run it again once that is resolved, and it says `23505` while the row holding the key is there);
one that comes after gets its ordinary `23505`. A transaction that read a row before the merge and commits a
change to it after gets a serialization failure (`40001`), at any isolation level, READ COMMITTED included:
retry it.

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

-- 4b. Alternative: MERGE it back. Safe when main has not been written since the
--     fork; last-writer-wins with no conflict detection otherwise.
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
on under `safe`, `balanced` and `agent`.

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
- **`AS OF` is mandatory on CREATE.** The single most common failure. See the top.
- **Merge does not detect conflicts.** Last-writer-wins, silently. Verify after any
  merge where the target may have moved, or prefer fork-test-discard.
- **`conflict_resolution` is rejected**, not ignored — the statement errors on ≥4.16.0.
- **Merge is "source into current"** for the library API. `db.merge_branch("dev")`
  merges `dev` into whatever branch you are on. Switch first, then merge.
- **Branches are per-DB-instance.** Without `ha-tier2` / `ha-branch-replication`, a
  branch created on one node is local to that node.
- **TRUNCATE on `main` does not touch branch overlays**, but TRUNCATE on a branch only
  clears that branch's writes; rows from the parent reappear.
- **Branches are not free.** Each carries a key prefix (`bdata:<id>:…`) and tombstones
  for deletes (`bdel:`). Drop branches you no longer need.
- **Long-lived branches diverge.** The longer a branch lives, the more likely an
  undetected merge conflict. Treat branches as short-lived units of work.
- **No explicit transactions on a branch yet.** Inside `BEGIN … COMMIT` on a branch,
  every statement that reads or writes data is refused with SQLSTATE `0A000`
  (transaction control and `SET`/`SHOW` still work). Run branch work in autocommit.
  For the same reason `COPY … FROM STDIN` and the embedded `execute_batch` on a
  branch autocommit chunk by chunk / statement by statement: they are **not atomic**
  on a branch — a failure part-way leaves the earlier rows on the branch.
- **Schema statements on a branch run alone.** `CREATE TABLE`, `CREATE TABLE … AS`,
  `DROP TABLE` and `ALTER TABLE … ADD COLUMN` wait for other statements on the branch
  (up to 30 s, then `55006`), and new statements wait for them. They are refused with
  `55006` while another open transaction has staged writes of the table: commit or roll
  it back and retry.
- **A branch with live descendants refuses schema statements** (`0A000`). A child reads
  through its parent, so changing a table under it would change what it sees. A merged child
  stays in its own children's chains, so a live grandchild counts. `DROP` the descendants first.
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
  those views now sees merged branches too. Operational listings (version GC, branch
  resolution, REST `/branches`, MCP) are deliberately still `Active`-only.

## Doc drift — sources that disagree with this file

| Source | Says | Status |
|---|---|---|
| `README.md` branching section | `CREATE BRANCH x FROM main;` (no `AS OF`) | **Would error.** Use `AS OF NOW`. |
| `README.md` MERGE warning | "merges nothing and reports success" | **Retracted** by `CHANGELOG.md` [4.16.0] |
| `docs/llms.txt` | same stale merge warning | Same retraction applies |
| Marketing repo Nano kit (v3.58.0) | "MERGE BRANCH is unreliable today" | Stale; also 3 majors behind |

When these disagree with `CHANGELOG.md` and the test suite, the changelog and tests win.

## See also
- `heliosdb-nano-time-travel` — point-in-time queries; required for `AS OF TIMESTAMP` forks.
- `heliosdb-nano-transactions` — an alternative isolation surface for multi-step work.
- `tests/branch_sql_integration_tests.rs`, `tests/branch_merge_surface_tests.rs` — the behavior matrix.
- `docs/guides/ha_cluster_tutorial.md` — multi-node branching.
