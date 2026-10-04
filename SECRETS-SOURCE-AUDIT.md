# Secret-source audit (P0-2, 2026-10-04)

Static audit of every secret source reachable from the `heliosdb-nano`
front door, at the reviewed secrets tip `099a90f5` as cherry-picked onto the
integration branch (the HTTP-auth work is already present there; see the
listener-policy section below). This is a code-inspection inventory, not a
runtime claim: no live daemon was exercised for it (the real-process harness
is `scripts/test_server_secret_argv.sh` -> `test_server_secret_argv.py`, gated
behind `HELIOSDB_PROC_REGRESSION=1`).

Files inspected: `src/main.rs`, `src/config.rs`,
`src/crypto/key_manager.rs`, `src/cli/dump.rs`, `src/cli/restore.rs`,
`deployment/docker/docker-entrypoint.sh`,
`deployment/docker/docker-compose.yml`.

## Summary

| Command | Secret | Source(s) | Value on argv? |
|---|---|---|---|
| `start` | PG auth password | `--password`, `--password-file`, `HELIOSDB_PASSWORD_FILE`, `HELIOSDB_PASSWORD` | only legacy `--password` |
| `start` | MCP bearer token | `--mcp-token`, `HELIOSDB_MCP_TOKEN` | only literal `--mcp-token` |
| `start` | TLS private key | `--tls-key`, `--mysql-tls-key` | path only, not key bytes |
| `start` | At-rest encryption key | config `[encryption].key_source` (`Environment` / `File` / `Kms`) | never (no CLI flag) |
| `dump` | Connection URI | `--connection` | yes (path/URI can embed credentials) |
| `restore` | Connection URI | `--connection` | yes (path/URI can embed credentials) |
| `init` / `repl` / `stop` / `status` / `code-graph hook` | none | — | — |

## `start`: password

Precedence, resolved in `resolve_password` (`src/main.rs:218`):

1. `--password <value>` — deprecated; emits a WARN (`src/main.rs:810`).
2. `--password-file <path>` — first line; empty file fails closed
   (`read_password_file`, `src/main.rs:183`).
3. `$HELIOSDB_PASSWORD_FILE` — same format.
4. `$HELIOSDB_PASSWORD` — environment value.

`--password` and `--password-file` are `conflicts_with` each other. For
`--auth password|md5|scram-sha-256`, an absent source is a startup error
(`src/main.rs` in the `Start` handler). `--auth trust` needs none.

**Empty values fail closed, never downgrade to trust.** An explicitly supplied
but empty `--password` (or an empty `--password-file` path) is rejected in
`resolve_password` with an error that contains no secret material. Empty
`HELIOSDB_PASSWORD` / `HELIOSDB_PASSWORD_FILE` values are treated as unset —
consistent with each other, and safe because a non-`trust` method with no
source then fails startup rather than starting without auth.

**Non-UTF8 is invalid config, not absence.** Env sources are read with
`var_os` (not `var().ok()`, which silently dropped non-UTF8 bytes). A
*selected* non-UTF8 `HELIOSDB_PASSWORD`, `HELIOSDB_PASSWORD_FILE`, or
`HELIOSDB_MCP_TOKEN` is a startup error; a non-UTF8 `--password-file` path is
refused (`reject_non_utf8_path`) because the daemon forwards the path as a
`String` argv element (`display()` would be lossy). A higher-precedence
explicit flag still suppresses an overridden non-UTF8 env value, so an
unrelated broken env var cannot fail a valid explicit configuration. Unix
regressions: `non_utf8_configured_sources_error_not_absent`,
`overridden_non_utf8_env_does_not_error`.

**Daemon re-exec.** `start --daemon` re-execs the binary
(`start_server_daemon`, `src/main.rs:1790`). The child never receives the
plaintext on its command line:

- a file source forwards only `--password-file <path>`;
- an argv/env source is exported to the child as `HELIOSDB_PASSWORD`
  (`daemon_password_handoff`, `src/main.rs:309`);
- `HELIOSDB_PASSWORD_FILE` is removed from the child environment when an
  explicit value source is authoritative (`src/main.rs:1976`).

The foreground (non-daemon) path consumes the plaintext in-process
(`PasswordSource::into_value`, `src/main.rs:154`). `PasswordSource` has a
manual, non-derived `Debug` (`src/main.rs:168`) that redacts both
`Argv`/`Env` values and a file's contents.

