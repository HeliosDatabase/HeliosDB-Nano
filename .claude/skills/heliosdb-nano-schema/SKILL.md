---
name: heliosdb-nano-schema
description: Define and inspect schema in HeliosDB-Nano. Covers CREATE/ALTER/DROP TABLE with PK/FK/UNIQUE/CHECK/DEFAULT constraints, regular and HNSW vector indexes, views, materialized views, stored procedures, and introspection through Postgres (`pg_class`, `information_schema`), SQLite (`sqlite_master`, `PRAGMA table_info`), and Nano-specific (`\d`, `\dt`, `\dS`, `\dmv`) surfaces. Also documents why `CREATE TRIGGER` succeeds but triggers never fire, why a `CREATE FUNCTION` registers but nothing can call it, and the one rule that makes `CREATE PROCEDURE` + `CALL` actually work — read this before proposing a trigger or a user-defined function. Use this when the user asks "create a table", "add an index", "describe", "what columns does X have", or anything about triggers, functions, or stored procedures.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Schema (DDL & Introspection)

## When to use
Any DDL operation: `CREATE`, `ALTER`, `DROP` against tables/indexes/views/functions; or asking the database what schema exists. Also read this before answering any trigger, function, or stored-procedure question — triggers are **not implemented** (Recipe 5) and user-defined functions are **registered but not callable by anything** (Recipe 6). `CREATE TRIGGER` and `CREATE FUNCTION` returning `OK` does not mean either one works. Stored procedures *do* work, under the one rule in Recipe 6.

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
| create function | SQL | ⚠️ **registers, but nothing can call it** — `SELECT f(1)` errors `Unknown scalar function: f`. See Recipe 6 before writing any `CREATE FUNCTION` |
| create procedure | SQL | ✅ works — `CREATE PROCEDURE p(a INT) LANGUAGE sql AS $$…$a…$$` + `CALL p(1)`. Either language (sql or plpgsql); params need the `$` sigil. See Recipe 6 |
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

Procedures are a real escape hatch — they execute and their arguments bind, in either
language, on every client path — with one rule: the body must reference parameters with a
`$` sigil (`$p_id` or `$1`), never a bare name. See Recipe 6 for the working form, and for
the two things to know before recommending it: through 4.11.0 `CALL` was a **silent no-op**
over the PostgreSQL extended protocol and the REST layer (fixed), and `CALL` inside an
explicit `BEGIN` on the embedded API / REPL is refused with an error.
Note that `CREATE FUNCTION` is *not* an option: nothing can call a user-defined function.

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

### Recipe 6: User-defined functions — NOT CALLABLE (read this instead of writing one)

**Do not propose a `CREATE FUNCTION` as a solution.** Every `CREATE FUNCTION` form is
accepted and the function is registered — and then **nothing in the database can call
it.** There is no working invocation route: not a `SELECT` list, not a `WHERE` clause,
not `FROM`, not `CALL`, not a bound-parameter query. All of them error.

All three registration forms succeed:

```sql
CREATE FUNCTION post_count(uid INTEGER) RETURNS INTEGER AS $$
DECLARE
    cnt INTEGER;
BEGIN
    SELECT COUNT(*) INTO cnt FROM posts WHERE author_id = uid;
    RETURN cnt;
END;
$$ LANGUAGE plpgsql;                                              -- OK, registered

CREATE FUNCTION dbl(x INTEGER) RETURNS INTEGER
    AS $$ SELECT x * 2 $$ LANGUAGE sql;                           -- OK, registered

CREATE FUNCTION dbl2(x INTEGER) RETURNS INTEGER RETURN x * 2;     -- OK, registered
```

Every way to call one:

```sql
SELECT post_count(7);                      -- ERROR: Unknown scalar function: post_count
SELECT dbl(21);                            -- ERROR: Unknown scalar function: dbl
SELECT public.dbl(21);                     -- ERROR: Unknown scalar function: public.dbl
SELECT id, dbl(id) FROM posts;             -- ERROR: Unknown scalar function: dbl
SELECT id FROM posts WHERE dbl(id) = 2;    -- ERROR: Unknown scalar function: dbl
SELECT * FROM dbl(21);                     -- ERROR: Table 'dbl' does not exist
CALL dbl(21);                              -- ERROR: Procedure 'dbl' does not exist
PERFORM dbl(21);                           -- ERROR: SQL parse error (PERFORM is not a statement)
```

This is identical on the embedded API and over the PostgreSQL wire, and identical
through the bound-parameter path — `execute_params("SELECT dbl($1)", …)` also returns
`Unknown scalar function: dbl`.

