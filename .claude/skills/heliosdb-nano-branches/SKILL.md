---
name: heliosdb-nano-branches
description: Database branching in HeliosDB-Nano — ephemeral fork-test-discard sandboxes. Create a branch from main (or any other branch, with an optional `AS OF` historical anchor), make isolated changes, validate, then discard. The primary pattern for agent sandboxes, migration rehearsals, A/B experiments, and short-lived "what if" workspaces. MERGE back exists but DOES NOT WORK — it reports success and merges zero rows, so never propose it; discard and re-apply validated SQL to main instead. Use this when the user says "branch", "fork the database", or wants to try a destructive change without affecting production data.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Database Branching — Fork-Test-Discard Sandboxes

Branches are ephemeral copy-on-write forks. The recommended lifecycle is
**fork → test → discard**: create a branch, run the risky work there, validate,
then `DROP` the branch and (if the change passed) re-apply the validated SQL to
`main`. `MERGE` does not work at all — see the warning below; never propose it.

## When to use
- Give an LLM agent a sandboxed copy-on-write workspace (the primary use).
- Rehearse a migration on production data without touching production.
- A/B test a new schema or query plan.
- Compare aggregates across "real" vs. "what if".

## ⚠️ MERGE DOES NOT WORK — do not propose it

`MERGE DATABASE BRANCH x INTO y` / `db.merge_branch(..)` **merges nothing and
reports success.** Measured 2026-08-19 against
`tests/branch_merge_conflict_tests.rs`:

```
main has key1; dev adds key2; merge dev -> main
  result.completed    = true      <- claims success
  result.conflicts    = []        <- claims no conflicts
  result.merged_keys  = 0         <- merged NOTHING
  main still lacks key2
```

Eleven of that suite's thirteen tests fail this way. The two that pass do so
only because their expected outcome is "the target is unchanged" — which a
no-op satisfies. Treat none of it as working.

**An earlier version of this warning said conflict detection was "unreliable"
and told you to verify the merged rows afterwards. That was wrong in the
dangerous direction** — it implies rows are merged. The real risk is silent data
loss: merge, see success with no conflicts, discard the branch, and the branch's
work is gone without ever reaching the target.

Never propose MERGE. Use fork-test-discard as designed: re-run the validated SQL
against the target branch. A fix is tracked (audit C11; the merge-base lookup in
`src/storage/branch.rs` reads the latest value instead of the historical base,
which is a real defect but not the whole story). This stayed invisible for months
because the merge suite is gated behind the non-default `internal-tests` feature
and never ran — see `docs/GATES.md` §3b.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create | SQL | `CREATE DATABASE BRANCH dev FROM main;` |
| create from past | SQL | `CREATE DATABASE BRANCH rehearsal FROM main AS OF TIMESTAMP '2026-04-29 12:00:00';` |
| switch | SQL / REPL | `USE BRANCH dev;` / `\use dev` |
| current | REPL | `\show branch` |
| list | SQL / REPL | `SELECT * FROM pg_database_branches();` / `\branches` |
| merge ❌ | SQL | `MERGE DATABASE BRANCH dev INTO main;` — **merges nothing, reports success**; see warning |
| drop | SQL | `DROP DATABASE BRANCH dev;` |
| library: create | Rust | `db.create_branch("dev")?` |
| library: switch | Rust | `db.switch_branch("dev")?` |
| library: merge ❌ | Rust | `db.merge_branch("dev")?` — **returns completed=true, merged_keys=0**; see warning |
| library: drop | Rust | `db.drop_branch("dev")?` |
| library: list | Rust | `db.list_branches()?` |

## Recipes

