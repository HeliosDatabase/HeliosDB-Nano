**`pg_policies` and `pg_policy` list the policies; `pg_class` reports the row-security switches.**

What was wrong: `pg_policies` and `pg_policy` were always empty and `pg_class.relrowsecurity` was always false. `pg_class` had no `relforcerowsecurity` column outside psql's `\d`.

What happens now: `pg_policies` has one row per policy (schema, table, name, `PERMISSIVE` / `RESTRICTIVE`, roles, command, `USING` text, `WITH CHECK` text); `pg_policy` has the same policies keyed by a stable number and joins to `pg_class` on `polrelid`; `pg_class.relrowsecurity` and `pg_class.relforcerowsecurity` are true when the table is switched on or forced, on every route including psql's `\d`.

Tests: `tests/rls_statements.rs`, `tests/rls_durability.rs`, `tests/rls_wire.rs`.
