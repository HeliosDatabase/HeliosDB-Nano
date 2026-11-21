**`SHOW BRANCHES`, `SHOW TOPOLOGY`, `SHOW DATABASES`, `SHOW PROCESSLIST` and `KILL` on the MySQL wire answer from real state.**

A `SHOW` statement the MySQL listener had no answer of its own for got an
empty OK, so `SHOW BRANCHES` and `SHOW TOPOLOGY` reported nothing. `SHOW
DATABASES` listed one fixed name, `SHOW PROCESSLIST` only the asking
connection, and `KILL` did not exist. A `SHOW` was classified by any keyword
anywhere in it, so `SHOW COLUMNS FROM my_tables` listed tables.

Now a `SHOW` the listener does not answer itself is run by the engine, and an
unknown one is an error. `SHOW DATABASES` lists the databases that exist and
`SHOW PROCESSLIST` the live sessions of both wires. `KILL QUERY id` interrupts
the statement that session is running (it ends with error 1317 and the
connection stays); `KILL [CONNECTION] id` also closes a MySQL connection. With
authenticated logins a session may kill its own user's sessions, a superuser
any. `KILL CONNECTION` for a session of the PostgreSQL wire is refused with
error 1235; `KILL QUERY` works for it.

Tests: `tests/mysql_show_kill.rs`.
