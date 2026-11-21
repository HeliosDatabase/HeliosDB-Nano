**`MERGE INTO` is supported (PostgreSQL 15 syntax).**

What was missing: `MERGE INTO target USING source ON … WHEN …` was refused; only `MERGE BRANCH`, a
different statement, existed.

What happens now: `WHEN MATCHED [AND …] THEN UPDATE | DELETE | DO NOTHING`, `WHEN NOT MATCHED [BY TARGET]
[AND …] THEN INSERT | DO NOTHING`, `WHEN NOT MATCHED BY SOURCE …` and `RETURNING` (with `merge_action()`)
run on the embedded API (text and bound-parameter calls) and on the PostgreSQL wire (simple and extended
protocol), with the command tag `MERGE n`. The source may be a table, a subquery, `VALUES` or a CTE. The
statement is atomic: a failure in any action undoes every action. A target row that a second source row
would change raises `21000`. A view or materialized view as target is refused with `42809`. Each action
is the ordinary INSERT, UPDATE or DELETE of one row, so constraints, foreign keys, defaults, serial
columns, indexes and branches behave as for those statements.

Not supported, refused with `0A000`: a subquery inside `ON`, a `WHEN` condition, a `SET` value or an
`INSERT` value (compute it in the `USING` source); `SET (a, b) = (…)`; `INSERT … ROW`.

Tests: `tests/merge_into_tests.rs`, `tests/sql_std_wire_tests.rs`.
