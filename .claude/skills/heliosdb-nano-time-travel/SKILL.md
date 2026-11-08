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

### Recipe 3: "Undo a bad UPDATE" — read the past straight into main

No branch, and above all no `MERGE`. A branch anchored before the bad `UPDATE` reads
the old rows, but it only copied them, and `MERGE` carries what a branch changed, never
what it copied (see `heliosdb-nano-branches`): merging it back leaves `main` as it is.
`AS OF` reads history directly, so a branch buys nothing here.

```sql
-- 1. Copy the affected rows into a holding table, read as they were 5 minutes
--    before the bad UPDATE
CREATE TABLE accounts_restored AS
SELECT * FROM accounts AS OF TIMESTAMP '2026-04-29 09:55:00'
 WHERE id IN (1,2,3);

-- 2. Sanity-check BEFORE writing anything back
SELECT r.id, a.balance AS balance_now, r.balance AS balance_then
  FROM accounts_restored r JOIN accounts a ON a.id = r.id;

-- 3. Replace the bad rows
UPDATE accounts a
   SET balance = r.balance
  FROM accounts_restored r
 WHERE a.id = r.id;

DROP TABLE accounts_restored;
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
`\snapshots` lists the snapshots kept in memory, the newest ones; on a busy store it says that older ones exist, which `AS OF` still reaches.

### Recipe 5: Use a system view for snapshot inventory
```sql
SELECT * FROM pg_database_branches();      -- branches incl. their AS OF anchor
SELECT * FROM pg_vector_index_stats();     -- vector index health
```

## Pitfalls
- **Retention is `storage.version_retention`** (`[storage]` in the TOML config: seconds or `"90s"`, `"15m"`, `"24h"`, `"7d"`, `"2w"`). `AS OF` (and `CREATE BRANCH … AS OF`) reaches any time inside it, however many commits followed. Older than it, once the version GC has run, `AS OF` is refused (the version-GC low watermark error, or "No snapshot at or before …" once that snapshot's record is gone too). The default is infinite: no history is ever collected, and the `snapshot:` records `AS OF` resolves a time through grow by one per commit (about 60 bytes each), on top of the row versions themselves, until `version_retention` is set.
- **`\snapshots` and `pg_snapshots` list only the snapshots kept in memory** (the newest 1000 to 2000). On a busy store older ones are not listed, and `\snapshots` says so; `AS OF` still reaches them.
- **`AS OF` reads from the version chain (MVCC)**, not from a separate copy. Heavy churn between then and now means more chain walking.
- **`AS OF` on a session bound to a branch is refused (`0A000`)**, whichever way it is spelled, `COUNT(*)` too: only main has history. To work with main as it was, create a branch anchored there (`heliosdb-nano-branches`). It reads what `AS OF` reads for the same time, and is refused where `AS OF` is (a `RENAME TO`, a refresh that swapped a view's data table, a column moved out of stored representation since).
- **A table's history goes with its name.** `DROP TABLE` and `RENAME TO` delete its versions, so a later table of the name does not read them as its own. So `AS OF` (and a transaction whose snapshot is older) is refused `0A000` for a time before the table took its name by a rename, also after renaming back, and so is a time before a materialized view's refresh that swapped its data table: nothing right is left to read there. After the rename, a row it moved that has no version at the time read and has changed since is refused the same way (what it was then is gone); a row not changed since reads as it is. The rename has its own snapshot record (4.42.0+), so a time just after it reads after it even when nothing else committed since. The transaction that ran the rename itself reads the table. A crash part way through the deletion leaves no table of the name, and the next open finishes it.
- **Columns dropped since are taken out, not shifted (4.42.0+).** A row version keeps the columns it was written with; `AS OF` a time before a `DROP COLUMN` (and a transaction begun before it, and the one that ran it) reads each value under its own column, the dropped one left out, a column added since reading its default. It used to show the dropped column's value under the next column's name. A version from before `RENAME COLUMN` reads under the new name. Before a `SET STORAGE` that moved a column out of dictionary, content-addressed or columnar storage, `AS OF` is refused `0A000` (those versions hold references the column no longer resolves).
- **No `AS OF` for writes**. You can read history but you cannot `INSERT/UPDATE/DELETE` "into the past". Use Recipe 3 instead.
- **Time format must be unambiguous**. Prefer ISO-8601 with timezone (`'2026-04-29 09:00:00+00:00'`).

## See also
- `heliosdb-nano-branches` — branch from a past state for write-mode forensics.
- `heliosdb-nano-backup` — durable point-in-time recovery beyond the live retention window.
- `heliosdb-nano-query` — `EXPLAIN ANALYZE` to see how time-travel walks the version chain.
