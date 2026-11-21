**Policies are applied wherever a table is read.**

What changed: the paths that answer a statement without the general planner (primary-key reads and writes, `count(*)`, prepared statements, cached plans and cached results) used to ask only whether a tenant was selected. They now ask whether the session's role is subject to row-level security, so a role with policies never takes a path that skips them, and no cache hands one role's rows to another.

Covered by tests, each through the simple and the parameter statement families: joins (including a table joined to itself), subqueries in `WHERE`, the select list, `FROM`, `HAVING` and `CASE`, common table expressions including recursive ones, `UNION` / `INTERSECT` / `EXCEPT`, `INSERT … SELECT`, `CREATE TABLE AS`, subqueries in `UPDATE` and `DELETE`, `RETURNING`, `ON CONFLICT DO UPDATE`, `PREPARE` / `EXECUTE`, reads inside a transaction, time travel (`AS OF`), branches, `EXPLAIN ANALYZE`, `COPY table TO` and `COPY (query) TO`, and the PostgreSQL and MySQL wires. Foreign-key checks are not subject to policies, as in PostgreSQL.

Tests: `tests/rls_coverage.rs`, `tests/rls_caches.rs`, `tests/rls_wire.rs`.
