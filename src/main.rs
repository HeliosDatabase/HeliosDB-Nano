//! `HeliosDB` Lite server binary

#![allow(
    clippy::uninlined_format_args,
    clippy::unused_async,
    clippy::too_many_lines,
    clippy::single_match_else,
    clippy::match_wildcard_for_single_variants,
    clippy::manual_string_new
)]

use clap::{Parser, Subcommand};
use heliosdb_nano::{Config, EmbeddedDatabase, Error, Result};
use std::ffi::OsString;
use std::path::PathBuf;
use tracing::info;

/// HA Replication Configuration
#[derive(Debug, Clone)]
struct HAConfig {
    role: String,
    replication_port: u16,
    primary_host: Option<String>,
    standby_hosts: Option<String>,
    observer_hosts: Option<String>,
    sync_mode: String,
    http_port: u16,
    http_listen: Option<String>,
    mcp_token: Option<String>,
    allow_remote_mcp: bool,
    allow_insecure_http: bool,
    node_id: Option<String>,
}

/// GH#28: CLI overrides for the `[server]` connection-lifetime keys. Every
/// field is an `Option` so an ABSENT flag never clobbers a config-file value;
/// the precedence is CLI > config file > default, resolved once in
/// `start_server` and shared by every listener.
struct ConnectionPolicyArgs {
    max_connections: Option<usize>,
    authentication_timeout: Option<String>,
    idle_session_timeout: Option<String>,
    idle_in_transaction_session_timeout: Option<String>,
    tcp_keepalives_idle: Option<String>,
    tcp_keepalives_interval: Option<String>,
    tcp_keepalives_count: Option<u32>,
    max_connections_warn_percent: Option<u8>,
}

impl ConnectionPolicyArgs {
    /// Overlay the flags that were given onto the loaded `[server]` section.
    fn apply_to(&self, server: &mut heliosdb_nano::config::ServerConfig) {
        if let Some(v) = self.max_connections {
            server.max_connections = v;
        }
        if let Some(v) = &self.authentication_timeout {
            server.authentication_timeout = v.clone();
        }
        if let Some(v) = &self.idle_session_timeout {
            server.idle_session_timeout = v.clone();
        }
        if let Some(v) = &self.idle_in_transaction_session_timeout {
            server.idle_in_transaction_session_timeout = v.clone();
        }
        if let Some(v) = &self.tcp_keepalives_idle {
            server.tcp_keepalives_idle = v.clone();
        }
        if let Some(v) = &self.tcp_keepalives_interval {
            server.tcp_keepalives_interval = v.clone();
        }
        if let Some(v) = self.tcp_keepalives_count {
            server.tcp_keepalives_count = v;
        }
        if let Some(v) = self.max_connections_warn_percent {
            server.max_connections_warn_percent = v;
        }
    }

    /// Forward ONLY the flags that were given to the daemon re-exec, so the
    /// child resolves CLI > config file > default exactly like the parent.
    fn push_cli_args(&self, args: &mut Vec<String>) {
        let mut push = |flag: &str, value: Option<String>| {
            if let Some(v) = value {
                args.push(flag.to_string());
                args.push(v);
            }
        };
        push("--max-connections", self.max_connections.map(|v| v.to_string()));
        push("--authentication-timeout", self.authentication_timeout.clone());
        push("--idle-session-timeout", self.idle_session_timeout.clone());
        push(
            "--idle-in-transaction-session-timeout",
            self.idle_in_transaction_session_timeout.clone(),
        );
        push("--tcp-keepalives-idle", self.tcp_keepalives_idle.clone());
        push("--tcp-keepalives-interval", self.tcp_keepalives_interval.clone());
        push(
            "--tcp-keepalives-count",
            self.tcp_keepalives_count.map(|v| v.to_string()),
        );
        push(
            "--max-connections-warn-percent",
            self.max_connections_warn_percent.map(|v| v.to_string()),
        );
    }
}

/// GH#29 (c7, M3): CLI overrides for the `[performance]` keys that bound a
/// query's memory. `None` means "not given", so the config file's value (and
/// then the built-in default) stands — CLI > config file > default, the same
/// precedence as [`ConnectionPolicyArgs`].
struct PerformanceArgs {
    join_memory_limit_mb: Option<usize>,
}

impl PerformanceArgs {
    /// Overlay the flags that were given onto the loaded `[performance]` section.
    fn apply_to(&self, performance: &mut heliosdb_nano::config::PerformanceConfig) {
        if let Some(v) = self.join_memory_limit_mb {
            performance.join_memory_limit_mb = v;
        }
    }

    /// Forward ONLY the flags that were given to the daemon re-exec.
    fn push_cli_args(&self, args: &mut Vec<String>) {
        if let Some(v) = self.join_memory_limit_mb {
            args.push("--join-memory-limit-mb".to_string());
            args.push(v.to_string());
        }
    }
}

/// Where the server password came from. The plaintext is secret; a file
/// *path* is not. This distinction lets the daemon re-exec hand the secret to
/// the child without ever placing it on the child's command line.
#[derive(Clone, PartialEq, Eq)]
enum PasswordSource {
    /// No password configured (`--auth trust`).
    Absent,
    /// Legacy `--password <value>` on the command line.
    Argv(String),
    /// `--password-file <path>` or `$HELIOSDB_PASSWORD_FILE`. `value` is the
    /// resolved first line; `path` is what the daemon child receives.
    File { path: PathBuf, value: String },
    /// `$HELIOSDB_PASSWORD`.
    Env(String),
}

impl PasswordSource {
    fn is_absent(&self) -> bool {
        matches!(self, PasswordSource::Absent)
    }

    /// Consume the source and yield the plaintext for the foreground server.
    fn into_value(self) -> Option<String> {
        match self {
            PasswordSource::Absent => None,
            PasswordSource::Argv(v) | PasswordSource::Env(v) => Some(v),
            PasswordSource::File { value, .. } => Some(value),
        }
    }
}

/// Manual (never derived) `Debug`: this type holds plaintext secrets, so a
/// derived formatter would print them into any `assert_eq!` failure, log line,
/// or panic backtrace. `Argv`/`Env` values and a file's *contents* are always
/// redacted; only the non-secret source shape and a file's path are shown —
/// matching the module's own rule that "a file *path* is not [secret]".
impl std::fmt::Debug for PasswordSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasswordSource::Absent => f.write_str("PasswordSource::Absent"),
            PasswordSource::Argv(_) => f.write_str("PasswordSource::Argv(<redacted>)"),
            PasswordSource::Env(_) => f.write_str("PasswordSource::Env(<redacted>)"),
            PasswordSource::File { path, .. } => {
                write!(f, "PasswordSource::File {{ path: {path:?}, value: <redacted> }}")
            }
        }
    }
}

/// Read a single-line password file: the first line, trailing newline ignored.
/// An empty file is a configuration error, not an empty password — fail closed.
fn read_password_file(path: &std::path::Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::config(format!("Cannot read --password-file {}: {e}", path.display())))?;
    let first = raw.lines().next().unwrap_or("").to_string();
    if first.is_empty() {
        return Err(Error::config(format!("--password-file {} is empty.", path.display())));
    }
    Ok(first)
}

/// Reject a non-UTF8 secret-file path before it is used or forwarded. The
/// daemon re-exec hands the path to the child as a `String` argv element, and
/// the rest of this module formats paths with `display()`. Refusing up front
/// keeps forwarding lossless and, just as importantly, keeps *invalid
/// configuration* distinct from *no configuration*: silently dropping a
/// configured source would let a broken secret fall through to `trust`.
fn reject_non_utf8_path(path: &std::path::Path, source: &str) -> Result<()> {
    if path.to_str().is_none() {
        return Err(Error::config(format!(
            "{source} names a path that is not valid UTF-8; use a UTF-8 path."
        )));
    }
    Ok(())
}

/// Resolve the password from every supported source, in precedence order:
/// `--password` > `--password-file` > `$HELIOSDB_PASSWORD_FILE` >
/// `$HELIOSDB_PASSWORD`. Pure (the environment is passed in as `OsString` so a
/// non-UTF8 value is *seen*, not dropped) so the precedence table can be
/// unit-tested.
///
/// Sources are resolved even for `--auth trust`, but that does **not** make the
/// SQL listener authenticate: under `trust` the wire stays trust, and this
/// module does not seed a wire `AuthManager` from the value. The resolved
/// source is retained only so the separate HTTP/auth track can tell an
/// explicitly configured credential from an entirely passwordless server
/// (a clear cross-track integration dependency, not implemented here).
///
/// Absence is distinct from invalid configuration: an empty env value is
/// "unset", but a *selected* non-UTF8 path/value is an error, never a fallback.
fn resolve_password(
    password: Option<String>,
    password_file: Option<PathBuf>,
    env_password_file: Option<OsString>,
    env_password: Option<OsString>,
) -> Result<PasswordSource> {
    match (password, password_file) {
        (Some(v), _) => {
            if v.is_empty() {
                // An explicit but empty --password is a configuration mistake,
                // not a passwordless server: refuse it instead of letting the
                // empty value survive into the daemon env and be filtered away,
                // silently turning a configured credential into trust.
                return Err(Error::config(
                    "The --password value is empty. Pass a non-empty password, or omit \
                     --password and use --password-file / HELIOSDB_PASSWORD_FILE / \
                     HELIOSDB_PASSWORD (or --auth trust for a passwordless server).",
                ));
            }
            Ok(PasswordSource::Argv(v))
        }
        (None, Some(path)) => {
            if path.as_os_str().is_empty() {
                return Err(Error::config(
                    "--password-file was given an empty path; provide the path to a \
                     readable, non-empty secret file.",
                ));
            }
            reject_non_utf8_path(&path, "--password-file")?;
            let value = read_password_file(&path)?;
            Ok(PasswordSource::File { path, value })
        }
        (None, None) => {
            if let Some(raw) = env_password_file {
                let path = PathBuf::from(raw);
                // Set-but-empty means "unset" (matching HELIOSDB_PASSWORD), so
                // fall through to the value source rather than reading "".
                if !path.as_os_str().is_empty() {
                    reject_non_utf8_path(&path, "HELIOSDB_PASSWORD_FILE")?;
                    let value = read_password_file(&path)?;
                    return Ok(PasswordSource::File { path, value });
                }
            }
            match env_password {
                Some(raw) if raw.is_empty() => Ok(PasswordSource::Absent),
                Some(raw) => raw.into_string().map(PasswordSource::Env).map_err(|_| {
                    Error::config(
                        "HELIOSDB_PASSWORD is not valid UTF-8; set a UTF-8 password or use \
                         --password-file / HELIOSDB_PASSWORD_FILE.",
                    )
                }),
                None => Ok(PasswordSource::Absent),
            }
        }
    }
}

/// Resolve the MCP-over-HTTP bearer token, mirroring [`resolve_password`]:
/// `--mcp-token` outranks `$HELIOSDB_MCP_TOKEN`, and neither is a file source.
///
/// An explicitly supplied but **empty** `--mcp-token` is a configuration error:
/// dropping it would leave a listener that the operator believes is
/// authenticated serving unauthenticated MCP. An empty `$HELIOSDB_MCP_TOKEN` is
/// treated as unset, matching the empty-`HELIOSDB_PASSWORD` semantics. A
/// *selected* non-UTF8 token is an error, not absence.
fn resolve_mcp_token(cli: Option<String>, env: Option<OsString>) -> Result<Option<String>> {
    match cli {
        Some(token) if token.is_empty() => Err(Error::config(
            "The --mcp-token value is empty. Pass a non-empty bearer token or omit \
             --mcp-token; an empty token would leave the MCP routes unauthenticated.",
        )),
        Some(token) => Ok(Some(token)),
        None => match env {
            Some(raw) if raw.is_empty() => Ok(None),
            Some(raw) => raw.into_string().map(Some).map_err(|_| {
                Error::config("HELIOSDB_MCP_TOKEN is not valid UTF-8; set a UTF-8 bearer token or pass --mcp-token.")
            }),
            None => Ok(None),
        },
    }
}

/// Append the daemon re-exec's password handoff to `args` and return a value
/// the caller must place in the CHILD'S environment (never its argv).
///
/// * A file source forwards only the (non-secret) path: `--password-file`.
/// * An argv/env source is returned for `Command::env("HELIOSDB_PASSWORD", …)`.
/// * The plaintext is never pushed onto `args`.
///
/// `path.display()` is only reached for a path that `resolve_password` already
/// proved UTF-8 (`reject_non_utf8_path`), so it is lossless here.
fn daemon_password_handoff(source: &PasswordSource, args: &mut Vec<String>) -> Option<String> {
    match source {
        PasswordSource::Absent => None,
        PasswordSource::File { path, .. } => {
            args.push("--password-file".to_string());
            args.push(path.display().to_string());
            None
        }
        PasswordSource::Argv(v) | PasswordSource::Env(v) => Some(v.clone()),
    }
}

/// A bounded plan for replacing the current process image so that no literal
/// credential survives on the new command line. Built by [`plan_start_reexec`]
/// from already-parsed arguments, so the planner never reads or mutates the
/// process-wide environment and can be unit-tested in isolation.
enum ReexecPlan {
    /// No literal secret flag was present; run in place (file/env sources only).
    None,
    /// Re-exec is required. `argv[0]` is retained verbatim; `env_set` carries
    /// the off-argv credential for the child and `env_unset` removes a
    /// lower-precedence source that the literal outranked.
    Exec {
        argv: Vec<OsString>,
        env_set: Vec<(OsString, OsString)>,
        env_unset: Vec<OsString>,
    },
}

/// Never derived: `env_set` holds plaintext credentials. The argv itself is
/// already secret-free, but it is still not worth printing, and every `env_set`
/// value is redacted.
impl std::fmt::Debug for ReexecPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReexecPlan::None => f.write_str("ReexecPlan::None"),
            ReexecPlan::Exec { argv, env_unset, .. } => write!(
                f,
                "ReexecPlan::Exec {{ argv_len: {}, env_set: <redacted>, env_unset: {env_unset:?} }}",
                argv.len()
            ),
        }
    }
}

/// Drop exactly the parsed secret options from a raw argv, leaving every other
/// argument — its order and non-UTF8 bytes — untouched.
///
/// The caller only reaches this after Clap has accepted the command, which
/// bounds the scanner: a split option is guaranteed a value token, and neither
/// `--password` nor `--mcp-token` is ever the value of an unrelated option
/// (none of the neighboring options accepts hyphen-leading values). Both the
/// `--flag value` and `--flag=value` syntaxes are handled. Everything after a
/// bare `--` is positional and is never touched, and a string merely
/// *containing* `--password` (an unrelated value) is preserved.
fn strip_secret_options(
    raw: &[OsString],
    remove_password: bool,
    remove_password_file: bool,
    remove_mcp_token: bool,
) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::with_capacity(raw.len());
    let Some((arg0, rest)) = raw.split_first() else {
        return out;
    };
    out.push(arg0.clone());

    let mut end_of_options = false;
    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        let text = token.to_str();
        if !end_of_options {
            if text == Some("--") {
                end_of_options = true;
                out.push(token.clone());
                i += 1;
                continue;
            }
            let consumes_value = |flag: &str, remove: bool| remove && text == Some(flag);
            if consumes_value("--password", remove_password)
                || consumes_value("--password-file", remove_password_file)
                || consumes_value("--mcp-token", remove_mcp_token)
            {
                // Skip the flag *and* its value.
                i += 2;
                continue;
            }
            let is_inline = |flag: &str, remove: bool| remove && text.map_or(false, |t| t.starts_with(flag));
            if is_inline("--password=", remove_password)
                || is_inline("--password-file=", remove_password_file)
                || is_inline("--mcp-token=", remove_mcp_token)
            {
                // `--flag=value` is a single token.
                i += 1;
                continue;
            }
        }
        out.push(token.clone());
        i += 1;
    }
    out
}

