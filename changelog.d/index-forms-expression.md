**Expression indexes are kept, used and may be UNIQUE: `CREATE [UNIQUE] INDEX … (lower(email))`.**

What was wrong: an index on an expression was recorded and nothing more. No write kept it, no
query read through it (such a query scanned), and `CREATE UNIQUE INDEX` on an expression was
refused.

What happens now, for an index over a btree (the default):

- The expression is evaluated on every write and the index is kept, inside transactions too.
- A query whose `WHERE` clause compares the same expression with a constant (`=`, or a range)
  reads through the index, and `EXPLAIN` names it. "The same expression" ignores letter case of
  function names, spaces, redundant parentheses and a table qualifier, and treats a bound
  parameter as the value bound to it. `ORDER BY expression LIMIT n` (ascending) walks the index.
- `CREATE UNIQUE INDEX … (lower(email))` is enforced on `INSERT`, `UPDATE` and
  `INSERT … ON CONFLICT DO NOTHING`; a NULL result is never a duplicate; rows that already repeat a
  key refuse the index with 23505. Columns and expressions can be mixed in one key
  (`(tenant, lower(email))`), and the index can be partial.
- Only immutable expressions: one that uses `now()`, `random()`, a sequence, a session value or a
  subquery is refused with 42P17. An expression that fails for a row already in the table refuses
  the index.
- `pg_indexes.indexdef` and both dump formats carry `UNIQUE` and the expression.
- A database that recorded such an index under the earlier release builds it when it is next
  opened; nothing has to be re-created. Tables with an expression index rebuild their indexes
  from the rows at open rather than from the index checkpoint.

Known limits: the index is probed for whole-number, text, boolean and UUID results; an expression
with another result type (a float, a timestamp, JSON) is kept and enforced but queries on it scan.
`ON CONFLICT (expression)` does not take the index as its arbiter. On a branch a UNIQUE expression
index is checked when the branch is merged. `RENAME COLUMN` of a column the expression reads is
still refused. An index `USING gin` or `gist` over an expression is still only recorded.

Tests: `tests/index_expression.rs` (and `tests/expression_index_tests.rs`, where the case that
pinned the refusal of a UNIQUE expression index now pins the refusal for `USING gin` only).