**Docker.** `docker-compose.yml` mounts `secrets/heliosdb_password.txt` as a
Docker secret and sets `HELIOSDB_PASSWORD_FILE`. Compose implements a file
secret as a bind mount, so the host owner/mode carry into the container; the
image is `USER heliosdb` (uid/gid 999, verified in `Dockerfile.binary:43` and
`deployment/docker/Dockerfile:44`). A `0600` file owned by the host login is
therefore unreadable to the server, and the docs now say to
`chown 999:999` + `chmod 600` (or `chmod 644` when ownership cannot change).
The entrypoint validates the file, then forwards `--password-file` (never
`--password`). An explicit `--password` / `--password-file` on the container
command line outranks the inherited `HELIOSDB_PASSWORD_FILE`, so a stale or
missing inherited file does not block a valid explicit flag. A binary that
predates `--password-file` is refused with an upgrade message; the entrypoint
has no `--password` fallback.

## `start`: MCP bearer token

Source: `--mcp-token <value>` or `$HELIOSDB_MCP_TOKEN`, resolved by
`resolve_mcp_token` (`src/main.rs:283`). `--mcp-token` outranks the environment;
an empty `--mcp-token` is a startup error and an empty `$HELIOSDB_MCP_TOKEN` is
treated as unset (`src/main.rs:291`). The token guards MCP-over-HTTP routes
(`run_http_listener`, `src/main.rs:2448`). On daemon re-exec it is handed to
the child through `HELIOSDB_MCP_TOKEN`, not argv (`src/main.rs:1937`).
`--allow-remote-mcp` (a boolean, not a secret) is forwarded on argv.

Status: the token source and daemon handoff are implemented. A literal
`--mcp-token` on the original command line stays visible in that process's
`/proc/<pid>/cmdline` for the process's **entire lifetime**, exactly like
`--password`; prefer the environment variable. No portable in-place argv scrub
is implemented — that best-effort requirement is explicitly **unresolved** in
this track. Whether the token actually *authenticates* depends on the binary
being built with the `mcp-endpoint` feature; without it the token is resolved
and handed off but no MCP routes are mounted, so an argv-only check does not
prove token authentication. The harness exposes `HELIOSDB_PROC_MCP_HTTP=1` to
require an end-to-end `/mcp` 401/200 check in feature-enabled runs.

## HTTP listener policy (combined with the HTTP-auth track)

The HTTP API now shares the PostgreSQL wire credential, so the two listeners can
never disagree about users or passwords. When the wire has any credential
(`--auth password|md5|scram-sha-256`, **or** `--auth trust --password …`, which
protects HTTP while leaving the wire open), every route except the fixed
status-only `/health` and CORS preflight requires HTTP Basic verified against the
same `AuthManager` the wire uses (`src/api/http_auth.rs`, applied in
`run_http_listener`, `src/main.rs`). BaaS JWT callers pass the outer check with
the dedicated `X-HeliosDB-Authorization: Basic …` header and keep
`Authorization: Bearer <app-jwt>` for the handler; the MCP bearer token stays
scoped to `/mcp`. See README "HTTP API authentication and exposure".

`--http-listen` defaults to loopback (`127.0.0.1`; the container entrypoint also
forces it) and never inherits `--listen`. A non-loopback plaintext bind is
refused at startup: **always** for an authenticated bind (Basic credentials would
cross the network in the clear — `--allow-insecure-http` does not cover them),
and for a trust-mode bind unless `--allow-insecure-http` is passed. The listener
never terminates TLS, so a wire `--tls-cert` does not make it HTTPS; requiring
TLS from plaintext clients (`--require-tls` / `hostssl`) remains the deferred
gap #53 and is not implemented. This track contributes the password source below;
the HTTP policy itself is the separate HTTP track's source.

## `start`: TLS private key

Flags: `--tls-cert` / `--tls-key` (PostgreSQL listener) and
`--mysql-tls-cert` / `--mysql-tls-key` (MySQL listener), each pair required
together (`src/main.rs:790`, `src/main.rs:795`). The values are file **paths**
and appear on argv; the private-key **bytes** are read by the server from the
file. Protection is the key file's permissions, not argv hygiene. The Docker
entrypoint, when no cert is supplied, generates a self-signed pair under
`$HELIOSDB_TLS_DIR` with `umask 077`.

## `start`: at-rest encryption key

There is no CLI flag for the encryption key. It is configured in the TOML file
loaded by `--config` / `-c`:

```toml
[encryption]
enabled = true
key_source = { Environment = "HELIOSDB_KEY" }
# or
key_source = { File = "/run/secrets/heliosdb_key" }
# or (not yet implemented)
key_source = { Kms = { provider = "aws", key_id = "arn:..." } }
```