/// Decide whether a `start` invocation must be re-executed to move a literal
/// credential off the command line, and build the exact child argv/env if so.
///
/// Pure: every environment source is passed in, so precedence is testable
/// without touching the real environment. An invalid or empty *selected*
/// credential is returned as an error here — before any `exec`.
///
/// Precedence note: a literal `--password` always wins, so the plan both sets
/// `HELIOSDB_PASSWORD` and drops the `--password-file` flag from argv and
/// `HELIOSDB_PASSWORD_FILE` from the environment; otherwise the file source
/// would outrank the value the operator actually selected.
fn plan_start_reexec(
    raw_args: Vec<OsString>,
    password: Option<String>,
    password_file: Option<PathBuf>,
    mcp_token: Option<String>,
    env_password_file: Option<OsString>,
    env_password: Option<OsString>,
    env_mcp_token: Option<OsString>,
) -> Result<ReexecPlan> {
    let literal_password = password.is_some();
    let literal_mcp_token = mcp_token.is_some();
    if !literal_password && !literal_mcp_token {
        // File/env sources already keep the secret out of argv; do not exec.
        return Ok(ReexecPlan::None);
    }

    // Validate and select exactly as the in-process resolver will, so an empty
    // or non-UTF8 selected source fails closed here rather than after exec.
    let resolved_password = resolve_password(
        password.clone(),
        password_file.clone(),
        env_password_file.clone(),
        env_password.clone(),
    )?;
    let resolved_mcp_token = resolve_mcp_token(mcp_token.clone(), env_mcp_token.clone())?;

    let mut env_set: Vec<(OsString, OsString)> = Vec::new();
    let mut env_unset: Vec<OsString> = Vec::new();

    if literal_password {
        // `resolve_password(Some(_), ..)` only ever yields `Argv`; the guard is
        // defensive, not a real branch.
        if let PasswordSource::Argv(value) = &resolved_password {
            env_set.push((OsString::from("HELIOSDB_PASSWORD"), OsString::from(value.clone())));
            if env_password_file.is_some() {
                env_unset.push(OsString::from("HELIOSDB_PASSWORD_FILE"));
            }
        }
    }
    if literal_mcp_token {
        if let Some(token) = &resolved_mcp_token {
            env_set.push((OsString::from("HELIOSDB_MCP_TOKEN"), OsString::from(token.clone())));
        }
    }

    // Remove the literal flags in both syntaxes. When the literal password
    // wins, `--password-file` is removed too so it cannot outrank the new env.
    let argv = strip_secret_options(&raw_args, literal_password, literal_password, literal_mcp_token);

    Ok(ReexecPlan::Exec {
        argv,
        env_set,
        env_unset,
    })
}

/// Replace the current process image from a plan. Split out so the failure path
/// can be unit-tested with a fake adapter, without a real `exec` or any
/// process-wide environment mutation.
trait ExecAdapter {
    fn exec(&self, plan: &ReexecPlan) -> Result<()>;
}

/// The real adapter: safe std `CommandExt::exec` on Linux, warning-only on
/// other platforms.
struct RealExec;

impl ExecAdapter for RealExec {
    fn exec(&self, plan: &ReexecPlan) -> Result<()> {
        match plan {
            ReexecPlan::None => Ok(()),
            ReexecPlan::Exec {
                argv,
                env_set,
                env_unset,
            } => exec_reexec(argv, env_set, env_unset),
        }
    }
}

/// Run the process replacement through `adapter`, propagating its (sanitized)
/// failure. On success this never returns.
fn run_reexec<E: ExecAdapter>(plan: &ReexecPlan, adapter: &E) -> Result<()> {
    adapter.exec(plan)
}

/// Sanitized, fail-closed error for a failed `exec`. It must never contain the
/// command, its arguments, or the environment.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn reexec_failure(error: &std::io::Error) -> Error {
    let detail = match error.raw_os_error() {
        Some(code) => format!("os error {code}"),
        None => format!("{:?}", error.kind()),
    };
    Error::config(format!(
        "refusing to start: could not re-execute the server without command-line secrets \
         ({detail}); no listener was opened"
    ))
}

#[cfg(target_os = "linux")]
fn exec_reexec(argv: &[OsString], env_set: &[(OsString, OsString)], env_unset: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    // `/proc/self/exe` resolves to the running binary even if its on-disk path
    // was replaced; `arg0` keeps the original program name. stdio is inherited
    // (Command's default), so the replacement keeps the same terminal/pipes.
    let mut command = std::process::Command::new("/proc/self/exe");
    let mut args = argv.iter();
    if let Some(arg0) = args.next() {
        command.arg0(arg0);
    }
    command.args(args);
    for (key, value) in env_set {
        command.env(key, value);
    }
    for key in env_unset {
        command.env_remove(key);
    }

    // Replaces the process image on success; returns only on failure.
    let error = command.exec();
    Err(reexec_failure(&error))
}

#[cfg(not(target_os = "linux"))]
fn exec_reexec(_argv: &[OsString], _env_set: &[(OsString, OsString)], _env_unset: &[OsString]) -> Result<()> {
    // The safe std process-replacement path used here is Linux-specific, so
    // there is nothing to execute and no re-exec occurred. The literal secret
    // stays on argv for the whole process lifetime; `preflight_argv_reexec`
    // already emitted the accurate, platform-specific warning. Never claim a
    // re-exec happened.
    Ok(())
}

/// One warning per literal secret option, scoped to whether this platform can
/// actually move it off argv. Neither branch ever prints the secret itself.
#[cfg(target_os = "linux")]
fn warn_literal_secret_options(password: bool, mcp_token: bool) {
    if password {
        tracing::warn!(
            "--password exposes the secret in the process table (visible to every local user via \
             `ps`) at launch; this process is re-executed with the secret moved off argv before \
             it serves. Prefer --password-file, HELIOSDB_PASSWORD_FILE or HELIOSDB_PASSWORD"
        );
    }
    if mcp_token {
        tracing::warn!(
            "--mcp-token exposes the secret in the process table (visible to every local user via \
             `ps`) at launch; this process is re-executed with the secret moved off argv before \
             it serves. Prefer HELIOSDB_MCP_TOKEN"
        );
    }
}

/// One warning per literal secret option. On non-Linux the re-exec cannot run,
/// so the warning must not claim it did.
#[cfg(not(target_os = "linux"))]
fn warn_literal_secret_options(password: bool, mcp_token: bool) {
    if password {
        tracing::warn!(
            "--password exposes the secret in the process table (visible to every local user via \
             `ps`); moving a literal command-line secret off argv by re-exec is not implemented \
             on this platform, so it remains visible for the whole process lifetime. Prefer \
             --password-file, HELIOSDB_PASSWORD_FILE or HELIOSDB_PASSWORD"
        );
    }
    if mcp_token {
        tracing::warn!(
            "--mcp-token exposes the secret in the process table (visible to every local user via \
             `ps`); moving a literal command-line secret off argv by re-exec is not implemented \
             on this platform, so it remains visible for the whole process lifetime. Prefer \
             HELIOSDB_MCP_TOKEN"
        );
    }
}

/// Synchronous pre-runtime preflight: if the parsed command is a `start` with a
/// literal `--password` / `--mcp-token`, replace the process with a secret-free
/// argv before any Tokio runtime or storage is created.
fn preflight_argv_reexec(cli: &Cli) -> Result<()> {
    let Commands::Start {
        password,
        password_file,
        mcp_token,
        ..
    } = &cli.command
    else {
        return Ok(());
    };

    let plan = plan_start_reexec(
        std::env::args_os().collect(),
        password.clone(),
        password_file.clone(),
        mcp_token.clone(),
        std::env::var_os("HELIOSDB_PASSWORD_FILE"),
        std::env::var_os("HELIOSDB_PASSWORD"),
        std::env::var_os("HELIOSDB_MCP_TOKEN"),
    )?;

    // Warn only for an actual re-exec plan (never for a file/env-only
    // invocation, and never after a fail-closed validation error). The text is
    // platform-specific so it never claims a re-exec on a platform that cannot
    // perform one.
    if let ReexecPlan::Exec { .. } = &plan {
        warn_literal_secret_options(password.is_some(), mcp_token.is_some());
    }
    run_reexec(&plan, &RealExec)
}

/// GH#28: one WARN per crossing of `[server] max_connections_warn_percent`
/// on a listener (edge-triggered via `swap`; re-arms when utilisation drops
/// back below the threshold). `Semaphore::available_permits()` is the single
/// source of truth — no side counter.
fn warn_listener_utilisation(
    listener: &str,
    warned: &std::sync::atomic::AtomicBool,
    limiter: &tokio::sync::Semaphore,
    max: usize,
    policy: &heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts,
) {
    // Shared with `PgServer::maybe_warn_utilisation` /
    // `MysqlServer::maybe_warn_utilisation` — see
    // `ConnectionTimeouts::maybe_warn_utilisation`.
    policy.maybe_warn_utilisation(listener, warned, limiter, max);
}

/// GH#28: render a policy duration for the startup banner (`0` = disabled).
fn banner_duration(d: std::time::Duration) -> String {
    if d.is_zero() {
        "disabled".to_string()
    } else {
        heliosdb_nano::protocol::postgres::timeouts::format_guc_duration_ms(d.as_millis() as u64)
    }
}

