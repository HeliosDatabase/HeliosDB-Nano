**The PostgreSQL and MySQL Unix sockets are no longer open to every local user by default.**

Both sockets were created with mode 0777 and there was no way to change it.
For the MySQL socket, which has no login, that meant any local user could run
SQL.

They are now created with mode 0770: the server's user and group.
`--pg-socket-mode` and `--mysql-socket-mode` (`[server] pg_socket_mode`,
`mysql_socket_mode`; octal) set another mode, and `0777` restores the old
behaviour. A value that is not octal permission bits refuses the start. The
MySQL socket is still refused when the server requires a password.

**Upgrade note.** A client that runs as a different user outside the server's
group can no longer connect to these sockets until the mode is widened or the
client joins the group.

Tests: `tests/admin_socket.rs`.
