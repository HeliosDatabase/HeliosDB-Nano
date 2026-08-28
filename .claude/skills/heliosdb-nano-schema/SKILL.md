---
name: heliosdb-nano-schema
description: Define and inspect schema in HeliosDB-Nano. Covers CREATE/ALTER/DROP TABLE with PK/FK/UNIQUE/CHECK/DEFAULT constraints, regular and HNSW vector indexes, views, materialized views, stored procedures, and introspection through Postgres (`pg_class`, `information_schema`), SQLite (`sqlite_master`, `PRAGMA table_info`), and Nano-specific (`\d`, `\dt`, `\dS`, `\dmv`) surfaces. Also documents why `CREATE TRIGGER` succeeds but trigger bodies never run, what user-defined functions can and cannot do (scalar calls work, under the `$`-sigil rule), and the one rule that makes `CREATE PROCEDURE` + `CALL` actually work — read this before proposing a trigger or a user-defined function. Use this when the user asks "create a table", "add an index", "describe", "what columns does X have", or anything about triggers, functions, or stored procedures.
allowed-tools: Bash(heliosdb-nano *), Bash(psql *), Read
---

# Schema (DDL & Introspection)

## When to use
Any DDL operation: `CREATE`, `ALTER`, `DROP` against tables/indexes/views/functions; or asking the database what schema exists. Also read this before answering any trigger, function, or stored-procedure question — triggers are **not implemented** (Recipe 5) and user-defined functions are **callable only in scalar position, and only with `$`-sigil parameters** (Recipe 6). `CREATE TRIGGER` returning `OK` does not mean it works; `CREATE FUNCTION` does work for scalar calls but NOT for `SELECT * FROM f()`, overloading, `CALL f()` or catalog introspection. Stored procedures *do* work, under the same `$`-sigil rule.

## Verbs

| Verb | Surface | One-liner |
|------|---------|-----------|
| create table | SQL | `CREATE TABLE t (id INT PRIMARY KEY, …)` |
| alter table (multi-op) | SQL | `ALTER TABLE t ADD COLUMN c TEXT, DROP COLUMN d, RENAME e TO f` |
| drop table | SQL | `DROP TABLE [IF EXISTS] t` |
| create index | SQL | `CREATE INDEX idx_t_c ON t(c)` |
| create vector index | SQL | `CREATE INDEX vidx ON t USING HNSW (embedding) WITH (dim = 384, metric = 'cosine')` |
| drop index | — | ❌ **not supported** — `DROP INDEX` errors (`DROP INDEX is not supported yet`). Through 4.19.0 it was planned as `DROP TABLE` and could destroy a table sharing the index's name. Drop the table, or leave the index in place |
| create view | SQL | `CREATE VIEW v AS SELECT …` |
| create materialized view | SQL | `CREATE MATERIALIZED VIEW mv AS SELECT … WITH (auto_refresh = true)` |
| create trigger | SQL | ⚠️ **registered, persisted, but the BODY never runs** — see "Triggers" below before writing any `CREATE TRIGGER` |
| create function | SQL | ✅ scalar calls work — `CREATE FUNCTION f(x INT) RETURNS INT AS $$ SELECT $1 * 2 $$ LANGUAGE sql` + `SELECT f(1)`. The `$` sigil is MANDATORY (a bare `x` is a column ref). No `SELECT * FROM f()`, no overloading, no `pg_proc` row. See Recipe 6 |
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
The `WITH (...)` clause is accepted in either position — after the query as above, or in the
PostgreSQL-standard spot between the view name and `AS` — and works with `IF NOT EXISTS`. You
can also flip it later: `ALTER MATERIALIZED VIEW user_stats SET (auto_refresh = true)`.

**`auto_refresh = true` only records the opt-in; it does not start anything.** The refresh loop
is driven by the embedded library API `EmbeddedDatabase::start_auto_refresh(config)` — there is
no CLI flag, SQL statement, HTTP endpoint or config key that starts it (`[materialized_views]
auto_refresh_default` is reserved and wired to nothing). Without that call, an opted-in view
behaves exactly like a manual one: it only changes on `REFRESH MATERIALIZED VIEW`. Do not tell a
user that `WITH (auto_refresh = true)` alone will keep a view current.

