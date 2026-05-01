---
name: heliosdb-nano-schema
description: Define and inspect schema in HeliosDB-Nano. Covers CREATE/ALTER/DROP TABLE with PK/FK/UNIQUE/CHECK/DEFAULT constraints, regular and HNSW vector indexes, views, materialized views, triggers, PL/pgSQL functions, and introspection through Postgres (`pg_class`, `information_schema`), SQLite (`sqlite_master`, `PRAGMA table_info`), and Nano-specific (`\d`, `\dt`, `\dS`, `\dmv`) surfaces. Use this when the user asks "create a table", "add an index", "describe", or "what columns does X have".
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Schema (DDL & Introspection)

## When to use
Any DDL operation: `CREATE`, `ALTER`, `DROP` against tables/indexes/views/triggers/functions; or asking the database what schema exists.

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
| create trigger | SQL | `CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN … END` |
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

### Recipe 5: Trigger (audit log on update)
```sql
CREATE TABLE audit_log (
    id          INTEGER PRIMARY KEY,
    table_name  TEXT,
    row_id      INTEGER,
    op          TEXT,
    ts          TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TRIGGER posts_audit
AFTER UPDATE ON posts
FOR EACH ROW
BEGIN
    INSERT INTO audit_log (table_name, row_id, op)
    VALUES ('posts', NEW.id, 'update');
END;
```

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
DROP TRIGGER IF EXISTS posts_audit ON posts;
```

## Pitfalls
- **`INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite-ism) is accepted** — translated to `BIGSERIAL` internally. Use freely in drop-in scenarios.
- **FK violations inside a single transaction** were fixed in v3.22.1 — older versions could see phantom violations during cascading deletes (see `BUGS_CODE_INDEX_FK_VIOLATION_v3_21_1.md`).
- **`PRAGMA foreign_keys = ON;` is a no-op-with-ack** — Nano enforces FKs by default; the PRAGMA exists only for sqlite3 source compatibility.
- **HNSW indexes require explicit `dim`** in `WITH (...)`. Mismatched embedding dimensions will fail at insert time, not at index creation.
- **Triggers fire only for row-level operations** (`FOR EACH ROW`). Statement-level triggers are not supported.
- **Materialized view auto-refresh** competes for CPU with foreground queries. Tune `max_cpu_percent`.

## See also
- `heliosdb-nano-query` — DML against the schema you defined.
- `heliosdb-nano-vector` — full HNSW + similarity workflow.
- `heliosdb-nano-migrate` — sqlite3 / PG / MySQL DDL compatibility notes.
- `docs/compatibility/sqlite.md` — SQLite-ism support matrix.
- `docs/compatibility/plpgsql.md` — PL/pgSQL feature support.
