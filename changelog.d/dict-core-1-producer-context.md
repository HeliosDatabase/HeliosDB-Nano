**System views can read more than storage: `pg_proc` lists user functions and procedures.**

A system view was produced from the storage engine alone. Anything kept outside it (functions
and procedures, trigger definitions, tenants and their policies, the session asking) was out of
reach, so `pg_proc` was always empty, also with routines defined.

A producer now receives a context: the storage engine plus, when the database is open, its
routine registry, its trigger definitions, its tenant manager, the live sessions and the current
session's user and branch. `pg_proc` is the first view to use it: one row per function
(`prokind` `f`) and procedure (`p`) with `proname`, `pronamespace`, `prolang`, `prorettype`,
`proargtypes`, `pronargs` and `prosrc`, on the embedded API and the PostgreSQL wire. Two columns
were added to the view (`prokind`, `pronargs`). A routine's `oid` is derived from its name and
does not change when another routine is created or dropped. Two databases open in one process
never see each other's routines.

`information_schema.routines` and `information_schema.parameters` are unchanged (still empty).

Tests: `tests/dict_core.rs`, `src/protocol/postgres/dict_core_wire_tests.rs`,
`src/sql/dict/context.rs`, `src/sql/dict/routines.rs`.