Once the worker is running, opted-in views are FULL-recomputed on a time-based staleness
schedule tuned by `[materialized_views]` (`refresh_check_interval_secs`,
`default_max_cpu_percent`, `max_concurrent_refreshes`). The per-view `max_cpu_percent` above is
stored but NOT enforced — the global limit governs. (Before the fix in 4.15.0, auto-refresh
never refreshed at all, in any spelling.)

Inspect with `\dmv` (REPL), `SELECT * FROM pg_matviews` (one row per materialised view, with its
`definition`; `ispopulated` is false until the first refresh), or `SELECT * FROM
pg_mv_staleness()`. No view projects the `auto_refresh` flag, so the opt-in cannot be read back
over the wire.

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
A user-defined FUNCTION is a *partial* alternative: `SELECT f(x)` works in scalar position
(under the `$`-sigil rule), but it cannot perform a side effect for you the way a procedure
body can, and `CALL f()` does not exist. See Recipe 6.

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
SELECT COUNT(*) FROM audit_log;                          -- 0  ← body never ran
```

This holds for every timing (`BEFORE` / `AFTER` / `INSTEAD OF`), every event, both
`FOR EACH ROW` and `FOR EACH STATEMENT`, and with or without a `WHEN (…)` clause —
all of them parse and register, none of them execute. It is true on EVERY interface:
embedded `execute()`/`execute_params()`, the PostgreSQL simple and extended query
protocols, the MySQL wire, the REPL and REST.

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

Since **4.20.0** this exception is uniform and durable:

- it applies identically on BOTH DML executor families, so a REST / JDBC / sqlx /
  psycopg-with-bound-params insert and a `psql` simple-query insert into the same table
  finally produce the SAME row, and `INSERT … RETURNING` (which always routes through the
  params family) reflects the rewrite. Through 4.19.0 it applied on the text family only.
- `CREATE TRIGGER` / `DROP TRIGGER` sent over the PostgreSQL **extended** query protocol
  used to fail outright with `Operator not yet implemented: CreateTrigger { … }`; they now
  succeed. A migration tool that previously caught that error will now see the statement
  pass — and create a trigger whose body does not run.
- the trigger's `WHEN (…)` clause and its enabled flag are honoured (before 4.20.0 the
  rewrite hit every row regardless of the predicate). `WHEN` false, NULL, or unevaluable
  all mean "not fired". Multiple recipes on one table fire in trigger-name order.
- it survives a restart (both the definition and the compiled recipe are persisted).
  Caveat: a trigger created after the last checkpoint and lost to a crash comes back
  definition-only, i.e. inert — `DROP TRIGGER` and re-create it.

**`DROP TABLE` deregisters the table's triggers (since 4.20.0).** In-memory
registrations and the persisted records both go. Before 4.20.0 the registration outlived
the table, so re-creating the table and its trigger failed with
`Trigger 'x' already exists on table 't'` and the name stayed burned for the lifetime of
the process.

**No trigger introspection exists either**: there is no `pg_trigger` view,
`information_schema.triggers` is always empty by design, psql's `\d` reports
`relhastriggers = false` (the column does not exist on `pg_class` on any other
route), and no REPL meta-command lists triggers. There is also no
`ALTER TABLE … ENABLE/DISABLE TRIGGER` surface, so the enabled flag the rewrite now
honours is only reachable from the library API.

Registration IS durable since 4.20.0. (Documentation correction: releases through 4.19.0
claimed a registered trigger "survives exactly one restart". It survived none — WAL replay
registered it into a registry the SQL executor never reads.)

### Recipe 6: User-defined functions — scalar calls work, under the `$`-sigil rule

**A `CREATE FUNCTION` is a usable solution for SCALAR logic**, on every client path, and
the definition survives a restart. It is NOT usable as a set-returning function, is not
overloadable, and is invisible to catalog introspection — the exact boundaries are below.

```sql
CREATE FUNCTION dbl(x INTEGER) RETURNS INTEGER
    AS $$ SELECT $1 * 2 $$ LANGUAGE sql;

