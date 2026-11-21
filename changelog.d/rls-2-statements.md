**`CREATE POLICY`, `ALTER POLICY`, `DROP POLICY` and `ALTER TABLE … ENABLE | DISABLE | FORCE | NO FORCE ROW LEVEL SECURITY`.**

What was missing: the statements were refused with "not supported" (0A000). A policy could be made only from the shell (`\tenant rls create`) or the library.

What happens now: `CREATE POLICY name ON table [AS PERMISSIVE | RESTRICTIVE] [FOR ALL | SELECT | INSERT | UPDATE | DELETE] [TO role, … | PUBLIC | CURRENT_USER] [USING (expr)] [WITH CHECK (expr)]`, `ALTER POLICY … RENAME TO`, `ALTER POLICY … [TO …] [USING (…)] [WITH CHECK (…)]`, `DROP POLICY [IF EXISTS]`, and the four table switches, on every route. Only the table's owner or a superuser may run them (42501 otherwise); the owner of a table is the role that created it, and a table created without a login role belongs to the bootstrap superuser. Errors carry PostgreSQL's codes: a policy that already exists 42710, one that does not exist 42704, a role that does not exist 42704, a table that does not exist 42P01, `WITH CHECK` on a `SELECT` or `DELETE` policy and `USING` on an `INSERT` policy 42601. An expression that names a column the table does not have is refused when the policy is written. A switch combined with another action in one `ALTER TABLE`, a policy on a materialized view and a policy statement on a branch are refused with 0A000.

Tests: `tests/rls_statements.rs`, `tests/rls_wire.rs`.
