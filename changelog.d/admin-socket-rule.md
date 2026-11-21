**One rule decides which session may administer the server.**

There was no single definition of an administrator's session: `SWITCHOVER`
looked at the role name, which under `trust` authentication is whatever the
client sent, and `pg_promote()` looked at nothing.

A session is now an administrator's when it arrived on the admin socket and
logged in as a superuser role, or when it arrived over TLS and logged in with
SCRAM or a password as a superuser role. Everything else is not: a TCP
connection without TLS, any `trust` session, the ordinary PostgreSQL and MySQL
sockets, the HTTP API and MCP. A refusal is SQLSTATE 42501 and says what was
refused, why, and how to connect as an administrator. An application that
embeds the database in its own process is not restricted.

Tests: the unit tests of the `admin` module, `tests/admin_socket.rs`.
