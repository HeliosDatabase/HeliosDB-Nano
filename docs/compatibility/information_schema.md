# `information_schema` Compatibility

> **Read the Status column before relying on any view here.** Of the
> views this page documents, ten return real rows; the rest resolve,
> report the correct SQL-standard column list, and return **zero rows
> always** — including when the objects they describe exist. An empty
> result from one of those is not "you have no routines"; it is "this
> view was never populated". Unknown `information_schema` views raise a
> loud error rather than returning empty, so a typo is still
> distinguishable from a real lookup.
>
> **Changed by HC3 (catalog unification).** `views`, `check_constraints`,
> `constraint_column_usage` and `catalog_name` now return real rows;
> `tables` now lists views with `table_type = 'VIEW'`; `schemata`
> enumerates schemas created with `CREATE SCHEMA` instead of three
> hardcoded rows; and `pg_views` / `pg_indexes` / `pg_matviews` are
> populated on **every** route. Before that release the PostgreSQL wire
> and the embedded/REPL/Python routes were served by two different
> implementations and could disagree — most visibly, the wire's
> `information_schema.columns` had no `table_schema` column, so
> `WHERE table_schema = 'public'` returned zero rows there while working
> embedded. There is now ONE implementation
> (`src/sql/phase3/system_views.rs`) behind all five interfaces.

Nano implements a subset of the PostgreSQL flavour of the SQL-standard
`information_schema`. All views are read-only. The populated ones
reflect catalog state at query time (no caching, no staleness).

Every Status below was measured over the PostgreSQL wire against a
database that had base tables, a view, a view-on-a-view, a `CHECK`
constraint, a foreign key, a registered function and an executed
`GRANT` — that is, against data that *should* have populated each one.
Every view marked **Populated** below, plus the ten privilege/role views,
resolves identically on the embedded API, the REPL, the Python binding
and both wires — they all reach the same registry.

**Eight views do NOT.** `routines`, `parameters`, `triggers`, `domains`,
`character_sets`, `collations`, `view_table_usage` and `view_column_usage`
are not registered in that registry: they are answered only by the
PostgreSQL-wire interceptor (`src/protocol/postgres/catalog.rs`), which
returns their correct column list and zero rows. The MySQL wire routes
`information_schema` queries through that same interceptor
(`src/protocol/mysql/handler.rs:1278` → `handle_information_schema`), so it
returns the same zero-row shape. On the embedded API, the REPL and the Python
binding they raise an unknown-relation error instead of returning the
documented zero rows. Their **Always empty** status below is therefore a
statement about the two wire protocols only.

## Covered views

### Tables, columns, and constraints

| View | Status | Notes |
|------|--------|-------|
| `information_schema.tables` | Populated | Filters by `table_schema`, `table_name`, `table_type`. Lists base tables (`BASE TABLE`) **and views** (`VIEW`). `table_schema` is the real schema and `table_name` the bare table, so a table in schema `app` reports `('app','t')` |
| `information_schema.columns` | Populated | 17 columns including `table_schema`, `data_type`, `udt_name`, `is_nullable`, `column_default`, `ordinal_position`, `is_identity`. `WHERE table_schema = 'public'` filters correctly on every route |
| `information_schema.key_column_usage` | Populated | PK and FK columns with `constraint_name` |
| `information_schema.table_constraints` | Populated | `PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK` |
| `information_schema.referential_constraints` | Populated | FK `match_option`, `update_rule`, `delete_rule` |
| `information_schema.check_constraints` | Populated | One row per `CHECK` constraint (named or anonymous), with `check_clause` rendered back to SQL from the stored expression |
| `information_schema.constraint_column_usage` | Populated | PK/UNIQUE columns, and the *referenced* columns for a foreign key |

### Schemas, catalogs, databases

| View | Status | Notes |
|------|--------|-------|
| `information_schema.schemata` | Populated | One row per schema, from the same enumeration `pg_namespace` uses — including a schema declared by `CREATE SCHEMA` that holds no tables |
| `information_schema.catalog_name` | Populated | One row: `catalog_name = 'heliosdb'` |
| `information_schema.character_sets` | **Always empty** (PG + MySQL wire) | Resolves; returns no rows. There is no UTF-8 entry to read |
| `information_schema.collations` | **Always empty** (PG + MySQL wire) | Resolves; returns no rows |

### Routines and parameters

| View | Status | Notes |
|------|--------|-------|
| `information_schema.routines` | **Always empty** (PG + MySQL wire) | Resolves and reports the full SQL-standard column list, but returns **zero rows**, including when functions and procedures have been registered via `CREATE FUNCTION` / `CREATE PROCEDURE`. Nano does not expose its runtime routine catalog through this view. ORM probes see "no user-defined routines" |
| `information_schema.parameters` | **Always empty** (PG + MySQL wire) | Same: correct column list, zero rows, always. No per-routine parameter rows are ever produced |