### Recipe 1: Migration rehearsal
```sql
-- 1. Branch off main
CREATE DATABASE BRANCH migration_v3_to_v4 FROM main;

USE BRANCH migration_v3_to_v4;

-- 2. Run the migration
ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD';
UPDATE orders SET currency = 'EUR' WHERE customer_country = 'DE';

-- 3. Validate
SELECT currency, COUNT(*) FROM orders GROUP BY currency;

-- 4a. Happy path (recommended): discard the rehearsal branch and re-run the
--     validated migration SQL directly on main.
USE BRANCH main;
DROP DATABASE BRANCH migration_v3_to_v4;
ALTER TABLE orders ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD';
UPDATE orders SET currency = 'EUR' WHERE customer_country = 'DE';

-- 4b. Alternative: MERGE the branch back — only if you accept the MERGE
--     warning above, and verify the merged rows afterwards.
-- USE BRANCH main;
-- MERGE DATABASE BRANCH migration_v3_to_v4 INTO main;
-- SELECT currency, COUNT(*) FROM orders GROUP BY currency;  -- verify!
-- DROP DATABASE BRANCH migration_v3_to_v4;

-- 4c. Sad path: discard
USE BRANCH main;
DROP DATABASE BRANCH migration_v3_to_v4;
```

### Recipe 2: Time-travel branch (rehearse against state from earlier today)
```sql
CREATE DATABASE BRANCH rewind FROM main
  AS OF TIMESTAMP '2026-04-29 09:00:00';

USE BRANCH rewind;
-- the branch sees data as it was at 09:00; live writes to main don't appear here
```

### Recipe 3: Embedded library
```rust
let db = EmbeddedDatabase::new("./mydata")?;

db.create_branch("dev")?;
db.switch_branch("dev")?;

db.execute("INSERT INTO posts (title) VALUES ('experimental')")?;

db.switch_branch("main")?;
db.merge_branch("dev")?;          // merges "dev" into the now-active "main"
                                  // ⚠️ see MERGE warning — verify results after
db.drop_branch("dev")?;
```

### Recipe 4: Per-agent sandboxes
```sql
-- Each agent run starts with its own ephemeral branch:
CREATE DATABASE BRANCH agent_run_42 FROM main;
USE BRANCH agent_run_42;
-- … agent does whatever it needs …
DROP DATABASE BRANCH agent_run_42;       -- always discard at end
```
This is the recommended pattern for letting an LLM execute SQL against a real DB without write risk to main — and the primary reason branches exist. Agent runs should fork, test, and discard; they should not merge.

### Recipe 5: A/B experiment routing (`ha-ab-testing` feature)
With `--features ha-ab-testing`, branches can be wired to traffic-split experiment rules. See `docs/guides/ha_cluster_tutorial.md` and the `heliosdb-nano-server` skill for the runtime config.

### Recipe 6: Selective branch replication (`ha-branch-replication` feature)
Branches can be marked for selective sync to specific remote replicas — useful for staging or jurisdiction-pinned data. See `docs/guides/ha_cluster_tutorial.md`.

## Pitfalls
- **MERGE does not merge.** It returns `completed = true`, `conflicts = []`, `merged_keys = 0` and leaves the target unchanged (see the warning at the top). Do not propose it; use fork-test-discard and re-run the validated SQL.
- **Branches are per-DB-instance**. Without `ha-tier2` / `ha-branch-replication`, a branch you create on one node is local to that node.
- **Merge is "source into current"**. `db.merge_branch("dev")` merges `dev` into whatever branch you're currently on. Switch first, then merge.
- **TRUNCATE on `main` does not touch branch overlays**, but TRUNCATE on a branch only clears that branch's writes; rows from the parent reappear. The ART-index branch guard handles this — see lib-tests `tests/branch_*.rs`.
- **Branches are not free**. Each one carries a key-prefix (`bdata:<id>:…`) and tombstones for deletes (`bdel:`). Drop branches you no longer need.
- **Long-lived branches diverge**. The longer a branch stays live, the higher the chance of merge conflicts on overlapping writes. Treat branches as short-lived units of work.

## See also
- `heliosdb-nano-time-travel` — point-in-time queries against any branch.
- `heliosdb-nano-transactions` — branches are an alternative isolation surface for multi-step work.
- `tests/branch_*.rs` — full behavior matrix.
- `docs/guides/ha_cluster_tutorial.md` — multi-node branching.
