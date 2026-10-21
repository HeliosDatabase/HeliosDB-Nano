# Server endpoints and development authentication

PostgreSQL and replication are different protocols on different ports. A primary
listens for SQL on `--port` (default 5432) and serves its standbys on ONE
replication port, `--replication-port` (default 5433, protocol `NPHYS002`): the
bootstrap copy, the physical WAL, heartbeats and acknowledgements all travel over
one connection per standby. Give the two ports distinct endpoints. A standby's
`--primary-host` must address the primary's replication port, not its SQL or HTTP
port; a standby pointed at either is told so ("--primary-host must name the
primary's --replication-port"), and changing retry delays will not correct it.
Listener bind failures are reported before server readiness.

For example, using separate private test data directories under one owner-only
parent (owned by the user that runs Nano) and one token file:

```bash
install -d -m 700 /srv/nano
head -c 32 /dev/urandom > replication.token && chmod 600 replication.token
heliosdb-nano start --data-dir /srv/nano/primary --listen 127.0.0.1 \
  --port 5432 --replication-role primary --replication-port 5433 \
  --replication-token-file replication.token --http-port 0
heliosdb-nano start --data-dir /srv/nano/standby --listen 127.0.0.1 \
  --port 5442 --replication-role standby --primary-host 127.0.0.1:5433 \
  --replication-token-file replication.token --http-port 0
```

The standby's data directory must be absent or empty on its first start; it
bootstraps a verified copy of the primary, then follows it, and a later start
resumes at its durable cursor. A standby refuses a primary with another dataset
history and stays up in `needs_recovery`. A successful connection does not by
itself mean the standby is current: readiness is its posture (`/ready`,
`heliosdb_require_ready()`). See [the physical standby guide](physical-standby.md),
the [history guard and interface restrictions](replication-history-guard.md) and
[recovering after a primary is recreated](offline-reseed.md).

The equivalent primary replication configuration is:

```toml
[replication]
role = "primary"
replication_port = 5433
token_file = "/etc/heliosdb/replication.token"
sync_mode = "async"          # async | semi-sync | sync
min_acks = 1                 # 1..=64
sync_timeout_ms = 10000      # 1..=86400000
sync_timeout_action = "warn" # warn | fail
startup_timeout_secs = 30    # 1..=3600; a standby's wait for streaming
```

Replication values resolve as explicit CLI flags, then file values, then defaults.
`--replication-startup-timeout-secs` overrides the standby's startup budget,
including when explicitly set to its 30-second default. Standby daemon readiness
allows that budget plus 5 seconds. Explicit `--replication-role standalone`
overrides a configured primary; explicit `--standby-hosts ''` clears its configured
peer list. Host endpoints accept DNS names, IPv4, or bracketed IPv6 with a nonzero
port. Unknown replication keys and invalid values are errors. `standby_hosts` is
informational: a primary accepts the standbys that connect to it and dials none.
`--replication-port` has no effect on a standby.

The replication port is served only by a persistent primary with a replication
token; `--physical-replication=false` turns it off. A primary that serves no
replication port never waits for standbys, whatever `sync_mode` says, and warns
at startup.

## One replication secret

One owner-only 32-byte token file authenticates the replication connection. Use
the identical file on the primary and every standby:

```bash
head -c 32 /dev/urandom > replication.token
chmod 600 replication.token
```

Configure the path; the token bytes are never read from TOML or the environment:

```toml
[replication]
token_file = "/etc/heliosdb/replication.token"
```

`--replication-token-file` overrides the file value. The file must be an owned,
non-symlink, single-link regular file, mode `0600` or stricter, containing exactly
32 raw bytes; anything else is refused before the server starts. The primary
checks the token in constant time before it reads any other frame, and before
any standby is registered or receives anything. A wrong token is answered with a
typed refusal (`AuthFailed`); the standby retries it three times in all (a token
file being replaced on both nodes), then stays up in `needs_recovery`.

## Loopback only until TLS replication

The replication connection is plaintext, so in this release the replication port
**always listens on loopback**: on `--listen`'s address when that is a loopback
address, otherwise on `127.0.0.1`, whatever `--listen` opens for PostgreSQL (a log
line says so). A standby's `--primary-host` must resolve to loopback too. Reach a
primary on another host through an authenticated tunnel that ends on loopback at
both sides, for example `ssh -N -L 127.0.0.1:5433:127.0.0.1:5433 primary.example`
on the standby host. Remote replication over TLS is spec P2.2.
`--allow-insecure-replication-remote` (or `[replication] allow_insecure_remote =
true`) is accepted and preserved in daemon mode, but changes nothing yet; a
startup WARN says so.

## HTTP and trust authentication

HTTP accepts a literal IPv4/IPv6 address or an IP:port socket:

```bash
heliosdb-nano start --memory --http-listen 127.0.0.1:8081
heliosdb-nano start --memory --http-listen '[::1]:8081'
```

An explicit `--http-port` overrides a port embedded in `--http-listen`; without
either, the port is 8080. `--http-port 0` disables HTTP even when the listen value
contains a port. Bare IPv6 addresses are supported; bracket IPv6 when attaching
a port. HTTP DNS names are not accepted. On a replication node, `/health` carries
the replication block and `/ready` answers 200 or 503 by readiness; both stay
public when the HTTP API requires credentials.

Remote trust authentication remains refused by default. For isolated development,
`--allow-insecure-trust` explicitly permits PostgreSQL trust on a non-loopback
interface and emits an insecure-development warning. It does not change the
selected authentication method; password/SCRAM authentication remains enforced
when selected. The option is preserved in daemon mode. Prefer loopback or password
authentication for ordinary deployments.
