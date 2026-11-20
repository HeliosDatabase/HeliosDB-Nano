**Names of the system dictionary are reserved; engine-owned tables refuse user DDL.**

A table could be created with the name of a system view (`CREATE TABLE pg_class (...)`
succeeded). The planner resolves a system view before a table, so the new table could be
written but every read of its name returned the system view instead.

- `CREATE TABLE` (also `CREATE TABLE ... AS`, `SELECT ... INTO` and temporary tables),
  `CREATE VIEW`, `CREATE MATERIALIZED VIEW` and `ALTER TABLE ... RENAME TO` now refuse a name
  that starts with `hdb_` or that is the name of a system view, in any schema, with SQLSTATE
  42939 (reserved name). Names that only resemble one (`hdbx`, `pg_class_backup`) are free.
- A database that already holds a user table with the name of a system view keeps reading
  that table: the table wins over the view, on every route, and a WARNING is logged when the
  database is opened. Once the table is renamed or dropped, the name is the system view again.
- `DROP TABLE`, `ALTER TABLE` and `ALTER TABLE ... RENAME TO` on a code-graph table
  (`_hdb_code_*`) or a knowledge-graph table (`_hdb_graph_*`) are refused with SQLSTATE 42501
  in a binary compiled with the feature that owns the table. A binary without that feature
  never touches these tables, so there they stay ordinary tables that can be removed.

A SQL script that creates a table with a reserved name is refused at that statement. A table
that reaches the catalog another way (a restored backup of an older database) is kept and
wins as described above.

Tests: `tests/dict_core.rs`, `src/protocol/postgres/dict_core_wire_tests.rs`,
`src/sql/dict/reserved.rs`.
