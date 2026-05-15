# `information_schema` Compatibility

> **Available since v3.24.0.** Earlier versions exposed only the
> introspection views that the `psql \d`-family commands required;
> SQL-standard `information_schema` queries against the long-tail
> views (`character_sets`, `routines`, `parameters`, …) returned
> empty result sets or undefined columns. v3.24.0 completes the surface
> and v3.24.0+ raises a loud error on `information_schema.<unknown>`
> rather than silently returning rows from the wrong view.

Nano implements the PostgreSQL flavour of the SQL-standard
`information_schema`. All views are read-only and reflect the catalog
state at query time (no caching, no staleness).

## Covered views

### Tables, columns, and constraints

| View | Status | Notes |
|------|--------|-------|
| `information_schema.tables` | Complete | Filters by `table_schema`, `table_name`, `table_type` |
| `information_schema.columns` | Complete | Includes `data_type`, `is_nullable`, `column_default`, `ordinal_position` |
| `information_schema.key_column_usage` | Complete | PK and FK columns with `constraint_name` |
| `information_schema.table_constraints` | Complete | `PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK` |
| `information_schema.referential_constraints` | Complete | FK `match_option`, `update_rule`, `delete_rule` |
| `information_schema.check_constraints` | Complete | `check_clause` is the raw SQL expression |
| `information_schema.constraint_column_usage` | Complete | Resolves FK / CHECK constraints back to their referenced columns |

### Schemas, catalogs, databases

| View | Status | Notes |
|------|--------|-------|
| `information_schema.schemata` | Complete | One row per registered schema |
| `information_schema.catalog_name` | Complete | Single-row view; returns the current database name |
| `information_schema.character_sets` | Complete (v3.24.0) | Single-row UTF-8 entry |
| `information_schema.collations` | Complete (v3.24.0) | UTF-8 collation + the `C` POSIX collation |

### Routines and parameters

| View | Status | Notes |
|------|--------|-------|
| `information_schema.routines` | Complete (v3.24.0) | PL/pgSQL functions registered via `CREATE FUNCTION` |
| `information_schema.parameters` | Complete (v3.24.0) | Per-routine parameter rows (ordinal, mode, data type) |

### Views and views-on-views

| View | Status | Notes |
|------|--------|-------|
| `information_schema.views` | Complete | `view_definition` is the raw `CREATE VIEW` body |
| `information_schema.view_table_usage` | Complete (v3.24.0) | Edges from views to the base tables they reference |
| `information_schema.view_column_usage` | Complete (v3.24.0) | Edges from views to the base columns they reference |

### Privileges (RLS / multi-tenancy)

| View | Status | Notes |
|------|--------|-------|
| `information_schema.table_privileges` | Complete | Resolved against the active `current_tenant()` |
| `information_schema.column_privileges` | Complete | Same |
| `information_schema.role_table_grants` | Complete (v3.24.0) | Pre-resolved grants per role |
| `information_schema.role_column_grants` | Complete (v3.24.0) | Same |

## Strict-unknown-view behaviour (v3.24.0+)

A reference to `information_schema.<unknown>` raises an error at parse
time:

```sql
SELECT * FROM information_schema.does_not_exist;
-- ERROR:  view 'information_schema.does_not_exist' does not exist
```

This is a deliberate change from earlier behaviour, which returned an
empty result set (mimicking a view that was defined but happened to be
empty). Returning empty silently let typos and stale ORM
introspection patterns hide for weeks; the loud error caught a
half-dozen real issues in dashboard migrations and CI tooling.

If you have a driver or ORM that *probes* `information_schema` for
optional views (e.g. SQLAlchemy `inspect()`), wrap the probe in a
`try/except` block on your side or use the catalog-aware probes from
[`pg_catalog`](https://www.postgresql.org/docs/current/catalog-pg-class.html)
instead — those return empty for unknown OIDs.

## Pairs with `pg_catalog`

The Postgres-native `pg_catalog` system catalog is supported in
parallel with `information_schema`. `pg_catalog` is the higher-fidelity
introspection surface — it carries OIDs, type modifiers, and the
internal table layout — while `information_schema` is the SQL-standard
portable surface.

See also:

- [Postgres docs · The Information Schema](https://www.postgresql.org/docs/current/information-schema.html)
  for the SQL-standard reference.
- [`docs/guides/database_management.md`](../guides/database_management.md)
  for the `CREATE DATABASE` flow that `catalog_name` reports on.
- [`docs/guides/authentication.md`](../guides/authentication.md) for
  how the `current_tenant()` referenced by the `*_privileges` views is
  derived from the connection.