#[derive(Parser)]
#[command(name = "heliosdb-nano")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "PostgreSQL & MySQL compatible database with vector search and encryption", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Start the database server
    Start {
        /// Data directory (required unless --memory is used)
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Use in-memory mode
        #[arg(short, long)]
        memory: bool,

        /// Port to listen on
        #[arg(short, long, default_value = "5432")]
        port: u16,

        /// Listen address
        #[arg(long, default_value = "127.0.0.1")]
        listen: String,

        /// Config file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Run server in background (daemon mode)
        #[arg(long)]
        daemon: bool,

        /// PID file location (only used with --daemon)
        #[arg(long, default_value = "./heliosdb.pid")]
        pid_file: PathBuf,

        /// Dump on shutdown (only for --memory mode)
        #[arg(long)]
        dump_on_shutdown: bool,

        /// Dump schedule (cron syntax, e.g., "0 */6 * * *")
        #[arg(long)]
        dump_schedule: Option<String>,

        /// TLS certificate file path (PEM format)
        #[arg(long)]
        tls_cert: Option<PathBuf>,

        /// TLS private key file path (PEM format)
        #[arg(long)]
        tls_key: Option<PathBuf>,

        /// Require TLS on the PostgreSQL TCP listener. Startup fails unless
        /// both --tls-cert and --tls-key are given; a client that does not
        /// complete a TLS handshake is refused instead of falling back to
        /// plaintext. PostgreSQL TCP only: it does not enable HTTPS, require
        /// MySQL TLS, or apply to the PostgreSQL Unix-socket listener (an
        /// authenticated local transport that stays plaintext). Default: off
        /// (TLS offered, plaintext accepted).
        #[arg(long)]
        require_tls: bool,

        /// Offer the X25519MLKEM768 hybrid post-quantum key-exchange group
        /// (draft-ietf-tls-ecdhe-mlkem) alongside classical groups on the
        /// PostgreSQL TLS listener, in addition to a classical fallback for
        /// clients that don't support it. Only meaningful with --tls-cert /
        /// --tls-key. Default: on. Accepts a bare flag or an explicit
        /// true/false value (`--tls-post-quantum[=true|false]`).
        #[arg(
            long,
            action = clap::ArgAction::Set,
            num_args = 0..=1,
            default_missing_value = "true",
            default_value = "true"
        )]
        tls_post_quantum: bool,

        /// Authentication method: trust, password, md5, scram-sha-256
        #[arg(long, default_value = "trust")]
        auth: String,

        /// Password for authentication (required for password/md5/scram-sha-256 auth).
        /// DEPRECATED: a command-line value is visible to every local user in
        /// the process table. Prefer --password-file, the HELIOSDB_PASSWORD_FILE
        /// environment variable, or HELIOSDB_PASSWORD — read when no flag is
        /// given. A warning is printed when this flag is used.
        #[arg(long)]
        password: Option<String>,

        /// Read the authentication password from this file (first line; the
        /// trailing newline is ignored). Keeps the password out of the process
        /// table — use it with Docker/Kubernetes secrets. May also be supplied
        /// through the HELIOSDB_PASSWORD_FILE environment variable.
        #[arg(long, conflicts_with = "password")]
        password_file: Option<PathBuf>,

        // ========== HA Replication Options ==========
        /// Replication role: standalone, primary, standby, observer
        #[arg(long, default_value = "standalone")]
        replication_role: String,

        /// Replication port for WAL streaming (default: 5433)
        #[arg(long, default_value = "5433")]
        replication_port: u16,

        /// Primary host for standbys to connect to (host:port)
        #[arg(long)]
        primary_host: Option<String>,

        /// Standby hosts for primary to track (comma-separated host:port)
        #[arg(long)]
        standby_hosts: Option<String>,

        /// Observer hosts for split-brain protection (comma-separated host:port)
        #[arg(long)]
        observer_hosts: Option<String>,

        /// Sync mode: async, semi-sync, sync
        #[arg(long, default_value = "async")]
        sync_mode: String,

        /// HTTP API port for health checks and MCP-over-HTTP (default: 8080, 0 disables HTTP)
        #[arg(long, default_value = "8080")]
        http_port: u16,

        /// HTTP API listen address. Defaults to loopback; use an explicit non-loopback
        /// address only with the applicable authentication/TLS policy.
        #[arg(long)]
        http_listen: Option<String>,

        /// Bearer token required for MCP-over-HTTP routes. Requires the
        /// mcp-endpoint feature. Read from $HELIOSDB_MCP_TOKEN when omitted.
        /// A command-line value is visible to every local user in the process
        /// table; prefer the environment variable.
        #[arg(long)]
        mcp_token: Option<String>,

        /// Mount MCP-over-HTTP routes on a non-loopback HTTP listener without MCP auth. Requires mcp-endpoint; unsafe.
        #[arg(long)]
        allow_remote_mcp: bool,

        /// Permit a non-loopback plaintext HTTP API bind only for an unauthenticated
        /// trust-mode listener. Authenticated HTTP remains refused over plaintext,
        /// even with this override, because Basic credentials would cross the network
        /// in the clear.
        #[arg(long)]
        allow_insecure_http: bool,

        /// Node ID (UUID) - auto-generated if not provided
        #[arg(long)]
        node_id: Option<String>,

        // ========== MySQL Protocol Options ==========
        /// Enable MySQL protocol listener
        #[arg(long)]
        mysql: bool,

        /// MySQL listen address (default: 127.0.0.1:3306, localhost-only for security)
        #[arg(long, default_value = "127.0.0.1:3306")]
        mysql_listen: String,

        /// MySQL Unix domain socket path (e.g. /tmp/heliosdb-mysql.sock).
        /// Enables local-only connections for PHP mysqli / WordPress embedded mode.
        #[arg(long)]
        mysql_socket: Option<PathBuf>,

        /// TLS certificate file path (PEM format) for the MySQL TCP listener.
        /// Both --mysql-tls-cert and --mysql-tls-key must be given together.
        /// TLS is offered (CLIENT_SSL), not required — plaintext MySQL
        /// clients still connect. Not used for the MySQL Unix-socket listener.
        #[arg(long)]
        mysql_tls_cert: Option<PathBuf>,

        /// TLS private key file path (PEM format) for the MySQL TCP listener.
        #[arg(long)]
        mysql_tls_key: Option<PathBuf>,

        /// Offer the X25519MLKEM768 hybrid post-quantum key-exchange group on
        /// the MySQL TLS listener, same as --tls-post-quantum for PostgreSQL.
        /// Only meaningful with --mysql-tls-cert / --mysql-tls-key. Default: on.
        /// Accepts a bare flag or an explicit true/false value
        /// (`--mysql-tls-post-quantum[=true|false]`).
        #[arg(
            long,
            action = clap::ArgAction::Set,
            num_args = 0..=1,
            default_missing_value = "true",
            default_value = "true"
        )]
        mysql_tls_post_quantum: bool,

        /// PostgreSQL Unix domain socket directory. If set, listens at
        /// `<dir>/.s.PGSQL.<port>` — the libpq default. Use with psql `-h /tmp`.
        #[arg(long)]
        pg_socket_dir: Option<PathBuf>,

        /// Maximum concurrent client connections PER LISTENER (the PostgreSQL
        /// TCP, PostgreSQL Unix-socket, MySQL TCP and MySQL Unix-socket
        /// listeners each get their own limit of this size). Default 100
        /// (`[server] max_connections`; this flag wins over the config file).
        /// When the limit is reached the server ACCEPTS the connection and
        /// closes it immediately without a PostgreSQL error packet, logging
        /// `Connection limit reached (N), rejecting <addr>` at WARN (the
        /// Unix-socket loops name the listener instead of an address). Raise it
        /// for fleets that pool many connections; `SHOW max_connections`
        /// reports the effective value.
        #[arg(long)]
        max_connections: Option<usize>,

        /// Memory a single join may materialize before the query is refused,
        /// in megabytes: the hash join's build side AND the right input of
        /// EVERY nested-loop join — RIGHT/FULL with a residual ON term,
        /// LATERAL, theta (`ON a.x > b.y`), and any ON no key term binds
        /// (`[performance] join_memory_limit_mb`; this flag wins
        /// over the config file, and the `HELIOSDB_HASH_JOIN_MEM_MB`
        /// environment variable wins over both). Default 1024. `0` = use the
        /// default. Exceeding it fails the query with a clear error instead
        /// of growing until the host runs out of memory. It does NOT cover the
        /// index nested loop's own result buffer, which is still uncapped.
        #[arg(long, value_name = "MB")]
        join_memory_limit_mb: Option<usize>,

        // ========== Connection-lifetime options (GH#28) ==========
        /// PostgreSQL `authentication_timeout`: bound on the WHOLE client
        /// handshake (startup packet, TLS, password / SCRAM). A socket that
        /// never completes authentication — an internet scanner — is closed
        /// and its connection slot released. PostgreSQL duration syntax; a
        /// bare integer is seconds. Default 60s; 0 disables. Server-scoped
        /// (`SET authentication_timeout` is refused with 55P02).
        #[arg(long, value_name = "DURATION")]
        authentication_timeout: Option<String>,

        /// PostgreSQL `idle_session_timeout`: close an authenticated session
        /// idle outside a transaction for longer than this (FATAL 57P05).
        /// Bare integer = milliseconds. Default 0 = disabled, as in
        /// PostgreSQL; sessions may override it with SET.
        #[arg(long, value_name = "DURATION")]
        idle_session_timeout: Option<String>,

        /// PostgreSQL `idle_in_transaction_session_timeout`: close a session
        /// idle INSIDE a transaction block for longer than this (FATAL 25P03,
        /// transaction rolled back). Bare integer = milliseconds. Default 0 =
        /// disabled; sessions may override it with SET.
        #[arg(long, value_name = "DURATION")]
        idle_in_transaction_session_timeout: Option<String>,

        /// PostgreSQL `tcp_keepalives_idle`: seconds of inactivity before the
        /// kernel sends the first keepalive probe on an accepted socket.
        /// 0 = operating-system default (SO_KEEPALIVE is always on).
        #[arg(long, value_name = "DURATION")]
        tcp_keepalives_idle: Option<String>,

        /// PostgreSQL `tcp_keepalives_interval`: seconds between keepalive
        /// probes. 0 = operating-system default.
        #[arg(long, value_name = "DURATION")]
        tcp_keepalives_interval: Option<String>,

        /// PostgreSQL `tcp_keepalives_count`: probes before a connection is
        /// declared dead. 0 = operating-system default.
        #[arg(long, value_name = "N")]
        tcp_keepalives_count: Option<u32>,

        /// Log a WARN (once per crossing) when in-use connections on a
        /// listener reach this percentage of --max-connections. Default 80;
        /// 0 disables.
        #[arg(long, value_name = "PERCENT")]
        max_connections_warn_percent: Option<u8>,
    },

    /// Stop a running server
    Stop {
        /// PID file location
        #[arg(long, default_value = "./heliosdb.pid")]
        pid_file: PathBuf,
    },

    /// Check server status
    Status {
        /// PID file location
        #[arg(long, default_value = "./heliosdb.pid")]
        pid_file: PathBuf,
    },

    /// Initialize a new database
    Init {
        /// Data directory
        #[arg(default_value = "./heliosdb-data")]
        data_dir: PathBuf,
    },

    /// Run embedded mode (REPL)
    Repl {
        /// Data directory (default: ./heliosdb-data)
        #[arg(short, long, default_value = "./heliosdb-data")]
        data_dir: PathBuf,

        /// Use in-memory database
        #[arg(short, long)]
        memory: bool,

        /// Dump on shutdown (only for --memory mode)
        #[arg(long)]
        dump_on_shutdown: bool,

        /// Dump output file (used with --dump-on-shutdown)
        #[arg(long)]
        dump_file: Option<PathBuf>,
    },

    /// Dump database to file
    Dump {
        /// Output file path
        #[arg(short, long, default_value = "backup.heliodump")]
        output: PathBuf,

        /// Data directory (required unless --connection is used)
        #[arg(short, long)]
        data_dir: Option<PathBuf>,

        /// Append incremental changes
        #[arg(short, long)]
        append: bool,

        /// Compression type (zstd, gzip, brotli, none)
        #[arg(long, default_value = "zstd")]
        compression: String,

        /// Connection string (for server mode)
        #[arg(long)]
        connection: Option<String>,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Restore database from dump file
    Restore {
        /// Input dump file path
        #[arg(short, long)]
        input: PathBuf,

        /// Target data directory
        #[arg(short, long)]
        target: Option<PathBuf>,

        /// Verify dump integrity before restore
        #[arg(long)]
        verify: bool,

        /// Restore even when the dump's rows violate the constraints it
        /// carries, reporting each violation as a warning instead of failing.
        /// Needed for a backup of a database that legitimately holds such rows
        /// (a NOT ENFORCED / LOCK-FREE constraint, or helios.fk_validation =
        /// 'audit'), since those are session settings that cannot be applied to
        /// the target this command opens.
        #[arg(long = "no-validate")]
        no_validate: bool,

        /// Connection string (for server mode)
        #[arg(long)]
        connection: Option<String>,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Code-graph (FR 2+) management. Opt-in on binaries built with
    /// `--features code-graph`.
    #[cfg(feature = "code-graph")]
    #[command(name = "code-graph")]
    CodeGraph {
        #[command(subcommand)]
        action: CodeGraphAction,
    },
}

/// Sub-actions for `heliosdb-nano code-graph`.
#[cfg(feature = "code-graph")]
#[derive(Subcommand)]
enum CodeGraphAction {
    /// Git-hook helper. Reads changed paths from stdin (one per line,
    /// as `git diff-tree --no-commit-id --name-only -r HEAD`
    /// produces), upserts each file's content into the source table
    /// and runs the code-graph indexer.
    Hook {
        /// `.helios-index/heliosdb-data` directory. Empty string ⇒
        /// in-memory (useful for dry-run / smoke tests).
        #[arg(short, long)]
        data_dir: PathBuf,
        /// Root of the repository the paths are relative to.
        #[arg(short, long, default_value = ".")]
        repo_root: PathBuf,
        /// Source-table name (default `src`). The table is created if
        /// it doesn't already exist.
        #[arg(short, long, default_value = "src")]
        source_table: String,
    },
}

fn main() -> Result<()> {
    // Initialize tracing. Default to WARN when RUST_LOG is unset so
    // operator-facing diagnostics (e.g. the slow-query log) are visible
    // out of the box instead of silently filtered (D3).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // P0-2: move a literal `start --password` / `--mcp-token` off argv and
    // replace the process image *before* any Tokio worker, database, or
    // listener exists. Env/file-only invocations are a no-op.
    preflight_argv_reexec(&cli)?;

    // Only now build the runtime: a successful re-exec above has already
    // replaced this image, so no worker thread or resource is ever discarded.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::config(format!("failed to start the async runtime: {e}")))?;
    runtime.block_on(run(cli))
}

/// The former `#[tokio::main]` body, reached only after the synchronous
/// argv-reexec preflight in [`main`].
async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Start {
            data_dir,
            memory,
            port,
            listen,
            config,
            daemon,
            pid_file,
            dump_on_shutdown,
            dump_schedule,
            tls_cert,
            tls_key,
            require_tls,
            tls_post_quantum,
            auth,
            password,
            password_file,
            replication_role,
            replication_port,
            primary_host,
            standby_hosts,
            observer_hosts,
            sync_mode,
            http_port,
            http_listen,
            mcp_token,
            allow_remote_mcp,
            allow_insecure_http,
            node_id,
            mysql,
            mysql_listen,
            mysql_socket,
            mysql_tls_cert,
            mysql_tls_key,
            mysql_tls_post_quantum,
            pg_socket_dir,
            max_connections,
            join_memory_limit_mb,
            authentication_timeout,
            idle_session_timeout,
            idle_in_transaction_session_timeout,
            tcp_keepalives_idle,
            tcp_keepalives_interval,
            tcp_keepalives_count,
            max_connections_warn_percent,
        } => {
            // Validate that either --data-dir or --memory is specified
            if !memory && data_dir.is_none() {
                return Err(Error::config(
                    "Either --data-dir or --memory must be specified. Use --help for more information.".to_string(),
                ));
            }

            // Validate TLS options
            if tls_cert.is_some() != tls_key.is_some() {
                return Err(Error::config(
                    "Both --tls-cert and --tls-key must be specified together for TLS.".to_string(),
                ));
            }
            // --require-tls is meaningless without a certificate/key pair, and
            // silently accepting it would leave an operator believing the
            // PostgreSQL TCP listener is TLS-only while it still offers
            // plaintext. Fail before the database is opened or a daemon is
            // spawned. PostgreSQL TCP only: this does not govern MySQL TLS or
            // the (authenticated, local) PostgreSQL Unix-socket listener.
            if require_tls && (tls_cert.is_none() || tls_key.is_none()) {
                return Err(Error::config(
                    "--require-tls requires both --tls-cert and --tls-key.".to_string(),
                ));
            }
            if mysql_tls_cert.is_some() != mysql_tls_key.is_some() {
                return Err(Error::config(
                    "Both --mysql-tls-cert and --mysql-tls-key must be specified together for MySQL TLS.".to_string(),
                ));
            }

            // Validate auth options. Password source precedence:
            // --password > --password-file > $HELIOSDB_PASSWORD_FILE
            // > $HELIOSDB_PASSWORD. A literal --password was already moved off
            // argv by `preflight_argv_reexec` (which owns the deprecation
            // warning) and reaches this point as the value source.
            let auth_lower = auth.to_lowercase();
            let password_source = resolve_password(
                password,
                password_file,
                std::env::var_os("HELIOSDB_PASSWORD_FILE"),
                std::env::var_os("HELIOSDB_PASSWORD"),
            )?;
            if auth_lower != "trust" && password_source.is_absent() {
                return Err(Error::config(format!(
                    "Authentication method '{}' requires a password: set --password-file, \
                     the HELIOSDB_PASSWORD_FILE or HELIOSDB_PASSWORD environment variable, \
                     or the (deprecated) --password.",
                    auth
                )));
            }

            // MCP-over-HTTP bearer token: same hygiene as the password. It is
            // read from $HELIOSDB_MCP_TOKEN when --mcp-token is absent, and the
            // daemon re-exec hands it to the child through the environment.
            let mcp_token = resolve_mcp_token(mcp_token, std::env::var_os("HELIOSDB_MCP_TOKEN"))?;

            // Scheduled dumps are not implemented. Fail hard instead of
            // silently accepting the flag — a no-op here means an operator
            // who relied on it has no backups (phantom-backup trap, C13).
            if dump_schedule.is_some() {
                return Err(Error::config(
                    "dump-schedule is not implemented yet; use OS cron with `heliosdb-nano dump`".to_string(),
                ));
            }

            let resolved_data_dir = data_dir.unwrap_or_else(|| PathBuf::from("./heliosdb-data"));

            // Build HA configuration
            let ha_config = HAConfig {
                role: replication_role,
                replication_port,
                primary_host,
                standby_hosts,
                observer_hosts,
                sync_mode,
                http_port,
                http_listen,
                mcp_token,
                allow_remote_mcp,
                allow_insecure_http,
                node_id,
            };

            // GH#28: connection-lifetime overrides (CLI > config file > default).
            let policy_args = ConnectionPolicyArgs {
                max_connections,
                authentication_timeout,
                idle_session_timeout,
                idle_in_transaction_session_timeout,
                tcp_keepalives_idle,
                tcp_keepalives_interval,
                tcp_keepalives_count,
                max_connections_warn_percent,
            };
            // GH#29 (c7, M3): `[performance]` overrides, same precedence.
            let performance_args = PerformanceArgs { join_memory_limit_mb };

            if daemon {
                start_server_daemon(
                    resolved_data_dir,
                    port,
                    listen,
                    config,
                    memory,
                    dump_on_shutdown,
                    pid_file,
                    tls_cert,
                    tls_key,
                    require_tls,
                    tls_post_quantum,
                    auth,
                    password_source,
                    ha_config,
                    mysql,
                    mysql_listen,
                    mysql_socket,
                    mysql_tls_cert,
                    mysql_tls_key,
                    mysql_tls_post_quantum,
                    pg_socket_dir,
                    policy_args,
                    performance_args,
                )
                .await
            } else {
                start_server(
                    resolved_data_dir,
                    port,
                    listen,
                    config,
                    memory,
                    dump_on_shutdown,
                    tls_cert,
                    tls_key,
                    require_tls,
                    tls_post_quantum,
                    auth,
                    password_source.into_value(),
                    ha_config,
                    mysql,
                    mysql_listen,
                    mysql_socket,
                    mysql_tls_cert,
                    mysql_tls_key,
                    mysql_tls_post_quantum,
                    pg_socket_dir,
                    policy_args,
                    performance_args,
                )
                .await
            }
        }
        Commands::Stop { ref pid_file } => stop_server(pid_file),
        Commands::Status { ref pid_file } => check_server_status(pid_file),
        Commands::Init { ref data_dir } => init_database(data_dir),
        Commands::Repl {
            data_dir,
            memory,
            dump_on_shutdown,
            dump_file,
        } => run_repl(data_dir, memory, dump_on_shutdown, dump_file),
        Commands::Dump {
            output,
            data_dir,
            append,
            compression,
            connection,
            verbose,
        } => {
            use heliosdb_nano::cli::DumpCommand;
            let cmd = DumpCommand {
                output,
                append,
                compression,
                connection,
                verbose,
                data_dir,
                memory: false,
            };
            cmd.execute()
        }
        Commands::Restore {
            input,
            target,
            verify,
            no_validate,
            connection,
            verbose,
        } => {
            use heliosdb_nano::cli::RestoreCommand;
            let cmd = RestoreCommand {
                input,
                target,
                verify,
                connection,
                verbose,
                no_validate,
            };
            cmd.execute()
        }

        #[cfg(feature = "code-graph")]
        Commands::CodeGraph { action } => match action {
            CodeGraphAction::Hook {
                data_dir,
                repo_root,
                source_table,
            } => {
                let stats = heliosdb_nano::code_graph::git_hook::run_from_stdin(&data_dir, &repo_root, &source_table)?;
                println!(
                    "code-graph hook: files_seen={} parsed={} unchanged={} skipped={} symbols={} refs={}",
                    stats.files_seen,
                    stats.files_parsed,
                    stats.files_unchanged,
                    stats.files_skipped,
                    stats.symbols_written,
                    stats.refs_written
                );
                Ok(())
            }
        },
    }
}

/// One-line durability contract derived from the active configuration (D3).
///
/// Never hardcoded: every clause is computed from `config.storage` so the
/// banner cannot drift from what the engine actually does.
fn durability_contract(config: &Config) -> String {
    use heliosdb_nano::config::WalSyncModeConfig;

    let s = &config.storage;
    let profile = config
        .profile
        .map(|p| format!("profile: {}; ", p.as_str()))
        .unwrap_or_default();

    if s.memory_only || !s.wal_enabled {
        return format!("{profile}durability: none (in-memory / WAL disabled) — data does not survive process exit");
    }

    let sync_mode = match s.wal_sync_mode {
        WalSyncModeConfig::Sync => "sync",
        WalSyncModeConfig::Async => "async",
        WalSyncModeConfig::GroupCommit => "group_commit",
    };

    if s.durable_commit {
        format!(
            "{profile}durability: power-loss durable commits (fsync at COMMIT, storage.durable_commit=on; wal_sync_mode={sync_mode})"
        )
    } else {
        format!(
            "{profile}durability: process-crash-safe (RocksDB WAL, wal_sync_mode={sync_mode}); power-loss durable commits: off (storage.durable_commit)"
        )
    }
}

/// PostgreSQL TCP SSL mode: `Require` when `--require-tls` is set, otherwise
/// the compatibility default `Prefer` (TLS offered, plaintext accepted).
fn pg_tcp_ssl_mode(require_tls: bool) -> heliosdb_nano::protocol::postgres::SslMode {
    if require_tls {
        heliosdb_nano::protocol::postgres::SslMode::Require
    } else {
        heliosdb_nano::protocol::postgres::SslMode::Prefer
    }
}

/// Build the `start` argv that `start_server_daemon` re-execs for the
/// PostgreSQL TLS flags. Split out so `--require-tls` forwarding can be
/// unit-tested without spawning a child process.
fn daemon_pg_tls_args(
    tls_cert: &Option<PathBuf>,
    tls_key: &Option<PathBuf>,
    require_tls: bool,
    tls_post_quantum: bool,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(cert) = tls_cert {
        args.push("--tls-cert".to_string());
        args.push(cert.display().to_string());
    }
    if let Some(key) = tls_key {
        args.push("--tls-key".to_string());
        args.push(key.display().to_string());
    }
    args.push("--tls-post-quantum".to_string());
    args.push(tls_post_quantum.to_string());
    if require_tls {
        args.push("--require-tls".to_string());
    }
    args
}

