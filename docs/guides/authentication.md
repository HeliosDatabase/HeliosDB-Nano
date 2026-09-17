# Authentication

HeliosDB Nano exposes four authentication modes on the PG-wire and
MySQL-wire listeners: `trust` (default for same-host development),
`password` (cleartext, suitable only with TLS), `md5` (PG legacy), and
`scram-sha-256` (PG 10+ default, standards-compliant).

```bash
# Production default — SCRAM-SHA-256 over TLS
heliosdb-nano start --data-dir ./mydata --mysql \
  --auth scram-sha-256 --password s3cret \
  --tls-cert cert.pem --tls-key key.pem
```

## Auth-mode summary

| Mode | When to use | Wire shape | Notes |
|------|-------------|-----------|-------|
| `trust` | Local dev, same-host embedded | No challenge | Same-host-only; see below |
| `password` | Behind TLS only | Cleartext over the socket | TLS strongly recommended |
| `md5` | Legacy PG clients only | MD5(password+salt) | Use SCRAM unless your client is < PG 9.5 |
| `scram-sha-256` | **Default for production** | RFC 5802 + PG SCRAM profile | Compatible with libpq / asyncpg / pgx / JDBC clients |

## SCRAM-SHA-256

The SCRAM parser handles the GS2 header that every conformant
libpq-family driver sends. The client-first message format is:

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

The real username comes from the StartupMessage `user` parameter, not
the empty SCRAM `n=` slot. Drivers do not need special handling for
Nano.

## Same-host-only `trust`

The `trust` mode disables password verification entirely and is
intended for local development. The engine rejects trust-mode
connections from non-loopback addresses — a connection from `127.0.0.1`
(or `::1`, or the Unix socket) is accepted, anything else gets the
standard authentication error path.

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

## StartupMessage `database` validation

When a PG-wire client connects, the StartupMessage carries a
`database` parameter (set by `psql -d <name>` or the driver's `dbname`
option). The engine validates that name against the catalog and rejects
unknown databases at handshake time, which prevents typos from silently
falling back to the default database.

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

### Custom password stores

Everything in this subsection is about the PostgreSQL-wire listener; the
MySQL-wire listener currently accepts any credentials (trust) whatever `--auth`
says, and is tracked separately.

A name that does not exist completes the *same* SCRAM exchange as one that
does — challenge, proof, and then the identical `28P01 password
authentication failed for user "..."` — so the wire never reveals which
accounts exist. Unknown users are challenged with deterministic *synthetic*
credentials, and a proof matching them is still rejected.

A **persistent** `PasswordStore` should keep a cryptographically random
32-byte secret beside its credential data and return it from
`scram_mock_authentication_secret()` (or hand it to
`SharedPasswordStore::with_mock_authentication_secret()`), so the synthetic
salt for a given name does not change across restarts — a salt that changes
per process is itself an account oracle. **Ephemeral** stores (whose
credentials die with the process anyway) need nothing: the wrapper generates
a random secret for its own lifetime.

**All verifiers in one backend must have the same shape.** The synthetic
challenge served for an absent name advertises exactly what
`default_scram_iterations()` returns and a **16-byte salt** — the shape
`ScramCredentials::from_password()` produces. Every credential the backend
returns from `get_credentials()` must therefore use that same iteration count
and a 16-byte salt. A store holding verifiers **imported** from another
system (PostgreSQL's `pg_authid.rolpassword`, an LDAP export, an older Nano
deployment with a different `--scram-iterations`) hands the attacker the
oracle back through `i=` and `s=`: any name whose challenge deviates from the
house shape is a name that exists. Normalise before relying on this property
— re-derive each verifier at the account's next successful login, or re-create
the accounts — and return the count you really derive with, not an aspirational
one.

## TLS

PG-wire TLS is negotiated via the standard SSLRequest pre-handshake.
Provide `--tls-cert` + `--tls-key` (PEM, X.509) and the server will
advertise SSL. Most drivers accept TLS automatically; `psql` requires
`sslmode=require` (or `verify-full` with a CA).

```bash
psql "host=127.0.0.1 port=5432 user=postgres dbname=myapp sslmode=require"
```

## SQL session identity

`current_user`, `session_user`, `current_role` (bare or as `current_role()`)
and `current_setting('session_authorization')` report the login name of the
connection asking: the PG-wire StartupMessage `user`, published only *after*
authentication succeeds (a rejected login never becomes a SQL identity, and a
startup packet with no `user` is refused with `08P01`); the MySQL-wire
handshake user — that listener is trust-only, so the name is asserted by the
client, not proved, and an *empty* handshake user is not an identity at all
(such a session reports `heliosdb`); or the name passed to `create_session()`
on the embedded API. A session-less embedded call reports the service user
`heliosdb`. Names are truncated to 63 bytes, PostgreSQL's identifier limit.
`SHOW session_authorization` answers the same name, but on the **PostgreSQL
wire only** — the embedded API and the MySQL wire do not serve that `SHOW`.

`current_role` is a **reserved word**, exactly as in PostgreSQL: an unquoted
`current_role` resolves to the function even when a table in scope has a column
of that name, on every dialect Nano accepts (including SQLite drop-in apps,
which SQLite itself would have resolved to the column). Quote it —
`SELECT "current_role" FROM audit` — to read the column.

This is identity **reporting**, not access control: no privilege or row-access
decision is made from these functions, `SET ROLE` / `SET SESSION
AUTHORIZATION` are refused because identity switching is not implemented, and
the multi-tenant API's RLS policies are a separate system with their own
session context. That refusal carries SQLSTATE `0A000` on the **PostgreSQL
wire in the default configuration** (`[authentication] legacy_acl_noop =
false`; set it to `true` and the PG wire silently acknowledges both statements
instead of refusing them); the embedded API and the MySQL wire have no
interceptor for them at all and refuse them with a plain engine error carrying
no SQLSTATE.

Identity reporting does not reach the catalog: `pg_tables.tableowner`,
`pg_roles` / `pg_user` and ACL grantors still report the literal service role,
so a wire login is a name no ownership surface knows about. In particular
`SELECT … FROM pg_tables WHERE tableowner = current_user` matched every table
before these functions were made real and now matches none — filter on a
literal owner name instead until ownership is tracked per role.

## See also

- [`upgrade.md`](upgrade.md) — what changes between auth-mode-affecting
  versions.
- [`database_management.md`](database_management.md) — the `CREATE
  DATABASE` flow that the StartupMessage validation references.
