# `information_schema` Compatibility

> **Read the Status column before relying on any view here.** Of the
> views this page documents, six return real rows; the rest resolve,
> report the correct SQL-standard column list, and return **zero rows
> always** — including when the objects they describe exist. An empty
> result from one of those is not "you have no views/constraints/
> routines"; it is "this view was never populated". Unknown
> `information_schema` views raise a loud error rather than returning
> empty, so a typo is still distinguishable from a real lookup.

Nano implements a subset of the PostgreSQL flavour of the SQL-standard
`information_schema`. All views are read-only. The populated ones
reflect catalog state at query time (no caching, no staleness).

Every Status below was measured over the PostgreSQL wire against a
database that had base tables, a view, a view-on-a-view, a `CHECK`
constraint, a foreign key, a registered function and an executed
`GRANT` — that is, against data that *should* have populated each one.

## Covered views

### Tables, columns, and constraints

| View | Status | Notes |
|------|--------|-------|
| `information_schema.tables` | Populated | Filters by `table_schema`, `table_name`, `table_type`. **Base tables only** — views are not listed, so there is no `table_type = 'VIEW'` row (PostgreSQL lists them) |
| `information_schema.columns` | Populated | Includes `data_type`, `is_nullable`, `column_default`, `ordinal_position` |
| `information_schema.key_column_usage` | Populated | PK and FK columns with `constraint_name` |
| `information_schema.table_constraints` | Populated | `PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK` |
| `information_schema.referential_constraints` | Populated | FK `match_option`, `update_rule`, `delete_rule` |
| `information_schema.check_constraints` | **Always empty** | Correct column list, zero rows, even with `CHECK` constraints defined. Use `table_constraints` (which does report a `CHECK` row) to detect their existence; the `check_clause` expression is not exposed anywhere |
| `information_schema.constraint_column_usage` | **Always empty** | Correct column list, zero rows |

### Schemas, catalogs, databases

| View | Status | Notes |
|------|--------|-------|
| `information_schema.schemata` | Populated | One row per registered schema |
| `information_schema.catalog_name` | **Not implemented** | Not a recognised view at all — querying it raises the same unknown-view error as a typo, it does not return empty |
| `information_schema.character_sets` | **Always empty** | Resolves; returns no rows. There is no UTF-8 entry to read |
| `information_schema.collations` | **Always empty** | Resolves; returns no rows |

### Routines and parameters

| View | Status | Notes |
|------|--------|-------|
| `information_schema.routines` | **Always empty** | Resolves and reports the full SQL-standard column list, but returns **zero rows**, including when functions and procedures have been registered via `CREATE FUNCTION` / `CREATE PROCEDURE`. Nano does not expose its runtime routine catalog through this view. ORM probes see "no user-defined routines" |
| `information_schema.parameters` | **Always empty** | Same: correct column list, zero rows, always. No per-routine parameter rows are ever produced |

A registered routine is invisible to every catalog client. `pg_proc` is empty for
the same reason — see [`pg_catalog`](#pairs-with-pg_catalog) below. Note also that
a registered function cannot be *called* by any SQL surface; see the
`heliosdb-nano-schema` skill (Recipe 6) for what does and does not run.

### Views and views-on-views

| View | Status | Notes |
|------|--------|-------|
| `information_schema.views` | **Always empty** | Zero rows even with views defined. There is no `view_definition` to read from this surface — `pg_views` is empty too, so a view's SQL text is not retrievable through any catalog view. Use `\d` in the REPL |
| `information_schema.view_table_usage` | **Always empty** | Zero rows; no view→table edges are produced |
| `information_schema.view_column_usage` | **Always empty** | Zero rows; no view→column edges are produced |

Nano supports `CREATE VIEW` and querying through views — it is only the
*introspection* of them that is missing.

### Privileges (RLS / multi-tenancy)

All four are **always empty**, measured after a successful
`GRANT SELECT ON t TO app_user`. They do not reflect grants, and they are
not wired to `current_tenant()`. Note that `CREATE ROLE` is itself
unsupported (`Statement not yet supported: CreateRole`), so there is no
role to resolve grants against in the first place. Do not use these views
to audit access; RLS policy state is not exposed here.

| View | Status | Notes |
|------|--------|-------|
| `information_schema.table_privileges` | **Always empty** | Zero rows after a `GRANT` |
| `information_schema.column_privileges` | **Always empty** | Same |
| `information_schema.role_table_grants` | **Always empty** | Same |
| `information_schema.role_column_grants` | **Always empty** | Same |

## Strict-unknown-view behaviour

A reference to `information_schema.<unknown>` raises an error at parse
time:

```sql
SELECT * FROM information_schema.does_not_exist;
-- ERROR:  information_schema.does_not_exist is not a recognised view;
--         HeliosDB Nano populates tables (base tables only, not views),
--         columns, schemata, key_column_usage, table_constraints and
--         referential_constraints. These resolve but are ALWAYS EMPTY:
--         views, view_table_usage, view_column_usage, check_constraints,
--         constraint_column_usage, routines, parameters, triggers,
--         sequences, domains, character_sets, collations, *_privileges,
--         role_*. Please file an issue if this view is needed.
```

Note that `catalog_name` produces this same error — it is not implemented
at all, rather than implemented-but-empty.

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

It is **not** a fallback for the always-empty views above: `pg_views`
returns zero rows with a view defined, and `pg_proc` returns zero rows
with a function registered, exactly as their `information_schema`
counterparts do. Where a view is listed as always-empty here, assume the
information is not retrievable from any catalog surface unless this page
says otherwise.

See also:

- [Postgres docs · The Information Schema](https://www.postgresql.org/docs/current/information-schema.html)
  for the SQL-standard reference.
- [`docs/guides/database_management.md`](../guides/database_management.md)
  for the `CREATE DATABASE` flow. (`catalog_name` does not report on it —
  that view is not implemented; the guide is the reference instead.)
- [`docs/guides/authentication.md`](../guides/authentication.md) for how
  `current_tenant()` is derived from the connection. It is not reachable
  through the `*_privileges` views, which are always empty.
