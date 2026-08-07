# `information_schema` Compatibility

> Nano exposes the long-tail SQL-standard `information_schema` views
> that ORMs, migration tools, and dashboard builders commonly probe,
> including `character_sets` and view-usage metadata. Unknown
> `information_schema` views raise a loud error rather than silently
> returning rows from the wrong view. A few views are present for shape
> only and are **always empty** — `routines` and `parameters` are the
> ones that matter in practice; check the Status column before relying
> on a view to return data.

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
| `information_schema.character_sets` | Complete | Single-row UTF-8 entry |
| `information_schema.collations` | Complete | UTF-8 collation + the `C` POSIX collation |

### Routines and parameters

| View | Status | Notes |
|------|--------|-------|
| `information_schema.routines` | Schema only — always empty | The view resolves and reports the full SQL-standard column list, but returns **zero rows**, including when functions and procedures have been registered via `CREATE FUNCTION` / `CREATE PROCEDURE`. Nano does not expose its runtime routine catalog through this view (`query_information_schema_routines` returns an empty row set by construction). ORM probes see "no user-defined routines" |
| `information_schema.parameters` | Schema only — always empty | Same: correct column list, zero rows, always. No per-routine parameter rows are ever produced |

A registered routine is invisible to every catalog client. `pg_proc` is empty for
the same reason — see [`pg_catalog`](#pairs-with-pg_catalog) below. Note also that
a registered function cannot be *called* by any SQL surface; see the
`heliosdb-nano-schema` skill (Recipe 6) for what does and does not run.

### Views and views-on-views

| View | Status | Notes |
|------|--------|-------|
| `information_schema.views` | Complete | `view_definition` is the raw `CREATE VIEW` body |
| `information_schema.view_table_usage` | Complete | Edges from views to the base tables they reference |
| `information_schema.view_column_usage` | Complete | Edges from views to the base columns they reference |

### Privileges (RLS / multi-tenancy)

| View | Status | Notes |
|------|--------|-------|
| `information_schema.table_privileges` | Complete | Resolved against the active `current_tenant()` |
| `information_schema.column_privileges` | Complete | Same |
| `information_schema.role_table_grants` | Complete | Pre-resolved grants per role |
| `information_schema.role_column_grants` | Complete | Same |

## Strict-unknown-view behaviour

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
