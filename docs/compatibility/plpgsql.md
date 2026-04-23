# PL/pgSQL compatibility

HeliosDB Nano accepts `DO $$ … $$` / `DO LANGUAGE plpgsql $tag$ … $tag$` blocks and executes **plain SQL** statement bodies. Full PL/pgSQL control flow (variables, `FOR … IN SELECT … LOOP`, `IF`/`ELSE`, `RAISE`, `EXCEPTION`) is **not** interpreted as of v3.14.0.

When a DO block contains PL/pgSQL syntax, the server returns a clear error — it does **not** silently no-op, which would corrupt migrations that rely on the block running.

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

## Roadmap

A minimal PL/pgSQL interpreter is tracked as a follow-up. Priority depends on customer demand — if you hit a real migration that doesn't fit the patterns above, open an issue with the block and we'll either adjust the rewrite recipes or fast-track the interpreter.

---

*Added in v3.14.1 (2026-04-20).*
