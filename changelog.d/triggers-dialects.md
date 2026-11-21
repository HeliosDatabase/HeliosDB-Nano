**MySQL and SQLite trigger syntax: inline bodies run.**

What was missing: `CREATE TRIGGER … BEGIN … END` and the MySQL single-statement body were parse errors.

What happens now, on every route including the MySQL wire:

- MySQL: `CREATE TRIGGER t BEFORE|AFTER INSERT|UPDATE|DELETE ON tbl FOR EACH ROW <statement>` and
  `… FOR EACH ROW BEGIN … END`, with `NEW.col` / `OLD.col`, `SET NEW.col = expr[, …]`,
  `IF … THEN … [ELSEIF … THEN …] [ELSE …] END IF`, `SIGNAL SQLSTATE 'xxxxx' [SET MESSAGE_TEXT = …]`
  and `INSERT`, `UPDATE`, `DELETE`, `REPLACE`, `SELECT` statements.
- SQLite: `CREATE TRIGGER [IF NOT EXISTS] t BEFORE|AFTER INSERT|UPDATE [OF cols]|DELETE ON tbl
  [FOR EACH ROW] [WHEN condition] BEGIN … END`, with `INSERT`, `UPDATE`, `DELETE`, `SELECT` statements,
  `SELECT RAISE(ABORT, 'message')` and `SELECT RAISE(IGNORE)`, each alone or with a `WHERE` or
  `FROM … WHERE` that decides whether it applies.
- `DROP TRIGGER [IF EXISTS] name` without `ON table`.

Refused at `CREATE TRIGGER` with a message that names the construct (`0A000`): `DECLARE`, loops, `CASE`
statements, `CALL`, user variables (`@x`), `SET` of anything but `NEW.col`, `SIGNAL` of a named
condition or with items other than `MESSAGE_TEXT`, `RAISE(ROLLBACK | FAIL, …)`, `RAISE(…)` inside an
expression, `FOLLOWS` / `PRECEDES`, and `CREATE TEMPORARY TRIGGER`.

Tests: `tests/trigger_dialect_forms.rs`.
