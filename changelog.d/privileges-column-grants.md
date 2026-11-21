**Column grants: `GRANT SELECT (a, b) ON t TO r` is accepted and enforced.**

A privilege limited to columns was refused as not supported.

Now `SELECT`, `INSERT`, `UPDATE` and `REFERENCES` can be granted and revoked on
columns. A statement needs the privilege on the table or on every column it
reads or writes, wherever the column appears: the select list, `WHERE`, `ORDER
BY`, `GROUP BY`, `HAVING`, a join condition, a window, a subquery, `RETURNING`,
`ON CONFLICT … DO UPDATE`. `SELECT *` and `INSERT` with no column list touch
every column. `count(*)` needs any one column. A column that cannot be
attributed to a table is refused, never allowed. Revoking a privilege on the
table removes its column grants too. Dropping a column forgets its grants;
renaming it keeps them. `has_column_privilege` and `has_any_column_privilege`
report them.

Tests: `tests/privilege_columns.rs`.