**Why.** The expression evaluator has no link to the function registry at all: its
scalar-function match ends in `_ => Err("Unknown scalar function: {}")`
(`src/sql/evaluator.rs:1154`), so no expression on any path can resolve a user
function. `FunctionRegistry::execute_function` (`src/sql/functions.rs:190`) has exactly
one call site in `src/`, and it is inside `#[cfg(test)] mod tests`
(`src/sql/functions.rs:603`) — the executor works in its unit test and is never reached
in production. `SELECT * FROM f()` is blocked separately: table-valued functions are a
fixed whitelist, `matches!(name, "generate_series" | "unnest")`
(`src/sql/planner.rs:2078`).

**No introspection either.** `information_schema.routines`, `information_schema.parameters`
and `pg_proc` are structurally present on the wire path and return zero rows *with a
function registered* — `query_information_schema_routines` returns `(schema, vec![])` by
construction (`src/protocol/postgres/catalog.rs:2398`). On the embedded path
`information_schema.routines` does not resolve at all and `pg_proc` returns no rows. A
registered function is invisible to every catalog client and every ORM probe.

**Use instead:** inline the expression (`SELECT id * 2 FROM posts`), a view
(`CREATE VIEW post_dbl AS SELECT id, id * 2 AS dbl FROM posts`), a column the
application maintains, or move the logic into application code.

#### Procedures DO work — in either language, with `$`-sigil parameters

Unlike functions, `CREATE PROCEDURE` + `CALL` genuinely executes, **and its arguments do
bind** — in both `LANGUAGE sql` and `LANGUAGE plpgsql` bodies, subject to one rule. Syntax
is Nano's own parser (`src/sql/parser.rs:1789`):
`CREATE [OR REPLACE] PROCEDURE name(params) LANGUAGE lang AS $$body$$`.

> **The rule — reference parameters and `DECLARE`d variables with a `$` sigil**, either by
> name (`$p_id`) or positionally (`$1`). A bare name is always a column reference, in
> either language. PostgreSQL resolves bare PL/pgSQL variable names; Nano deliberately
> does not, so that a variable can never silently shadow a column of the same name.

> **Fixed since 4.11.0 — which client you use mattered, and it does not any more.** Nano
> has two DML executor families. `db.execute()` — psql simple-query, the whole MySQL wire,
> the REPL, the embedded API — had a real `CALL` handler. `db.execute_params()` — the
> PostgreSQL **extended** protocol (psycopg with server-side bind, JDBC, sqlx, Drizzle,
> node-postgres) and every REST/BaaS write — did not: `CALL p()` returned success with `1`
> row affected, **never ran the body**, and "succeeded" for a procedure that did not exist.
> Both now share one implementation, and `CALL` reports **0** rows affected on both. If you
> are auditing an existing deployment, procedures invoked from an extended-protocol driver
> or `/rest/v1` before this fix did not run.

> **Limitation — `CALL` inside an explicit `BEGIN`, embedded API and REPL only.** A
> procedure body runs by re-entering the executor, which re-takes the process-wide
> transaction lock; that lock is not reentrant, so `db.execute("BEGIN")` then `CALL p()`
> used to hang the calling thread. It is now refused with an error naming the procedure and
> saying the body did not run. Issue the `CALL` outside the transaction, or inline the body.
> A `BEGIN` over the PG or MySQL **wire** is a per-session transaction and is unaffected.
> Related, and long-standing: a procedure body does **not** join its caller's transaction —
> its writes autocommit and survive a `ROLLBACK` of the enclosing transaction
> (`docs/plans/ROADMAP_V5.md` §2.11).

The working recipe:

```sql
CREATE TABLE audit_log (id INTEGER, op TEXT);

-- By name. Both parameters bind.
CREATE PROCEDURE log_named(p_id INTEGER, p_op TEXT) LANGUAGE sql
    AS $$INSERT INTO audit_log VALUES ($p_id, $p_op)$$;
CALL log_named(42, 'hello');    -- OK → row (42, 'hello') is inserted

-- Positionally. Same result.
CREATE PROCEDURE log_pos(p_id INTEGER, p_op TEXT) LANGUAGE sql
    AS $$INSERT INTO audit_log VALUES ($1, $2)$$;
CALL log_pos(7, 'seven');       -- OK → row (7, 'seven') is inserted

-- LANGUAGE plpgsql, same sigil, same result. NEW: through 4.10.2 a plpgsql body
-- substituted nothing at all, by any spelling.
CREATE PROCEDURE log_pg(p_id INTEGER, p_op TEXT) LANGUAGE plpgsql
    AS $$BEGIN INSERT INTO audit_log VALUES ($p_id, $p_op); END$$;
CALL log_pg(7, 'seven');        -- OK → row (7, 'seven') is inserted
```

A zero-parameter body works too (`CREATE PROCEDURE touch() LANGUAGE sql AS $$INSERT INTO
audit_log VALUES (0, 'touch')$$;` → `CALL touch();`), and a body that simply never mentions
its parameter succeeds while discarding the argument — silently, with no warning.

