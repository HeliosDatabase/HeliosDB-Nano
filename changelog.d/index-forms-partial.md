**Partial indexes: `CREATE [UNIQUE] INDEX … WHERE predicate` keeps and uses its predicate.**

What was wrong: the `WHERE` clause of `CREATE INDEX` was read and then dropped. The index covered
every row of the table. A partial UNIQUE index, the usual soft-delete rule
(`CREATE UNIQUE INDEX … ON users (email) WHERE deleted_at IS NULL`), was enforced on every row, so
the e-mail of a deleted user could never be used again; `pg_indexes` and dumps showed an index
without a predicate.

What happens now:

- The predicate is stored with the index. The index holds only the rows for which it is true, and
  a row enters or leaves the index when an `UPDATE` changes the answer.
- A UNIQUE partial index is enforced among those rows only, on `INSERT`, `UPDATE`,
  `INSERT … ON CONFLICT DO NOTHING`, inside transactions (a rollback gives the key back) and when
  the index is created (rows that already repeat a key refuse it with 23505).
- A query reads through the index only when its `WHERE` clause implies the index predicate, and
  `EXPLAIN` then names the index (`Index Scan using … (… AND index predicate: …)`). Implication is
  decided for: the same condition (an equality written either way round); `x IS NOT NULL` from any
  comparison of `x` with a constant, a `BETWEEN` or an `IN` list; a boolean column from
  `column = true` and the reverse; and a comparison of `x` with a whole number from a tighter one
  (`amount > 2000` from `amount > 2500` or `amount = 2990`). Anything else is answered by a scan.
- `pg_indexes.indexdef` and both dump formats carry the predicate; a restart, a restore and a
  standby keep the index partial.
- A predicate that is not immutable (`now()`, `random()`, a subquery) is refused with 42P17. A
  partial index is a btree index: `USING gin | gist | hnsw … WHERE` is refused with 0A000.
- `ALTER TABLE … RENAME COLUMN` of a column the predicate reads is refused (drop the index first);
  `DROP COLUMN` of such a column drops the index.

Known limits: `ON CONFLICT (columns)` and `ON CONFLICT ON CONSTRAINT name` do not take a partial
index as their arbiter (the conflict is reported as 23505); on a branch a partial UNIQUE index is
checked when the branch is merged, not on each write; `ORDER BY … LIMIT` is not served from a
partial index. A partial index created by an earlier release was recorded without its predicate
and stays an index over every row: drop it and create it again.

Tests: `tests/index_partial.rs`.
