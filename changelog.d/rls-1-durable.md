**Row-level security policies, the table switches and the tenant registry are stored with the database.**

What was wrong: a policy and a tenant existed only in memory. After a restart the tenant registry was empty and every policy was gone, so a table that had been protected was readable by everyone. Dumps did not carry policies, and a standby had none.

What happens now: every policy, every table's `ENABLE` / `FORCE` switch and every tenant (with its plan and limits) is a record in the database. They are there after a restart, they are written to SQL dumps and binary dumps and recreated by a restore (after the rows, so the restore itself is not filtered), and a physical standby receives them like any other catalog record. A policy follows its table through `ALTER TABLE … RENAME`, and dropping a table drops its policies: a new table of the same name starts without them. Restoring a dump whose policy names a role that does not exist fails with an error naming the role, instead of restoring the table without the policy. Custom tenant plans (as opposed to tenants) are still kept in memory only.

Tests: `tests/rls_durability.rs`, `tests/rls_dump_restore.rs`.
