# BUG — `CREATE BRANCH IF NOT EXISTS` misparses the branch name

**Severity:** MEDIUM | **Status:** OPEN (follow-up) | **Found:** v3.34 cross-agent
fix loop, during T4 (Codex surfaced; Claude confirmed/characterized)
**Not part of the 13-item v3.34 checklist** — filed as a separate follow-up.

## Summary

`CREATE BRANCH IF NOT EXISTS <name> AS OF NOW` does not model the `IF NOT EXISTS`
clause. The parser (`parse_create_branch_sql` → `LogicalPlan::CreateBranch`)
treats the literal token **`IF`** as the branch name and silently drops
`NOT EXISTS <name>`. Consequences:

1. A branch named **`IF`** is created instead of the intended `<name>`.
2. The intended branch is **never created**.
3. `IF NOT EXISTS` provides **no idempotency** — a second identical statement
   errors with `Branch 'IF' already exists`.

## Reproduction (embedded API; same on the wire)

```rust
let db = EmbeddedDatabase::new_in_memory()?;
db.execute("CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW")?;        // Ok(0)
db.query("SELECT branch_name FROM pg_database_branches()", &[])?;       // => ["IF", "main"]
db.execute("CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW");          // Err: Branch 'IF' already exists
```

Observed: branch list is `["IF", "main"]`; second call errors.
Expected: branch `feature_x` is created; the second call is a no-op (returns
success without error) because the branch already exists.

## Root cause

`IF NOT EXISTS` is not represented in `LogicalPlan::CreateBranch` and is not
stripped/handled in `parse_create_branch_sql`, so the branch-name extraction
consumes `IF` as the name. (The plain `CREATE [DATABASE] BRANCH <name> ... AS OF
...` forms are correct — see the v3.34 T4 regression `tests/v334_t4_branch_as_of_now_names.rs`,
which is why T4 itself is "already-fixed": the documented T4 repro used a real
name, not `IF NOT EXISTS`.)

## Suggested fix

- Model `if_not_exists: bool` on `LogicalPlan::CreateBranch` (mirroring
  `CREATE TABLE IF NOT EXISTS`).
- Strip/parse the `IF NOT EXISTS` tokens in `parse_create_branch_sql` before
  reading the branch name.
- In the executor, when `if_not_exists` is set and the branch exists, return
  success (no-op) instead of the "already exists" error.

## Regression to add with the fix

```sql
CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW;   -- creates feature_x
-- pg_database_branches() contains 'feature_x', not 'IF'
CREATE BRANCH IF NOT EXISTS feature_x AS OF NOW;   -- no-op, no error
```
Also cover `CREATE DATABASE BRANCH IF NOT EXISTS <name> FROM <parent> AS OF NOW`.
