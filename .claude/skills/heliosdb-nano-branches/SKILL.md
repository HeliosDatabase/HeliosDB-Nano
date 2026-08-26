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
