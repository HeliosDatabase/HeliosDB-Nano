**SQL cursors: `DECLARE … CURSOR`, `FETCH`, `MOVE`, `CLOSE`, `pg_cursors`.**

What was missing: every cursor statement was refused.

What happens now: `DECLARE name [BINARY] [INSENSITIVE] [[NO] SCROLL] CURSOR [WITH | WITHOUT HOLD] FOR
query`; `FETCH` and `MOVE` with `NEXT`, `PRIOR`, `FIRST`, `LAST`, `ABSOLUTE n`, `RELATIVE n`,
`FORWARD [n | ALL]`, `BACKWARD [n | ALL]`, a bare count and `ALL`; `CLOSE name | ALL`; `DISCARD ALL`
closes them. The command tags are `DECLARE CURSOR`, `FETCH n`, `MOVE n` and `CLOSE CURSOR`, and a `FETCH`
is described with the cursor's columns on the simple and the extended protocol. A cursor belongs to the
session that declared it: two sessions can use the same name. Without `HOLD` a cursor needs a transaction
block (`25P01`) and ends with it; `WITH HOLD` survives `COMMIT` and is dropped by the `ROLLBACK` of the
transaction that declared it and at the end of the session. `pg_cursors` lists the session's cursors.
Embedded handles and engine-level sessions have cursors too.

Limits to know: `DECLARE` runs the query once, under the snapshot and the role of the declaring
statement, and keeps the result; the result is bounded by the new `[session] cursor_max_rows` setting
(default 100000, `54000` beyond it). A `FETCH` under a role or tenant other than the declaring one is
refused with `42501`. `ROLLBACK TO SAVEPOINT` does not close cursors declared after the savepoint.
`BINARY` is recorded in `pg_cursors` but does not change the row format.

Tests: `tests/sql_cursor_tests.rs`, `tests/sql_std_wire_tests.rs`.
