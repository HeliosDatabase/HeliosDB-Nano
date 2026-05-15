# Database Management (`CREATE DATABASE` / `DROP DATABASE`)

> **Available since v3.25.0.** Before v3.25.0, the engine ran a single
> implicit database and rejected `CREATE DATABASE` at parse time. Tools
> that expect to provision multiple databases on connect (Gitea, Drizzle,
> SQLAlchemy multi-tenant patterns, Diesel's `setup` command) now work
> against Nano with no engine flags.

## SQL surface

```sql
CREATE DATABASE myapp;
CREATE DATABASE myapp_test;

DROP DATABASE myapp_test;
```

Both statements are accepted on the PG-wire (`psql`, psycopg2, pgx,
JDBC, node-postgres) and the simple-Q and extended-Q paths. They are
parsed by the standard `sqlparser` AST and routed through the same
planner as DDL on tables.

### `IF NOT EXISTS` / `IF EXISTS`

```sql
CREATE DATABASE IF NOT EXISTS myapp;
DROP DATABASE IF EXISTS myapp_test;
```

`IF NOT EXISTS` makes `CREATE DATABASE` idempotent — useful in
migration scripts and Docker entrypoints.

## Behaviour notes

- Databases are **logically isolated** at the catalog level. `SELECT *
  FROM users` in database `myapp` and `SELECT * FROM users` in database
  `myapp_test` are different tables.
- A connection's active database is set by the PG-wire StartupMessage
  `database` parameter (`psql -d myapp …`). Since **v3.25.0** the
  startup handshake validates the requested database against the
  catalog and rejects unknown names with a clear error — previously a
  typo silently fell back to the default database.
- `DROP DATABASE` is transactional w.r.t. its own catalog write but
  is **not** safe to run while other connections hold the same
  database open. The driver will see "database does not exist" on the
  next round-trip after the drop commits.
- The implicit default database (the one the engine creates on first
  startup) cannot be dropped. Drop user-created databases by name.

## Mapping to multi-tenancy

For most multi-tenant workloads we recommend the row-level-security
tenant model documented in
[`docs/guides/tenancy_skill.md`](https://github.com/Dimensigon/HDB-HeliosDB-Nano)
(see the `heliosdb-nano-tenant` skill). It is one to two orders of
magnitude cheaper per-tenant than `CREATE DATABASE`, scales to
thousands of tenants on a single binary, and supports tiered plans
with per-tenant resource limits.

`CREATE DATABASE` is the right choice when:

- You need **strict catalog-level isolation** (different schemas per
  database, not just different rows).
- You're running tools that expect the PG `\c <db>` connect-time
  switch.
- You're staging short-lived test databases inside CI.

## See also

- [`docs/compatibility/information_schema.md`](information_schema.md) —
  the PG-style introspection views that report on databases.
- [`docs/guides/authentication.md`](authentication.md) — SCRAM-SHA-256,
  trust, and PG-wire StartupMessage validation.