`EncryptionConfig::default` uses `Environment("HELIOSDB_ENCRYPTION_KEY")`
(`src/config.rs:1097`); `KeySource` is defined at `src/config.rs:1203` and
loaded by `KeyManager::load_key_from_source` (`src/crypto/key_manager.rs:54`).
`Environment` reads the variable, `File` reads a 32-byte or 64-hex file, and
`Kms` currently returns "not yet supported". The key never appears on argv.
Zero-knowledge (`[encryption.zke]`) keys are per-request, client-supplied, and
never persisted by the server.

## `dump` / `restore`: connection URI limitation

Both subcommands accept `--connection <string>` (`src/main.rs:649`,
`src/main.rs:681`), passed to `DumpCommand` / `RestoreCommand`. A connection
URI such as `postgres://user:password@host/db` embeds credentials in a single
argv token, so the password is visible to any local user via
`/proc/<pid>/cmdline`. There is no redaction or off-argv form.

Mitigating fact: server mode is not implemented — `DumpCommand` and
`RestoreCommand` reject `--connection` with
"Server mode … not yet implemented" (`src/cli/dump.rs:50`,
`src/cli/restore.rs:67`), so the URI is not used to open a connection today.
If server mode is implemented, it must accept the credential from a file or
environment source rather than a URI on argv, or document this exposure
explicitly.

## Commands with no secret sources

`init`, `repl`, `stop`, `status`, and (feature-gated) `code-graph hook` take
no credentials. `stop` / `status` take only a PID-file path.

## Environment vs argv

Handing a secret to a child through the environment removes it from
`/proc/<pid>/cmdline`, but **not** from the host — the process owner and
privileged users can read `/proc/<pid>/environ`. The P0-2 fix is specifically
an argv-exposure fix; it makes no claim that environment handoff is secret
from the same uid. A literal `--password` / `--mcp-token` on the original
command line is visible in `/proc/<pid>/cmdline` for the **whole process
lifetime**; no portable in-place argv scrub exists here, so that requirement is
explicitly **unresolved**. Migrate to a file or environment source.

## Known gaps

- Environment handoff remains readable via `/proc/<pid>/environ`.
- **No portable in-place argv scrub** for a legacy literal `--password` /
  `--mcp-token`; the value is world-readable in `/proc/<pid>/cmdline` for the
  entire process lifetime. Warning + off-argv alternatives only mitigate it.
- The daemon child re-reads the password **file** named on its command line,
  so deleting or mutating that file between parent resolution and child
  startup can fail or change the credential. An inherited private descriptor
  would remove the window; it is not implemented, and the harness does **not**
  claim to cover this race: its `deleted-before-parent-read` case removes the
  file before the parent reads it and is therefore equivalent to a missing
  file. A real fixture would have to interleave the child's startup read.
- `--connection` connection-URI credentials (above) are on argv if server
  mode is ever wired up.
- Docker file-secret ownership (uid/gid 999) is documented, but no container
  was started to prove the mount is readable; that bind-mount/UID check is
  pending the scheduler (a YAML parse is not runtime coverage).

## Verification

- `sh -n deployment/docker/docker-entrypoint.sh`
- `scripts/test_docker_entrypoint_secret.sh` (static + stub; no build)
- `python3 -c "compile(open('scripts/test_server_secret_argv.py').read(),'x','exec')"` and
  `bash -n`/`sh -n` on the wrapper (static; avoids writing `__pycache__`)
- `python3 scripts/test_secret_argv_mocks.py` (mocked-process tests: negative
  config cases fail on stray child/pidfile; EPERM/changed-identity cleanup is
  recorded; no signals or process launches)
- `HELIOSDB_PROC_REGRESSION=1 HELIOSDB_NANO_BIN=<abs> \
   HELIOSDB_PROC_EVIDENCE_DIR=<abs> scripts/test_server_secret_argv.sh`
  (real daemons; gated, not run by default; exit 3 = requested-but-unavailable).
  The harness requires the child to **own** the SQL/HTTP listener (PID fd
  socket-inode match against `/proc/net/tcp{,6}`), requires an actual
  SQLSTATE-28 authentication rejection for the wrong-password case, and
  re-hashes the binary before/after the suite.
- `cargo test --bin heliosdb-nano` covers the `resolve_password` /
  `resolve_mcp_token` / `daemon_password_handoff` / redacted-`Debug` unit tests
  and the Unix non-UTF8 regressions in `src/main.rs`.
