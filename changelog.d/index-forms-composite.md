**Composite indexes: an ordinary multi-column index holds every column.**

What was wrong: `CREATE INDEX ix ON t (a, b)` indexed `a` alone. `b` was dropped without a word,
`pg_indexes` showed `(a)`, and a query on both columns read every row with that `a`.

What happens now:

- The index key is all the columns, in order. `pg_indexes.indexdef` and both dump formats list
  them all.
- A query reads through the index for: equality on all the columns (one probe); equality on a
  leading run of them; and either of these plus a range (`<`, `<=`, `>`, `>=`, `BETWEEN`) on the
  next column. `EXPLAIN` names the index and the condition it serves
  (`Index Scan using ix on t (a = 3 AND b >= 5)`). A condition that skips the first column does
  not use it.
- `ORDER BY` on the index columns with a `LIMIT` walks the index in order instead of sorting
  (`Index Ordered Scan using ix …`), also behind equality on the leading columns
  (`WHERE a = 3 ORDER BY b LIMIT 10`). This applies to ascending order over `NOT NULL` whole-number
  and text columns; other shapes are sorted as before.
- Every row an index hands back is checked against the whole `WHERE` clause again.

Existing databases: an earlier release recorded only the leading column of such an index, so the
other columns are not known. That index comes back as an index on its leading column, which is
still correct and still used. Drop it and create it again to get the multi-column index. Indexes
created from now on are rebuilt whole at every open.

Tests: `tests/index_composite.rs`.
