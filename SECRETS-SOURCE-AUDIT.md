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
| `start` | PG auth password | `--password`, `--password-file`, `HELIOSDB_PASSWORD_FILE`, `HELIOSDB_PASSWORD` | legacy `--password` only until the Linux re-exec below moves it off argv |
| `start` | MCP bearer token | `--mcp-token`, `HELIOSDB_MCP_TOKEN` | literal `--mcp-token` only until the Linux re-exec below moves it off argv |
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

**Foreground re-exec to strip argv (Linux).** A `start` that carries a literal
`--password` (or `--mcp-token`) is re-executed from `/proc/self/exe` with the
safe standard-library `CommandExt::exec` **before** the Tokio runtime, storage,
or any listener is created (`plan_start_reexec` / `preflight_argv_reexec`,
`src/main.rs:430`). The replacement argv is the original argv with exactly the
parsed secret option removed — both the `--flag value` and `--flag=value`
syntaxes, and only an option the parser actually consumed — while PID, stdio,
argv[0], every unrelated argument (including non-UTF8 paths), and credential
precedence are preserved. The value is carried in the child's
`HELIOSDB_PASSWORD` / `HELIOSDB_MCP_TOKEN` environment rather than on argv.
A literal `--password` and an explicit `--password-file` are **mutually
exclusive** at the CLI (Clap rejects the two-flag command) and a literal
`--mcp-token` has no file sibling; the only lower-precedence source that can
accompany a literal `--password` is the inherited `HELIOSDB_PASSWORD_FILE`. When
the literal wins, the plan removes that inherited `HELIOSDB_PASSWORD_FILE` from
the child environment so the environment source cannot outrank the
operator-selected value after the exec. (The planner also strips an explicit
`--password-file` token defensively, but no reachable CLI invocation combines it
with `--password`.) A selected
invalid/empty credential is refused before any `exec`; an `exec` failure is a
sanitized, fail-closed error that opens no listener. Environment/file-only
invocations do not re-exec at all, so the change is inert for the recommended
sources. A re-exec loop marker is unnecessary: the replacement argv has no
literal secret flag, so the re-parse resolves an environment source and plans
no second exec. On non-Linux platforms the safe replacement path is not
implemented; the process keeps the warning-only behavior and the limitation
below.

**Daemon re-exec.** `start --daemon` then spawns the detached worker
(`start_server_daemon`). That child never receives the plaintext on its command
line either:

- a file source forwards only `--password-file <path>`;
- an argv/env source is exported to the child as `HELIOSDB_PASSWORD`
  (`daemon_password_handoff`, `src/main.rs:315`);
- `HELIOSDB_PASSWORD_FILE` is removed from the child environment when an
  explicit value source is authoritative.

The foreground (non-daemon) path consumes the plaintext in-process
(`PasswordSource::into_value`, `src/main.rs:155`). `PasswordSource` has a
manual, non-derived `Debug` (`src/main.rs:169`) that redacts both
`Argv`/`Env` values and a file's contents; `ReexecPlan` has a parallel
redacting `Debug`.

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

