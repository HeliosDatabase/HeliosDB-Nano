---
name: heliosdb-nano-query
description: DML and queries in HeliosDB-Nano. Covers INSERT (basic, OR REPLACE/IGNORE, ON CONFLICT, INSERT…SELECT, RETURNING), UPDATE/DELETE with RETURNING, parameter styles (`?`, `$1`, `:name`, `@name`), EXPLAIN/EXPLAIN ANALYZE, CTEs, window functions, set ops, and the result-cache. Use this when the user is writing SELECT/INSERT/UPDATE/DELETE/MERGE statements or asking why a query is slow.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Query & DML

## When to use
Any task involving SELECT / INSERT / UPDATE / DELETE / MERGE / EXPLAIN, or parameterized statements from a client library.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| insert | SQL | `INSERT INTO t (a, b) VALUES (1, 'x')` |
| insert or replace | SQL | `INSERT OR REPLACE INTO t (a, b) VALUES (1, 'x')` |
| insert or ignore | SQL | `INSERT OR IGNORE INTO t (a, b) VALUES (1, 'x')` |
| upsert (ON CONFLICT) | SQL | `INSERT INTO t (a, b) VALUES (1, 'x') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b` |
| insert from select | SQL | `INSERT INTO t (a, b) SELECT a, b FROM other_t` |
| insert returning | SQL | `INSERT INTO t (a) VALUES (1) RETURNING id` |
| update | SQL | `UPDATE t SET b = 'y' WHERE a = 1 RETURNING b` |
| delete | SQL | `DELETE FROM t WHERE a = 1 RETURNING *` |
| merge | SQL | `MERGE INTO target USING src ON … WHEN MATCHED … WHEN NOT MATCHED …` |
| select | SQL | `SELECT … FROM … WHERE … GROUP BY … HAVING … ORDER BY … LIMIT …` |
| explain | SQL | `EXPLAIN [ANALYZE] SELECT …` |
| parameterized (PG) | SQL | `SELECT * FROM t WHERE a = $1` |
| parameterized (SQLite-ish) | SQL | `SELECT * FROM t WHERE a = ?` |
| parameterized (named) | SQL | `SELECT * FROM t WHERE a = :name` (also `@name`) |

## Parameter style cheat-sheet

Nano accepts all four styles in any client (planner auto-renumbers `?` → `$N`):

| Style | Example | Common in |
|-------|---------|-----------|
| `?` | `WHERE a = ? AND b = ?` | sqlite3, JDBC |
| `$N` | `WHERE a = $1 AND b = $2` | psycopg2, sqlx, native PG |
| `:name` | `WHERE a = :user AND b = :status` | sqlalchemy, named binds |
| `@name` | `WHERE a = @user` | C#/.NET clients |

**Don't mix `?` and `$N` in the same statement** — the planner rejects ambiguous mixes.

## Recipes

### Recipe 1: Basic CRUD
```sql
INSERT INTO users (email) VALUES ('a@x.com'), ('b@x.com');
UPDATE users SET email = 'a2@x.com' WHERE id = 1 RETURNING email;
DELETE FROM users WHERE id = 2 RETURNING *;
SELECT id, email FROM users ORDER BY id;
```

### Recipe 2: Upsert (ON CONFLICT) — preferred form
```sql
INSERT INTO users (email, last_seen)
VALUES ('alice@x.com', NOW())
ON CONFLICT (email) DO UPDATE
   SET last_seen = EXCLUDED.last_seen;
```
The standard portable form. Works for any client that speaks PG wire.

### Recipe 3: SQLite-ism — `INSERT OR REPLACE`/`OR IGNORE`
```sql
INSERT OR REPLACE INTO settings (key, value) VALUES ('theme', 'dark');
INSERT OR IGNORE  INTO seen (event_id) VALUES (12345);
```
Translated internally to `ON CONFLICT … DO UPDATE` / `DO NOTHING`. Use either form; pick what matches the rest of your codebase.

