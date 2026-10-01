# Server endpoints and development authentication

PostgreSQL and native replication are different protocols. A primary listens for
SQL on `--port` (default 5432) and native WAL on `--replication-port` (default
5433). Give them distinct endpoints. A standby's `--primary-host` must address
the primary's native replication port, not its SQL port. PostgreSQL ErrorResponse
bytes at that endpoint indicate a protocol mismatch; changing retry delays will
not correct it. Listener bind failures are reported before server readiness.

For example, using separate private test data directories:

```bash
heliosdb-nano start --data-dir /srv/nano-primary --listen 127.0.0.1 \
  --port 5432 --replication-role primary --replication-port 5433 --http-port 0
heliosdb-nano start --data-dir /srv/nano-standby --listen 127.0.0.1 \
  --port 5442 --replication-role standby --primary-host 127.0.0.1:5433 --http-port 0
```

A successful native connection does not prove catchup across a restart or a
recreated primary. See [offline recovery limitations](offline-reseed.md).

The equivalent primary replication configuration is:

```toml
[replication]
role = "primary"
replication_port = 5433
sync_mode = "async"
```

Replication values resolve as explicit CLI flags, then file values, then defaults.
Explicit `--replication-role standalone` overrides a configured primary; explicit
`--standby-hosts ''` clears its configured peer list. Host endpoints accept DNS
names, IPv4, or bracketed IPv6 with a nonzero port. Unknown replication keys and
invalid values are errors. `standby_hosts` and `observer_hosts` are arrays; as with
the existing CLI, these lists do not make tier-1 actively dial those peers.

HTTP accepts a literal IPv4/IPv6 address or an IP:port socket:

```bash
heliosdb-nano start --memory --http-listen 127.0.0.1:8081
heliosdb-nano start --memory --http-listen '[::1]:8081'
```

An explicit `--http-port` overrides a port embedded in `--http-listen`; without
either, the port is 8080. `--http-port 0` disables HTTP even when the listen value
contains a port. Bare IPv6 addresses are supported; bracket IPv6 when attaching
a port. HTTP DNS names are not accepted.

Remote trust authentication remains refused by default. For isolated development,
`--allow-insecure-trust` explicitly permits PostgreSQL trust on a non-loopback
interface and emits an insecure-development warning. It does not change the
selected authentication method; password/SCRAM authentication remains enforced
when selected. The option is preserved in daemon mode. Prefer loopback or password
authentication for ordinary deployments.
