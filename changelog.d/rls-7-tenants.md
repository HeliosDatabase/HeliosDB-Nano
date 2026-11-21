**A tenant policy is a stored policy, and `\tenant rls delete` deletes.**

What was wrong: policies made with `\tenant rls create` or `TenantManager::create_rls_policy` lived in a separate in-memory list, and `\tenant rls delete` printed "not yet implemented".

What happens now: the tenant commands and the library API read and write the same stored policies as `CREATE POLICY`. A policy made either way appears in `pg_policies`, survives a restart and can be changed or dropped with SQL. `\tenant rls delete <table> <policy>` removes it, and `TenantManager::delete_rls_policy` does the same from the library. A session bound to a tenant with row security on is subject to the policies of a table as before, whether or not the table was switched on with `ENABLE ROW LEVEL SECURITY`; switching the table on makes them apply to every role that is not exempt. Creating a policy of an existing name through the library replaces it.

Tests: `tests/rls_semantics.rs`, `tests/rls_durability.rs`, `tests/repl_tenant_commands.rs`.
