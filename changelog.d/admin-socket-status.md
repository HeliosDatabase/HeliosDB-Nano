**`heliosdb-nano status` asks the running server.**

`status` only looked at the PID file and the data directory's lock: it could
say that a process existed, not what the server was doing.

When the server's admin socket answers, `status` now reports from the server
itself: its version, role (primary or standby), state, readiness, epoch, a
standby's replication lag when it is known, and the number of connections.
When the server cannot be asked, the answer comes from the files as before.
The report says which it is (`Source:`; `"source"`, `"server"` and
`"server_unavailable"` with `--json`). The exit codes are unchanged.

For later tools there is a small library helper that finds the admin socket of
the server owning a data directory (or named by a PID file), tells whether a
server listens on it, and runs statements on it.

Tests: `tests/admin_socket.rs`, and the unit tests of the `admin` module.