Substitution is **literal-aware**: a `$`-token inside a `'string literal'`, an `E'…'`
escape string, a `"quoted identifier"`, a `--` or `/* … */` comment, or a `$tag$ … $tag$`
block is data, not a placeholder. Names use longest-match, so `$p` never captures the
prefix of `$p_id` and `$1` never captures the prefix of `$10`, in any declaration order.
Substituted values are never re-scanned, so argument data cannot influence how another
placeholder is interpolated.

What fails:

```sql
-- A bare name never works — in EITHER language. The `$` is required.
CREATE PROCEDURE bad1(n INTEGER) LANGUAGE sql
    AS $$INSERT INTO audit_log VALUES (n, 'x')$$;
CALL bad1(7);                   -- ERROR: Column 'n' not found in schema

CREATE PROCEDURE bad2(n INTEGER) LANGUAGE plpgsql
    AS $$BEGIN INSERT INTO audit_log VALUES (n, 'x'); END$$;
CALL bad2(7);                   -- ERROR: Column 'n' not found in schema

-- A placeholder naming nothing is left verbatim on purpose, so typos still fail loudly.
CREATE PROCEDURE bad3(p_id INTEGER) LANGUAGE sql
    AS $$INSERT INTO audit_log VALUES ($oops, 'x')$$;
CALL bad3(7);
-- ERROR: Invalid parameter placeholder: $oops. Expected format: $1, $2, etc.
```

**Why.** Both languages go through one shared scanner, `src/sql/interpolate.rs`.
`LANGUAGE sql` calls it from `execute_sql_procedure` (`src/sql/functions.rs`) with the
declared parameters plus the call's arguments; `LANGUAGE plpgsql` calls it from
`ExecutionContext::interpolate` (`src/sql/procedural/runtime.rs`) with the procedural
variable scope plus the call's arguments, immediately before each body statement runs.
Because the scanner only matches `$`-prefixed tokens, a bare `n` survives and reaches the
planner as a column reference.

**Limitation — `:=` assignments.** The procedural expression parser does not evaluate
expressions; it stores the raw expression TEXT, so a local assigned `v := a + 1` holds the
string `a + 1` and `$v` interpolates that text quoted, not a computed value. Parameter
references are the reliable case. `EXECUTE '<dynamic sql>'` is *not* interpolated, matching
PostgreSQL.

`CALL` executes at all because `FunctionRegistry::execute_procedure` has a real call site —
unlike `execute_function`, which is why functions are dead and procedures are not. That call
site is now `EmbeddedDatabase::execute_call_plan` (`src/lib.rs`), the single implementation
both executor families dispatch to; the `sql::Executor` `Call` arm
(`src/sql/executor/mod.rs`) returns an error rather than a status message, because an
`Executor` holds no function registry and cannot run a body.

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
- **User-defined functions are not callable by anything** — `CREATE FUNCTION` returning `OK` only means it was registered. `SELECT f(x)` errors `Unknown scalar function: f`, in a `SELECT` list, a `WHERE` clause, a `FROM` clause, via `CALL`, and via bound parameters, on both the embedded API and the wire. There is no `PERFORM`. See Recipe 6.
- **Procedures work in either language, but the `$` sigil is mandatory** — a *bare* parameter name fails with `Column 'n' not found in schema` in both `LANGUAGE sql` and `LANGUAGE plpgsql`, deliberately, so a variable can never shadow a column. (plpgsql binding is new; through 4.10.2 a plpgsql body substituted nothing at all.) An argument a body never mentions is silently discarded, and a `$`-token inside a string literal, comment or `$tag$…$tag$` block is data, not a placeholder. `CALL` now runs the body on every client path and reports 0 rows affected — through 4.11.0 it was a silent no-op (returning 1 row affected, and "succeeding" for a missing procedure) over the PG extended protocol and the REST layer. On the embedded API / REPL only, `CALL` inside an explicit `BEGIN` is refused with an error. See Recipe 6.
- **`information_schema.routines`, `information_schema.parameters` and `pg_proc` are always empty** — they return zero rows even with a function registered, so no ORM or catalog client can discover a user-defined routine.
- **Materialized view auto-refresh** competes for CPU with foreground queries. Tune `max_cpu_percent`.

## See also
- `heliosdb-nano-query` — DML against the schema you defined.
- `heliosdb-nano-vector` — full HNSW + similarity workflow.
- `heliosdb-nano-migrate` — sqlite3 / PG / MySQL DDL compatibility notes.
- `docs/compatibility/sqlite.md` — SQLite-ism support matrix.
- `docs/compatibility/plpgsql.md` — `DO`-block support, plus the `CREATE FUNCTION` / `CREATE PROCEDURE` limits behind Recipe 6.
- `docs/compatibility/information_schema.md` — which `information_schema` views return data and which are always empty (`routines`, `parameters`).
