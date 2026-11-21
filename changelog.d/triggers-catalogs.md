**`information_schema.triggers` and `pg_trigger` list the triggers that exist.**

What was missing: `information_schema.triggers` was an always-empty answer on the PostgreSQL wire and an
unknown table everywhere else; `pg_trigger` did not exist.

What happens now: both are views on every route. `information_schema.triggers` has one row per trigger
and event with its table, timing, level, `WHEN` condition and action (`EXECUTE FUNCTION f('a')`, or the
inline body). `pg_trigger` has one row per trigger with PostgreSQL's `tgtype` bits, `tgenabled` (`O` or
`D`), the argument count, and the table and function names in `tgrelname` and `tgfname`.

Tests: `tests/trigger_catalog_views.rs`.
