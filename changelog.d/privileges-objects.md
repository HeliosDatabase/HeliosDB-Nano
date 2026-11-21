**Privileges on sequences, functions and procedures, schemas and databases; `has_table_privilege` and its siblings.**

Grants were accepted on tables and sequences only, and `GRANT … ON ALL TABLES
IN SCHEMA` was refused. The `has_*_privilege` functions did not exist.

Now `GRANT` and `REVOKE` accept `ON SEQUENCE`, `ON FUNCTION | PROCEDURE |
ROUTINE`, `ON SCHEMA`, `ON DATABASE`, and `ON ALL TABLES | SEQUENCES |
FUNCTIONS IN SCHEMA`, and the privileges are enforced: `nextval` needs `USAGE`
or `UPDATE` on the sequence, `currval` `USAGE` or `SELECT`, `setval` `UPDATE`;
calling a routine needs `EXECUTE`, which `PUBLIC` holds until it is revoked; a
routine's statements are checked against the role that calls it; reaching
anything in a schema needs `USAGE` on it and creating in it needs `CREATE`
(`PUBLIC` holds `USAGE` on `public`, and not `CREATE`); `CREATE SCHEMA` needs
`CREATE` on the database; a PostgreSQL-wire connection needs `CONNECT` on the
database it names, which `PUBLIC` holds until it is revoked.

`has_table_privilege`, `has_column_privilege`, `has_any_column_privilege`,
`has_schema_privilege`, `has_sequence_privilege`, `has_function_privilege`,
`has_database_privilege` and `pg_has_role` answer what a statement would be
told. `VACUUM`, `REINDEX`, `CHECKPOINT`, branch statements, `CREATE EXTENSION`
and partition attach and detach need a superuser. A grant on a type, domain,
language or tablespace is refused with SQLSTATE `0A000`.

Tests: `tests/privilege_objects.rs`.
