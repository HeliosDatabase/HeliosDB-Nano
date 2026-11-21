**The wires report a refusal in their own terms: SQLSTATE `42501` on PostgreSQL, 1142 and 1044 on MySQL.**

On the PostgreSQL wire a refused statement answers `42501` with PostgreSQL's
message, on the simple and the extended protocol; a prepared statement is
checked each time it is executed, so it stops working when the privilege is
revoked and works again when it is granted back. `COPY … FROM STDIN` needs
`INSERT` and `COPY … TO STDOUT` needs `SELECT`. `SET ROLE`, `RESET ROLE` and
`SET SESSION AUTHORIZATION` run on the connection's session. A role without
`CONNECT` on the database it names is refused at connection start.

On the MySQL wire a refused statement answers in MySQL's codes and wording:
1142 `SELECT command denied to user 'u'@'%' for table 't'` for a table or view
(the command is the statement's own verb), 1044 `Access denied for user
'u'@'%' to database 's'` for a schema, 1370 for a routine or a sequence, and
1227 for a statement that needs a role attribute. `root`, like any login that
names no role in the catalog, is the server's own account and is not restricted.

Tests: `tests/privilege_wire.rs`.