SELECT dbl(21);                            -- 42
SELECT public.dbl(21);                     -- 42
SELECT id, dbl(id) FROM posts;             -- evaluated per row
SELECT id FROM posts WHERE dbl(id) = 2;    -- filters
-- identical through bound parameters:
execute_params("SELECT dbl($1)", [21])     -- 42
```

**THE `$` SIGIL IS MANDATORY** — the single rule that catches everyone. Reference a
parameter as `$1` or `$name`. A bare `x` is a COLUMN reference and fails with
`Column 'x' not found in schema`. This is deliberate (a variable must never silently
shadow a column) and it applies to `LANGUAGE sql` and `LANGUAGE plpgsql` alike. So the
PostgreSQL-idiomatic `AS $$ SELECT x * 2 $$` does NOT work; write `$$ SELECT $1 * 2 $$`.

PL/pgSQL function bodies support a `DECLARE` section (no `:=` initialisers), SQL
statements, `SELECT … INTO <var>`, nested blocks, and `RETURN <expr>`:

```sql
CREATE FUNCTION post_count(uid INTEGER) RETURNS INTEGER AS $$
DECLARE cnt INTEGER;
BEGIN
    SELECT COUNT(*) INTO cnt FROM posts WHERE author_id = $uid;   -- $uid, not uid
    RETURN $cnt;                                                  -- $cnt, not cnt
END;
$$ LANGUAGE plpgsql;

SELECT post_count(7);                      -- 2
```

**Control flow inside a FUNCTION body is REFUSED, loudly.** `IF` / `CASE` / `LOOP` /
`WHILE` / `FOR`, `:=` assignment, `DECLARE v INT := …`, `RAISE`, `EXIT` / `CONTINUE`,
cursors, dynamic `EXECUTE`, `RETURN NEXT` / `RETURN QUERY` and `EXCEPTION` handlers all
produce an error naming the construct. The reason: the procedural expression parser
stores an expression's raw TEXT instead of parsing it, so an `IF` condition would never be
true and the ELSE branch would always run. An error beats that wrong answer. Express the
branch in SQL instead — `RETURN (SELECT CASE WHEN $1 > 0 THEN 1 ELSE 2 END)`.
(PROCEDURE bodies are not gated, so an `IF` in a procedure still silently mis-branches.)

**Recursion** is bounded by `[session] udf_max_call_depth` in `config.toml` (default 32);
a self-recursive body fails with an explicit depth-limit error.

**One transaction limitation.** On the **embedded API and the REPL**, calling a function
inside an explicit `BEGIN` is refused with an error — the body re-enters the executor and
would deadlock on the global, non-reentrant transaction lock. A `BEGIN` over the PG or
MySQL wire is a per-session transaction and is unaffected. As with `CALL`, the body does
not join the caller's transaction.

**What still does NOT work:**

```sql
SELECT * FROM dbl(21);                     -- ERROR: Table 'dbl' does not exist
                                           --   (no set-returning functions; RETURNS TABLE(...)
                                           --    parses but its column list is discarded)
CALL dbl(21);                              -- ERROR: Procedure 'dbl' does not exist
                                           --   (functions and procedures are separate namespaces)
PERFORM dbl(21);                           -- ERROR: SQL parse error (PERFORM is not a statement)
SELECT reporting.dbl(21);                  -- ERROR: Unknown scalar function (only `public.` qualifies)

CREATE FUNCTION f(a INT) …;                -- OK
CREATE FUNCTION f(a TEXT, b TEXT) …;       -- ERROR: Function 'f' already exists (no overloading —
                                           --   the registry keys on the name alone)

CREATE FUNCTION g(x INT) RETURNS INT RETURN x * 2;   -- registers, but the stored body is the
                                                     -- literal text `RETURN x * 2`, which is not
                                                     -- SQL: calling it errors. Use the AS $$…$$ form.
