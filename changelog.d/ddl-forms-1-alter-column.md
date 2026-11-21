**`ALTER TABLE … ALTER COLUMN` does what it says: `SET DEFAULT`, `DROP DEFAULT`, `SET NOT NULL`, `TYPE … USING`, `ADD GENERATED … AS IDENTITY`, `DROP IDENTITY`, `SET STATISTICS`.**

What was wrong: only `DROP NOT NULL` worked. Every other form answered "Unsupported ALTER TABLE operation: …"
followed by a debug rendering of the syntax tree, reported as an internal error (SQLSTATE XX000), and
`DROP IDENTITY` and `SET STATISTICS` were syntax errors. Migration tools emit these forms all the time.

What happens now:

- `SET DEFAULT expr` / `DROP DEFAULT`: the default is stored as `CREATE TABLE` stores one and used by every
  later INSERT (also through the PostgreSQL wire and by a statement prepared before the change). Rows that
  exist keep their values. A default the column cannot hold is refused when it is set.
- `SET NOT NULL` reads the rows first and refuses with 23502, naming the column, when one holds NULL.
- `TYPE newtype [USING expr]` (`SET DATA TYPE`): every row gets its new value (the cast, or the `USING`
  expression) in the statement's one write, with the schema; the column's indexes are built again over the new
  values and the table's CHECK constraints are validated against the new rows first. A value that cannot be
  cast refuses the statement and names its row and the value; two rows the new type makes equal under a
  PRIMARY KEY or UNIQUE key are 23505; a default that has no value of the new type is refused. Nothing is
  written when the statement is refused. An identity column keeps generating keys (its new type must be an
  integer type).
- `ADD GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` on an integer primary key, `DROP IDENTITY [IF EXISTS]`.
- `SET STATISTICS n` is stored and shown as `pg_attribute.attstattarget` (new column; -1 is the default). It
  follows a column rename and goes with the column.
- An `ALTER TABLE` form that is still not supported answers SQLSTATE 0A000 and names the form in SQL words
  (for example `ALTER TABLE … DROP PRIMARY KEY is not supported by HeliosDB Nano`).

Limits, each an explicit error (0A000) and not a wrong answer:

- `TYPE` is refused for a column a view uses (drop the view, change the type, create the view again, as in
  PostgreSQL), for a column a generated column reads, for a column in a foreign key unless both types are
  integer types, for a column under a vector or expression index, for a column kept in dictionary,
  content-addressed or columnar storage, in a transaction that has written the table, and while a branch reads
  the table.
- A read `AS OF` a time before a type change is refused, as it is across a rename: an older version of a row
  holds a value of the old type.
- `ADD GENERATED … AS IDENTITY` is refused on a column that is not the table's integer primary key: generated
  keys are assigned to that column only.
- `RENAME COLUMN` of a column an expression index reads is still refused (0A000); drop the index first.

Tests: `tests/alter_column_forms.rs`.
