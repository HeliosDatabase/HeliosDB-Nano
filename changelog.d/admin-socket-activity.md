**`pg_stat_activity` shows how each session arrived and how it logged in.**

Nothing told an operator which sessions were on a privileged path.

`pg_stat_activity` has two more columns after PostgreSQL's own: `listener`
(`admin`, `unix`, `tcp`, `tls`, `mysql`) and `auth_method` (`peer`, `scram`,
`password`, `md5`, `trust`). Both are NULL for a session no listener accepted.

Tests: `tests/admin_socket.rs`.
