---
name: heliosdb-nano-schema
description: Define and inspect schema in HeliosDB-Nano. Covers CREATE/ALTER/DROP TABLE with PK/FK/UNIQUE/CHECK/DEFAULT constraints, regular and HNSW vector indexes, views, materialized views, PL/pgSQL functions, and introspection through Postgres (`pg_class`, `information_schema`), SQLite (`sqlite_master`, `PRAGMA table_info`), and Nano-specific (`\d`, `\dt`, `\dS`, `\dmv`) surfaces. Also documents why `CREATE TRIGGER` succeeds but triggers never fire — read this before proposing a trigger. Use this when the user asks "create a table", "add an index", "describe", "what columns does X have", or anything about triggers.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Schema (DDL & Introspection)

## When to use
Any DDL operation: `CREATE`, `ALTER`, `DROP` against tables/indexes/views/functions; or asking the database what schema exists. Also read this before answering any trigger question — triggers are **not implemented** (§ "Triggers — not implemented" below), and `CREATE TRIGGER` returning `OK` does not mean the trigger works.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create table | SQL | `CREATE TABLE t (id INT PRIMARY KEY, …)` |
| alter table (multi-op) | SQL | `ALTER TABLE t ADD COLUMN c TEXT, DROP COLUMN d, RENAME e TO f` |
| drop table | SQL | `DROP TABLE [IF EXISTS] t` |
| create index | SQL | `CREATE INDEX idx_t_c ON t(c)` |
| create vector index | SQL | `CREATE INDEX vidx ON t USING HNSW (embedding) WITH (dim = 384, metric = 'cosine')` |
| drop index | SQL | `DROP INDEX idx_t_c` |
| create view | SQL | `CREATE VIEW v AS SELECT …` |
| create materialized view | SQL | `CREATE MATERIALIZED VIEW mv AS SELECT … WITH (auto_refresh = true)` |
| create trigger | SQL | ⚠️ **registered but never fires** — see "Triggers — not implemented" below before writing any `CREATE TRIGGER` |
| create function | SQL (PL/pgSQL subset) | `CREATE FUNCTION f(x INT) RETURNS INT AS $$ BEGIN RETURN x*2; END $$ LANGUAGE plpgsql` |
| list tables | REPL / SQL | `\dt` / `SELECT * FROM pg_tables` |
| describe table | REPL / SQL | `\d t` / `PRAGMA table_info(t)` / `SELECT * FROM information_schema.columns WHERE table_name='t'` |
| list materialized views | REPL | `\dmv` |
| list system views | REPL | `\dS` |

## Recipes

### Recipe 1: Create a normalized table set with FKs and indexes
```sql
CREATE TABLE users (
    id        INTEGER PRIMARY KEY,
    email     TEXT UNIQUE NOT NULL,
    created   TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE posts (
    id         INTEGER PRIMARY KEY,
    author_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title      TEXT NOT NULL,
    body       TEXT,
    published  BOOLEAN DEFAULT FALSE
);

CREATE INDEX idx_posts_author ON posts(author_id);
CREATE INDEX idx_posts_published ON posts(published) WHERE published = TRUE;
```

### Recipe 2: Multi-op `ALTER TABLE` in one statement
```sql
ALTER TABLE posts
    ADD COLUMN updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    DROP COLUMN body,
    RENAME COLUMN title TO headline;
```
Each clause is applied atomically; failure of any clause rolls all of them back. (See `lib.rs` `AlterTableMulti` plan.)

### Recipe 3: HNSW vector index for similarity search
```sql
CREATE TABLE docs (
    id        INTEGER PRIMARY KEY,
    title     TEXT,
    embedding VECTOR(384)
);

CREATE INDEX docs_emb_idx ON docs
USING HNSW (embedding) WITH (dim = 384, metric = 'cosine');

-- Query (top-5 nearest)
SELECT id, title FROM docs ORDER BY embedding <-> $1 LIMIT 5;
```
See `heliosdb-nano-vector` for full vector-search recipes.

### Recipe 4: Materialized view with auto-refresh
```sql
CREATE MATERIALIZED VIEW user_stats AS
    SELECT user_id, COUNT(*) AS posts
      FROM posts
     GROUP BY user_id
WITH (auto_refresh = true, max_cpu_percent = 15);
```
Inspect with `\dmv` (REPL) or `SELECT * FROM pg_matviews`.

### Recipe 5: Triggers — NOT IMPLEMENTED (read this instead of writing one)

**Do not propose a trigger as a solution.** `CREATE TRIGGER` is accepted and the
trigger is registered, but **no trigger body is ever executed**. Nothing fires on
INSERT/UPDATE/DELETE, and there is no error, no warning, and no log line — an audit
trigger just silently produces nothing. Use the application layer, an explicit second
statement in the same transaction, or a `CREATE PROCEDURE` invoked with `CALL`.

What each form actually does today:

```sql
-- (1) SQLite / MySQL inline body → PARSE ERROR. This grammar does not exist here.
CREATE TRIGGER posts_audit AFTER UPDATE ON posts FOR EACH ROW
BEGIN INSERT INTO audit_log (op) VALUES ('update'); END;
-- ERROR: SQL parse error: … Expected: EXECUTE, found: BEGIN
-- (`CREATE TRIGGER IF NOT EXISTS …` is also a parse error — no such clause.)

-- (2) PostgreSQL form → SUCCEEDS, registers, then does nothing whatsoever.
CREATE FUNCTION audit_posts() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO audit_log (table_name, op) VALUES ('posts', 'update');
    RETURN NEW;
END
$$ LANGUAGE plpgsql;                                     -- OK

CREATE TRIGGER posts_audit AFTER UPDATE ON posts
FOR EACH ROW EXECUTE FUNCTION audit_posts();             -- OK, registered

UPDATE posts SET title = 'x' WHERE id = 1;               -- OK
SELECT COUNT(*) FROM audit_log;                          -- 0  ← never fired
```

This holds for every timing (`BEFORE` / `AFTER` / `INSTEAD OF`), every event, both
`FOR EACH ROW` and `FOR EACH STATEMENT`, and with or without a `WHEN (…)` clause —
all of them parse and register, none of them execute.

**The one exception that does have an effect.** A `BEFORE INSERT … FOR EACH ROW
EXECUTE FUNCTION f()` whose function body contains literal `NEW.<col> = <expr>`
assignments and/or `RETURN NULL` rewrites (or skips) the row being inserted:

```sql
CREATE FUNCTION force_draft() RETURNS TRIGGER AS $$
BEGIN NEW.published = FALSE; RETURN NEW; END
$$ LANGUAGE plpgsql;

CREATE TRIGGER posts_draft BEFORE INSERT ON posts FOR EACH ROW
EXECUTE FUNCTION force_draft();
-- INSERT INTO posts (id, author_id, title, published) VALUES (1, 1, 'x', TRUE);
--   → the row is stored with published = FALSE
-- A body of `BEGIN RETURN NULL; END` instead makes the INSERT silently store nothing.
```
This is a text scan of the function body, not execution: **INSERT only** (not
`BEFORE UPDATE` / `BEFORE DELETE`, which register the recipe but never apply it), no
`OLD`, and no side effects — an `INSERT`/`UPDATE`/`RAISE` inside the body is ignored.

**`DROP TABLE` does not cascade to triggers.** The registration outlives the table,
so re-creating the table and its trigger fails with
`Trigger 'x' already exists on table 't'` — the name stays burned for the lifetime of
the process. `DROP TRIGGER x ON t` before dropping the table, or use a fresh name.

**No trigger introspection exists either**: there is no `pg_trigger` view,
`information_schema.triggers` is always empty by design, `pg_class.relhastriggers` is
hardcoded `false`, and no REPL meta-command lists triggers. On a disk-backed database a
registered trigger survives exactly one restart (WAL replay restores it, then the WAL is
truncated and nothing reloads it from the catalog), so registration is not durable either.

### Recipe 6: PL/pgSQL function
```sql
CREATE FUNCTION post_count(uid INTEGER) RETURNS INTEGER AS $$
DECLARE
    cnt INTEGER;
BEGIN
    SELECT COUNT(*) INTO cnt FROM posts WHERE author_id = uid;
    RETURN cnt;
END;
$$ LANGUAGE plpgsql;

SELECT post_count(1);
```

### Recipe 7: Inspect schema (three ways)
**Postgres-style (works in any client):**
```sql
SELECT table_name FROM information_schema.tables WHERE table_schema = 'public';
SELECT column_name, data_type, is_nullable
  FROM information_schema.columns
 WHERE table_name = 'posts'
 ORDER BY ordinal_position;
```
**SQLite-style (drop-in compat):**
```sql
SELECT name, sql FROM sqlite_master WHERE type='table';
PRAGMA table_info(posts);
```
**Nano REPL meta-commands:**
```
\dt              -- list user tables (size, row count)
\d posts         -- columns, indexes, constraints, FKs of one table
\dS              -- list system dictionary views
\dmv             -- list materialized views
\indexes posts   -- index recommendations
```

### Recipe 8: Drop with safety
```sql
DROP TABLE IF EXISTS posts CASCADE;        -- cascade through FKs
DROP INDEX IF EXISTS idx_posts_author;
DROP VIEW IF EXISTS user_stats;
DROP TRIGGER IF EXISTS posts_audit ON posts;   -- works; `ON <table>` is mandatory
```

## Pitfalls
- **`INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite-ism) is accepted** — translated to `BIGSERIAL` internally. Use freely in drop-in scenarios.
- **FK violations inside a single transaction** were fixed in v3.22.1 — older versions could see phantom violations during cascading deletes.
- **`PRAGMA foreign_keys = ON;` is a no-op-with-ack** — Nano enforces FKs by default; the PRAGMA exists only for sqlite3 source compatibility.
- **HNSW indexes require explicit `dim`** in `WITH (...)`. Mismatched embedding dimensions will fail at insert time, not at index creation.
- **Triggers never fire at all** — row-level *and* statement-level, every timing, every event. `CREATE TRIGGER` returning `OK` only means it was registered. See Recipe 5; never rely on a trigger for correctness.
- **Materialized view auto-refresh** competes for CPU with foreground queries. Tune `max_cpu_percent`.

## See also
- `heliosdb-nano-query` — DML against the schema you defined.
- `heliosdb-nano-vector` — full HNSW + similarity workflow.
- `heliosdb-nano-migrate` — sqlite3 / PG / MySQL DDL compatibility notes.
- `docs/compatibility/sqlite.md` — SQLite-ism support matrix.
- `docs/compatibility/plpgsql.md` — PL/pgSQL feature support.