```

**No introspection.** `information_schema.routines`, `information_schema.parameters` and
`pg_proc` still return zero rows with a function registered, so a function is invisible to
ORM probes and catalog clients. `POST /rest/v1/rpc/<fn>` is still HTTP 501.

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
-- DROP INDEX is NOT supported: it errors, and `IF EXISTS` does not silence it.
-- (Through 4.19.0 it was planned as DROP TABLE — `DROP INDEX IF EXISTS posts`
--  silently dropped the TABLE `posts`. Never issue it against older builds.)
DROP VIEW IF EXISTS user_stats;
DROP TRIGGER IF EXISTS posts_audit ON posts;   -- works; `ON <table>` is mandatory
```

## Pitfalls
- **`INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite-ism) is accepted** — translated to `BIGSERIAL` internally. Use freely in drop-in scenarios.
- **FK violations inside a single transaction** were fixed in v3.22.1 — older versions could see phantom violations during cascading deletes.
- **`PRAGMA foreign_keys = ON;` is a no-op-with-ack** — Nano enforces FKs by default; the PRAGMA exists only for sqlite3 source compatibility.
- **HNSW indexes require explicit `dim`** in `WITH (...)`. Mismatched embedding dimensions will fail at insert time, not at index creation.
- **Trigger BODIES never execute** — row-level *and* statement-level, every timing, every event, every interface. `CREATE TRIGGER` returning `OK` only means it was registered and persisted. The sole exception is the `BEFORE INSERT … FOR EACH ROW` `NEW.<col> = <expr>` / `RETURN NULL` row rewrite. See Recipe 5; never rely on a trigger body for correctness.
- **User-defined functions are SCALAR-only, and the `$` sigil is mandatory** — `SELECT f(x)` works in a `SELECT` list, a `WHERE` clause and through bound parameters, on the embedded API and both wires, and survives a restart. But `SELECT * FROM f()` fails as a missing table, `CALL f()` fails (`Procedure 'f' does not exist`), there is no `PERFORM`, no overloading, no non-`public` qualifier, and `pg_proc` / `information_schema.routines` stay empty. A body that writes `x` instead of `$1` fails with a column error. PL/pgSQL `IF`/loops/`:=` inside a FUNCTION body are refused with an explicit error. See Recipe 6.
- **Procedures work in either language, but the `$` sigil is mandatory** — a *bare* parameter name fails with `Column 'n' not found in schema` in both `LANGUAGE sql` and `LANGUAGE plpgsql`, deliberately, so a variable can never shadow a column. (plpgsql binding is new; through 4.10.2 a plpgsql body substituted nothing at all.) An argument a body never mentions is silently discarded, and a `$`-token inside a string literal, comment or `$tag$…$tag$` block is data, not a placeholder. `CALL` now runs the body on every client path and reports 0 rows affected — through 4.11.0 it was a silent no-op (returning 1 row affected, and "succeeding" for a missing procedure) over the PG extended protocol and the REST layer. On the embedded API / REPL only, `CALL` inside an explicit `BEGIN` is refused with an error. See Recipe 6.
- **`information_schema.routines`, `information_schema.parameters` and `pg_proc` never report a user-defined routine** — so no ORM or catalog client can discover one. `pg_proc` is registered everywhere and returns zero rows. `routines` and `parameters` return zero rows over the PostgreSQL and MySQL wires, but on the embedded API / REPL / Python binding they are *unknown relations* and raise an error — they are not registered in the phase-3 view registry.
- **Materialized view auto-refresh** competes for CPU with foreground queries. Tune `max_cpu_percent`.

## See also
- `heliosdb-nano-query` — DML against the schema you defined.
- `heliosdb-nano-vector` — full HNSW + similarity workflow.
- `heliosdb-nano-migrate` — sqlite3 / PG / MySQL DDL compatibility notes.
- `docs/compatibility/sqlite.md` — SQLite-ism support matrix.
- `docs/compatibility/plpgsql.md` — `DO`-block support, plus the `CREATE FUNCTION` / `CREATE PROCEDURE` limits behind Recipe 6.
- `docs/compatibility/information_schema.md` — which `information_schema` views return data and which are always empty (`routines`, `parameters`).