### Recipe 4: `INSERT … SELECT` (bulk copy + transform)
```sql
INSERT INTO archive_orders (id, customer, total)
SELECT id, customer, total
  FROM orders
 WHERE created < NOW() - INTERVAL '90 days';
```
For very large batches see `heliosdb-nano-transactions` (bulk-load patterns).

### Recipe 5: Parameterized (Python — psycopg2)
```python
cur.execute("SELECT id, email FROM users WHERE created > %s AND status = %s", (since, 'active'))
for row in cur.fetchall():
    print(row)
```
psycopg2 uses `%s`; Nano accepts it (treated as `$1, $2, …`).

### Recipe 6: Parameterized (Python — sqlite3 drop-in)
```python
from heliosdb_sqlite import connect
conn = connect('./mydata.db')
conn.execute("SELECT * FROM users WHERE id = ?", (1,))
conn.execute("SELECT * FROM users WHERE name = :name", {"name": "alice"})
```

### Recipe 7: Window functions
```sql
SELECT id, customer, total,
       SUM(total) OVER (PARTITION BY customer ORDER BY id) AS running_total,
       RANK()    OVER (PARTITION BY customer ORDER BY total DESC) AS rk
  FROM orders;
```

### Recipe 8: CTE / recursive CTE
```sql
WITH RECURSIVE descendants(id, parent_id, depth) AS (
    SELECT id, parent_id, 0 FROM categories WHERE id = 1
    UNION ALL
    SELECT c.id, c.parent_id, d.depth + 1
      FROM categories c JOIN descendants d ON c.parent_id = d.id
)
SELECT * FROM descendants ORDER BY depth, id;
```

### Recipe 9: Set operations
```sql
SELECT email FROM users_a
UNION
SELECT email FROM users_b;       -- distinct union

SELECT email FROM users_a
INTERSECT
SELECT email FROM users_b;

SELECT email FROM users_a
EXCEPT
SELECT email FROM users_b;
```

### Recipe 10: EXPLAIN — read a query plan
```sql
EXPLAIN SELECT * FROM posts WHERE author_id = 5;
EXPLAIN ANALYZE SELECT * FROM posts WHERE author_id = 5;  -- with timing
```
Look for: index usage, full-scan flags, rowcount estimates. If you see "SeqScan" on a column you expected to be indexed, see `heliosdb-nano-schema` Recipe 1 and add an index.

## Pitfalls
- **NULL semantics are SQL three-valued logic** (since v3.20). `NULL = NULL` is `NULL`, not `TRUE`. Use `IS [NOT] NULL` or `IS [NOT] DISTINCT FROM`.
- **`COUNT(col)` skips NULL rows; `COUNT(*)` does not.** Fast path is `COUNT(*)` only.
- **`MIN/MAX` on empty set returns `NULL`**, not an error.
- **Result cache** (128-entry LRU per connection) is invalidated on DML/DDL touching the involved tables. Repeated identical SELECTs are nearly free.
- **`RETURNING` works for INSERT/UPDATE/DELETE** in both PG- and MySQL-wire paths. The Python sqlite3 SDK uses it transparently to populate `cursor.lastrowid`.
- **ORDER BY ordinal positions** (e.g. `ORDER BY 2 DESC`) are valid SQL-92 and supported.
- **Cross-process ON CONFLICT (path) DO UPDATE** has a known bug (FR `cross_process_on_conflict`): re-attaching to a populated DB from a different process can insert duplicates instead of updating. Single-process workflows are unaffected.

## See also
- `heliosdb-nano-schema` — define the tables/indexes you query.
- `heliosdb-nano-transactions` — wrap multi-statement work, savepoints, bulk loads.
- `heliosdb-nano-vector` — `<->`, `<#>`, `<=>` similarity operators.
- `heliosdb-nano-time-travel` — query historical state via `AS OF`.
