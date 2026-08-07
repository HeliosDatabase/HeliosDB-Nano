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

### `CREATE PROCEDURE`: works, with two rules

Syntax is Nano's own: `CREATE [OR REPLACE] PROCEDURE name(params) LANGUAGE lang AS $$body$$`.
`CALL` executes the body **and binds the call's arguments into it**, provided you follow two
rules.

**Rule 1 — the procedure must be `LANGUAGE sql`.** **Rule 2 — the body must reference
parameters with a `$` sigil**, by name (`$p_id`) or positionally (`$1`). A bare parameter name
never resolves.

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
```

A zero-parameter body works, and a body that never mentions its parameter succeeds while
silently discarding the argument.

What fails:

```sql
-- LANGUAGE plpgsql substitutes nothing, by any spelling.
CREATE PROCEDURE bad1(p_id INTEGER) LANGUAGE plpgsql
    AS $$BEGIN INSERT INTO audit VALUES ($p_id, 'x'); END$$;
CALL bad1(7);   -- ERROR: Invalid parameter placeholder: $p_id. Expected format: $1, $2, etc.

CREATE PROCEDURE bad2(p_id INTEGER) LANGUAGE plpgsql
    AS $$BEGIN INSERT INTO audit VALUES ($1, 'x'); END$$;
CALL bad2(7);   -- ERROR: Parameter $1 not provided. Expected 1 parameters, got 0

-- A bare name fails in EITHER language — the `$` is required.
CREATE PROCEDURE bad3(n INTEGER) LANGUAGE sql AS $$INSERT INTO audit VALUES (n, 'x')$$;
CALL bad3(7);   -- ERROR: Column 'n' not found in schema
```

**Why.** `LANGUAGE sql` routes to `execute_sql_procedure` (`src/sql/functions.rs:353`), which
textually substitutes `$1`…`$N` from the call's arguments and then `$<paramname>` from the
declared parameter list (`:361`–`:372`) before executing the body — that is the entire binding
mechanism, and it only matches `$`-prefixed tokens, which is why a bare `n` survives and
reaches the planner as a column reference. `LANGUAGE plpgsql` routes to
`execute_plpgsql_procedure` (`:381`), which declares the parameters into the procedural scope
but passes body statements on as verbatim text
(`ProceduralStatement::Execute { sql, .. } => (ctx.sql_executor)(sql)?`,
`src/sql/procedural/runtime.rs:446`) and runs them with no bind parameters
(`src/lib.rs:5557`). So `$p_id` reaches the planner's placeholder parser, which requires a
numeric index (`src/sql/planner.rs:3476`), and `$1` arrives unbound.

Within those rules a procedure is a legitimate replacement for a trigger. It is **not** a
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
