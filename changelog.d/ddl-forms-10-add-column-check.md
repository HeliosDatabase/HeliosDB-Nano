**A `CHECK` given with `ALTER TABLE … ADD COLUMN` is kept and enforced.**

Two source comments disagreed on whether it was. It is: the clause is added as a CHECK constraint in the same
statement, existing rows are validated against it (a default that breaks it refuses the statement and adds no
column), and later INSERTs and UPDATEs are checked. No behaviour changed; the stale comment was corrected and
the behaviour is now pinned on both statement families, over the PostgreSQL wire and across a restart.

Tests: `tests/add_column_check_kept.rs`.
