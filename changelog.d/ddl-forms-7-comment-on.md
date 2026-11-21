**`COMMENT ON` stores the comment.**

What was wrong: `COMMENT ON … IS '…'` was accepted and the text thrown away. `pg_description` was always empty,
there was no `obj_description()` or `col_description()`, and a dump lost every comment.

What happens now: the comment is stored for tables, columns, indexes, views, materialized views, functions,
schemas, constraints, sequences and the other kinds a script may name. `COMMENT … IS NULL` (or an empty
string) removes it. `pg_description` lists the comments on tables, columns, indexes, views, schemas and
constraints under the object ids the other catalog views use, and `obj_description(oid [, catalog])`,
`col_description(table_oid, column_number)` and `shobj_description()` return them. SQL and binary dumps carry
every comment and a restore brings it back. A table's comments (its own, its columns', its constraints') move
with `RENAME TO` and go with `DROP TABLE`; a column's follows `RENAME COLUMN` and goes with `DROP COLUMN`; an
index's and a view's go with the object.

As before, a comment on a table or a column needs its target to exist (unless `IF EXISTS` is written) and the
other forms are taken as written, so a script from another database that comments on an object this one does
not have (an extension, a constraint under another name) still loads. Such a comment is stored and dumped,
and has no row in `pg_description`. A comment on a function, sequence or schema is not removed when the
object is dropped.

Tests: `tests/comment_on_stored.rs`.
