# PL/pgSQL compatibility

This page is about **`DO` blocks**. HeliosDB Nano accepts `DO $$ … $$` / `DO LANGUAGE plpgsql $tag$ … $tag$` blocks and executes **plain SQL** statement bodies. Full PL/pgSQL control flow (variables, `FOR … IN SELECT … LOOP`, `IF`/`ELSE`, `RAISE`, `EXCEPTION`) is **not** interpreted.

When a DO block contains PL/pgSQL syntax, the server returns a clear error — it does **not** silently no-op, which would corrupt migrations that rely on the block running.

> **`CREATE FUNCTION` is a different, and worse, story.** Nothing on this page implies
> that a `CREATE FUNCTION … LANGUAGE plpgsql AS $$ … $$` body is interpreted or that the
> function can be called. It cannot. See
> ["Named routines"](#named-routines-create-function--create-procedure) below.

## Supported

- `DO $$ BEGIN <plain SQL>; <plain SQL>; END $$;`
- `DO $tag$ BEGIN <plain SQL>; END $tag$;`
- `DO LANGUAGE plpgsql $$ <plain SQL> $$;`

Bodies may contain `CREATE`, `ALTER`, `DROP`, `INSERT`, `UPDATE`, `DELETE`, and `SELECT` statements separated by `;`. Each runs as its own implicit transaction.

## Not supported

- `DECLARE <name> <type>;` — variables
- `FOR <var> IN SELECT … LOOP` / `FOR <i> IN 1..n LOOP` — loops
- `IF <cond> THEN … ELSIF … END IF;` — conditionals
- `WHILE <cond> LOOP … END LOOP;`
- `RAISE NOTICE | EXCEPTION | WARNING …;`
- `RETURN`, `PERFORM`, `EXIT`, `CONTINUE`
- `EXCEPTION WHEN … THEN … END;` — block-level error handling
- Variable assignment (`<var> := <expr>`)
- Cursors (`DECLARE cur CURSOR FOR …; FETCH …`)

## Error message

When HeliosDB Nano detects PL/pgSQL control-flow tokens in a DO block body, it returns:

```
ERROR:  PL/pgSQL control flow (`<KEYWORD>`) inside DO blocks is not yet
        supported in HeliosDB Nano. Rewrite the block as plain SQL, or
        execute each statement separately.
        See: docs/compatibility/plpgsql.md
```

`<KEYWORD>` is the first control-flow token we spotted, chosen to help you locate the offending line.

## Named routines (`CREATE FUNCTION` / `CREATE PROCEDURE`)

Everything above concerns anonymous `DO` blocks. Named routines are a separate surface
with separate — and more severe — limits.

### `CREATE FUNCTION`: registers, but nothing can call it

All three forms are accepted and the function is stored in the registry:

```sql
CREATE FUNCTION dbl(x INTEGER) RETURNS INTEGER AS $$ BEGIN RETURN x * 2; END $$ LANGUAGE plpgsql;  -- OK
CREATE FUNCTION dbl(x INTEGER) RETURNS INTEGER AS $$ SELECT x * 2 $$ LANGUAGE sql;                 -- OK
CREATE FUNCTION dbl(x INTEGER) RETURNS INTEGER RETURN x * 2;                                       -- OK
```

The body is **never interpreted**, because it is never reached: there is no invocation
route.

```sql
SELECT dbl(21);                            -- ERROR: Unknown scalar function: dbl
SELECT public.dbl(21);                     -- ERROR: Unknown scalar function: public.dbl
SELECT id, dbl(id) FROM t;                 -- ERROR: Unknown scalar function: dbl
SELECT id FROM t WHERE dbl(id) = 2;        -- ERROR: Unknown scalar function: dbl
SELECT * FROM dbl(21);                     -- ERROR: Table 'dbl' does not exist
CALL dbl(21);                              -- ERROR: Procedure 'dbl' does not exist
PERFORM dbl(21);                           -- ERROR: SQL parse error (PERFORM is not a statement)
```

Same on the embedded API and over the PostgreSQL wire, and same through bound
parameters (`SELECT dbl($1)` → `Unknown scalar function: dbl`). The cause is structural:
the expression evaluator holds no reference to the function registry and falls through to
`Unknown scalar function` (`src/sql/evaluator.rs:1154`), and
`FunctionRegistry::execute_function` (`src/sql/functions.rs:190`) has exactly one call
site in the crate, inside `#[cfg(test)] mod tests` (`src/sql/functions.rs:603`).
`SELECT * FROM f()` is blocked separately by a fixed table-function whitelist of
`generate_series | unnest` (`src/sql/planner.rs:2078`).

A registered function is also invisible: `information_schema.routines`,
`information_schema.parameters` and `pg_proc` return zero rows with it registered (see
[`information_schema` compatibility](information_schema.md)).

### `CREATE PROCEDURE`: works, with one rule

Syntax is Nano's own: `CREATE [OR REPLACE] PROCEDURE name(params) LANGUAGE lang AS $$body$$`.
`CALL` executes the body **and binds the call's arguments into it**, in **both**
`LANGUAGE sql` and `LANGUAGE plpgsql` bodies, provided you follow one rule.

> **Fixed since 4.11.0 — `CALL` used to run nothing over the extended protocol.** Nano has two
> DML executor families. `db.execute()` (psql simple-query, the whole MySQL wire, the REPL, the
> embedded API) had a real `CALL` handler; `db.execute_params()` — the PostgreSQL **extended**
> protocol (psycopg with server-side bind, JDBC, sqlx, Drizzle, node-postgres) and every
> REST/BaaS write — did not. On that path `CALL p()` returned success with `1` row affected,
> **never ran the body**, and reported success even for a procedure that did not exist. Both
> families now dispatch to one shared implementation, and `CALL` reports **0** rows affected
> (PostgreSQL's `CALL` command tag carries no row count). If you relied on a procedure invoked
> from an extended-protocol driver or through `/rest/v1`, re-check that its work actually
> happened.

**The rule — reference parameters and `DECLARE`d variables with the `$` sigil**, by name
(`$p_id`) or positionally (`$1`). A bare name is always a column reference. (PostgreSQL itself
resolves bare PL/pgSQL variable names; Nano deliberately does not, so that a variable can never
silently shadow a column.)

```sql
CREATE TABLE audit (id INTEGER, op TEXT);

-- By name.
CREATE PROCEDURE log_named(p_id INTEGER, p_op TEXT) LANGUAGE sql
    AS $$INSERT INTO audit VALUES ($p_id, $p_op)$$;
CALL log_named(42, 'hello');   -- OK → (42, 'hello') inserted

-- Positionally. Same result.
CREATE PROCEDURE log_pos(p_id INTEGER, p_op TEXT) LANGUAGE sql
    AS $$INSERT INTO audit VALUES ($1, $2)$$;
CALL log_pos(7, 'seven');      -- OK → (7, 'seven') inserted

-- LANGUAGE plpgsql, same sigil, same result. (New: through 4.10.2 a plpgsql body
-- substituted nothing — `$p_id` errored `Invalid parameter placeholder` and `$1`
-- errored `Parameter $1 not provided`.)
CREATE PROCEDURE log_pg(p_id INTEGER) LANGUAGE plpgsql
    AS $$BEGIN INSERT INTO audit VALUES ($p_id, 'x'); END$$;
CALL log_pg(7);                -- OK → (7, 'x') inserted
```

A zero-parameter body works, and a body that never mentions its parameter succeeds while
silently discarding the argument.

Substitution is **literal-aware**: a `$`-token inside a `'string literal'`, an `E'…'` escape
string, a `"quoted identifier"`, a `--` or `/* … */` comment, or a `$tag$ … $tag$` block is
data, not a placeholder, and is passed through untouched. Names use longest-match, so `$p` can
never capture the prefix of `$p_id` and `$1` can never capture the prefix of `$10`, in any
declaration order. Substituted values are never re-scanned, so argument data cannot influence
the interpolation of another placeholder. A placeholder that resolves to nothing is left
verbatim on purpose, so a typo still fails loudly downstream.

What fails:

```sql
-- A bare name fails in EITHER language — the `$` is required.
CREATE PROCEDURE bad1(n INTEGER) LANGUAGE sql AS $$INSERT INTO audit VALUES (n, 'x')$$;
CALL bad1(7);   -- ERROR: Column 'n' not found in schema

CREATE PROCEDURE bad2(n INTEGER) LANGUAGE plpgsql
    AS $$BEGIN INSERT INTO audit VALUES (n, 'x'); END$$;
CALL bad2(7);   -- ERROR: Column 'n' not found in schema

-- A placeholder that names nothing stays verbatim and reaches the planner.
CREATE PROCEDURE bad3(p_id INTEGER) LANGUAGE sql AS $$INSERT INTO audit VALUES ($oops, 'x')$$;
CALL bad3(7);   -- ERROR: Invalid parameter placeholder: $oops. Expected format: $1, $2, etc.
```

**Why.** Both languages go through one shared scanner, `src/sql/interpolate.rs`. The
`LANGUAGE sql` path calls it from `execute_sql_procedure` (`src/sql/functions.rs`) with the
call's declared parameters and arguments; the `LANGUAGE plpgsql` path calls it from
`ExecutionContext::interpolate` (`src/sql/procedural/runtime.rs`) with the procedural variable
scope plus the call's arguments, immediately before each body statement is executed. Because
the scanner matches only `$`-prefixed tokens, a bare `n` survives and reaches the planner as a
column reference.

**Limitation — `:=` assignments.** The procedural expression parser does not evaluate
expressions; it stores the raw expression TEXT. So a local assigned with `v := a + 1` holds the
string `a + 1`, and `$v` interpolates that text quoted, not a computed value. Parameter
references are the reliable case. `EXECUTE '<dynamic sql>'` is *not* interpolated, matching
PostgreSQL.

**Limitation — `CALL` inside an explicit transaction, embedded API and REPL only.** A procedure
body runs by re-entering the executor, which re-takes the process-wide transaction lock; that
lock is not reentrant, so `db.execute("BEGIN")` followed by `CALL p()` used to **hang the
calling thread**. It is now refused with an error naming the procedure and stating that the body
did not run. Issue the `CALL` outside the transaction, or inline the body as ordinary
statements. A `BEGIN` over the PostgreSQL or MySQL **wire** opens a per-session transaction,
which does not use that lock, and is unaffected.

**Behaviour to know — a procedure body does not join its caller's transaction.** Body statements
are executed through a fresh autocommit path, so under a wire `BEGIN` (or the embedded RAII
`db.begin_transaction()` handle) the body's writes commit independently and survive a
`ROLLBACK` of the enclosing transaction. This is long-standing behaviour, not new; it is tracked
in `docs/plans/ROADMAP_V5.md` §2.11.

Within that rule a procedure is a legitimate replacement for a trigger. It is **not** a
replacement for a callable function — the body is a SQL statement to run, not an expression to
evaluate, and `CALL` returns no value.

## Migration patterns

### Backfill loop → plain SQL `UPDATE … FROM`

Common Drizzle / Prisma migration:

```sql
DO $$
DECLARE u RECORD;
BEGIN
  FOR u IN SELECT id, email FROM users LOOP
    INSERT INTO user_profile (user_id, display_name)
    VALUES (u.id, u.email);
  END LOOP;
END $$;
```

Rewrite as a single `INSERT … SELECT`:

```sql
INSERT INTO user_profile (user_id, display_name)
SELECT id, email FROM users;
```

### Conditional index creation → `CREATE INDEX IF NOT EXISTS`

```sql
-- Not supported
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = 'users_email_idx') THEN
    CREATE INDEX users_email_idx ON users(email);
  END IF;
END $$;

-- Use instead
CREATE INDEX IF NOT EXISTS users_email_idx ON users(email);
```

### Conditional data load → `INSERT … ON CONFLICT DO NOTHING`

```sql
-- Not supported
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM tenants WHERE id = 'default') THEN
    INSERT INTO tenants (id, name) VALUES ('default', 'Default Tenant');
  END IF;
END $$;

-- Use instead
INSERT INTO tenants (id, name) VALUES ('default', 'Default Tenant')
ON CONFLICT (id) DO NOTHING;
```

## Follow-up

A minimal PL/pgSQL interpreter is tracked as a follow-up, as is wiring the function
registry into the expression evaluator so `CREATE FUNCTION` becomes callable at all.
Priority depends on customer demand — if you hit a real migration that doesn't fit the
patterns above, open an issue with the block and we'll either adjust the rewrite recipes
or fast-track the interpreter.
