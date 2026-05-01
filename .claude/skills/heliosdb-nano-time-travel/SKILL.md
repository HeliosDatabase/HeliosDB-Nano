---
name: heliosdb-nano-time-travel
description: Query historical state in HeliosDB-Nano. Every read can be anchored to a past timestamp via `AS OF TIMESTAMP '…'`; the engine returns the snapshot as it was at that moment. Useful for audit/forensics, "what changed", reverting after a bad UPDATE, and comparing aggregates across time. Use this when the user says "what did this look like yesterday", "before that bug", "show me the state at 10am", or "diff against last week".
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Time-Travel Queries

## When to use
- Audit / forensics ("when did this row change").
- Recover from a bad UPDATE without restoring from backup.
- Compare aggregates between historical points.
- Anchor a branch to a past state (see `heliosdb-nano-branches` Recipe 2).

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| time-travel SELECT | SQL | `SELECT * FROM t AS OF TIMESTAMP '2026-04-29 09:00:00';` |
| list snapshots | REPL | `\snapshots` |
| toggle LSN display | REPL | `\show lsn` |
| branch from past | SQL | `CREATE DATABASE BRANCH x FROM main AS OF TIMESTAMP '…';` |

## Recipes

### Recipe 1: Read state from a past timestamp
```sql
SELECT id, balance
  FROM accounts AS OF TIMESTAMP '2026-04-29 09:00:00'
 WHERE id = 42;
```
The result reflects what was visible at that wall-clock time, regardless of writes since.

### Recipe 2: Diff between then and now
```sql
WITH past AS (
    SELECT id, balance FROM accounts AS OF TIMESTAMP '2026-04-29 09:00:00'
), current AS (
    SELECT id, balance FROM accounts
)
SELECT current.id,
       past.balance    AS balance_then,
       current.balance AS balance_now,
       current.balance - past.balance AS delta
  FROM past JOIN current ON past.id = current.id
 WHERE past.balance IS DISTINCT FROM current.balance;
```

### Recipe 3: "Undo a bad UPDATE" via branch + merge
```sql
-- 1. Snapshot what the table looked like before the bad write
CREATE DATABASE BRANCH undo FROM main
  AS OF TIMESTAMP '2026-04-29 09:55:00';        -- 5 min before the bad UPDATE

USE BRANCH undo;

-- 2. Copy the affected rows into a holding table
CREATE TABLE accounts_restored AS
SELECT * FROM accounts WHERE id IN (1,2,3);

-- 3. Merge the holding table back into main
USE BRANCH main;
MERGE DATABASE BRANCH undo INTO main;            -- brings accounts_restored over

-- 4. Replace bad rows
UPDATE accounts a
   SET balance = r.balance
  FROM accounts_restored r
 WHERE a.id = r.id;

DROP TABLE accounts_restored;
DROP DATABASE BRANCH undo;
```

### Recipe 4: List available snapshots / show LSN
```
heliosdb> \snapshots
   id |          taken_at         | lsn
   ---+---------------------------+----------
   42 | 2026-04-29 09:00:00.123Z  | 0/1A2B3C
   …
heliosdb> \show lsn      -- toggles LSN column in subsequent SELECTs
```

### Recipe 5: Use a system view for snapshot inventory
```sql
SELECT * FROM pg_database_branches();      -- branches incl. their AS OF anchor
SELECT * FROM pg_vector_index_stats();     -- vector index health
```

## Pitfalls
- **Snapshot retention is finite**. Configurable in `[storage]` (TOML config). Once a snapshot ages out, `AS OF` for that timestamp returns "snapshot not available".
- **`AS OF` reads from the version chain (MVCC)**, not from a separate copy. Heavy churn between then and now means more chain walking.
- **`AS OF` against a branch reads that branch's view at the timestamp** — if the branch was created later, behaviour follows the branch's `AS OF` anchor.
- **No `AS OF` for writes**. You can read history but you cannot `INSERT/UPDATE/DELETE` "into the past". Use the branch + merge pattern (Recipe 3) instead.
- **Time format must be unambiguous**. Prefer ISO-8601 with timezone (`'2026-04-29 09:00:00+00:00'`).

## See also
- `heliosdb-nano-branches` — branch from a past state for write-mode forensics.
- `heliosdb-nano-backup` — durable point-in-time recovery beyond the live retention window.
- `heliosdb-nano-query` — `EXPLAIN ANALYZE` to see how time-travel walks the version chain.
