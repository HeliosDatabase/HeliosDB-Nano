**A running server can be administered through a local admin socket.**

Until now the only path into a running server was its PostgreSQL or MySQL
listener, and the only thing the command line could do to a running server
was send it a signal.

A server with a data directory now also listens on a Unix socket inside it,
`<data-dir>/.s.hdb.admin` (`--admin-socket <path>` or `[server] admin_socket`
moves it, `off` disables it). It speaks the PostgreSQL wire protocol, so every
statement works on it. The file is created with mode 0600 for the user that
runs the server; a directory that others can write gets no socket, and the
log says why. The server reads who connected from the kernel and disconnects
anyone who is neither its own user nor root before answering anything. The
server's own user logs in as role `postgres` without a password; every other
role logs in with the server's password method, and nothing on this socket is
accepted by `trust`. A socket file left by a crashed server is replaced after
checking that nothing listens on it, and a clean shutdown removes it. The
socket lives beside the store without disturbing a switchover or a standby's
directory checks.

Tests: `tests/admin_socket.rs`, and the unit tests of the `admin` module.
