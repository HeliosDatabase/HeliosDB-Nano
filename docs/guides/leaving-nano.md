# Leaving Nano: a SQL dump PostgreSQL loads

`heliosdb-nano dump --format sql --dialect postgres` writes a plain SQL script that
PostgreSQL loads with `psql`. Use it to move a database to PostgreSQL, or to keep an
exit you have tested.

```bash
heliosdb-nano dump --data-dir ./mydata --format sql --dialect postgres --output app.sql
createdb app
psql -v ON_ERROR_STOP=1 -d app -f app.sql
```

## Flags

| Flag | Meaning |
|---|---|
| `--format binary\|sql` | `binary` (default) is the `restore` format; `sql` is a script |
| `--dialect nano\|postgres` | `nano` (default) is restored with `execute_sql_script`; `postgres` is for `psql` |
| `--tables a,b` | only these tables; views, sequences and foreign keys to other tables are not written (the keys appear as comments) |
| `--schema-only` | DDL only |
| `--data-only` | rows and sequence positions only |

The file is written to a temporary file next to the target and renamed into place when
complete. A failed dump leaves no file and does not touch an existing one.

## What the PostgreSQL dialect changes

| Nano | In the postgres dialect |
|---|---|
| a `VECTOR(n)` column | `CREATE EXTENSION IF NOT EXISTS vector;` first (pgvector must be installed) |
| an HNSW index | `USING hnsw ("col" vector_<metric>_ops)` with the index's own metric |
| a SERIAL / IDENTITY column | `SERIAL` / `BIGSERIAL`, then `setval(pg_get_serial_sequence(…), max(key))` so new keys continue after the loaded ones |
| a `gin` / `gist` index on a plain column | written as a `-- NOT PORTABLE:` comment and listed in the header (PostgreSQL needs an operator class it cannot infer) |

Everything the dump does not carry is listed at the top of the file as `-- NOT CARRIED:`
lines: materialized views, triggers, product quantization of a vector index, NUMERIC
precision and scale (Nano keeps none), and views or sequences when `--tables` is used.

## Not covered

- A dump over `--connection` (server mode) is not implemented; dump the data directory.
- Loading `pg_dump` output back into Nano is a different path (COPY blocks and SET lines).
- Rows are read one table at a time; a very large table is held in memory while it is
  written.
