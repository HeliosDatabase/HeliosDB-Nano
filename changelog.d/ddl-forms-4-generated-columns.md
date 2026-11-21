**`GENERATED ALWAYS AS (expr) STORED` columns are computed.**

What was wrong: the clause was accepted and the column stored as a plain column. An INSERT that left it out
stored NULL, an INSERT or UPDATE could write any value into it, and an UPDATE of a column it reads left it
stale. `GENERATED ALWAYS AS (expr) VIRTUAL` was a syntax error.

What happens now:

- The value is computed on INSERT (`VALUES`, `DEFAULT VALUES`, `INSERT … SELECT`) and again whenever an UPDATE
  or an `ON CONFLICT DO UPDATE` assigns a column the expression reads.
- A direct write is refused with 428C9, except `DEFAULT`.
- The expression may not hold a subquery, an aggregate or window function, a parameter, a function that is not
  immutable (`now()`, `random()`, …), or a reference to another generated column or to a column the table does
  not have: 42P17.
- `ALTER TABLE … ADD COLUMN … GENERATED ALWAYS AS (expr) STORED` computes the value of every existing row in
  the statement's one write.
- The column is indexed like any column (also UNIQUE).
- `pg_attribute.attgenerated` is `s`, `information_schema.columns.is_generated` is `ALWAYS` with the
  `generation_expression`; SQL and binary dumps carry the column as a generated column.
- Every write path that validates CHECK constraints verifies the stored value against the expression, so a
  path that does not compute the column refuses the row rather than storing a stale value.

Limits, each an explicit error:

- `VIRTUAL` (and the form with neither `STORED` nor `VIRTUAL`, which PostgreSQL 18 reads as `VIRTUAL`) is
  refused with 0A000.
- `COPY` and `MERGE` do not compute the column: a row whose generated value is absent or wrong is refused.
- An INSERT that leaves out a serial or identity key which a generated column reads is refused (0A000): give
  the key a value.
- A column a generated column reads cannot be dropped or renamed until the generated column is dropped;
  `ALTER COLUMN … TYPE` of a generated column, or of a column one reads, is refused (0A000).
- `INSERT … SELECT` into a table with a generated column needs the query's output columns to have distinct
  names (0A000 otherwise).

Tests: `tests/generated_columns_stored.rs`.