Status: the token source and handoff are implemented. On Linux a literal
`--mcp-token` is moved off argv by the same foreground re-exec described above
(and by the daemon handoff), so it is not visible in `/proc/<pid>/cmdline` after
the server starts; the initial launch window, shell history, and a parent
process's argv still saw it, and `/proc/<pid>/environ` remains owner-readable
(the accepted off-argv model). This is not an in-place argv rewrite. On
non-Linux platforms the literal token remains visible for the process's
**entire lifetime** exactly like `--password`; prefer the environment variable.
Whether the token actually *authenticates* depends on the binary
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
never terminates TLS, so a wire `--tls-cert` does not make it HTTPS, and
`--require-tls` does not govern it. `--require-tls` (gap #53) is now implemented
for the **PostgreSQL TCP** listener only: it selects `SslMode::Require` there,
fails startup unless `--tls-cert` and `--tls-key` are both given, and refuses a
client that does not complete a TLS handshake instead of falling back to
plaintext. It does not enable HTTPS; does not require TLS on the MySQL wire
listeners (`--mysql` / `--mysql-socket` keep their independent `--mysql-tls-*`
options); and does not apply to the PostgreSQL Unix-socket listener
(`--pg-socket-dir`), a local authenticated transport that stays plaintext. This
track contributes the password source below; the HTTP policy itself is the
separate HTTP track's source.

## `start`: TLS private key

Flags: `--tls-cert` / `--tls-key` (PostgreSQL listener) and
`--mysql-tls-cert` / `--mysql-tls-key` (MySQL listener), each pair required
together. The values are file **paths** and appear on argv; the private-key
**bytes** are read by the server from the file. Protection is the key file's
permissions, not argv hygiene. The Docker entrypoint, when no cert is supplied,
generates a self-signed pair under `$HELIOSDB_TLS_DIR` with `umask 077`.
`--require-tls` changes only the **PostgreSQL TCP** listener's `SslMode`
(`Prefer` by default, `Require` when set); it does not add HTTPS to the HTTP
API, does not TLS-wrap the PostgreSQL Unix-socket listener, and does not govern
the MySQL wire listeners. The daemon re-exec forwards `--require-tls` and the
explicit post-quantum value through the extracted `daemon_pg_tls_args` helper.

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
from the same uid.

On Linux, a literal `--password` / `--mcp-token` is re-exec'd off the command
line before the server starts (above), so the long-lived foreground process and
the daemon worker no longer advertise it in `/proc/<pid>/cmdline`. This is a
process-image replacement, **not** an in-place argv rewrite; it cannot erase
what was already observed. The initial launch/shell invocation, shell history,
any parent process that saw the original argv, and same-uid/privileged access
to the environment all remain out of scope. On non-Linux platforms the literal
secret still survives for the **whole process lifetime**. Prefer a file or
environment source.

## Known gaps

- Environment handoff remains readable via `/proc/<pid>/environ`.
- **No in-place argv rewrite**: the Linux re-exec removes the literal secret
  from the new process image, but the initial launch window and any parent
  process that observed the original argv are not retroactively scrubbed, and
  non-Linux platforms have no replacement path at all (warning only). This is
  an argv-*exposure* reduction, not a guarantee for the launching context.
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
- `python3 scripts/test_argv_reexec_mocks.py` (mocked-lifecycle tests for the
  Python `ReexecHarness` subclass: failed launcher with a live worker, missing
  pidfile orphan recovery, an exception after a foreground spawn still leaves
  the child registered for cleanup, an identity mismatch is never signalled,
  and a failed wait/reap is reported; no processes launched). These exercise
  the **Python harness**, not the Rust planner; plus
  `python3 -c "compile(open('scripts/test_argv_reexec.py').read(),'x','exec')"`
- `HELIOSDB_PROC_REGRESSION=1 HELIOSDB_NANO_BIN=<abs> \
   HELIOSDB_PROC_EVIDENCE_DIR=<abs> scripts/test_argv_reexec.sh`
  (real foreground/daemon re-exec evidence; gated, not run by default; exit 3 =
  requested-but-unavailable). It proves the foreground PID keeps its identity
  with a secret-free final argv, exact `SELECT 1` auth and wrong-password
  rejection for split/equals password and MCP options, the daemon PID-file
  handoff, and that an invalid credential leaves no residual listener after it
  exits.
- The `scripts/test_server_secret_argv.sh` real-daemon secrets harness is
  unchanged in scope and still covers listener ownership, SQLSTATE-28, and
  binary hash-binding. Required TLS adds its own bounded runtime harness,
  `scripts/test_require_tls_runtime.py` plus
  the mocked companion `scripts/test_require_tls_runtime_harness.py`, with CLI
  and parse coverage in `tests/require_tls_cli_tests.rs`. The harness is bounded
  (per-probe subprocess timeouts, capped log output, and explicit
  socket/`AF_UNIX`-inode and PID-file classification instead of `st_ino`
  equality). **Status: not run** — the live require-TLS gate and the
  `HELIOSDB_PROC_REGRESSION` live gates remain unrun; this audit contributes no
  live evidence for them.
- `cargo test --bin heliosdb-nano` would cover the `resolve_password` /
  `resolve_mcp_token` / `daemon_password_handoff` / redacted-`Debug` unit tests,
  the Unix non-UTF8 regressions, the new `Cli::try_parse_from` secret-source
  tests, and the `plan_start_reexec` / `strip_secret_options` / `exec`-adapter
  tests in `src/main.rs`. **Status: not run in this review round** — no cargo
  build/test or live process was executed; the Rust unit tests remain unrun
  until root gates. The Python mocked tests are not a substitute for them and
  are not claimed as Rust regression evidence.
