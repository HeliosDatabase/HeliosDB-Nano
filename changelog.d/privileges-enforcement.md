**`GRANT` and `REVOKE` are enforced: a statement runs only if the session's role may run it.**

Until now grants were recorded and could be read back, but nothing checked
them: every connection that authenticated could read and change everything.

Now every statement is checked against the role of its session, on every
route: the PostgreSQL simple and extended protocols, the MySQL wire, sessions
of the embedded API, prepared statements each time they run, `COPY`, `EXPLAIN`,
and the primary-key and cached paths that answer without planning. The check
order is superuser, ownership, grants to the role, grants to the roles it is a
member of, grants to `PUBLIC`. Every table a statement reads is checked,
including the ones in subqueries, joins, CTEs and `INSERT … SELECT`; `UPDATE`
and `DELETE` with a `WHERE`, and `RETURNING`, need `SELECT` on what they read.
A view is read with the caller's right on the view and the view owner's rights
on its tables. A refusal is SQLSTATE `42501` with PostgreSQL's wording
(`permission denied for table orders`). A `REVOKE` takes effect on the next
statement of every open session, and an answer cached for one role is never
served to another.

Who is checked: a session whose login is a role created with `CREATE ROLE`. Not
checked: the embedded API with no session (the application owns the process);
`postgres` and `helios`; and a login that names no role in the catalog (the
account given at start, or any other name on a `trust` listener), which is the
server's own account. A network connection with no authenticated role is
refused.

**Upgrading.** Nothing is rewritten when a 4.x database is opened: the roles
and grants it already holds now take effect. A role that could do everything in
4.x can do what it was granted. Read what each role holds before upgrading a
server that roles log in to: `\du`, `SELECT * FROM pg_roles`, and
`SELECT grantee, table_name, privilege_type FROM information_schema.table_privileges`.
Tables that have no recorded owner belong to the bootstrap superuser.

See `docs/compatibility/privileges.md`. Tests: `tests/privilege_enforcement.rs`.
