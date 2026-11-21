**`SWITCHOVER`, `SWITCHOVER CHECK`, `pg_promote()` and `SET NODE ALIAS` need an administrator's session.**

`SWITCHOVER` accepted any session that called itself `postgres` or `helios`,
which on a `trust` listener is any client. `pg_promote()` and `SET NODE ALIAS`
had no check at all: any session that could connect to a standby could promote
it.

All four now ask the one rule. On the server's admin socket, or over TLS as a
superuser role with a password, they work as before. Anywhere else they answer
SQLSTATE 42501 with a message that says how to connect as an administrator —
including a session on plain TCP as `postgres` under `trust`, and in both the
simple and the extended protocol.

**Upgrade note.** A tool that sends `SELECT pg_promote(...)` or `SWITCHOVER`
over a plain or `trust` connection (a failover manager, a proxy, a script)
must now use the admin socket on the node, or a TLS connection as a superuser
role with a password.

Tests: `tests/admin_socket.rs`; `tests/physical_switchover.rs`,
`tests/physical_promote_online.rs` and `tests/physical_promote.rs` now run
these statements on the admin socket.
