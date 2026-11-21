**MySQL session variables are real: `SET autocommit = 0` opens a transaction, and every other `SET` takes effect, is stored, or is refused.**

The MySQL listener answered OK to every `SET` and kept nothing. A client that
sent `SET autocommit = 0` (PyMySQL, SQLAlchemy and JDBC with autocommit off)
believed it was in a transaction and was not: its `ROLLBACK` undid nothing.
`SELECT @@name` answered an empty string for a variable it did not know.

Now each connection has its own variables:

- `autocommit = 0`: the first statement that reads or writes a table opens a
  transaction that stays open until `COMMIT` or `ROLLBACK`. DDL, `BEGIN` while
  one is open and `SET autocommit = 1` commit it, as in MySQL. The status flags
  of every answer say whether autocommit is on and whether a transaction is open.
- `SET [SESSION] TRANSACTION ISOLATION LEVEL`, `transaction_isolation`,
  `tx_isolation`: the engine's isolation level (`READ UNCOMMITTED` is served
  and reported as `READ COMMITTED`). `SET [SESSION] TRANSACTION READ ONLY` and
  `START TRANSACTION READ ONLY` refuse writes with error 1792.
- `foreign_key_checks = 0` turns foreign-key checking off for that session's
  statements only, as a dump restore needs. `sql_safe_updates`, `sql_select_limit`,
  `max_execution_time` (the statement timeout), `wait_timeout` (the idle limit
  of the connection) and `innodb_lock_wait_timeout` take effect.
- `sql_mode`, `time_zone`, `NAMES` and the character-set variables accept what
  this server is (strict, UTC, UTF-8) and refuse the rest with error 1231:
  `ANSI_QUOTES`, another time zone, `latin1`.
- Variables with no effect here (`unique_checks`, `net_write_timeout`,
  `sql_quote_show_create`, …) are stored and read back; `docs/compatibility/mysql.md`
  lists every variable and what it does.
- User variables (`SET @x = …`, `SELECT @x`, `@x := …`) are kept per connection.
- An unknown system variable is error 1193. `SET GLOBAL` is refused.
- `SELECT @@name`, `SHOW [SESSION | GLOBAL] VARIABLES [LIKE | WHERE]` and
  `SHOW STATUS` read these values and the engine's own parameters. `LIKE` is a
  pattern over the whole name, as in MySQL.
- `COM_RESET_CONNECTION` forgets all of it. `/*!40101 SET … */` is run.
- A statement cancelled by `KILL QUERY` is error 1317; one that ran past
  `max_execution_time` is error 3024. `CONNECTION_ID()` answers.

Tests: `tests/mysql_session_vars.rs`.
