# Privileges

Since 5.0, `GRANT` and `REVOKE` are enforced: a statement runs only if the
session's role may run it. This page says who is checked, what is checked, and
what is not implemented.

## Who is checked

| Caller | Checked as |
|--------|------------|
| The embedded API with no session (`db.execute`, `db.query`, the REPL) | Nobody. The application owns the process and its data files. |
| A session whose login names a role created with `CREATE ROLE` | That role. |
| A session whose login is `postgres` or `helios` | A superuser. |
| A session whose login names no role in the catalog: the account given at start, any other name on a `trust` listener, the MySQL client's `root` | The server's own account: a superuser. |
| A network connection with no authenticated role | Nobody is asking: refused for everything that needs a privilege. |

A login is matched against the catalog on its first statement. Dropping a role
does not turn its open sessions into the server's account: they keep being
checked as a role, which now holds only what `PUBLIC` holds.

Under `trust` the login is whatever the client sent. Privileges are a boundary
only on a `password`, `md5` or `scram-sha-256` listener.

`SET ROLE r` switches to a role the session is a member of (a superuser may
switch to any role); `RESET ROLE` and `SET ROLE NONE` return. `SET SESSION
AUTHORIZATION r` is for superusers. `current_user` and `current_role` answer the
role statements are checked as; `session_user` answers the login. `SET ROLE`
also works on the embedded API, which is how an application drops to a role for
a block of statements.

## Check order

Superuser, then ownership, then grants to the role, then grants to the roles it
is a member of (through every level; a role declared `NOINHERIT` does not pass
on what was granted to the roles it is in), then grants to `PUBLIC`.

## What is checked

| Object | Privileges | Notes |
|--------|------------|-------|
| Table, view, materialized view | `SELECT` `INSERT` `UPDATE` `DELETE` `TRUNCATE` `REFERENCES` `TRIGGER` | Every relation a statement reads is checked: joins, subqueries, CTEs, set operations, `INSERT … SELECT`. `UPDATE` / `DELETE` with a `WHERE`, and `RETURNING`, also need `SELECT` on the columns they read. |
| Column | `SELECT` `INSERT` `UPDATE` `REFERENCES` | A statement needs the privilege on the table or on every column it touches. `SELECT *` touches every column. `count(*)` needs any one column. |
| View | | The caller needs `SELECT` on the view; the view's owner needs `SELECT` on the tables the view reads. Column grants on a view are not honoured: grant on the view. |
| Sequence | `USAGE` `SELECT` `UPDATE` | `nextval` needs `USAGE` or `UPDATE`, `currval` `USAGE` or `SELECT`, `setval` `UPDATE`. A serial or identity column fills itself without a sequence privilege. |
| Function, procedure | `EXECUTE` | `PUBLIC` holds it by default, as in PostgreSQL; revoke it from `PUBLIC` to restrict a routine. A routine runs with the rights of the role that calls it. |
| Schema | `USAGE` `CREATE` | `USAGE` is needed to reach anything in the schema, `CREATE` to create in it. `PUBLIC` holds `USAGE` on `public` by default and does not hold `CREATE`. |
| Database | `CONNECT` `CREATE` `TEMPORARY` | `PUBLIC` holds `CONNECT` and `TEMPORARY` by default. `CREATE` allows `CREATE SCHEMA`. `CONNECT` is checked when a PostgreSQL-wire connection starts. |

Ownership: the role that creates a table, view, sequence, schema or routine
owns it. `ALTER`, `DROP`, `CREATE INDEX`, `REFRESH MATERIALIZED VIEW` need the
owner (or a member of the owning role). `TRUNCATE` needs the owner or the
`TRUNCATE` privilege; `CREATE TRIGGER` the owner or `TRIGGER`. A foreign key
needs `REFERENCES` on the table it points at. An object nobody is recorded as
owning (everything created before 5.0, and everything created through the
embedded API) belongs to the bootstrap superuser. `ALTER … OWNER TO role`
changes the owner: by a superuser, or by the owner when it is a member of the
new owner.

Grants: the owner grants and revokes freely. Anyone else needs the privilege
`WITH GRANT OPTION`, and may revoke only the grants they issued. `REVOKE`
refuses while the grantee has granted the privilege on (SQLSTATE `2BP01`) unless
`CASCADE` is given. `GRANT … ON ALL TABLES | SEQUENCES | FUNCTIONS IN SCHEMA`
grants on what exists at that moment. Dropping an object forgets its grants; a
new object of the same name starts with none. Renaming a table or a column
keeps its grants.

Roles: `CREATE ROLE`, `ALTER ROLE` and `DROP ROLE` need `CREATEROLE`; only a
superuser gives or takes `SUPERUSER`, `REPLICATION` and `BYPASSRLS`, and only a
superuser alters or drops a superuser. A role may change its own password.
`GRANT role TO member [WITH ADMIN OPTION]` needs `CREATEROLE` or the admin
option on the role. `CREATE DATABASE` needs `CREATEDB`. Branch statements,
`CREATE EXTENSION`, `VACUUM`, `REINDEX`, `CHECKPOINT` and partition attach and
detach need a superuser.

## Errors

SQLSTATE `42501`, with PostgreSQL's wording: `permission denied for table t`,
`must be owner of table t`, `permission denied for schema s`, `permission denied
to create role`. On the MySQL wire: 1142 `SELECT command denied to user
'u'@'%' for table 't'`, 1044 `Access denied for user 'u'@'%' to database 's'`
for a schema, 1370 for a routine or a sequence, 1227 otherwise.

## Asking

`has_table_privilege`, `has_column_privilege`, `has_any_column_privilege`,
`has_schema_privilege`, `has_sequence_privilege`, `has_function_privilege`,
`has_database_privilege` and `pg_has_role` answer from the same check a
statement gets, with or without a role as first argument.
`information_schema.table_privileges` lists table grants with their grantor;
`\du` and `pg_roles` list roles and their attributes.

## Not implemented

- `ALTER DEFAULT PRIVILEGES`: refused with `0A000`.
- `SECURITY DEFINER` routines: refused with `0A000` when the routine is defined.
- `SET LOCAL ROLE`: refused with `0A000`.
- `REASSIGN OWNED`, `DROP OWNED`.
- Privileges on types, domains, languages, tablespaces, foreign servers and
  large objects: `GRANT` on them is refused with `0A000`. `DROP TYPE` needs a
  superuser.
- `CREATE ROLE … IN ROLE / ROLE / ADMIN`: refused; create the role, then
  `GRANT role TO member`.
- `pg_tables.tableowner`, `pg_class.relowner`, `information_schema.column_privileges`
  and the other grant views beside `table_privileges` do not report recorded
  owners, column grants, routine grants or memberships yet.
- The REST API and the MCP endpoint run their statements as the server's own
  account.

## Upgrading from 4.x

Nothing is rewritten when a 4.x database is opened. The roles and grants it
recorded, which 4.x never enforced, now decide. Before upgrading a server that
roles log in to, read what they hold:

```sql
SELECT grantee, table_schema, table_name, privilege_type
FROM information_schema.table_privileges ORDER BY 1, 2, 3;
SELECT rolname, rolsuper, rolcreaterole, rolcanlogin FROM pg_roles;
```

A role that could read and write everything in 4.x can, after the upgrade, do
what it was granted. Tables keep working for the server's own account and for
`postgres`, which own everything that has no recorded owner.
