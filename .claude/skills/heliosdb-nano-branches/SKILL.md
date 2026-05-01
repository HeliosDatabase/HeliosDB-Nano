---
name: heliosdb-nano-branches
description: Database branching in HeliosDB-Nano. Create a branch from main (or any other branch, with an optional `AS OF` historical anchor), make isolated changes, then merge back. Useful for migration rehearsals, A/B experiments, agent sandboxes, feature flags, and short-lived "what if" workspaces. Use this when the user says "branch", "fork the database", or wants to try a destructive change without affecting production data.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Database Branching

## When to use
- Rehearse a migration on production data without touching production.
- A/B test a new schema or query plan.
- Give an LLM agent a sandboxed copy-on-write workspace.
- Compare aggregates across "real" vs. "what if".

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create | SQL | `CREATE DATABASE BRANCH dev FROM main;` |
| create from past | SQL | `CREATE DATABASE BRANCH rehearsal FROM main AS OF TIMESTAMP '2026-04-29 12:00:00';` |
| switch | SQL / REPL | `USE BRANCH dev;` / `\use dev` |
| current | REPL | `\show branch` |
| list | SQL / REPL | `SELECT * FROM pg_database_branches();` / `\branches` |
| merge | SQL | `MERGE DATABASE BRANCH dev INTO main;` |
| drop | SQL | `DROP DATABASE BRANCH dev;` |
| library: create | Rust | `db.create_branch("dev")?` |
| library: switch | Rust | `db.switch_branch("dev")?` |
| library: merge | Rust | `db.merge_branch("dev")?` (merges source into current) |
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

-- 4a. Happy path: merge back
USE BRANCH main;
MERGE DATABASE BRANCH migration_v3_to_v4 INTO main;
DROP DATABASE BRANCH migration_v3_to_v4;

-- 4b. Sad path: discard
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
This is the recommended pattern for letting an LLM execute SQL against a real DB without write risk to main.

### Recipe 5: A/B experiment routing (`ha-ab-testing` feature)
With `--features ha-ab-testing`, branches can be wired to traffic-split experiment rules. See `docs/guides/ha_cluster_tutorial.md` and the `heliosdb-nano-server` skill for the runtime config.

### Recipe 6: Selective branch replication (`ha-branch-replication` feature)
Branches can be marked for selective sync to specific remote replicas — useful for staging or jurisdiction-pinned data. See `docs/guides/ha_cluster_tutorial.md`.

## Pitfalls
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
