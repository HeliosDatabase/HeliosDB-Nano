**A system view without a producer is an error, not an empty answer.**

The registry of system views answered any registered view it had no producer for with zero
rows. A view registered by mistake, or one whose producer was lost in a refactoring, looked
exactly like a view that is legitimately empty.

A registered view with no producer now fails with an internal error that names the view. Views
that are empty on purpose (for example `pg_inherits`, `pg_publication`, `pg_range`, `pg_enum`,
the column-privilege and role-membership views of `information_schema`) each have their own
explicit producer. A test enumerates every registered view, requires a producer for it, and
checks that every row has as many values as the view declares columns. All 77 registered views
have a producer today, so no statement changes its answer.

Tests: `src/sql/dict/registry_tests.rs`, `tests/dict_core.rs`.
