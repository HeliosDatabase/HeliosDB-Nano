# Secret-source audit (P0-2, 2026-10-04)

Static audit of every secret source reachable from the `heliosdb-nano`
front door, at commit `e8b0461b` plus the second-pass changes on
`fix/nano-secrets-20261004`. This is a code-inspection inventory, not a
runtime claim: no live daemon was exercised for it (the real-process harness
is `scripts/test_server_secret_argv.sh`, gated behind
`HELIOSDB_PROC_REGRESSION=1`).

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

Precedence, resolved in `resolve_password` (`src/main.rs:180`):

1. `--password <value>` — deprecated; emits a WARN (`src/main.rs:714`).
2. `--password-file <path>` — first line; empty file fails closed
   (`read_password_file`, `src/main.rs:164`).
3. `$HELIOSDB_PASSWORD_FILE` — same format.
4. `$HELIOSDB_PASSWORD` — environment value.

`--password` and `--password-file` are `conflicts_with` each other. For
`--auth password|md5|scram-sha-256`, an absent source is a startup error
(`src/main.rs:727`). `--auth trust` needs none.

**Daemon re-exec.** `start --daemon` re-execs the binary
(`start_server_daemon`, `src/main.rs:1693`). The child never receives the
plaintext on its command line:

- a file source forwards only `--password-file <path>`;
- an argv/env source is exported to the child as `HELIOSDB_PASSWORD`
  (`daemon_password_handoff`, `src/main.rs:212`);
- `HELIOSDB_PASSWORD_FILE` is removed from the child environment when an
  explicit value source is authoritative (`src/main.rs:1875`).

The foreground (non-daemon) path consumes the plaintext in-process
(`PasswordSource::into_value`, `src/main.rs:153`).

**Docker.** `docker-compose.yml` mounts `secrets/heliosdb_password.txt` as a
Docker secret and sets `HELIOSDB_PASSWORD_FILE`. The entrypoint validates the
file, then forwards `--password-file` (never `--password`). A binary that
predates `--password-file` is refused with an upgrade message; the entrypoint
has no `--password` fallback.

## `start`: MCP bearer token

Source: `--mcp-token <value>` or `$HELIOSDB_MCP_TOKEN` (read when the flag is
absent, `src/main.rs:737`). The token guards MCP-over-HTTP routes
(`run_http_listener`, `src/main.rs:2351`). On daemon re-exec it is handed to
the child through `HELIOSDB_MCP_TOKEN`, not argv (`src/main.rs:1882`).
`--allow-remote-mcp` (a boolean, not a secret) is forwarded on argv.

Status: the token source and daemon handoff are implemented. A literal
`--mcp-token` on the original command line is still visible in that process's
argv at launch time (same portable limitation as `--password`); prefer the
environment variable.

## `start`: TLS private key

Flags: `--tls-cert` / `--tls-key` (PostgreSQL listener) and
`--mysql-tls-cert` / `--mysql-tls-key` (MySQL listener), each pair required
together (`src/main.rs:691`, `src/main.rs:696`). The values are file **paths**
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

Both subcommands accept `--connection <string>` (`src/main.rs:552`,
`src/main.rs:584`), passed to `DumpCommand` / `RestoreCommand`. A connection
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
from the same uid. Launch-time argv exposure of a literal `--password` /
`--mcp-token` cannot be removed portably once the shell has already started
the process; migrate to a file or environment source.

## Known gaps

- Environment handoff remains readable via `/proc/<pid>/environ`.
- The daemon child re-reads the password **file** named on its command line,
  so deleting or mutating that file between parent resolution and child
  startup can fail or change the credential. An inherited private descriptor
  would remove the window; it is not implemented.
- `--connection` connection-URI credentials (above) are on argv if server
  mode is ever wired up.
- Literal `--password` / `--mcp-token` on the original command line are
  inherently exposed; only the deprecation warning and off-argv alternatives
  mitigate this.

## Verification

- `sh -n deployment/docker/docker-entrypoint.sh`
- `scripts/test_docker_entrypoint_secret.sh` (static + stub; no build)
- `HELIOSDB_PROC_REGRESSION=1 scripts/test_server_secret_argv.sh` (real
  daemon; gated, not run by default)
- `cargo test --bin heliosdb-nano` covers the `resolve_password` /
  `daemon_password_handoff` unit tests in `src/main.rs`.