A registered routine is invisible to every catalog client. `pg_proc` is empty for
the same reason — see [`pg_catalog`](#pairs-with-pg_catalog) below. The function
itself *is* callable (`SELECT f(x)` in scalar position, with `$`-sigil
parameters); only its introspection is missing. See the `heliosdb-nano-schema`
skill (Recipe 6) for the exact boundaries.

### Views and views-on-views

| View | Status | Notes |
|------|--------|-------|
| `information_schema.views` | Populated | One row per view, `view_definition` = the stored `CREATE VIEW` body. `pg_views.definition` carries the same text. `is_updatable` / `is_insertable_into` are `NO`: Nano's views are read-only |
| `information_schema.view_table_usage` | **Always empty** (PG + MySQL wire) | Zero rows; no view→table edges are produced |
| `information_schema.view_column_usage` | **Always empty** (PG + MySQL wire) | Zero rows; no view→column edges are produced |

Views are also visible as `pg_class` rows with `relkind = 'v'` (their
columns in `pg_attribute`), and as `information_schema.tables` rows with
`table_type = 'VIEW'`. Only the view→table / view→column *edge* views
(`view_table_usage`, `view_column_usage`) are still unpopulated.

### Privileges

> **Roles and grants are stored and introspectable. Privileges are NOT
> enforced.** HeliosDB Nano performs no permission check on any read or write
> path. A row in `table_privileges` means "somebody ran `GRANT`" — it does not
> mean access is restricted. Do not use these views to audit access, and do not
> treat a successful `GRANT` as a security control. Enforcement is a tracked
> follow-up.

`CREATE ROLE` / `ALTER ROLE` / `DROP ROLE` (and the `CREATE USER` /
`ALTER USER` / `DROP USER` spellings) are real DDL and persist to the catalog,
and `GRANT` / `REVOKE` persist an ACL record instead of silently discarding it.
`table_privileges` and `role_table_grants` report those records. `role_table_grants`
mirrors `table_privileges` exactly, because there is no session identity to
filter by yet (`current_user` is still a hardcoded literal).

Under the default configuration a `GRANT`/`REVOKE` that names a role or table
which does not exist is an ERROR. Set `[authentication] legacy_acl_noop = true`
to restore the pre-4.20 leniency (unknown names skipped, statement succeeds).

| View | Status | Notes |
|------|--------|-------|
| `information_schema.table_privileges` | **Populated** | One row per stored (grantee, table, privilege) |
| `information_schema.role_table_grants` | **Populated** | Mirrors `table_privileges` (no session identity) |
| `information_schema.column_privileges` | **Always empty** | Column-level grants are rejected at plan time |
| `information_schema.role_column_grants` | **Always empty** | Same |
| `information_schema.usage_privileges` | **Always empty** | Sequence grants are stored but not surfaced here yet |
| `information_schema.role_usage_grants` | **Always empty** | Same |
| `information_schema.role_routine_grants` | **Always empty** | No routine grants |
| `information_schema.applicable_roles` | **Always empty** | Role membership is rejected at plan time |
| `information_schema.enabled_roles` | **Always empty** | Same |
| `information_schema.administrable_role_authorizations` | **Always empty** | Same |

All ten resolve on **every** route — embedded, REPL, Python binding, PostgreSQL
wire and MySQL wire. They previously resolved (empty) only on the PG wire and
failed as unknown relations everywhere else.

### `pg_roles` / `pg_user` / `pg_authid`

These used to return two FABRICATED superusers (`postgres` oid 10, `helios` oid
11) with every attribute bit `true`, regardless of configuration. They now
report the persisted role catalog: the two virtual built-ins are still listed
for backward compatibility (and their bits remain as meaningless as before,
since nothing enforces them), followed by every created role with its REAL
bits. `pg_authid` is now registered on the live route — it previously existed
only in a dead registry and failed as an unknown relation. Neither view ever
emits a stored password: `rolpassword` renders `********` or NULL.

`CREATE ROLE … PASSWORD` is **not** wired to wire authentication. It records a
password in the catalog; it does not create a connectable account.

## Strict-unknown-view behaviour

A reference to `information_schema.<unknown>` raises an error at parse
time:

```sql
SELECT * FROM information_schema.does_not_exist;
-- ERROR:  information_schema.does_not_exist is not a recognised view;
--         HeliosDB Nano populates catalog_name, tables (base tables AND
--         views), columns, schemata, views, key_column_usage,
--         table_constraints, constraint_column_usage,
--         referential_constraints, check_constraints, sequences,
--         table_privileges and role_table_grants — the last two report
--         STORED grants; HeliosDB does NOT enforce SQL privileges.
--         These resolve but are ALWAYS EMPTY: view_table_usage,
--         view_column_usage, routines, parameters, triggers, domains,
--         character_sets, collations, column_privileges,
--         usage_privileges, role_column_grants, role_usage_grants,
--         role_routine_grants, applicable_roles, enabled_roles,
--         administrable_role_authorizations.
--         Please file an issue if this view is needed.
```

`catalog_name` no longer produces this error — it is a real one-row view.

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

These `pg_catalog` views are populated on every route:

| View | Status | Notes |
|------|--------|-------|
| `pg_views` | Populated | `schemaname`, `viewname`, `viewowner`, `definition` (the stored `CREATE VIEW` body) |
| `pg_indexes` | Populated | Primary-key, `UNIQUE`, manual B-tree and HNSW vector indexes with a rendered `indexdef`. Previously reachable only over the PostgreSQL wire — the embedded/REPL/Python routes errored |
| `pg_matviews` | Populated | One row per materialised view with its `definition`. Previously empty on every route while a working implementation sat unreachable in a dead registry |
| `pg_class` | Populated | Tables (`r`), indexes (`i`), sequences (`S`) and views (`v`) |

`pg_proc` is still **empty** with a function registered, for the same
reason `information_schema.routines` is: the function registry hangs off
`EmbeddedDatabase`, which the system-view executor cannot reach. Same for
`pg_policies` / `pg_policy` (RLS policies live in the tenant manager).
Widening that context is a named follow-up.

See also:

- [Postgres docs · The Information Schema](https://www.postgresql.org/docs/current/information-schema.html)
  for the SQL-standard reference.
- [`docs/guides/database_management.md`](../guides/database_management.md)
  for the `CREATE DATABASE` flow. (`catalog_name` reports the single
  implicit catalogue name only; the guide is the reference for the rest.)
- [`docs/guides/authentication.md`](../guides/authentication.md) for how
  `current_tenant()` is derived from the connection. It is not reachable
  through the `*_privileges` views, which are always empty.