#[allow(clippy::too_many_arguments)]
async fn start_server(
    data_dir: PathBuf,
    port: u16,
    listen: String,
    config_path: Option<PathBuf>,
    memory_mode: bool,
    dump_on_shutdown: bool,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    require_tls: bool,
    tls_post_quantum: bool,
    auth: String,
    password: Option<String>,
    ha_config: HAConfig,
    mysql_enabled: bool,
    mysql_listen: String,
    mysql_socket: Option<PathBuf>,
    mysql_tls_cert: Option<PathBuf>,
    mysql_tls_key: Option<PathBuf>,
    mysql_tls_post_quantum: bool,
    pg_socket_dir: Option<PathBuf>,
    policy_args: ConnectionPolicyArgs,
    performance_args: PerformanceArgs,
) -> Result<()> {
    use colored::Colorize;
    use heliosdb_nano::protocol::postgres::auth::{AuthManager, AuthMethod};
    use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
    use heliosdb_nano::protocol::postgres::ssl::SslConfig;
    use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
    use heliosdb_nano::protocol::postgres::{InMemoryPasswordStore, PasswordStore, SharedPasswordStore};
    use heliosdb_nano::storage::{DumpCompressionType, DumpManager, DumpMode, DumpOptions};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Instant;

    let startup_time = Instant::now();

    // Print startup banner
    println!();
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║           HeliosDB Nano v{:<32} ║", env!("CARGO_PKG_VERSION"));
    println!("║   PostgreSQL-compatible database with enterprise features     ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");
    println!();

    // Load config
    let mut db_config = if let Some(ref path) = config_path {
        println!("[1/4] Loading configuration from {}...", path.display());
        Config::from_file(path.clone())?
    } else {
        println!("[1/4] Using default configuration...");
        Config::default()
    };

    // CLI flags win over the config file for storage location/mode.
    if memory_mode {
        db_config.storage.memory_only = true;
        db_config.storage.path = None;
        db_config.storage.wal_enabled = false;
    } else {
        db_config.storage.memory_only = false;
        db_config.storage.path = Some(data_dir.clone());
    }

    // GH#28: connection-lifetime policy — CLI > config file > default, resolved
    // ONCE here and shared by the PostgreSQL TCP / Unix-socket listeners, the
    // MySQL listeners and (keepalive only) the replication listener. This also
    // makes `[server] max_connections` live: it used to be read by nothing.
    policy_args.apply_to(&mut db_config.server);
    db_config.server.validate_connection_policy()?;
    // GH#29 (c7, M3): the join materialization cap, CLI > config > default.
    // `EmbeddedDatabase` installs it process-wide when it takes `db_config`.
    performance_args.apply_to(&mut db_config.performance);
    let max_connections = db_config.server.max_connections;
    let connection_policy = ConnectionTimeouts::from_server_config(&db_config.server);
    if ConnectionTimeouts::legacy_idle_alias_in_effect(&db_config.server) {
        tracing::warn!(
            "[server] idle_timeout_secs = {} is deprecated and is being applied as idle_session_timeout; \
             set [server] idle_session_timeout (PostgreSQL syntax, e.g. \"{}s\") instead",
            db_config.server.idle_timeout_secs,
            db_config.server.idle_timeout_secs
        );
    }

    // D3: one-line durability contract, derived from the active config.
    let durability_line = durability_contract(&db_config);
    println!("      {durability_line}");
    info!("{durability_line}");

    // Taken before `db_config` is MOVED into the database below. The HTTP
    // listener needs `[api]` (the JWT signing key above all) and would otherwise
    // have no access to it — which is part of why `[api]` was a dead config
    // section: nothing downstream of this point could read it.
    let api_config = db_config.api.clone();

    // ------------------------------------------------------------------
    // P0: resolve the HTTP exposure policy BEFORE any service starts.
    //
    // The HTTP listener shares the PostgreSQL wire's AuthManager, so both
    // agree on which users/credentials are valid. The bind policy is
    // enforced here, before the database is opened or HA starts, and the
    // HTTP address is computed now so the HTTP surface cannot start on a
    // wider interface by accident.
    // ------------------------------------------------------------------
    let auth_method = match auth.to_lowercase().as_str() {
        "trust" => AuthMethod::Trust,
        "password" => AuthMethod::CleartextPassword,
        "md5" => AuthMethod::Md5,
        "scram-sha-256" | "scram" => AuthMethod::ScramSha256,
        other => {
            return Err(Error::config(format!(
                "Unknown authentication method: '{}'. Use: trust, password, md5, scram-sha-256",
                other
            )))
        }
    };
    let auth_display = match &auth_method {
        AuthMethod::Trust => "Trust (development mode)",
        AuthMethod::CleartextPassword => "Cleartext Password",
        AuthMethod::Md5 => "MD5",
        AuthMethod::ScramSha256 => "SCRAM-SHA-256",
    };

    // ONE AuthManager, shared by the PostgreSQL wire listener and the HTTP
    // auth policy. Construction mirrors what `PgServer::new` used to do per
    // method: default users under MD5; a seeded SCRAM/cleartext store when a
    // password is configured.
    let auth_manager: Arc<AuthManager> = if auth_method == AuthMethod::Md5 {
        if let Some(ref pwd) = password {
            let mut mgr = AuthManager::new(AuthMethod::Md5);
            mgr.add_user("postgres".to_string(), pwd.clone());
            mgr.add_user("helios".to_string(), pwd.clone());
            Arc::new(mgr)
        } else {
            Arc::new(AuthManager::new(AuthMethod::Md5).with_default_users())
        }
    } else if matches!(auth_method, AuthMethod::CleartextPassword | AuthMethod::ScramSha256) {
        if let Some(ref pwd) = password {
            let mut store = InMemoryPasswordStore::new();
            store
                .add_user("postgres", pwd)
                .map_err(|e| Error::config(format!("Failed to add user: {e}")))?;
            store
                .add_user("helios", pwd)
                .map_err(|e| Error::config(format!("Failed to add user: {e}")))?;
            let shared_store = SharedPasswordStore::new(store);
            Arc::new(AuthManager::with_password_store(auth_method, shared_store))
        } else {
            Arc::new(AuthManager::new(auth_method).with_default_users())
        }
    } else {
        Arc::new(AuthManager::new(AuthMethod::Trust).with_default_users())
    };

    // P0-1: HTTP Basic authenticates against the wire AuthManager when, and
    // only when, the wire has a credential (any non-trust method).
    // A trust wire listener can still carry an explicit password for HTTP
    // protection. Keep that password out of the wire AuthManager (trust means
    // the wire accepts anyone), but require it on every HTTP route.
    //
    // P0-4/P0-5: the limiter bounds are operator-tunable through `[api]`. A zero
    // value is a misconfiguration (it would disarm the limiter or lock everyone
    // out), so it is rejected at startup rather than silently clamped.
    let http_rate_limits = heliosdb_nano::api::HttpRateLimits {
        max_failures: api_config.http_auth_max_failures,
        failure_window: std::time::Duration::from_secs(api_config.http_auth_failure_window_secs),
        max_tracked_peers: api_config.http_auth_max_tracked_peers,
        max_in_flight: api_config.http_auth_max_in_flight,
    };
    let mcp_bearer: Option<std::sync::Arc<str>> = ha_config.mcp_token.clone().map(std::sync::Arc::from);
    let http_auth_policy = std::sync::Arc::new(if matches!(auth_method, AuthMethod::Trust) && password.is_some() {
        heliosdb_nano::api::HttpAuthPolicy::from_wire_with_limits(
            password.clone(),
            mcp_bearer,
            ha_config.allow_insecure_http,
            http_rate_limits,
        )
        .map_err(Error::config)?
    } else {
        heliosdb_nano::api::HttpAuthPolicy::from_auth_manager_with_limits(
            Some(std::sync::Arc::clone(&auth_manager)),
            mcp_bearer,
            ha_config.allow_insecure_http,
            http_rate_limits,
        )
        .map_err(Error::config)?
    });

    let tls_enabled = tls_cert.is_some();
    let http_health_disabled = ha_config.http_port == 0;
    // P0-3: HTTP defaults to loopback, never to `--listen`.
    let http_addr: Option<SocketAddr> = if http_health_disabled {
        None
    } else {
        let http_bind = heliosdb_nano::api::resolve_http_listen(ha_config.http_listen.as_deref());
        let addr: SocketAddr = format!("{}:{}", http_bind, ha_config.http_port)
            .parse()
            .map_err(|e| Error::config(format!("Invalid HTTP address: {e}")))?;
        // The HTTP listener never terminates TLS, so a wire `--tls-cert` does
        // not make it HTTPS. `http_bind_safety_check` fails closed on any
        // non-loopback plaintext bind.
        if let Err(reason) = heliosdb_nano::api::http_bind_safety_check(addr, &http_auth_policy, false) {
            return Err(Error::config(reason));
        }
        Some(addr)
    };

    // P0-8: the MySQL wire listener is trust-only. Enabling it while the
    // PostgreSQL wire requires a credential silently bypasses that credential
    // for any MySQL client, so fail closed at startup rather than warn. This
    // covers BOTH MySQL transports: the TCP listener (`--mysql`) and the Unix
    // domain socket (`--mysql-socket`). A UDS is a local credential-free path
    // into the same SQL engine, so it is not exempt just because it is local.
    if (mysql_enabled || mysql_socket.is_some()) && (password.is_some() || !matches!(auth_method, AuthMethod::Trust)) {
        let transport = if mysql_enabled { "--mysql" } else { "--mysql-socket" };
        return Err(Error::config(format!(
            "MySQL authentication is not implemented: the MySQL listener accepts any username, so \
             {transport} alongside the PostgreSQL {auth_display} method would let any MySQL client run \
             SQL without the configured password. Disable {transport}, or use --auth trust for an \
             explicit local/dev deployment."
        )));
    }

    // Open database (in-memory mode avoids disk I/O for all operations)
    let db = if memory_mode {
        println!("[2/4] Initializing in-memory database...");
        Arc::new(EmbeddedDatabase::with_config(db_config)?)
    } else {
        println!("[2/4] Initializing database at {}...", data_dir.display());
        Arc::new(EmbeddedDatabase::with_config(db_config)?)
    };
    println!("      Database initialized successfully");

    // Configure PostgreSQL server
    let pg_addr: SocketAddr = format!("{listen}:{port}")
        .parse()
        .map_err(|e| Error::config(format!("Invalid listen address: {e}")))?;

    // Build server config
    let mut pg_config = PgServerConfig::with_address(pg_addr)
        .with_auth_method(auth_method)
        .with_max_connections(max_connections)
        .with_timeouts(connection_policy.clone());

    // Configure TLS if specified. `--require-tls` selects `SslMode::Require`
    // (no plaintext fallback) for the PostgreSQL TCP listener; the default
    // remains `SslMode::Prefer` (TLS offered, plaintext accepted).
    if let (Some(cert_path), Some(key_path)) = (&tls_cert, &tls_key) {
        let ssl_config =
            SslConfig::new(pg_tcp_ssl_mode(require_tls), cert_path, key_path).with_post_quantum(tls_post_quantum);
        pg_config = pg_config.with_ssl(ssl_config);
    }

    println!("[3/4] Configuring server...");
    println!("      - Listen address: {pg_addr}");
    if let Some(addr) = http_addr {
        println!("      - HTTP address: {addr}");
    }
    println!("      - Max connections: {max_connections} (per listener)");
    println!(
        "      - Authentication timeout: {}",
        banner_duration(connection_policy.authentication_timeout)
    );
    println!(
        "      - Idle session timeout: {}",
        banner_duration(connection_policy.idle_session_timeout)
    );
    println!(
        "      - Idle-in-transaction timeout: {}",
        banner_duration(connection_policy.idle_in_transaction_session_timeout)
    );
    println!(
        "      - TCP keepalive: {}",
        match &connection_policy.tcp_keepalive {
            Some(ka) => format!(
                "on (idle {}, interval {}, count {})",
                if ka.idle.is_zero() {
                    "OS default".to_string()
                } else {
                    banner_duration(ka.idle)
                },
                if ka.interval.is_zero() {
                    "OS default".to_string()
                } else {
                    banner_duration(ka.interval)
                },
                if ka.retries == 0 {
                    "OS default".to_string()
                } else {
                    ka.retries.to_string()
                },
            ),
            None => "off".to_string(),
        }
    );
    println!("      - Authentication: {auth_display}");
    println!("      - SSL/TLS: {}", if tls_enabled { "Enabled" } else { "Disabled" });
    if dump_on_shutdown {
        println!("      - Dump on shutdown: Enabled");
    }

    // HA Configuration
    let ha_role = ha_config.role.to_lowercase();
    if ha_role != "standalone" {
        println!("      - Replication role: {}", ha_config.role);
        println!("      - Replication port: {}", ha_config.replication_port);
        println!("      - Sync mode: {}", ha_config.sync_mode);
        if let Some(ref primary) = ha_config.primary_host {
            println!("      - Primary host: {primary}");
        }
        if let Some(ref standbys) = ha_config.standby_hosts {
            println!("      - Standby hosts: {standbys}");
        }
        if let Some(ref observers) = ha_config.observer_hosts {
            println!("      - Observer hosts: {observers}");
        }
        println!("      - HTTP health port: {}", ha_config.http_port);
    }

    // Create PostgreSQL server with appropriate auth configuration
    // Share the single AuthManager built before startup with the wire listener.
    let pg_server = PgServer::with_shared_auth_manager(pg_config, Arc::clone(&db), Arc::clone(&auth_manager))?;

    println!("[4/4] Starting server...");
    println!();
    println!("════════════════════════════════════════════════════════════════");
    println!(
        "  Server ready! Started in {:.2}s",
        startup_time.elapsed().as_secs_f64()
    );
    println!("════════════════════════════════════════════════════════════════");
    println!();
    println!("  Connect using:");
    println!();
    println!("    psql:       psql -h {listen} -p {port}");
    println!("    Python:     psycopg2.connect(host='{listen}', port={port})");
    println!("    Node.js:    pg.connect({{ host: '{listen}', port: {port} }})");
    println!("    JDBC:       jdbc:postgresql://{listen}:{port}/heliosdb");
    println!();
    println!("    Compatibility notes:");
    println!("      FTS:         docs/compatibility/fts.md");
    println!("      ORM matrix:  https://github.com/HeliosDatabase/HeliosDB-Nano/blob/main/docs/compatibility/orm.md");
    println!("      Known gaps:  SELECT heliosdb_capability_report();");
    if mysql_enabled {
        println!();
        println!(
            "    mysql:      mysql -h {} -P {}",
            mysql_listen.split(':').next().unwrap_or("127.0.0.1"),
            mysql_listen.split(':').nth(1).unwrap_or("3306")
        );
        println!(
            "    PyMySQL:    pymysql.connect(host='{}', port={})",
            mysql_listen.split(':').next().unwrap_or("127.0.0.1"),
            mysql_listen.split(':').nth(1).unwrap_or("3306")
        );
    }
    println!();
    println!(
        "  For REPL mode (single-user):  heliosdb-nano repl -d {}",
        data_dir.display()
    );
    println!();
    println!("  Press Ctrl+C to shut down");
    println!("────────────────────────────────────────────────────────────────");
    println!();

    // Log for tracing subscribers
    info!("HeliosDB Nano server listening on {}", pg_addr);

    // Start HA components if enabled
    #[cfg(feature = "ha-tier1")]
    let _ha_handles = if ha_role != "standalone" {
        start_ha_components(&ha_config, &listen, port, db.storage.clone()).await?
    } else {
        HAHandles::default()
    };

    // Start HTTP endpoint (health checks, plus MCP routes when compiled in).
    //
    // KanttBan bug #2 (v3.27.0): a `--http-port` collision used to take
    // the entire server process down silently — `start_health_server`
    // returned an `Err`, the future completed early, the `tokio::select!`
    // health-server arm fired, and shutdown happened right after the
    // "Server ready!" banner. Now: bind eagerly so we surface the
    // failure at ERROR before the banner, and run the accept loop in a
    // detached task that the main `select!` never observes — so a
    // late-life health failure cannot tear down the database listener.
    // `--http-port 0` opts out (no HTTP endpoint, no listener).
    //
    // The join handle is kept (rather than fully detaching the task) so the
    // shutdown arm can abort it alongside the MySQL/Unix-socket listeners.
    // This task holds an `Arc::clone(&db)`; leaving it running past shutdown
    // pins the refcount above zero, so `EmbeddedDatabase::drop` — and with it
    // the ordered close-time row-counter flush and R4.2 snapshot checkpoint —
    // never runs at all under the default `--http-port 8080`. Aborting it is
    // what lets that work happen on the default `start` invocation, not just
    // under `--http-port 0`. The main `select!` never *observes* this task, so
    // a late-life HTTP failure cannot tear the database listener down.
    //
    // The address and the exposure policy were resolved before the database
    // opened (P0-3/P0-4); only the bind happens here.
    let http_handle: Option<tokio::task::JoinHandle<()>> = if let Some(http_addr) = http_addr {
        match tokio::net::TcpListener::bind(http_addr).await {
            Ok(listener) => {
                info!(
                    "HTTP endpoint at http://{http_addr} (/health, /version, /docs, /rest/v1, /auth/v1, /realtime/v1)"
                );
                if http_auth_policy.is_required() {
                    info!(
                        "HTTP API authentication is required on {http_addr} \
                         (HTTP Basic with the wire credential, or the MCP bearer token when configured)"
                    );
                } else {
                    tracing::warn!(
                        "HTTP API authentication is DISABLED for {} because the wire protocol is in \
                         trust mode; keep this listener on loopback or pass --allow-insecure-http \
                         only with compensating controls",
                        http_addr
                    );
                }
                let http_db = Arc::clone(&db);
                let mcp_token = ha_config.mcp_token.clone();
                let allow_remote_mcp = ha_config.allow_remote_mcp;
                let http_api_config = api_config.clone();
                let policy = std::sync::Arc::clone(&http_auth_policy);
                Some(tokio::spawn(async move {
                    if let Err(e) =
                        run_http_listener(listener, http_db, mcp_token, allow_remote_mcp, http_api_config, policy).await
                    {
                        tracing::error!("HTTP server accept loop exited: {e}");
                    }
                }))
            }
            Err(e) => {
                tracing::error!(
                    "Failed to bind HTTP endpoint on {}: {} (os error {:?}). \
                     The database listener stays up; pass --http-port <free> or \
                     --http-port 0 to silence this.",
                    http_addr,
                    e,
                    e.raw_os_error(),
                );
                None
            }
        }
    } else {
        info!("HTTP endpoint disabled (--http-port 0)");
        None
    };

    // Start MySQL listener if enabled.
    //
    // Uses `protocol::mysql::server::MysqlServer` (mirroring the PostgreSQL
    // `PgServer` abstraction) instead of an inline accept loop, so the MySQL
    // TCP listener gets the same TLS-negotiation machinery PostgreSQL has —
    // including PQ hybrid key-exchange support — from day one.
    let mysql_handle = if mysql_enabled {
        let mysql_addr: SocketAddr = mysql_listen
            .parse()
            .map_err(|e| Error::config(format!("Invalid MySQL listen address '{}': {}", mysql_listen, e)))?;

        // P0-8: a non-trust PostgreSQL method with --mysql already failed closed
        // before the database opened. Reaching here means the wire is in trust
        // mode, so the trust-only MySQL listener is consistent with an explicit
        // local/dev deployment — but say so loudly if it is exposed off loopback.
        if !mysql_addr.ip().is_loopback() {
            tracing::warn!(
                "MySQL listener on {mysql_addr} is trust-only and the PostgreSQL wire is also in \
                 trust mode; both accept any client. Use --mysql-listen 127.0.0.1 unless this is a \
                 deliberate local/dev deployment."
            );
        }

        let mysql_tls_enabled = mysql_tls_cert.is_some();
        let mut mysql_server_config = heliosdb_nano::protocol::mysql::MysqlServerConfig::with_address(mysql_addr)
            .with_max_connections(max_connections)
            .with_timeouts(connection_policy.clone());
        if let (Some(cert_path), Some(key_path)) = (&mysql_tls_cert, &mysql_tls_key) {
            let mysql_ssl_config = heliosdb_nano::protocol::mysql::MysqlSslConfig::new(cert_path, key_path)
                .with_post_quantum(mysql_tls_post_quantum);
            mysql_server_config = mysql_server_config.with_ssl(mysql_ssl_config);
        }

        let mysql_server = heliosdb_nano::protocol::mysql::MysqlServer::new(mysql_server_config, Arc::clone(&db))?;
        info!(
            "MySQL protocol listening on {} (tls: {})",
            mysql_addr,
            if mysql_tls_enabled { "enabled" } else { "disabled" }
        );
        Some(tokio::spawn(async move {
            if let Err(e) = mysql_server.serve().await {
                tracing::error!("MySQL server error: {}", e);
            }
        }))
    } else {
        None
    };

    // Start MySQL Unix domain socket listener if requested (local-only, no TCP)
    // Useful for PHP mysqli / WordPress embedded-mode pointing at
    // /var/run/mysqld/mysqld.sock or equivalent.
    #[cfg(unix)]
    let mysql_unix_handle = if let Some(ref socket_path) = mysql_socket {
        let path = socket_path.clone();
        // Remove stale socket file if it exists (best-effort)
        let _ = std::fs::remove_file(&path);
        let mysql_db = Arc::clone(&db);
        let conn_counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(1_000_000));
        let conn_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(max_connections));
        let policy = connection_policy.clone();
        let utilisation_warned = std::sync::atomic::AtomicBool::new(false);
        info!("MySQL Unix socket listening on {}", path.display());
        println!("    mysql (UDS): mysql --socket={}", path.display());
        Some(tokio::spawn(async move {
            let listener = match tokio::net::UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("Failed to bind MySQL Unix socket {}: {}", path.display(), e);
                    return;
                }
            };
            // Permissive mode so non-root clients can connect
            let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o777));
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let permit = match Arc::clone(&conn_limiter).try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                tracing::warn!("MySQL UDS connection limit reached ({}), rejecting", max_connections);
                                drop(stream);
                                continue;
                            }
                        };
                        warn_listener_utilisation(
                            "MySQL UDS",
                            &utilisation_warned,
                            &conn_limiter,
                            max_connections,
                            &policy,
                        );
                        let db_clone = Arc::clone(&mysql_db);
                        let conn_id = conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let conn_policy = policy.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) =
                                heliosdb_nano::protocol::mysql::handler::handle_mysql_connection_unix_with_timeouts(
                                    db_clone,
                                    stream,
                                    conn_id,
                                    conn_policy,
                                )
                                .await
                            {
                                tracing::debug!("MySQL UDS connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("MySQL UDS accept error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }))
    } else {
        None
    };
    // Unix-domain sockets don't exist on this platform — reject the flag loudly instead of silently ignoring it.
    #[cfg(not(unix))]
    let mysql_unix_handle: Option<tokio::task::JoinHandle<()>> = None;
    #[cfg(not(unix))]
    if mysql_socket.is_some() {
        return Err(Error::io(
            "unix sockets are not available on this platform (--mysql-socket); use the TCP listener instead",
        ));
    }

    // Start PostgreSQL Unix domain socket listener if requested.
    // libpq uses `<dir>/.s.PGSQL.<port>` when host starts with `/`.
    #[cfg(unix)]
    let pg_unix_handle = if let Some(ref sock_dir) = pg_socket_dir {
        let sock_path = sock_dir.join(format!(".s.PGSQL.{}", port));
        let _ = std::fs::create_dir_all(sock_dir);
        let _ = std::fs::remove_file(&sock_path);
        let pg_db = Arc::clone(&db);
        let policy = connection_policy.clone();
        // P0-2: the UDS listener authenticates with the SAME credential the TCP
        // listener serves. A local socket is an alternate transport, not an auth
        // bypass; with a configured method this now demands the wire credential
        // exactly like `127.0.0.1:port`.
        let pg_uds_auth = Arc::clone(&auth_manager);
        // sprinter dfc3d6f2c341: the same `max_connections` cap the TCP
        // listener enforces (`PgServer::serve`) and the MySQL UDS loop above
        // already had. Without it a local client could open connections
        // without limit on this listener — past `max_connections`, and on to
        // file-descriptor and memory exhaustion — while `SHOW max_connections`
        // still reported the limit. Per-listener, like every other limiter
        // here (`--max-connections` documents that).
        let pg_uds_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(max_connections));
        let pg_uds_warned = std::sync::atomic::AtomicBool::new(false);
        info!("PostgreSQL Unix socket listening on {}", sock_path.display());
        println!("    psql (UDS):  psql -h {} -p {}", sock_dir.display(), port);
        Some(tokio::spawn(async move {
            let listener = match tokio::net::UnixListener::bind(&sock_path) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("Failed to bind PG Unix socket {}: {}", sock_path.display(), e);
                    return;
                }
            };
            // EXPLICIT mode rather than whatever the process umask happens to
            // be (sprinter dfc3d6f2c341). 0o777 is PostgreSQL's own default
            // for `unix_socket_permissions` — the socket is the local
            // equivalent of a listening port and authentication still applies —
            // so this pins the documented default in place, it does not
            // tighten or loosen anything. There is no config knob for it yet;
            // add one here if a deployment needs 0o770 with a socket group.
            let _ = std::fs::set_permissions(&sock_path, std::os::unix::fs::PermissionsExt::from_mode(0o777));
            let conn_counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(2_000_000));
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        // Acquire BEFORE spawning, and close the socket when
                        // the limit is reached — the exact shape (and the
                        // same WARN) as the TCP accept loop in
                        // `PgServer::serve` and the MySQL UDS loop above.
                        let permit = match Arc::clone(&pg_uds_limiter).try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                tracing::warn!("PG UDS connection limit reached ({}), rejecting", max_connections);
                                drop(stream);
                                continue;
                            }
                        };
                        warn_listener_utilisation(
                            "PostgreSQL UDS",
                            &pg_uds_warned,
                            &pg_uds_limiter,
                            max_connections,
                            &policy,
                        );
                        let db_clone = Arc::clone(&pg_db);
                        let auth_clone = Arc::clone(&pg_uds_auth);
                        let conn_id = conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let conn_policy = policy.clone();
                        // `_permit` is declared FIRST so it drops LAST — after
                        // the handler's `Drop` has rolled back and released the
                        // session — and it is never cloned: the slot is
                        // released exactly once.
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) =
                                heliosdb_nano::protocol::postgres::handler::handle_connection_unix_with_timeouts(
                                    db_clone,
                                    stream,
                                    auth_clone,
                                    conn_id,
                                    conn_policy,
                                )
                                .await
                            {
                                tracing::debug!("PG UDS connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("PG UDS accept error: {}", e);
                    }
                }
            }
        }))
    } else {
        None
    };
    // Unix-domain sockets don't exist on this platform — reject the flag loudly instead of silently ignoring it.
    #[cfg(not(unix))]
    let pg_unix_handle: Option<tokio::task::JoinHandle<()>> = None;
    #[cfg(not(unix))]
    if pg_socket_dir.is_some() {
        return Err(Error::io(
            "unix sockets are not available on this platform (--pg-socket-dir); use the TCP listener instead",
        ));
    }

    // Start server with graceful shutdown handling. The health
    // server runs in its own detached task (see above) so a failure
    // there cannot tear the database listener down.
    tokio::select! {
        result = pg_server.serve() => {
            // Server stopped (error or normal shutdown)
            if let Err(ref e) = result {
                tracing::error!("Server error: {e}");
            }
            result?;
        }
        // SIGINT *and* SIGTERM. Awaiting only `ctrl_c()` here left SIGTERM on
        // its default Unix disposition — immediate termination, no `Drop`, so
        // none of `EmbeddedDatabase::drop`'s ordered close-time work (row
        // counters, then the R4.2 index-snapshot checkpoint) ran under any
        // service manager, nor under this binary's own `stop` subcommand,
        // which sends `kill -TERM`. See `heliosdb_nano::wait_for_shutdown_signal`.
        signal = heliosdb_nano::wait_for_shutdown_signal() => {
            println!();
            println!("{}", "Received shutdown signal...".yellow());
            tracing::info!("{signal} received — running clean shutdown");
            // Abort the HTTP accept loop first: it is the one listener task
            // holding an `Arc<EmbeddedDatabase>` clone, so until it is gone
            // the refcount cannot reach zero and `Drop` cannot run.
            if let Some(h) = http_handle {
                h.abort();
            }
            if let Some(h) = mysql_handle {
                h.abort();
            }
            if let Some(h) = mysql_unix_handle {
                h.abort();
            }
            if let Some(h) = pg_unix_handle {
                h.abort();
            }
            // Best-effort unlink of Unix socket files on shutdown.
            #[cfg(unix)]
            {
                if let Some(ref p) = mysql_socket { let _ = std::fs::remove_file(p); }
                if let Some(ref d) = pg_socket_dir {
                    let p = d.join(format!(".s.PGSQL.{}", port));
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }

    // Perform dump on shutdown if enabled
    if dump_on_shutdown {
        println!();
        println!("{}", "Performing database dump before shutdown...".cyan());

        let dump_path = data_dir.join("shutdown_dump.heliodump");
        let dump_manager = DumpManager::new(data_dir.clone(), DumpCompressionType::Zstd);

        let options = DumpOptions {
            output_path: dump_path.clone(),
            mode: DumpMode::Full,
            compression: DumpCompressionType::Zstd,
            append: false,
            tables: None,
            verbose: false,
            connection: None,
            format: heliosdb_nano::storage::DumpOutputFormat::Binary,
        };

        match dump_manager.dump(&options, db.as_ref()) {
            Ok(report) => {
                println!("{}", "Shutdown dump completed successfully!".green().bold());
                println!("  Tables: {}", report.tables_dumped);
                println!("  Rows: {}", report.rows_dumped);
                println!("  Size: {} bytes (compressed)", report.bytes_written);
                println!("  Duration: {} ms", report.duration_ms);
                println!("  Output: {}", dump_path.display().to_string().cyan());
            }
            Err(e) => {
                tracing::error!("Failed to dump database on shutdown: {e}");
            }
        }
    }

    println!();
    println!("Server shutdown complete. Goodbye!");
    Ok(())
}

fn init_database(data_dir: &PathBuf) -> Result<()> {
    println!();
    println!("Initializing new HeliosDB Nano database...");
    println!("  Location: {}", data_dir.display());

    // Create directory
    std::fs::create_dir_all(data_dir)?;

    // Initialize database
    let db = EmbeddedDatabase::new(data_dir)?;

    // Close database
    db.close()?;

    println!();
    println!("Database initialized successfully!");
    println!();
    println!("Next steps:");
    println!("  Start server:    heliosdb-nano start -d {}", data_dir.display());
    println!("  Start REPL:      heliosdb-nano repl -d {}", data_dir.display());
    println!();

    Ok(())
}

fn run_repl(data_dir: PathBuf, memory: bool, dump_on_shutdown: bool, dump_file: Option<PathBuf>) -> Result<()> {
    use colored::Colorize;
    use heliosdb_nano::repl::{ReplConfig, ReplShell};
    use heliosdb_nano::storage::{DumpCompressionType, DumpManager, DumpMode, DumpOptions};

    // Open database with user-friendly output
    let db = if memory {
        println!("Starting REPL with in-memory database...");
        println!("  Note: Data will be lost when you exit.");
        EmbeddedDatabase::new_in_memory()?
    } else {
        println!("Starting REPL with persistent storage at {}...", data_dir.display());
        EmbeddedDatabase::new(&data_dir)?
    };

    // Create REPL configuration
    let config = ReplConfig::default();

    // Create and run REPL
    let mut shell = ReplShell::new(db, config)?;

    // If dump_on_shutdown is requested, handle the shutdown dump after REPL exits
    let result = shell.run();

    if dump_on_shutdown && result.is_ok() {
        let dump_path = dump_file.unwrap_or_else(|| PathBuf::from("heliosdb_dump.heliodump"));
        println!();
        println!("{}", "Dumping database on shutdown...".cyan());

        // Create dump manager (data_dir is used for metadata tracking, not for reading data)
        let dump_manager = DumpManager::new(data_dir, DumpCompressionType::Zstd);

        // Configure dump options
        let options = DumpOptions {
            output_path: dump_path.clone(),
            mode: DumpMode::Full,
            compression: DumpCompressionType::Zstd,
            append: false,
            tables: None,
            verbose: false,
            connection: None,
            format: heliosdb_nano::storage::DumpOutputFormat::Binary,
        };

        // Perform dump using the shell's database reference
        match dump_manager.dump(&options, shell.db()) {
            Ok(report) => {
                println!("{}", "Dump completed successfully!".green().bold());
                println!("  Tables: {}", report.tables_dumped);
                println!("  Rows: {}", report.rows_dumped);
                println!("  Size: {} bytes (compressed)", report.bytes_written);
                println!("  Duration: {} ms", report.duration_ms);
                println!("  Output: {}", dump_path.display().to_string().cyan());
            }
            Err(e) => {
                tracing::error!("Failed to dump database: {e}");
            }
        }
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn start_server_daemon(
    data_dir: PathBuf,
    port: u16,
    listen: String,
    config_path: Option<PathBuf>,
    memory: bool,
    dump_on_shutdown: bool,
    pid_file: PathBuf,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    require_tls: bool,
    tls_post_quantum: bool,
    auth: String,
    password: PasswordSource,
    ha_config: HAConfig,
    mysql: bool,
    mysql_listen: String,
    mysql_socket: Option<PathBuf>,
    mysql_tls_cert: Option<PathBuf>,
    mysql_tls_key: Option<PathBuf>,
    mysql_tls_post_quantum: bool,
    pg_socket_dir: Option<PathBuf>,
    policy_args: ConnectionPolicyArgs,
    performance_args: PerformanceArgs,
) -> Result<()> {
    // Process management (kill -0 liveness probe, daemon re-exec) is unix-only.
    #[cfg(unix)]
    use std::process::{Command, Stdio};

    // Check if server is already running — the stale-PID probe shells out to kill(1), unix-only.
    #[cfg(unix)]
    if pid_file.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                // Check if process is running
                #[cfg(unix)]
                {
                    if Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                    {
                        return Err(Error::io(format!(
                            "Server is already running (PID: {pid}). Use 'heliosdb-nano stop' to stop it."
                        )));
                    }
                }
            }
        }
    }

    println!("Starting HeliosDB server in daemon mode...");

    // Prepare command arguments
    let mut args = vec![
        "start".to_string(),
        "--data-dir".to_string(),
        data_dir.display().to_string(),
        "--port".to_string(),
        port.to_string(),
        "--listen".to_string(),
        listen.clone(),
    ];
    // GH#28: --max-connections and the connection-lifetime flags are forwarded
    // ONLY when given (previously --max-connections was always pushed, which
    // would now clobber a config-file value in the child).
    policy_args.push_cli_args(&mut args);
    performance_args.push_cli_args(&mut args);

    if let Some(cfg) = config_path {
        args.push("--config".to_string());
        args.push(cfg.display().to_string());
    }

    // Forward in-memory / dump options (previously dropped by the re-exec,
    // same bug class as the 3.36.0 --max-connections fix; see C13).
    if memory {
        args.push("--memory".to_string());
    }
    if dump_on_shutdown {
        args.push("--dump-on-shutdown".to_string());
    }

    // Forward MySQL protocol and Unix-socket options (previously dropped).
    if mysql {
        args.push("--mysql".to_string());
    }
    args.push("--mysql-listen".to_string());
    args.push(mysql_listen);
    if let Some(sock) = mysql_socket {
        args.push("--mysql-socket".to_string());
        args.push(sock.display().to_string());
    }
    if let Some(dir) = pg_socket_dir {
        args.push("--pg-socket-dir".to_string());
        args.push(dir.display().to_string());
    }

    // Add TLS options
    args.extend(daemon_pg_tls_args(&tls_cert, &tls_key, require_tls, tls_post_quantum));

    if let Some(cert) = mysql_tls_cert {
        args.push("--mysql-tls-cert".to_string());
        args.push(cert.display().to_string());
    }
    if let Some(key) = mysql_tls_key {
        args.push("--mysql-tls-key".to_string());
        args.push(key.display().to_string());
    }
    args.push("--mysql-tls-post-quantum".to_string());
    args.push(mysql_tls_post_quantum.to_string());

    // Add auth options. The password is NEVER placed on the child's argv:
    // a file-backed source forwards only the path (--password-file), and an
    // argv/env source is returned for the CHILD'S environment below. This is
    // the P0-2 fix — the old code did `args.push("--password")` and leaked the
    // secret into `ps` / `/proc/<pid>/cmdline` for every daemon child.
    args.push("--auth".to_string());
    args.push(auth);
    let child_password: Option<String> = daemon_password_handoff(&password, &mut args);

    // Add HA options
    args.push("--replication-role".to_string());
    args.push(ha_config.role.clone());
    args.push("--replication-port".to_string());
    args.push(ha_config.replication_port.to_string());
    args.push("--sync-mode".to_string());
    args.push(ha_config.sync_mode.clone());
    args.push("--http-port".to_string());
    args.push(ha_config.http_port.to_string());
    if let Some(http_listen) = ha_config.http_listen {
        args.push("--http-listen".to_string());
        args.push(http_listen);
    }
    // The MCP bearer token is a secret too: hand it to the child through the
    // environment (same reason as the password), never its argv.
    let child_mcp_token: Option<String> = ha_config.mcp_token.clone();
    if ha_config.allow_remote_mcp {
        args.push("--allow-remote-mcp".to_string());
    }
    if ha_config.allow_insecure_http {
        args.push("--allow-insecure-http".to_string());
    }
    if let Some(primary) = ha_config.primary_host {
        args.push("--primary-host".to_string());
        args.push(primary);
    }
    if let Some(standbys) = ha_config.standby_hosts {
        args.push("--standby-hosts".to_string());
        args.push(standbys);
    }
    if let Some(observers) = ha_config.observer_hosts {
        args.push("--observer-hosts".to_string());
        args.push(observers);
    }
    if let Some(node_id) = ha_config.node_id {
        args.push("--node-id".to_string());
        args.push(node_id);
    }

    // Fork the process
    #[cfg(unix)]
    {
        let exe = std::env::current_exe().map_err(|e| Error::io(format!("Failed to get current executable: {e}")))?;

        let mut command = Command::new(&exe);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Secrets travel in the child's environment (owner-readable
        // /proc/<pid>/environ) or as a file path — never as an argument.
        if let Some(secret) = child_password.as_deref() {
            command.env("HELIOSDB_PASSWORD", secret);
            // A file source outranks the value source, so drop any inherited
            // HELIOSDB_PASSWORD_FILE: the value we resolved must stay
            // authoritative (e.g. explicit --password with a stale env file).
            command.env_remove("HELIOSDB_PASSWORD_FILE");
        }
        if let Some(token) = child_mcp_token.as_deref() {
            command.env("HELIOSDB_MCP_TOKEN", token);
        }
        let mut child = command
            .spawn()
            .map_err(|e| Error::io(format!("Failed to spawn daemon process: {e}")))?;

        let pid = child.id();

        // KanttBan bug #1 (v3.27.0): the parent used to print the
        // "Daemon Started" banner unconditionally and return 0 before
        // the child had bound its port — even when the worker died at
        // startup (e.g. http-port collision, see #2). Now: poll the
        // child's PG port for up to 5 s waiting for it to be
        // accepting connections, OR observe the child's exit status
        // first. If neither happens within the deadline we fail with
        // a non-zero exit so caller scripts (`scripts/heliosdb.sh`)
        // can detect failure without polling `ss` themselves.
        let target = format!("{}:{}", listen, port);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut child_dead = false;
        loop {
            // Did the child exit prematurely?
            match child.try_wait() {
                Ok(Some(status)) => {
                    child_dead = true;
                    let _ = std::fs::remove_file(&pid_file);
                    return Err(Error::io(format!(
                        "Daemon worker exited at startup (status: {status}). \
                         Check the logs for the bind / config error and retry."
                    )));
                }
                Ok(None) => {} // Still running.
                Err(e) => {
                    tracing::warn!("Failed to poll daemon child: {e}");
                }
            }
            // Try connecting to the PG port.
            if std::net::TcpStream::connect_timeout(
                &target.parse::<std::net::SocketAddr>().unwrap_or_else(|_| {
                    // listen may be 0.0.0.0; probe 127.0.0.1 instead.
                    format!("127.0.0.1:{port}").parse().expect("valid loopback addr")
                }),
                std::time::Duration::from_millis(200),
            )
            .is_ok()
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                // Don't leave a zombie if we time out — kill the child
                // and fail loudly. Caller can retry.
                let _ = child.kill();
                let _ = std::fs::remove_file(&pid_file);
                return Err(Error::io(format!(
                    "Daemon did not become ready on {target} within 5s. \
                     Most common cause: another listener already on this port \
                     (or on the HTTP health port, see #2 in BUGS_HELIOSDB.md). \
                     Pass --port and --http-port to non-conflicting values."
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = child_dead;

        // Detach so the daemon outlives the launching shell.
        // `child.id()` is preserved in the PID file even after we drop.
        std::mem::forget(child);

        // Write PID file (we already verified the worker is up).
        std::fs::write(&pid_file, pid.to_string()).map_err(|e| Error::io(format!("Failed to write PID file: {e}")))?;

        println!();
        println!("╔═══════════════════════════════════════════════════════════════╗");
        println!("║              HeliosDB Nano Daemon Started                      ║");
        println!("╚═══════════════════════════════════════════════════════════════╝");
        println!();
        println!("  Status:         RUNNING");
        println!("  PID:            {pid}");
        println!("  Address:        {listen}:{port}");
        println!("  Data directory: {}", data_dir.display());
        println!("  PID file:       {}", pid_file.display());
        println!();
        println!("  Connect using:");
        println!("    psql -h {listen} -p {port}");
        println!();
        println!("  Management commands:");
        println!("    heliosdb-nano status     Check server status");
        println!("    heliosdb-nano stop       Stop the server");
        println!();

        Ok(())
    }

    // Daemonization (re-exec + detach + SIGTERM management) is unix-only; on Windows run in the
    // foreground or wrap the process in a service manager.
    #[cfg(not(unix))]
    {
        // only the unix path reads/writes the PID file or builds the re-exec
        let _ = (&pid_file, &args, &child_password, &child_mcp_token);
        Err(Error::io(
            "daemon mode is not available on this platform; run 'heliosdb-nano start' in the \
             foreground or use a service wrapper (e.g. a Windows service / NSSM)",
        ))
    }
}

fn stop_server(pid_file: &PathBuf) -> Result<()> {
    if !pid_file.exists() {
        return Err(Error::io(format!(
            "PID file not found: {}. Server may not be running.",
            pid_file.display()
        )));
    }

    let pid_str = std::fs::read_to_string(pid_file).map_err(|e| Error::io(format!("Failed to read PID file: {e}")))?;

    let pid = pid_str
        .trim()
        .parse::<i32>()
        .map_err(|e| Error::io(format!("Invalid PID in file: {e}")))?;

    println!("Stopping HeliosDB server (PID: {pid})...");

    #[cfg(unix)]
    {
        use std::process::{Command, Stdio};

        // Send SIGTERM to gracefully stop the server
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| Error::io(format!("Failed to send signal: {e}")))?;

        if status.success() {
            // Wait a bit for graceful shutdown
            std::thread::sleep(std::time::Duration::from_secs(2));

            // Check if process is still running
            let still_running = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            if still_running {
                println!("Server did not stop gracefully, sending SIGKILL...");
                Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .ok();
            }

            // Remove PID file
            std::fs::remove_file(pid_file).map_err(|e| Error::io(format!("Failed to remove PID file: {e}")))?;

            println!("Server stopped successfully");
            Ok(())
        } else {
            Err(Error::io(format!(
                "Failed to stop server. Process {pid} may not exist."
            )))
        }
    }

    #[cfg(not(unix))]
    {
        Err(Error::io("Server management is only supported on Unix systems"))
    }
}

fn check_server_status(pid_file: &PathBuf) -> Result<()> {
    if !pid_file.exists() {
        println!("Status: NOT RUNNING");
        println!("PID file not found: {}", pid_file.display());
        return Ok(());
    }

    let pid_str = std::fs::read_to_string(pid_file).map_err(|e| Error::io(format!("Failed to read PID file: {e}")))?;

    let pid = pid_str
        .trim()
        .parse::<i32>()
        .map_err(|e| Error::io(format!("Invalid PID in file: {e}")))?;

    #[cfg(unix)]
    {
        use std::process::{Command, Stdio};

        // Check if process is running
        let is_running = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if is_running {
            println!("Status: RUNNING");
            println!("  PID: {pid}");
            println!("  PID file: {}", pid_file.display());

            // Try to get process info
            if let Ok(output) = Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "lstart="])
                .output()
            {
                if output.status.success() {
                    let start_time = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !start_time.is_empty() {
                        println!("  Started: {start_time}");
                    }
                }
            }
        } else {
            println!("Status: NOT RUNNING");
            println!("  PID file exists but process {pid} is not running");
            println!("  You may need to remove stale PID file: {}", pid_file.display());
        }
    }

    #[cfg(not(unix))]
    {
        println!("Status: UNKNOWN (platform not supported)");
        println!("  PID: {}", pid);
    }

    Ok(())
}

// ========== HA Helper Functions ==========

/// Handles for HA components
#[allow(dead_code)]
#[derive(Default)]
struct HAHandles {
    #[allow(dead_code)]
    replication_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Start HA replication components based on role
#[cfg(feature = "ha-tier1")]
async fn start_ha_components(
    ha_config: &HAConfig,
    listen: &str,
    port: u16,
    storage: std::sync::Arc<heliosdb_nano::storage::StorageEngine>,
) -> Result<HAHandles> {
    use heliosdb_nano::replication::{
        config::PrimaryConfig,
        ha_state::{ha_state, HARole, NodeConfig, SyncMode as HASyncMode},
        streaming::{StreamingClient, StreamingClientConfig, StreamingServer, StreamingServerConfig},
        wal_applicator::WalApplicator,
        wal_store::{WalStore, WalStoreConfig},
        SyncModeConfig,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    let node_id = if let Some(ref id) = ha_config.node_id {
        Uuid::parse_str(id).map_err(|e| Error::config(format!("Invalid node ID: {e}")))?
    } else {
        Uuid::new_v4()
    };

    info!("Starting HA components with node ID: {}", node_id);

    // Parse sync mode
    let sync_mode = match ha_config.sync_mode.to_lowercase().as_str() {
        "async" => SyncModeConfig::Async,
        "semi-sync" | "semisync" => SyncModeConfig::SemiSync {
            min_acks: 1,
            timeout_ms: 5000,
        },
        "sync" => SyncModeConfig::Sync {
            min_applied: 1,
            timeout_ms: 10000,
        },
        other => {
            return Err(Error::config(format!(
                "Unknown sync mode: '{}'. Use: async, semi-sync, sync",
                other
            )))
        }
    };

    let role = ha_config.role.to_lowercase();

    // Set HA state configuration for system views
    let ha_role = HARole::from_str(&role);
    let ha_sync_mode = HASyncMode::from_str(&ha_config.sync_mode);

    ha_state().set_config(NodeConfig {
        node_id,
        role: ha_role,
        listen_addr: listen.to_string(),
        port,
        replication_port: ha_config.replication_port,
        sync_mode: ha_sync_mode,
        primary_host: ha_config.primary_host.clone(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    });

    match role.as_str() {
        "primary" => {
            // Start streaming server for primary
            let repl_addr = format!("{}:{}", listen, ha_config.replication_port)
                .parse()
                .map_err(|e| Error::config(format!("Invalid replication address: {e}")))?;

            let wal_store = Arc::new(WalStore::new(WalStoreConfig::default()));
            wal_store
                .init()
                .await
                .map_err(|e| Error::io(format!("WAL store init failed: {e}")))?;

            let server_config = StreamingServerConfig {
                listen_addr: repl_addr,
                sync_mode,
                max_standbys: 10,
                heartbeat_interval: Duration::from_secs(1),
                // GH#28: keepalive ONLY — no timeout ever reaches the
                // replication stream (a standby is idle between WAL segments).
                tcp_keepalive: heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts::from_server_config(
                    &storage.config().server,
                )
                .tcp_keepalive,
                ..Default::default()
            };

            let server = StreamingServer::new(server_config, node_id, wal_store);
            info!("Streaming replication server starting on {}", repl_addr);

            // Spawn server task
            let handle = tokio::spawn(async move {
                if let Err(e) = server.start().await {
                    tracing::error!("Streaming server error: {}", e);
                }
            });

            Ok(HAHandles {
                replication_handle: Some(handle),
            })
        }
        "standby" => {
            // Start streaming client for standby
            let primary_host = ha_config
                .primary_host
                .as_ref()
                .ok_or_else(|| Error::config("--primary-host required for standby role".to_string()))?;

            // Resolve hostname to IP address (supports Docker DNS)
            let primary_addr = tokio::net::lookup_host(primary_host)
                .await
                .map_err(|e| Error::config(format!("Cannot resolve primary host '{}': {}", primary_host, e)))?
                .next()
                .ok_or_else(|| Error::config(format!("No address found for primary host '{}'", primary_host)))?;

            // Initialize query forwarder for transparent write routing (HeliosProxy feature)
            // Extract hostname from primary_host (format: "host:replication_port")
            // Query forwarding connects to primary's postgres port, not replication port
            {
                use heliosdb_nano::replication::query_forwarder::init_query_forwarder;
                let primary_hostname = primary_host.split(':').next().unwrap_or(primary_host);
                // Primary's postgres port - use environment variable or default to 5432
                let primary_pg_port = std::env::var("HELIOSDB_PRIMARY_PG_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(5432u16);
                init_query_forwarder(primary_hostname.to_string(), primary_pg_port);
                info!(
                    "Query forwarder initialized for transparent write routing to {}:{}",
                    primary_hostname, primary_pg_port
                );
            }

            let client_config = StreamingClientConfig {
                node_id,
                primary_addr,
                sync_mode,
                connect_timeout: Duration::from_secs(30),
                reconnect_delay: Duration::from_secs(5),
                max_reconnect_attempts: 0, // Unlimited
            };

            let (client, entry_rx) = StreamingClient::new(client_config);
            info!("Streaming client connecting to primary at {}", primary_host);

            // Create WAL Applicator for applying replicated entries
            let primary_hostname = primary_host.split(':').next().unwrap_or(primary_host);
            let primary_pg_port = std::env::var("HELIOSDB_PRIMARY_PG_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432u16);

            let applicator_config = PrimaryConfig {
                host: primary_hostname.to_string(),
                port: primary_pg_port,
                connect_timeout: Duration::from_secs(30),
                use_tls: false,
            };

            let applicator = Arc::new(WalApplicator::new(applicator_config));

            // Start the WAL applicator with storage engine
            applicator
                .start_with_storage(storage.clone())
                .await
                .map_err(|e| Error::io(format!("Failed to start WAL applicator: {}", e)))?;
            info!("WAL Applicator started, ready to apply replicated entries");

            // Get the applicator's queue sender for forwarding entries
            let queue_tx = applicator.get_queue_sender();

            // Spawn task to forward entries from streaming client to WAL applicator
            let _forward_handle = tokio::spawn(async move {
                info!("WAL entry forwarder task started");
                let mut entry_rx = entry_rx;
                while let Some(entry) = entry_rx.recv().await {
                    info!("Forwarder: received entry LSN={}, forwarding to applicator", entry.lsn);
                    if let Err(e) = queue_tx.send(entry).await {
                        tracing::error!("Failed to forward WAL entry to applicator: {}", e);
                        break;
                    }
                    info!("Forwarder: entry forwarded successfully");
                }
                info!("WAL entry forwarder stopped");
            });

            // Spawn streaming client task
            let client_handle = tokio::spawn(async move {
                if let Err(e) = client.start().await {
                    tracing::error!("Streaming client error: {}", e);
                }
            });

            // Keep applicator alive
            let _applicator_ref = applicator;

            Ok(HAHandles {
                replication_handle: Some(client_handle),
            })
        }
        "observer" => {
            info!("Starting as observer node");
            Ok(HAHandles::default())
        }
        _ => Err(Error::config(format!(
            "Unknown replication role: '{}'. Use: standalone, primary, standby, observer",
            ha_config.role
        ))),
    }
}

/// HTTP listener for Docker health checks, the BaaS HTTP surface, and (when
/// compiled with `mcp-endpoint`) MCP-over-HTTP routes.
///
/// P0-1: `http_auth` wraps the entire assembled router, so every non-health route
/// requires the wire credential when one is configured. The caller owns the
/// already-bound `TcpListener` so bind failures are reported up front
/// (see KanttBan bug #2 / v3.28.0 fix); the non-loopback bind *policy* is enforced
/// in `start_server` before this is reached.
async fn run_http_listener(
    listener: tokio::net::TcpListener,
    db: std::sync::Arc<EmbeddedDatabase>,
    mcp_token: Option<String>,
    allow_remote_mcp: bool,
    api_config: heliosdb_nano::config::ApiConfig,
    http_auth: std::sync::Arc<heliosdb_nano::api::HttpAuthPolicy>,
) -> std::io::Result<()> {
    use axum::{routing::get, Json, Router};

    async fn health() -> Json<serde_json::Value> {
        Json(serde_json::json!({ "status": "ok" }))
    }

    // The BaaS layer the README advertises as built in — PostgREST-style
    // `/rest/v1/*`, `/auth/v1/*`, `/realtime/v1/websocket`, Swagger `/docs` +
    // `/openapi.json`, `/version` — plus the auth bridge and the OAuth providers
    // from `[[api.oauth_providers]]`.
    //
    // The assembly itself lives in `ApiServer::from_config`, NOT here. It used to
    // be written out inline at this spot while `tests/baas_http_surface_tests.rs`
    // built its own router a second time; the two drifted, and the OAuth registry
    // was the casualty — no caller ever attached it, so `/auth/v1/authorize`
    // answered 503 for every operator who configured a provider, with a green test
    // suite the whole time. One seam, exercised by both, is the fix. The rationale
    // for each decision that assembly makes (the JWT key policy, why a refused
    // provider is not fatal, why an empty registry is left unattached) is on
    // `from_config`'s own doc comment.
    let local_addr = listener.local_addr()?;
    let (api_server, warnings) =
        heliosdb_nano::api::ApiServer::from_config(local_addr, std::sync::Arc::clone(&db), &api_config);
    // `from_config` never logs; it hands back what an operator needs to see. Only
    // the ephemeral-JWT notice is expected in a correct default deployment — a
    // refused OAuth provider or a failed auth bootstrap means an advertised feature
    // is dead on this process, which is an error, not a nicety.
    for warning in warnings {
        if warning.starts_with(heliosdb_nano::api::EPHEMERAL_JWT_WARNING_PREFIX) {
            tracing::warn!("{warning}");
        } else {
            tracing::error!("{warning}");
        }
    }

    let app = api_server
        .into_router()
        // `ApiServer` already serves `/health`; only `/` is additional.
        .route("/", get(health));

    #[cfg(feature = "mcp-endpoint")]
    let app = {
        let http_addr = listener.local_addr()?;
        let state = if let Some(token) = mcp_token {
            heliosdb_nano::mcp::McpState::new(db)
                .with_auth(heliosdb_nano::mcp::McpAuth::BearerToken(std::sync::Arc::from(token)))
        } else {
            heliosdb_nano::mcp::McpState::new(db)
        };
        let bind_result = if allow_remote_mcp {
            Ok(())
        } else {
            heliosdb_nano::mcp::bind_safety_check(http_addr, &state.auth)
        };
        match bind_result {
            Ok(()) => {
                tracing::info!("MCP endpoint mounted at http://{}/mcp", http_addr);
                heliosdb_nano::mcp::attach_mcp_routes(app, state)
            }
            Err(e) => {
                tracing::warn!("{e}; MCP routes are not mounted on this listener");
                app
            }
        }
    };

    #[cfg(not(feature = "mcp-endpoint"))]
    {
        if mcp_token.is_some() || allow_remote_mcp {
            tracing::warn!("MCP HTTP flags were supplied, but this binary was not built with the mcp-endpoint feature");
        }
        let _ = db;
    }

    // P0-1: wrap the WHOLE assembled surface (BaaS + MCP) with the wire
    // credential policy. The layer is applied last, so it runs first; it exempts
    // only the side-effect-free health route and CORS preflight. Because the MCP
    // mount is inside the same router, `/mcp*` cannot be reached without satisfying
    // the wire policy, and a real request still passes MCP's own bearer check.
    let app = heliosdb_nano::api::attach_http_auth(app, http_auth);

    // Attach peer connection info so the auth layer can rate-limit per peer.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[cfg(feature = "mcp-endpoint")]
    use serde_json::json;
    use serde_json::Value;

    /// Unique scratch dir for password-file tests.
    fn pw_scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hdb-pwtest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build an argv from UTF-8 literals for the re-exec planner tests.
    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Lossy view of an argv for order/absence assertions.
    fn lossy(argv: &[OsString]) -> Vec<String> {
        argv.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// Extract the `(password, password_file, mcp_token)` triple from a parsed
    /// `start` command for the parser-level secret tests.
    fn start_secret_args(cli: &Cli) -> (Option<String>, Option<PathBuf>, Option<String>) {
        let Commands::Start {
            password,
            password_file,
            mcp_token,
            ..
        } = &cli.command
        else {
            panic!("expected a start command");
        };
        (password.clone(), password_file.clone(), mcp_token.clone())
    }

    /// The real parser accepts both secret syntaxes and maps them to the
    /// literal value the planner receives.
    #[test]
    fn cli_parses_split_and_equals_secrets() {
        let password_cases: &[&[&str]] = &[
            &["heliosdb-nano", "start", "--password", "pw-1"],
            &["heliosdb-nano", "start", "--password=pw-1"],
        ];
        for args in password_cases {
            let cli = Cli::try_parse_from(args.iter().copied()).unwrap();
            let (password, password_file, _) = start_secret_args(&cli);
            assert_eq!(password.as_deref(), Some("pw-1"), "argv {args:?}");
            assert!(password_file.is_none(), "argv {args:?}");
        }

        let token_cases: &[&[&str]] = &[
            &["heliosdb-nano", "start", "--mcp-token", "tok-1"],
            &["heliosdb-nano", "start", "--mcp-token=tok-1"],
        ];
        for args in token_cases {
            let cli = Cli::try_parse_from(args.iter().copied()).unwrap();
            let (_, _, mcp_token) = start_secret_args(&cli);
            assert_eq!(mcp_token.as_deref(), Some("tok-1"), "argv {args:?}");
        }
    }

    /// Two explicit password-source flags are rejected by the CLI: the
    /// planner's "drop the explicit --password-file flag" arm is defensive,
    /// not a reachable command.
    #[test]
    fn cli_rejects_two_explicit_password_sources() {
        assert!(Cli::try_parse_from([
            "heliosdb-nano",
            "start",
            "--password",
            "pw",
            "--password-file",
            "/tmp/pw",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "heliosdb-nano",
            "start",
            "--password-file",
            "/tmp/pw",
            "--password",
            "pw",
        ])
        .is_err());
    }

    /// A literal CLI password is accepted with no explicit file flag and wins
    /// over an inherited `HELIOSDB_PASSWORD_FILE`, which the plan removes from
    /// the child environment.
    #[test]
    fn cli_literal_password_beats_inherited_env_file() {
        let cli = Cli::try_parse_from(["heliosdb-nano", "start", "--password", "pw"]).unwrap();
        let (password, password_file, mcp_token) = start_secret_args(&cli);
        assert_eq!(password.as_deref(), Some("pw"));
        assert!(password_file.is_none());

        let plan = plan_start_reexec(
            os_args(&["heliosdb-nano", "start", "--password", "pw"]),
            password,
            password_file,
            mcp_token,
            Some(OsString::from("/env/file")),
            None,
            None,
        )
        .unwrap();
        let ReexecPlan::Exec { env_set, env_unset, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        assert_eq!(
            env_set,
            vec![(OsString::from("HELIOSDB_PASSWORD"), OsString::from("pw"))]
        );
        assert!(env_unset.contains(&OsString::from("HELIOSDB_PASSWORD_FILE")));
    }

    /// Values that merely *contain* a secret flag name are ordinary values and
    /// parse unchanged.
    #[test]
    fn cli_parses_values_containing_secret_names() {
        let cli = Cli::try_parse_from([
            "heliosdb-nano",
            "start",
            "--data-dir",
            "/tmp/--password-not-a-flag",
            "--node-id",
            "node--mcp-token-x",
        ])
        .unwrap();
        let Commands::Start { data_dir, node_id, .. } = &cli.command else {
            panic!("expected a start command");
        };
        assert_eq!(
            data_dir.as_deref(),
            Some(std::path::Path::new("/tmp/--password-not-a-flag"))
        );
        assert_eq!(node_id.as_deref(), Some("node--mcp-token-x"));
    }

    #[test]
    fn password_argv_beats_file_and_env() {
        // A missing file path proves the argv arm short-circuits before I/O.
        let got = resolve_password(
            Some("argv-pw".to_string()),
            Some(PathBuf::from("/no/such/password-file")),
            Some(OsString::from("/no/such/env-file")),
            Some(OsString::from("env-pw")),
        )
        .unwrap();
        assert_eq!(got, PasswordSource::Argv("argv-pw".to_string()));
        assert_eq!(got.into_value().as_deref(), Some("argv-pw"));
    }

    #[test]
    fn password_file_reads_first_line_and_rejects_empty() {
        let dir = pw_scratch_dir("file");
        let path = dir.join("pw");
        std::fs::write(&path, "s3cret\nextra line ignored\n").unwrap();
        let got = resolve_password(None, Some(path.clone()), None, None).unwrap();
        assert_eq!(
            got,
            PasswordSource::File {
                path: path.clone(),
                value: "s3cret".to_string()
            }
        );

        std::fs::write(&path, "\n").unwrap();
        assert!(
            resolve_password(None, Some(path.clone()), None, None).is_err(),
            "an empty password file must fail closed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn password_file_env_beats_env_value() {
        let dir = pw_scratch_dir("envfile");
        let path = dir.join("pw");
        std::fs::write(&path, "from-file\n").unwrap();
        let got = resolve_password(
            None,
            None,
            Some(path.clone().into_os_string()),
            Some(OsString::from("from-value")),
        )
        .unwrap();
        assert_eq!(
            got,
            PasswordSource::File {
                path,
                value: "from-file".to_string()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn password_env_value_used_as_last_resort() {
        let got = resolve_password(None, None, None, Some(OsString::from("env-pw"))).unwrap();
        assert_eq!(got, PasswordSource::Env("env-pw".to_string()));
    }

    #[test]
    fn trust_auth_needs_no_password() {
        let got = resolve_password(None, None, None, None).unwrap();
        assert!(got.is_absent());
        assert_eq!(got.into_value(), None);
    }

    #[test]
    fn trust_auth_keeps_explicit_environment_password() {
        let got = resolve_password(None, None, None, Some(OsString::from("env-pw"))).unwrap();
        assert_eq!(got, PasswordSource::Env("env-pw".to_string()));
    }

    #[test]
    fn password_rejects_empty_explicit_literal() {
        // `--password ""` must fail, not resolve to an empty Argv secret that
        // the daemon handoff would then silently filter away.
        assert!(resolve_password(Some(String::new()), None, None, None).is_err());
    }

    #[test]
    fn password_rejects_empty_explicit_file_path() {
        assert!(resolve_password(None, Some(PathBuf::new()), None, None).is_err());
    }

    #[test]
    fn empty_env_password_file_falls_through_to_env_value() {
        // An empty HELIOSDB_PASSWORD_FILE is "set but empty" -> treated as
        // unset, so the value source still applies (consistent with the empty
        // HELIOSDB_PASSWORD semantics below).
        let got = resolve_password(None, None, Some(OsString::new()), Some(OsString::from("env-pw"))).unwrap();
        assert_eq!(got, PasswordSource::Env("env-pw".to_string()));
    }

    #[test]
    fn empty_env_password_is_absent() {
        let got = resolve_password(None, None, None, Some(OsString::new())).unwrap();
        assert!(got.is_absent());
        assert_eq!(got.into_value(), None);
    }

    #[test]
    fn mcp_token_flag_beats_env_and_rejects_empty() {
        assert_eq!(
            resolve_mcp_token(Some("flag-token".to_string()), Some(OsString::from("env-token"))).unwrap(),
            Some("flag-token".to_string())
        );
        assert!(resolve_mcp_token(Some(String::new()), Some(OsString::from("env-token"))).is_err());
        assert_eq!(
            resolve_mcp_token(None, Some(OsString::from("env-token"))).unwrap(),
            Some("env-token".to_string())
        );
        assert_eq!(resolve_mcp_token(None, Some(OsString::new())).unwrap(), None);
        assert_eq!(resolve_mcp_token(None, None).unwrap(), None);
    }

    /// R3 item 9 (Unix): a *selected* non-UTF8 env source must error, never be
    /// silently dropped into absence (which would fall through to `trust`).
    #[cfg(unix)]
    #[test]
    fn non_utf8_configured_sources_error_not_absent() {
        use std::os::unix::ffi::OsStringExt;
        let bad_path = OsString::from_vec(vec![b'/', 0x80, 0xff]);
        let bad_value = OsString::from_vec(vec![0x80, 0xfe]);

        assert!(resolve_password(None, None, Some(bad_path.clone()), None).is_err());
        assert!(resolve_password(None, None, None, Some(bad_value.clone())).is_err());
        assert!(resolve_password(None, Some(PathBuf::from(bad_path)), None, None).is_err());
        assert!(resolve_mcp_token(None, Some(bad_value)).is_err());
    }

    /// R3 item 9 (Unix): an env source that is *overridden* by an explicit flag
    /// must not turn into an unrelated fatal error.
    #[cfg(unix)]
    #[test]
    fn overridden_non_utf8_env_does_not_error() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0x80, 0xff]);

        let got = resolve_password(Some("argv-pw".to_string()), None, Some(bad.clone()), Some(bad.clone())).unwrap();
        assert_eq!(got, PasswordSource::Argv("argv-pw".to_string()));

        let dir = pw_scratch_dir("nonutf8-override");
        let path = dir.join("pw");
        std::fs::write(&path, "file-pw\n").unwrap();
        let got = resolve_password(None, Some(path), Some(bad.clone()), None).unwrap();
        assert_eq!(got.into_value().as_deref(), Some("file-pw"));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            resolve_mcp_token(Some("flag-token".to_string()), Some(bad)).unwrap(),
            Some("flag-token".to_string())
        );
    }

    /// R1 regression: the `assert_eq!` assertions above force `Debug`, so a
    /// derived formatter on `PasswordSource` would print plaintext. Prove the
    /// manual impl redacts every secret-bearing arm.
    #[test]
    fn password_source_debug_redacts_secrets() {
        let file_secret = "file-contents-must-not-print";
        let rendered = [
            format!("{:?}", PasswordSource::Argv("argv-secret-must-not-print".to_string())),
            format!("{:?}", PasswordSource::Env("env-secret-must-not-print".to_string())),
            format!(
                "{:?}",
                PasswordSource::File {
                    path: PathBuf::from("/run/secrets/heliosdb_password"),
                    value: file_secret.to_string(),
                }
            ),
        ];
        for shown in &rendered {
            assert!(shown.contains("<redacted>"), "expected redaction marker in {shown}");
            assert!(!shown.contains("argv-secret-must-not-print"));
            assert!(!shown.contains("env-secret-must-not-print"));
            assert!(!shown.contains(file_secret));
        }
        assert_eq!(format!("{:?}", PasswordSource::Absent), "PasswordSource::Absent");
    }

    /// The core P0-2 invariant: the daemon re-exec must never place the
    /// plaintext on the child's argv. A file source forwards only the path;
    /// every other source is returned for the child's environment.
    #[test]
    fn daemon_never_puts_a_secret_on_child_argv() {
        let mut args = vec!["start".to_string()];
        let env = daemon_password_handoff(&PasswordSource::Argv("hunter2".to_string()), &mut args);
        assert_eq!(env.as_deref(), Some("hunter2"));
        assert!(!args.iter().any(|a| a == "hunter2"), "argv leaked the secret");
        assert!(!args.iter().any(|a| a == "--password"), "argv used --password");

        let mut args = vec!["start".to_string()];
        let env = daemon_password_handoff(&PasswordSource::Env("env-secret".to_string()), &mut args);
        assert_eq!(env.as_deref(), Some("env-secret"));
        assert!(!args.iter().any(|a| a == "env-secret" || a == "--password"));

        let mut args = vec!["start".to_string()];
        let env = daemon_password_handoff(
            &PasswordSource::File {
                path: PathBuf::from("/run/secrets/heliosdb_password"),
                value: "file-secret".to_string(),
            },
            &mut args,
        );
        assert!(env.is_none(), "a file source must not copy the value into env");
        assert!(args.iter().any(|a| a == "--password-file"));
        assert!(args.iter().any(|a| a == "/run/secrets/heliosdb_password"));
        assert!(
            !args.iter().any(|a| a == "file-secret" || a == "--password"),
            "argv leaked the secret"
        );
    }

    /// P0-2 argv re-exec: a file/env-only `start` must not trigger a process
    /// replacement (there is nothing on argv to remove).
    #[test]
    fn reexec_plan_is_none_for_file_and_env_only() {
        let argv = os_args(&["heliosdb-nano", "start", "--auth", "scram-sha-256"]);
        assert!(matches!(
            plan_start_reexec(
                argv.clone(),
                None,
                Some(PathBuf::from("/run/secrets/pw")),
                None,
                None,
                None,
                None,
            )
            .unwrap(),
            ReexecPlan::None
        ));
        assert!(matches!(
            plan_start_reexec(argv, None, None, None, None, Some(OsString::from("env-pw")), None,).unwrap(),
            ReexecPlan::None
        ));
    }

    /// A literal `--password` is removed from argv (both syntaxes), moved to the
    /// child environment, and argv[0] plus every unrelated argument survives.
    #[test]
    fn reexec_password_moves_off_argv_and_preserves_rest() {
        let argv = os_args(&[
            "heliosdb-nano",
            "start",
            "--data-dir",
            "/tmp/d",
            "--password",
            "hunter2",
            "--auth",
            "scram-sha-256",
        ]);
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        let ReexecPlan::Exec {
            argv,
            env_set,
            env_unset,
        } = plan
        else {
            panic!("expected a re-exec plan");
        };
        let argv = lossy(&argv);
        assert_eq!(argv[0], "heliosdb-nano");
        assert!(!argv.iter().any(|a| a == "hunter2" || a == "--password"));
        assert!(argv.iter().any(|a| a == "--data-dir"));
        assert!(argv.iter().any(|a| a == "/tmp/d"));
        assert!(argv.iter().any(|a| a == "--auth"));
        assert_eq!(
            env_set,
            vec![(OsString::from("HELIOSDB_PASSWORD"), OsString::from("hunter2"))]
        );
        assert!(env_unset.is_empty());

        // The equals syntax is a single token and is removed the same way.
        let argv = os_args(&["heliosdb-nano", "start", "--password=hunter2"]);
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        let ReexecPlan::Exec { argv, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        let argv = lossy(&argv);
        assert!(argv.iter().all(|a| !a.contains("hunter2") && !a.contains("--password")));
        assert_eq!(argv, vec!["heliosdb-nano".to_string(), "start".to_string()]);
    }

    /// When the literal password wins, both lower-precedence file sources are
    /// removed: the `--password-file` flag from argv and the inherited
    /// `HELIOSDB_PASSWORD_FILE` from the environment.
    ///
    /// The explicit `--password-file` arm is **defensive only**: the CLI marks
    /// `--password` / `--password-file` mutually exclusive, so Clap rejects the
    /// combination (see `cli_rejects_two_explicit_password_sources`) and the
    /// only lower source a real invocation can carry alongside `--password` is
    /// the inherited `HELIOSDB_PASSWORD_FILE`. The planner keeps handling both
    /// so it stays correct for any caller, not just today's parser.
    #[test]
    fn reexec_password_drops_overridden_file_sources() {
        let argv = os_args(&[
            "heliosdb-nano",
            "start",
            "--password-file",
            "/run/file",
            "--password",
            "argv-pw",
        ]);
        let plan = plan_start_reexec(
            argv,
            Some("argv-pw".to_string()),
            Some(PathBuf::from("/run/file")),
            None,
            Some(OsString::from("/env/file")),
            None,
            None,
        )
        .unwrap();
        let ReexecPlan::Exec {
            argv,
            env_set,
            env_unset,
        } = plan
        else {
            panic!("expected a re-exec plan");
        };
        let argv = lossy(&argv);
        assert!(!argv.iter().any(|a| a == "--password-file" || a == "/run/file"));
        assert!(env_unset.contains(&OsString::from("HELIOSDB_PASSWORD_FILE")));
        assert_eq!(
            env_set,
            vec![(OsString::from("HELIOSDB_PASSWORD"), OsString::from("argv-pw"))]
        );
    }

    /// A literal `--mcp-token` (split and equals) is moved to the MCP env var;
    /// an unrelated `--password-file` is preserved when the password is not a
    /// literal.
    #[test]
    fn reexec_mcp_token_off_argv_and_keeps_password_file() {
        // A private scratch file, not a host fixture: the planner validates the
        // selected file source (it actually reads it) even when the literal is
        // only the MCP token.
        let dir = pw_scratch_dir("reexec-mcp-file");
        let pw_path = dir.join("pw");
        std::fs::write(&pw_path, "file-pw\n").unwrap();
        let pw_text = pw_path.to_string_lossy().into_owned();
        let argv = os_args(&[
            "heliosdb-nano",
            "start",
            "--password-file",
            pw_text.as_str(),
            "--mcp-token",
            "tok-a",
            "--allow-remote-mcp",
        ]);
        let plan = plan_start_reexec(
            argv,
            None,
            Some(pw_path.clone()),
            Some("tok-a".to_string()),
            None,
            None,
            None,
        )
        .unwrap();
        let ReexecPlan::Exec { argv, env_set, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        let argv = lossy(&argv);
        assert!(!argv.iter().any(|a| a == "tok-a" || a == "--mcp-token"));
        assert!(argv.iter().any(|a| a == "--password-file"));
        assert!(argv.iter().any(|a| a == &pw_text));
        assert_eq!(
            env_set,
            vec![(OsString::from("HELIOSDB_MCP_TOKEN"), OsString::from("tok-a"))]
        );

        let argv = os_args(&["heliosdb-nano", "start", "--mcp-token=tok-b"]);
        let plan = plan_start_reexec(argv, None, None, Some("tok-b".to_string()), None, None, None).unwrap();
        let ReexecPlan::Exec { argv, env_set, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        assert_eq!(lossy(&argv), vec!["heliosdb-nano".to_string(), "start".to_string()]);
        assert_eq!(
            env_set,
            vec![(OsString::from("HELIOSDB_MCP_TOKEN"), OsString::from("tok-b"))]
        );
    }

    /// Unrelated strings that merely contain `--password`, plus all arguments
    /// after `--`, are preserved exactly.
    #[test]
    fn reexec_preserves_unrelated_and_post_terminator_args() {
        let argv = os_args(&[
            "heliosdb-nano",
            "start",
            "--data-dir",
            "/tmp/--password-not-a-flag",
            "--password",
            "hunter2",
            "--",
            "--password",
            "positional",
        ]);
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        let ReexecPlan::Exec { argv, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        assert_eq!(
            lossy(&argv),
            vec![
                "heliosdb-nano".to_string(),
                "start".to_string(),
                "--data-dir".to_string(),
                "/tmp/--password-not-a-flag".to_string(),
                "--".to_string(),
                "--password".to_string(),
                "positional".to_string(),
            ]
        );
    }

    /// Non-UTF8 arguments that are not one of our flags survive untouched; only
    /// the valid-UTF8 secret pair is removed.
    #[cfg(unix)]
    #[test]
    fn reexec_preserves_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;
        let weird = OsString::from_vec(vec![b'/', 0x80, 0xff]);
        let argv = vec![
            OsString::from("heliosdb-nano"),
            OsString::from("start"),
            OsString::from("--data-dir"),
            weird.clone(),
            OsString::from("--password"),
            OsString::from("hunter2"),
        ];
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        let ReexecPlan::Exec { argv, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        assert_eq!(argv[3], weird);
        assert_eq!(argv.len(), 4);
    }

    /// An explicit but empty secret must fail before any `exec`.
    #[test]
    fn reexec_refuses_empty_literal_before_exec() {
        let argv = os_args(&["heliosdb-nano", "start", "--password", ""]);
        assert!(plan_start_reexec(argv, Some(String::new()), None, None, None, None, None).is_err());

        let argv = os_args(&["heliosdb-nano", "start", "--mcp-token", ""]);
        assert!(plan_start_reexec(argv, None, None, Some(String::new()), None, None, None).is_err());
    }

    /// The replacement argv has no literal flag left, so a second parse sees an
    /// env-only source and must not re-exec again (no loop).
    #[test]
    fn reexec_does_not_loop() {
        let argv = os_args(&[
            "heliosdb-nano",
            "start",
            "--password",
            "hunter2",
            "--auth",
            "scram-sha-256",
        ]);
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        let ReexecPlan::Exec { argv, .. } = plan else {
            panic!("expected a re-exec plan");
        };
        let second = plan_start_reexec(argv, None, None, None, None, Some(OsString::from("hunter2")), None).unwrap();
        assert!(matches!(second, ReexecPlan::None));
    }

    /// The exec failure path is fail-closed and its error names neither the
    /// command nor any secret.
    #[test]
    fn reexec_failure_is_sanitized_and_fails_closed() {
        let err = reexec_failure(&std::io::Error::from_raw_os_error(13));
        let shown = format!("{err}");
        assert!(shown.contains("os error 13"));
        assert!(!shown.contains("hunter2"));
        assert!(!shown.contains("/proc/self/exe"));
    }

    /// A failing adapter propagates an error from `run_reexec` without touching
    /// the real process environment or image.
    #[test]
    fn reexec_adapter_failure_fails_closed() {
        struct FailingExec;
        impl ExecAdapter for FailingExec {
            fn exec(&self, _plan: &ReexecPlan) -> Result<()> {
                Err(Error::config("simulated exec failure"))
            }
        }
        let argv = os_args(&["heliosdb-nano", "start", "--password", "hunter2"]);
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        assert!(run_reexec(&plan, &FailingExec).is_err());
    }

    /// The plan's `Debug` must not print the credential it carries.
    #[test]
    fn reexec_plan_debug_redacts_secrets() {
        let argv = os_args(&["heliosdb-nano", "start", "--password", "hunter2"]);
        let plan = plan_start_reexec(argv, Some("hunter2".to_string()), None, None, None, None, None).unwrap();
        let shown = format!("{plan:?}");
        assert!(shown.contains("<redacted>"));
        assert!(!shown.contains("hunter2"));
    }

    #[tokio::test]
    async fn daemon_http_listener_serves_health() {
        let db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = run_http_listener(
                listener,
                db,
                None,
                false,
                heliosdb_nano::config::ApiConfig::default(),
                std::sync::Arc::new(heliosdb_nano::api::HttpAuthPolicy::disabled()),
            )
            .await;
        });

        let body: Value = reqwest::get(format!("http://{addr}/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["status"], "ok");

        handle.abort();
    }

    #[cfg(feature = "mcp-endpoint")]
    #[tokio::test]
    async fn daemon_http_listener_mounts_mcp_post() {
        let db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = run_http_listener(
                listener,
                db,
                None,
                false,
                heliosdb_nano::config::ApiConfig::default(),
                std::sync::Arc::new(heliosdb_nano::api::HttpAuthPolicy::disabled()),
            )
            .await;
        });

        let resp: Value = reqwest::Client::new()
            .post(format!("http://{addr}/mcp"))
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            resp["result"]["tools"]
                .as_array()
                .map_or(false, |tools| !tools.is_empty()),
            "expected MCP tools/list response, got {resp}"
        );

        handle.abort();
    }

    #[cfg(feature = "mcp-endpoint")]
    #[tokio::test]
    async fn daemon_http_listener_does_not_mount_remote_mcp_without_auth() {
        let db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let handle = tokio::spawn(async move {
            let _ = run_http_listener(
                listener,
                db,
                None,
                false,
                heliosdb_nano::config::ApiConfig::default(),
                std::sync::Arc::new(heliosdb_nano::api::HttpAuthPolicy::disabled()),
            )
            .await;
        });

        let health: Value = reqwest::get(format!("http://{addr}/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["status"], "ok");

        let status = reqwest::get(format!("http://{addr}/mcp/info")).await.unwrap().status();
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

        handle.abort();
    }

    #[cfg(feature = "mcp-endpoint")]
    #[tokio::test]
    async fn daemon_http_listener_mounts_remote_mcp_with_bearer_token() {
        let db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let handle = tokio::spawn(async move {
            let _ = run_http_listener(
                listener,
                db,
                Some("td8-secret".to_string()),
                false,
                heliosdb_nano::config::ApiConfig::default(),
                std::sync::Arc::new(heliosdb_nano::api::HttpAuthPolicy::disabled()),
            )
            .await;
        });

        let client = reqwest::Client::new();
        let unauth = client
            .get(format!("http://{addr}/mcp/info"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(unauth, reqwest::StatusCode::UNAUTHORIZED);

        let info: Value = client
            .get(format!("http://{addr}/mcp/info"))
            .bearer_auth("td8-secret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(info["serverInfo"]["name"], "heliosdb-nano");

        handle.abort();
    }

    // ------------------------------------------------------------------
    // Issue 53: `--require-tls` is PostgreSQL TCP only.
    // ------------------------------------------------------------------

    #[test]
    fn pg_tcp_ssl_mode_defaults_to_prefer_and_require_tls_selects_require() {
        use heliosdb_nano::protocol::postgres::SslMode;
        assert_eq!(pg_tcp_ssl_mode(false), SslMode::Prefer);
        assert_eq!(pg_tcp_ssl_mode(true), SslMode::Require);
    }

    #[test]
    fn daemon_pg_tls_args_forwards_require_tls_only_when_set() {
        let cert = Some(PathBuf::from("/etc/helios/cert.pem"));
        let key = Some(PathBuf::from("/etc/helios/key.pem"));

        let required = daemon_pg_tls_args(&cert, &key, true, true);
        assert!(required.windows(2).any(|w| w == ["--tls-cert", "/etc/helios/cert.pem"]));
        assert!(required.windows(2).any(|w| w == ["--tls-key", "/etc/helios/key.pem"]));
        assert!(
            required.iter().any(|a| a == "--require-tls"),
            "require-tls must be forwarded to the daemon child, got {required:?}"
        );

        let preferred = daemon_pg_tls_args(&cert, &key, false, true);
        assert!(
            !preferred.iter().any(|a| a == "--require-tls"),
            "require-tls must not be forwarded when off, got {preferred:?}"
        );
    }

    fn start_cli(extra: &[&str]) -> Commands {
        let argv: Vec<String> = std::iter::once("heliosdb-nano".to_string())
            .chain(std::iter::once("start".to_string()))
            .chain(std::iter::once("--memory".to_string()))
            .chain(extra.iter().map(|s| (*s).to_string()))
            .collect();
        Cli::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("failed to parse {extra:?}: {e}"))
            .command
    }

    fn post_quantum_flags(command: &Commands) -> (bool, bool) {
        match command {
            Commands::Start {
                tls_post_quantum,
                mysql_tls_post_quantum,
                ..
            } => (*tls_post_quantum, *mysql_tls_post_quantum),
            _ => unreachable!("expected start subcommand"),
        }
    }

    #[test]
    fn post_quantum_flags_accept_legacy_bare_flag_and_explicit_values() {
        let cases: &[(&[&str], bool, bool)] = &[
            (&[], true, true),
            (&["--tls-post-quantum"], true, true),
            (&["--tls-post-quantum=true"], true, true),
            (&["--tls-post-quantum=false"], false, true),
            (&["--mysql-tls-post-quantum"], true, true),
            (&["--mysql-tls-post-quantum=true"], true, true),
            (&["--mysql-tls-post-quantum=false"], true, false),
            (&["--tls-post-quantum", "true"], true, true),
            (&["--mysql-tls-post-quantum", "false"], true, false),
        ];
        for (extra, want_pg, want_mysql) in cases {
            let command = start_cli(extra);
            let (pg, mysql) = post_quantum_flags(&command);
            assert_eq!(pg, *want_pg, "postgres flag for {extra:?}");
            assert_eq!(mysql, *want_mysql, "mysql flag for {extra:?}");
        }
    }

    #[test]
    fn daemon_pg_tls_args_forwarding_parses_for_both_post_quantum_values() {
        let cert = Some(PathBuf::from("/etc/helios/cert.pem"));
        let key = Some(PathBuf::from("/etc/helios/key.pem"));

        for pq in [true, false] {
            let forwarded = daemon_pg_tls_args(&cert, &key, false, pq);
            let argv: Vec<String> = std::iter::once("heliosdb-nano".to_string())
                .chain(std::iter::once("start".to_string()))
                .chain(std::iter::once("--memory".to_string()))
                .chain(forwarded)
                .collect();
            let command = Cli::try_parse_from(argv)
                .unwrap_or_else(|e| panic!("daemon forwarding rejected for pq={pq}: {e}"))
                .command;
            let (pg, _) = post_quantum_flags(&command);
            assert_eq!(pg, pq, "daemon must forward post-quantum value {pq}");
        }
    }
}
