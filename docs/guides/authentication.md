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

The MySQL-wire listener has its own from-scratch TLS support: pass
`--mysql-tls-cert` + `--mysql-tls-key` (PEM, X.509) alongside `--mysql`.
MySQL clients request TLS via the `CLIENT_SSL` capability flag rather than
PostgreSQL's SSLRequest pre-handshake, but the certificate/key file shape is
identical.

```bash
heliosdb-nano start --data-dir ./mydata --mysql \
  --mysql-tls-cert cert.pem --mysql-tls-key key.pem
```

### Post-quantum hybrid key exchange

Both listeners build their `rustls` `ServerConfig` from an explicit,
PQ-aware `CryptoProvider` (aws-lc-rs-backed) rather than the implicit
process-wide default. By default (`--tls-post-quantum true` /
`--mysql-tls-post-quantum true`, both default `true`) the server offers
`X25519MLKEM768` — the draft-ietf-tls-ecdhe-mlkem hybrid post-quantum
key-exchange group — ahead of the classical groups (`X25519`,
`SECP256R1`, `SECP384R1`) in TLS 1.3's group preference list. A client that
also supports the hybrid group negotiates it; a classical-only client falls
back to `X25519` as normal. Set the flag to `false` to restrict the server
to classical key exchange only (e.g. to match a compliance baseline that
has not yet approved PQ algorithms).

**What this protects, and what it does not.** PQ hybrid key exchange
defends the *session key* against harvest-now-decrypt-later: traffic
captured today and stored for a future large-scale quantum computer to
attack cannot have its key-exchange step broken retroactively, because the
session key depends on the classical exchange **and** the post-quantum
one — an attacker has to break both. It does **not** change anything about
certificate or key **storage, rotation, or issuance**. The server's
certificate is still signed with a classical algorithm (see below), the
private key on disk is still just a PEM file, and none of that becomes more
or less secure because the key-exchange group changed. Operators are still
fully responsible for:

- **File permissions.** The private key file should be `0600` (owner
  read/write only) and owned by the service user the Nano process runs as
  — never group- or world-readable. `CertificateManager::save_cert_files`
  sets `0600` automatically on Unix for certificates it generates, but a
  key file supplied from elsewhere (a CA-issued cert, a cert-manager
  volume mount, etc.) is not automatically re-permissioned and should be
  checked (`stat -c %a key.pem`, or `ls -l`) after every deploy.
  Certificate files themselves are public data and don't need the same
  restriction, but keeping them under the same directory permissions as
  the key is a reasonable default.
- **Rotation cadence.** PQ hybrid TLS does not extend or shorten how long a
  certificate should live — continue rotating on whatever cadence your
  compliance regime or CA's issuance defaults require (90 days is common
  for publicly-trusted certs; internal CAs vary). A compromised or
  soon-to-expire key is exactly as urgent to rotate as it was before this
  feature existed.
- **Issuance discipline.** Nothing here changes how you decide who gets a
  certificate signed, what CA policy governs it, or how revocation is
  handled. `CertificateManager::generate_self_signed` and
  `generate_test_cert` are development/test helpers, not a substitute for
  a real issuance process in production.

**Certificate signatures remain classical, and that is a known gap, not a
scheduled feature.** The server's own certificate — the thing that proves
"you are actually talking to this server" — is still signed with RSA or
ECDSA today; the PQ work in this release covers key exchange only. Signing
certificates with a post-quantum algorithm (e.g. ML-DSA / Dilithium) is a
real gap for the "harvest-now-decrypt-later" threat model in its strongest
form (an attacker who can also forge future certificate chains), but it is
**not scheduled for any specific Nano release** — do not represent it as
committed or imminent in any release plan, customer communication, or
compliance document. If that gap matters for your threat model today, the
mitigation is procedural (shorter certificate lifetimes, tighter CA
issuance control), not something this TLS layer currently closes.

### Checking what a connection actually negotiated

Because a client and server independently decide whether to offer the PQ
hybrid group, "PQ TLS is enabled on the server" does not by itself tell you
what any given connection negotiated — a classical-only client still
connects successfully, just without the PQ protection. Both listeners
capture the negotiated key-exchange group per connection (via rustls's
`negotiated_key_exchange_group()`) and expose it:

```sql
-- PostgreSQL wire: per-connection, real-time.
SHOW ssl_key_exchange;
--  ssl_key_exchange
-- ------------------
--  X25519MLKEM768      -- PQ hybrid was negotiated
-- (or "X25519" for a classical connection, or empty on a plaintext one)
```

```sql
-- MySQL wire: as a SHOW VARIABLES entry, and via @@ssl_kx_group.
SHOW VARIABLES LIKE 'ssl_kx_group';
SELECT @@ssl_kx_group;
```

Both listeners also log the negotiated group at DEBUG level when TLS is
active, alongside the existing connection-accept log line — useful for
confirming PQ negotiation in a `RUST_LOG=debug` session without querying
each connection.

### MySQL mutual TLS (client certificate verification)

The MySQL listener can require a client certificate signed by a configured
CA, in addition to the server certificate the client already verifies.
This is a library-level `MysqlSslConfig` option today (no CLI flag yet):

```rust
use heliosdb_nano::protocol::mysql::{MysqlSslConfig};

let ssl_config = MysqlSslConfig::new("cert.pem", "key.pem")
    .with_client_cert_verification("ca.pem"); // sets require_client_cert = true
```

With this set, `MysqlSslNegotiator` builds its `rustls::ServerConfig` with
a `WebPkiClientVerifier` over a `RootCertStore` loaded from `ca.pem`
instead of `.with_no_client_auth()`. A client that presents no certificate,
or a certificate signed by a different CA, fails the TLS handshake itself
— the connection never reaches MySQL authentication. This is independent
of (and layered on top of) whatever `--auth` mode governs the
username/password step; mTLS proves the *connection*, not necessarily the
*application user*.

PostgreSQL's `SslMode::VerifyCA` / `VerifyFull` enum variants exist in this
version but are **not** wired to client-certificate verification — the
negotiator unconditionally builds its `ServerConfig` with
`.with_no_client_auth()` regardless of mode, and `ca_cert_path` is stored
but never loaded. Selecting `VerifyCA` or `VerifyFull` today silently
behaves like `Require` (TLS required, no client cert checked). Treat this
as a known gap on the PostgreSQL wire, not a redundant/alternate path to
the MySQL mTLS support above.

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
