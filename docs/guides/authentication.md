# Authentication

HeliosDB Nano exposes four authentication modes on the PG-wire and
MySQL-wire listeners: `trust` (default for same-host development),
`password` (cleartext, suitable only with TLS), `md5` (PG legacy), and
`scram-sha-256` (PG 10+ default, **standards-compliant since v3.26.0**).

```bash
# Production default — SCRAM-SHA-256 over TLS
heliosdb-nano start --data-dir ./mydata --mysql \
  --auth scram-sha-256 --password s3cret \
  --tls-cert cert.pem --tls-key key.pem
```

## Auth-mode summary

| Mode | When to use | Wire shape | Notes |
|------|-------------|-----------|-------|
| `trust` | Local dev, same-host embedded | No challenge | Same-host-only since v3.26.0 (see below) |
| `password` | Behind TLS only | Cleartext over the socket | TLS strongly recommended |
| `md5` | Legacy PG clients only | MD5(password+salt) | Use SCRAM unless your client is < PG 9.5 |
| `scram-sha-256` | **Default for production** | RFC 5802 + PG SCRAM profile | Re-enabled for libpq / asyncpg / pgx / JDBC clients at v3.26.0 |

## SCRAM-SHA-256

Since **v3.26.0** the SCRAM parser correctly handles the GS2 header
that every conformant libpq-family driver sends. The client-first
message format is:

```
n,,n=,r=<24-char-nonce>
^^^                          GS2 channel-binding flag (no cbind = "n")
   ^                         GS2 authzid (empty per the Postgres SCRAM profile)
     ^^                      SCRAM client-first-message bare:
       ^^                       n=  → empty per RFC 5802 + PG SCRAM profile
                                       (the real username comes from
                                        the StartupMessage `user` param)
                                r=  → 24-char base64 nonce
```

Older Nano releases (< 3.26.0) misparsed the leading `n,,` header and
indexed the channel-binding flag as the username — every libpq /
asyncpg / pgx / node-postgres / JDBC client failed handshake with
`Invalid SCRAM client-first-message: missing GS2 authzid slot`. The
v3.26.0 fix is **server-side only**; drivers do not need to be
upgraded.

## Same-host-only `trust` (v3.26.0)

The `trust` mode disables password verification entirely and is
intended for local development. Since v3.26.0 the engine **rejects
trust-mode connections from non-loopback addresses** — a connection
from `127.0.0.1` (or `::1`, or the Unix socket) is accepted, anything
else gets the standard authentication error path.

```bash
# Loopback only — accepted
psql -h 127.0.0.1 -U postgres
psql -h /tmp     -U postgres    # Unix socket

# Network interface — rejected even though the engine is in trust mode
psql -h 192.168.1.10 -U postgres
# → FATAL:  authentication failed: trust mode requires a loopback connection
```

To allow non-loopback connections without a password, run the engine
in an embedded container with the listener bound to a Unix socket only
(see "Embedded mode" in [README.md](../../README.md#start-the-server)),
or move to `scram-sha-256` for any network-exposed listener.

## StartupMessage `database` validation (v3.25.0)

When a PG-wire client connects, the StartupMessage carries a
`database` parameter (set by `psql -d <name>` or the driver's `dbname`
option). Since v3.25.0 the engine validates that name against the
catalog and rejects unknown databases at handshake time. Previously a
typo silently fell back to the default database, which masked
configuration drift in multi-database setups.

See [`database_management.md`](database_management.md) for the
`CREATE DATABASE` / `DROP DATABASE` SQL surface that backs this.

## Password file and user catalog

`--password` on the command line sets the password for the default
user. For multi-user setups, use `--password-file` (TOML format):

```toml
# users.toml
[postgres]
password = "s3cret"
[gitea]
password = "gitea"
[readonly]
password = "ro"
```

```bash
heliosdb-nano start --data-dir ./mydata \
  --auth scram-sha-256 --password-file users.toml
```

The password file is read at startup and the stored credentials
include a SHA-256-derived `StoredKey` + `ServerKey` per user.
SCRAM-SHA-256 verifies the client proof against `StoredKey` without
ever transmitting the password.

## TLS

PG-wire TLS is negotiated via the standard SSLRequest pre-handshake.
Provide `--tls-cert` + `--tls-key` (PEM, X.509) and the server will
advertise SSL. Most drivers accept TLS automatically; `psql` requires
`sslmode=require` (or `verify-full` with a CA).

```bash
psql "host=127.0.0.1 port=5432 user=postgres dbname=myapp sslmode=require"
```

## See also

- [`upgrade.md`](upgrade.md) — what changes between auth-mode-affecting
  versions.
- [`database_management.md`](database_management.md) — the `CREATE
  DATABASE` flow that the StartupMessage validation references.
