**Triggers run their function: row and statement level, BEFORE and AFTER, for INSERT, UPDATE, DELETE and TRUNCATE.**

What was missing: `CREATE TRIGGER` was accepted and the trigger never fired. The one exception was a
`BEFORE INSERT` row trigger whose function was nothing but `NEW.col = …` assignments and `RETURN`, which
was turned into a row rewrite once, at `CREATE TRIGGER`.

What happens now: the trigger's function runs through the PL/pgSQL interpreter when the trigger fires.

- Events and timing: row-level `BEFORE` and `AFTER` for `INSERT`, `UPDATE [OF columns]` and `DELETE`;
  statement-level `BEFORE` and `AFTER` for the same and for `TRUNCATE`; several events in one trigger
  (`INSERT OR UPDATE`); the `WHEN (condition)` clause; triggers of one kind fire in name order.
- The function sees `NEW` and `OLD` by field (`NEW.qty`, `OLD.qty`), may assign `NEW.col := …` in a
  `BEFORE` row trigger, and has `TG_OP`, `TG_NAME`, `TG_WHEN`, `TG_LEVEL`, `TG_TABLE_NAME`,
  `TG_TABLE_SCHEMA`, `TG_NARGS` and `TG_ARGV[n]`.
- Return value: a `BEFORE` row trigger that returns `NULL` skips the row; the `NEW` it returns is what is
  stored; the return value of an `AFTER` trigger is ignored. A function that ends without `RETURN` fails
  the statement.
- `RAISE EXCEPTION 'text %', value` fails the statement with the formatted message and SQLSTATE `P0001`
  (or the one given with `SQLSTATE '…'`), and nothing of the statement stays. `RAISE NOTICE` and
  `RAISE WARNING` are delivered to a PostgreSQL client as notices.
- The function's SQL runs in the transaction of the statement that fired it: its writes commit and roll
  back with it (also to a savepoint), and they fire the triggers of the tables they write to. A chain
  deeper than 64 levels (or one that would exhaust the stack first) is stopped with SQLSTATE `54001`.
- `AFTER` row triggers run when all rows of the statement are in place, then the `AFTER` statement
  triggers, as in PostgreSQL.
- Every statement route fires them: `execute` and `execute_params`, the PostgreSQL simple and extended
  protocols, the MySQL wire, `INSERT … SELECT`, `INSERT … ON CONFLICT` (insert triggers, and update
  triggers for the rows `DO UPDATE` changes), `RETURNING`, a procedure's writes, and foreign-key actions
  on a child table (its row triggers). A table with a trigger leaves the fast paths and the bulk `COPY`
  path, which cannot fire it; a route that cannot fire a table's trigger refuses the write (`0A000`).
- A table without a trigger pays one atomic load per statement.

Not in this change: `INSTEAD OF` triggers on views, constraint and deferrable triggers and `REFERENCING`
transition tables are refused at `CREATE TRIGGER` (`0A000`) instead of being accepted and never fired.
Local variables of a trigger function keep the `$name` rule of procedures. In a `BEFORE INSERT` trigger
a generated key that the statement did not supply is still `NULL`; an `AFTER` trigger sees it.

Tests: `tests/trigger_row_events.rs`, `tests/trigger_raise_and_nesting.rs`, `tests/trigger_write_routes.rs`.
