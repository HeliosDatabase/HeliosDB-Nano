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
    physical: heliosdb_nano::config::PhysicalReplicationConfig,
    role: String,
    replication_port: u16,
    startup_timeout_secs: u64,
    primary_host: Option<String>,
    standby_hosts: Option<String>,
    observer_hosts: Option<String>,
    sync_mode: String,
    http_port: u16,
    http_listen: Option<String>,
    mcp_token: Option<String>,
    allow_remote_mcp: bool,
    allow_insecure_http: bool,
    allow_insecure_trust: bool,
    node_id: Option<String>,
    /// One owner-only 32-byte token file authenticating both replication channels.
    token_file: Option<PathBuf>,
    /// Acknowledge plaintext token transport on a non-loopback replication listener.
    allow_insecure_remote: bool,
    /// Spec P1.3: `[replication] max_standby_lag` (TOML only; a daemon child
    /// re-reads it from the forwarded `--config`).
    max_standby_lag: std::time::Duration,
    /// Spec P1.3: `[replication] on_unrecoverable = "exit"`.
    exit_on_unrecoverable: bool,
}

/// Standbys receive their full verification window plus the ordinary startup allowance.
fn daemon_readiness_timeout(role: &str, replication_startup_secs: u64) -> std::time::Duration {
    std::time::Duration::from_secs(if role.eq_ignore_ascii_case("standby") {
        replication_startup_secs.saturating_add(5)
    } else {
        5
    })
}

/// Literal addresses only, with explicit port taking precedence over socket suffix.
fn parse_listen_address(value: &str, explicit_port: Option<u16>, default_port: u16) -> Result<std::net::SocketAddr> {
    use std::net::{IpAddr, SocketAddr};
    if let Ok(mut address) = value.parse::<SocketAddr>() {
        if let Some(port) = explicit_port {
            address.set_port(port);
        }
        return Ok(address);
    }
    let bare = value
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(value);
    let ip = bare.parse::<IpAddr>().map_err(|_| {
        Error::config(format!(
        "Invalid listen address '{value}': use an IPv4/IPv6 address or IP:port (bracket IPv6 sockets, e.g. [::1]:8080)"
    ))
    })?;
    Ok(SocketAddr::new(ip, explicit_port.unwrap_or(default_port)))
}

fn resolve_http_address(value: &str, port: Option<u16>) -> Result<Option<std::net::SocketAddr>> {
    // Explicit disable wins even over an invalid or port-bearing address.
    if port == Some(0) {
        return Ok(None);
    }
    let address = parse_listen_address(value, port, 8080)?;
    if address.port() == 0 {
        return Ok(None);
    }
    Ok(Some(address))
}

fn split_hosts(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(',').map(|s| s.trim().to_string()).collect()
    }
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
    /// Install/follow a closed physical receiver; never exposes SQL or serving readiness.
    Resync(ResyncArgs),
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

        /// Permit unauthenticated non-loopback PostgreSQL access for isolated development only.
        #[arg(long)]
        allow_insecure_trust: bool,

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

        #[command(flatten)]
        physical: PhysicalSourceArgs,

        // ========== HA Replication Options ==========
        /// Replication role: standalone, primary, standby, observer
        #[arg(long)]
        replication_role: Option<String>,

        /// Replication port for WAL streaming (default: 5433)
        #[arg(long)]
        replication_port: Option<u16>,

        /// Standby initial history verification timeout in seconds (1..=3600; default: 30)
        #[arg(long)]
        replication_startup_timeout_secs: Option<u64>,

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
        #[arg(long)]
        sync_mode: Option<String>,

        /// HTTP API port for health checks and MCP-over-HTTP (default: 8080, 0 disables HTTP)
        #[arg(long)]
        http_port: Option<u16>,

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

        /// Permit the replication token over a non-loopback plaintext native listener
        /// (isolated development only; mirrors --allow-insecure-trust).
        #[arg(long)]
        allow_insecure_replication_remote: bool,

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
            allow_insecure_trust,
            password,
            password_file,
            physical,
            replication_role,
            replication_port,
            replication_startup_timeout_secs,
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
            allow_insecure_replication_remote,
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

            // Resolve replication once: explicit CLI values > TOML > defaults.
            // Forward the resolved values through daemon re-exec as well.
            let mut replication = if let Some(path) = &config {
                Config::from_file(path.clone())?.replication
            } else {
                heliosdb_nano::config::ReplicationConfig::default()
            };
            if let Some(value) = replication_role {
                replication.role = value;
            }
            if let Some(value) = replication_port {
                replication.replication_port = value;
            }
            if let Some(value) = replication_startup_timeout_secs {
                replication.startup_timeout_secs = value;
            }
            if let Some(value) = primary_host {
                replication.primary_host = Some(value);
            }
            if let Some(value) = standby_hosts {
                replication.standby_hosts = split_hosts(&value);
            }
            if let Some(value) = observer_hosts {
                replication.observer_hosts = split_hosts(&value);
            }
            if let Some(value) = sync_mode {
                replication.sync_mode = value;
            }
            if let Some(value) = node_id {
                replication.node_id = Some(value);
            }
            // CLI flag only turns the opt-in on; a config-file `true` is kept.
            if allow_insecure_replication_remote {
                replication.allow_insecure_remote = true;
            }
            // Read before `apply` consumes the flags: only an explicit
            // `--physical-replication=false` keeps a primary's physical source off.
            let explicit_physical = physical.physical_replication;
            physical.apply(&mut replication);
            replication.role = replication.role.to_ascii_lowercase();
            replication.sync_mode = replication.sync_mode.to_ascii_lowercase();
            replication.validate()?;
            resolve_physical_listen(&mut replication)?;
            #[cfg(feature = "ha-tier1")]
            auto_enable_physical_source(&mut replication, explicit_physical, memory, &resolved_data_dir)?;
            #[cfg(not(feature = "ha-tier1"))]
            let _ = explicit_physical;
            validate_physical_source(
                &replication.physical,
                replication.token_file.as_deref(),
                &replication.role,
                memory,
                &resolved_data_dir,
            )?;
            if replication.role == "standby" && replication.primary_host.is_none() {
                return Err(Error::config(
                    "standby role requires --primary-host or [replication] primary_host".to_string(),
                ));
            }
            #[cfg(not(feature = "ha-tier1"))]
            if replication.role != "standalone" {
                return Err(Error::config(
                    "replication requires a build with the ha-tier1 feature".to_string(),
                ));
            }
            let http_address = resolve_http_address(
                &heliosdb_nano::api::resolve_http_listen(http_listen.as_deref()),
                http_port,
            )?;
            // Validated above; read before the fields below are moved out.
            let max_standby_lag = replication.max_standby_lag_duration()?;
            let exit_on_unrecoverable = replication.exit_on_unrecoverable();
            // Build HA configuration
            let ha_config = HAConfig {
                physical: replication.physical,
                role: replication.role,
                replication_port: replication.replication_port,
                startup_timeout_secs: replication.startup_timeout_secs,
                primary_host: replication.primary_host,
                standby_hosts: Some(replication.standby_hosts.join(",")),
                observer_hosts: Some(replication.observer_hosts.join(",")),
                sync_mode: replication.sync_mode,
                http_port: http_address.map_or(0, |address| address.port()),
                http_listen: http_address.map(|address| address.ip().to_string()),
                mcp_token,
                allow_remote_mcp,
                allow_insecure_http,
                allow_insecure_trust,
                node_id: replication.node_id,
                token_file: replication.token_file,
                allow_insecure_remote: replication.allow_insecure_remote,
                max_standby_lag,
                exit_on_unrecoverable,
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
        Commands::Resync(args) => run_physical_resync(args).await,
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

    validate_physical_source(
        &ha_config.physical,
        ha_config.token_file.as_deref(),
        &ha_config.role,
        memory_mode,
        &data_dir,
    )?;
    let startup_time = Instant::now();
    let guarded_standby = ha_config.role.eq_ignore_ascii_case("standby");
    if guarded_standby && (mysql_enabled || mysql_socket.is_some()) {
        return Err(Error::config(
            "history-guarded standby does not support MySQL listeners; remove --mysql/--mysql-socket and connect to the primary for writes",
        ));
    }
    // Spec P1.3: a standby is a physical replica. These are configuration
    // errors, refused before anything starts — unlike a replication refusal,
    // which the standby survives (`[replication] on_unrecoverable`).
    if guarded_standby {
        if memory_mode {
            return Err(Error::config(
                "a physical standby requires persistent storage: remove --memory and give it a --data-dir",
            ));
        }
        if ha_config.token_file.is_none() {
            return Err(Error::config(
                "a physical standby requires --replication-token-file or [replication] token_file: the token its \
                 primary's physical source accepts",
            ));
        }
        if dump_on_shutdown {
            return Err(Error::config(
                "--dump-on-shutdown is not supported on a standby: its data directory holds only the replicated \
                 store, and a dump file there would stop it from reopening; dump the primary instead",
            ));
        }
    }

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

    db_config.replication = heliosdb_nano::config::ReplicationConfig {
        physical: ha_config.physical.clone(),
        role: ha_config.role.clone(),
        replication_port: ha_config.replication_port,
        startup_timeout_secs: ha_config.startup_timeout_secs,
        primary_host: ha_config.primary_host.clone(),
        standby_hosts: split_hosts(ha_config.standby_hosts.as_deref().unwrap_or("")),
        observer_hosts: split_hosts(ha_config.observer_hosts.as_deref().unwrap_or("")),
        sync_mode: ha_config.sync_mode.clone(),
        node_id: ha_config.node_id.clone(),
        token_file: ha_config.token_file.clone(),
        allow_insecure_remote: ha_config.allow_insecure_remote,
        // TOML-only settings: keep the values the config file loaded above.
        logical_wal_retention_entries: db_config.replication.logical_wal_retention_entries,
        max_standby_lag: db_config.replication.max_standby_lag.clone(),
        on_unrecoverable: db_config.replication.on_unrecoverable.clone(),
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
        // `http_listen` is stored as a bare IP string (`SocketAddr::ip().to_string()`
        // drops IPv6 brackets), so recombine it with the explicit port through the
        // parser rather than `format!("{host}:{port}")` — the latter turns `::1` +
        // `8080` into the address `::1:8080` instead of a socket.
        let http_bind = heliosdb_nano::api::resolve_http_listen(ha_config.http_listen.as_deref());
        let addr: SocketAddr = parse_listen_address(&http_bind, Some(ha_config.http_port), 8080)?;
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

    // Configure PostgreSQL server
    let pg_addr: SocketAddr = format!("{listen}:{port}")
        .parse()
        .map_err(|e| Error::config(format!("Invalid listen address: {e}")))?;

    // Bind PostgreSQL before the database opens and before HA starts: an
    // occupied port — or an overlapping replication endpoint — fails here,
    // synchronously, before RocksDB is locked or a standby starts copying.
    // Nothing is accepted until the serving loop below starts.
    let pg_listener = tokio::net::TcpListener::bind(pg_addr)
        .await
        .map_err(|e| Error::network(format!("Failed to bind PostgreSQL listener {pg_addr}: {e}")))?;

    // Spec P1.3: a standby never opens its data directory as an ordinary,
    // writable database. Its start path (`StandbyRuntime`) bootstraps a
    // physical copy from the primary when the directory does not exist yet,
    // follows the primary through the physical receiver, and serves the
    // applied store read-only; the listeners open once it streams, or when
    // `replication.startup_timeout_secs` runs out — then in whatever posture
    // it has, refusing data reads (SQLSTATE 55000) until it is ready.
    #[cfg(feature = "ha-tier1")]
    let mut standby_runtime: Option<StandbyRuntime> = None;
    #[cfg(feature = "ha-tier1")]
    let standby_db: Option<Arc<EmbeddedDatabase>> = if guarded_standby {
        println!("[2/4] Starting physical standby at {}...", data_dir.display());
        let runtime = StandbyRuntime::start(
            &ha_config,
            &data_dir,
            &db_config,
            &pg_addr.ip().to_string(),
            pg_addr.port(),
        )?;
        let gate = runtime
            .open_gate(std::time::Duration::from_secs(ha_config.startup_timeout_secs))
            .await;
        match gate {
            Ok(GateOutcome::Open) => {}
            Ok(GateOutcome::Shutdown) => {
                runtime.shutdown().await;
                println!("Server shutdown complete. Goodbye!");
                return Ok(());
            }
            Err(error) => {
                runtime.shutdown().await;
                return Err(error);
            }
        }
        let db = runtime.listener_database()?;
        standby_runtime = Some(runtime);
        Some(db)
    } else {
        None
    };
    #[cfg(not(feature = "ha-tier1"))]
    let standby_db: Option<Arc<EmbeddedDatabase>> = None;

    // Open database (in-memory mode avoids disk I/O for all operations)
    let db = if let Some(db) = standby_db {
        db
    } else if memory_mode {
        println!("[2/4] Initializing in-memory database...");
        Arc::new(EmbeddedDatabase::with_config(db_config)?)
    } else {
        println!("[2/4] Initializing database at {}...", data_dir.display());
        Arc::new(EmbeddedDatabase::with_config(db_config)?)
    };
    println!("      Database initialized successfully");

    // Spec P1.3: what a primary (or observer) reports to the SQL probes and
    // `/health` — its written physical sequence and its physical endpoint.
    #[cfg(feature = "ha-tier1")]
    if !guarded_standby && !ha_config.role.eq_ignore_ascii_case("standalone") {
        heliosdb_nano::standby_posture::install_primary(heliosdb_nano::standby_posture::PrimaryStatus {
            role: if ha_config.role.eq_ignore_ascii_case("observer") {
                "observer"
            } else {
                "primary"
            },
            engine: Arc::downgrade(&db.storage),
            endpoint: ha_config.physical.enabled.then(|| ha_config.physical.listen.clone()),
            sync_mode: ha_config.sync_mode.clone(),
        });
    }

    // Build server config
    let mut pg_config = PgServerConfig::with_address(pg_addr)
        .with_auth_method(auth_method)
        .with_allow_insecure_trust(ha_config.allow_insecure_trust)
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
        println!("      - History startup timeout: {}s", ha_config.startup_timeout_secs);
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
        if ha_config.physical.enabled {
            println!("      - Physical source: {}", ha_config.physical.listen);
        }
        if guarded_standby {
            println!("      - Physical primary: {}", ha_config.physical.listen);
            println!(
                "      - Max standby lag: {}",
                banner_duration(ha_config.max_standby_lag)
            );
            println!(
                "      - On unrecoverable: {}",
                if ha_config.exit_on_unrecoverable {
                    "exit"
                } else {
                    "stay-up"
                }
            );
        }
        println!("      - HTTP health port: {}", ha_config.http_port);
    }

    // Create PostgreSQL server with appropriate auth configuration
    // Share the single AuthManager built before startup with the wire listener.
    // (Its listener was bound before the database opened, above.)
    let pg_server = PgServer::with_shared_auth_manager(pg_config, Arc::clone(&db), Arc::clone(&auth_manager))?;

    // The database every new connection is served by. Fixed, except on a
    // standby whose listeners opened on the placeholder before its store was
    // attached: then it changes once, to the store (spec P1.3).
    let (serving_db_tx, serving_db_rx) = tokio::sync::watch::channel(Arc::clone(&db));
    #[cfg(feature = "ha-tier1")]
    let serving_forward = standby_runtime
        .as_ref()
        .map(|runtime| runtime.publish_store(serving_db_tx.clone()));
    drop(serving_db_tx);

    // Start HA components if enabled. A standby's replication belongs to its
    // `StandbyRuntime` (above); it has no native HA task of its own here.
    #[cfg(feature = "ha-tier1")]
    let mut ha_handles = if ha_role == "standalone" || guarded_standby {
        HAHandles::default()
    } else {
        start_ha_components(
            &ha_config,
            &pg_addr.ip().to_string(),
            pg_addr.port(),
            db.storage.clone(),
        )
        .await?
    };

    // Bind physical source before publishing any readiness. Its directly owned
    // future is polled alongside PostgreSQL only after startup succeeds.
    #[cfg(feature = "ha-tier1")]
    let physical_source =
        prepare_physical_source(&ha_config.physical, ha_config.token_file.as_deref(), &db.storage).await?;

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
    // opened (P0-3/P0-4); only the bind happens here. A history-guarded
    // standby binds the same listener but serves only health/version routes.
    let http_handle: Option<tokio::task::JoinHandle<()>> = if let Some(http_addr) = http_addr {
        match tokio::net::TcpListener::bind(http_addr).await {
            Ok(listener) => {
                info!(
                    "HTTP endpoint at http://{http_addr} (/health, /version, /docs, /rest/v1, /auth/v1, /realtime/v1)"
                );
                if guarded_standby {
                    info!("HTTP endpoint is history-guarded standby health-only: {guarded_standby}");
                }
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
                // Spec P1.3: a replication node's `/health` carries its
                // replication block and `/ready` answers its readiness.
                let replication_status: Option<heliosdb_nano::api::ReplicationStatus> =
                    (ha_role != "standalone").then(|| {
                        let status: heliosdb_nano::api::ReplicationStatus = std::sync::Arc::new(|| {
                            let block = heliosdb_nano::standby_posture::node_block()?;
                            let ready = block.ready;
                            Some((ready, serde_json::to_value(block).ok()?))
                        });
                        status
                    });
                if replication_status.is_some() {
                    info!("HTTP readiness at http://{http_addr}/ready (200 when ready, 503 otherwise)");
                }
                Some(tokio::spawn(async move {
                    if let Err(e) = run_http_listener_with_replication(
                        listener,
                        http_db,
                        mcp_token,
                        allow_remote_mcp,
                        http_api_config,
                        policy,
                        guarded_standby,
                        replication_status,
                    )
                    .await
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
        // Read per connection, like the TCP listener: a standby's store can
        // replace its placeholder after this listener opened (spec P1.3).
        let pg_db = serving_db_rx.clone();
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
                        let db_clone = Arc::clone(&pg_db.borrow());
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

    #[cfg(feature = "ha-tier1")]
    if let Err(error) = ha_handles.ensure_running().await {
        for handle in [&http_handle, &mysql_handle, &mysql_unix_handle, &pg_unix_handle]
            .into_iter()
            .flatten()
        {
            handle.abort();
        }
        ha_handles.shutdown().await;
        return Err(error);
    }

    // Daemon parents must observe completion of this child's startup, not
    // merely a successful TCP connect to a prebound socket or another process.
    if let Some(path) = std::env::var_os("HELIOSDB_NANO_READY_FILE") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| Error::io(format!("Cannot acknowledge daemon readiness: {e}")))?;
        write!(file, "{}", std::process::id()).map_err(|e| Error::io(format!("Cannot write daemon readiness: {e}")))?;
    }

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

    // Start server with graceful shutdown handling. The health
    // server runs in its own detached task (see above) so a failure
    // there cannot tear the database listener down.
    #[cfg(feature = "ha-tier1")]
    let (physical_stop, physical_shutdown) = tokio::sync::watch::channel(false);
    #[cfg(feature = "ha-tier1")]
    let physical_service = serve_physical_source(physical_source, physical_shutdown);
    #[cfg(feature = "ha-tier1")]
    tokio::pin!(physical_service);
    #[cfg(feature = "ha-tier1")]
    let mut physical_finished = false;

    let server_result: Result<()> = tokio::select! {
        biased;
        // A standby's refusal ends serving only with `on_unrecoverable =
        // "exit"`; by default it stays up in `needs_recovery` and this arm
        // never completes (spec P1.3, field finding J2).
        error = async {
            #[cfg(feature = "ha-tier1")]
            {
                match standby_runtime.as_ref() {
                    Some(runtime) => runtime.unrecoverable().await,
                    None => ha_handles.failure().await,
                }
            }
            #[cfg(not(feature = "ha-tier1"))]
            { std::future::pending::<Error>().await }
        } => {
            tracing::error!("Replication stopped; closing database listeners: {error}");
            Err(error)
        }
        result = async {
            #[cfg(feature = "ha-tier1")]
            { (&mut physical_service).await }
            #[cfg(not(feature = "ha-tier1"))]
            { std::future::pending::<Result<()>>().await }
        } => {
            #[cfg(feature = "ha-tier1")]
            { physical_finished = true; }
            result.and_then(|()| Err(Error::network("physical source stopped unexpectedly")))
        },
        result = async {
            if guarded_standby {
                pg_server.serve_with_listener_dynamic(pg_listener, serving_db_rx.clone()).await
            } else {
                pg_server.serve_with_listener(pg_listener).await
            }
        } => result,
        signal = heliosdb_nano::wait_for_shutdown_signal() => {
            println!();
            println!("{}", "Received shutdown signal...".yellow());
            tracing::info!("{signal} received — running clean shutdown");
            Ok(())
        }
    };

    // A terminal history failure must leave no detached listener serving this
    // receiver. The TCP accept future above is dropped by select; auxiliary
    // accept loops are explicitly aborted on every shutdown/error path.
    for handle in [http_handle, mysql_handle, mysql_unix_handle, pg_unix_handle]
        .into_iter()
        .flatten()
    {
        handle.abort();
        let _ = handle.await;
    }
    #[cfg(feature = "ha-tier1")]
    let server_result = {
        let _ = physical_stop.send(true);
        let cleanup = if physical_finished {
            Ok(())
        } else {
            (&mut physical_service).await
        };
        ha_handles.shutdown().await;
        if let Some(forward) = serving_forward {
            forward.abort();
            let _ = forward.await;
        }
        if let Some(runtime) = standby_runtime.take() {
            runtime.shutdown().await;
        }
        match server_result {
            Err(error) => Err(error),
            Ok(()) => cleanup,
        }
    };
    #[cfg(unix)]
    {
        if let Some(ref path) = mysql_socket {
            let _ = std::fs::remove_file(path);
        }
        if let Some(ref directory) = pg_socket_dir {
            let _ = std::fs::remove_file(directory.join(format!(".s.PGSQL.{port}")));
        }
    }
    // Do not prolong a poisoned receiver's process lifetime with shutdown dumps.
    server_result?;

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
    // The plaintext-remote opt-ins are not secrets and must reach the child.
    if ha_config.allow_insecure_trust {
        args.push("--allow-insecure-trust".to_string());
    }
    if ha_config.allow_insecure_remote {
        args.push("--allow-insecure-replication-remote".to_string());
    }
    args.push("--auth".to_string());
    args.push(auth.clone());
    let child_password: Option<String> = daemon_password_handoff(&password, &mut args);

    append_physical_daemon_args(&mut args, &ha_config.physical, ha_config.token_file.as_deref());

    // Add HA options
    args.push("--replication-role".to_string());
    args.push(ha_config.role.clone());
    args.push("--replication-port".to_string());
    args.push(ha_config.replication_port.to_string());
    args.push("--replication-startup-timeout-secs".to_string());
    args.push(ha_config.startup_timeout_secs.to_string());
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

        let ready_dir =
            tempfile::tempdir().map_err(|e| Error::io(format!("Cannot create daemon readiness directory: {e}")))?;
        let ready_file = ready_dir.path().join("ready");
        let mut command = Command::new(&exe);
        command
            .args(&args)
            .env("HELIOSDB_NANO_READY_FILE", &ready_file)
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

        // Only this child's explicit acknowledgment establishes readiness.
        let readiness_timeout = daemon_readiness_timeout(&ha_config.role, ha_config.startup_timeout_secs);
        let deadline = std::time::Instant::now() + readiness_timeout;
        loop {
            // Did the child exit prematurely?
            match child.try_wait() {
                Ok(Some(status)) => {
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
            if std::fs::read_to_string(&ready_file).is_ok_and(|value| value == pid.to_string()) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                // Don't leave a zombie if we time out — kill the child
                // and fail loudly. Caller can retry.
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&pid_file);
                return Err(Error::io(format!(
                    "Daemon did not acknowledge listener readiness within {}s. Run in the foreground to inspect configuration and listener errors.", readiness_timeout.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // Do not leave an untracked daemon behind if its PID cannot be recorded.
        if let Err(error) = std::fs::write(&pid_file, pid.to_string()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::io(format!("Failed to write PID file: {error}")));
        }
        drop(child);
        if ha_config.allow_insecure_trust && auth.eq_ignore_ascii_case("trust") {
            eprintln!("INSECURE DEVELOPMENT OVERRIDE: daemon may permit unauthenticated non-loopback database access (--allow-insecure-trust).");
        }
        if ha_config.allow_insecure_remote && ha_config.token_file.is_some() {
            eprintln!("INSECURE REPLICATION OVERRIDE: daemon replication token may cross the network in plaintext (--allow-insecure-replication-remote).");
        }

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

/// Owned HA tasks. A transport or applicator failure ends CLI serving.
#[cfg(feature = "ha-tier1")]
#[derive(Default)]
struct HAHandles {
    replication_handle: Option<tokio::task::JoinHandle<Result<()>>>,
    forwarding_handle: Option<tokio::task::JoinHandle<Result<()>>>,
    applicator: Option<std::sync::Arc<heliosdb_nano::replication::wal_applicator::WalApplicator>>,
    client_status: Option<tokio::sync::watch::Receiver<heliosdb_nano::replication::streaming::StreamingClientStatus>>,
    _ephemeral_wal: Option<tempfile::TempDir>,
}

#[cfg(feature = "ha-tier1")]
impl HAHandles {
    fn status_error(&self) -> Option<Error> {
        use heliosdb_nano::replication::streaming::StreamingClientStatus;
        match self.client_status.as_ref().map(|status| status.borrow().clone()) {
            Some(StreamingClientStatus::Failed { error }) => Some(Error::network(format!(
                "Replication history verification failed: {error}"
            ))),
            Some(StreamingClientStatus::Stopped) => Some(Error::network("Replication client stopped")),
            _ => None,
        }
    }

    async fn ensure_running(&mut self) -> Result<()> {
        if let Some(error) = self.status_error() {
            return Err(error);
        }
        if self.replication_handle.as_ref().is_some_and(|task| task.is_finished())
            || self.forwarding_handle.as_ref().is_some_and(|task| task.is_finished())
        {
            return Err(self.failure().await);
        }
        Ok(())
    }

    async fn failure(&mut self) -> Error {
        if let Some(error) = self.status_error() {
            return error;
        }
        async fn terminal_status(
            status: &mut Option<
                tokio::sync::watch::Receiver<heliosdb_nano::replication::streaming::StreamingClientStatus>,
            >,
        ) -> Error {
            use heliosdb_nano::replication::streaming::StreamingClientStatus;
            let Some(status) = status else {
                return std::future::pending().await;
            };
            loop {
                let current = status.borrow().clone();
                match current {
                    StreamingClientStatus::Failed { error } => {
                        return Error::network(format!("Replication history verification failed: {error}"))
                    }
                    StreamingClientStatus::Stopped => return Error::network("Replication client stopped"),
                    _ => (),
                }
                if status.changed().await.is_err() {
                    return Error::network("Replication client status closed");
                }
            }
        }

        async fn completed(
            handle: &mut Option<tokio::task::JoinHandle<Result<()>>>,
        ) -> std::result::Result<Result<()>, tokio::task::JoinError> {
            match handle.as_mut() {
                Some(handle) => handle.await,
                None => std::future::pending().await,
            }
        }
        let (transport, result) = tokio::select! {
            biased;
            error = terminal_status(&mut self.client_status) => return error,
            result = completed(&mut self.replication_handle) => (true, result),
            result = completed(&mut self.forwarding_handle) => (false, result),
        };
        let name = if transport {
            let _ = self.replication_handle.take();
            "replication transport"
        } else {
            let _ = self.forwarding_handle.take();
            "replication applicator forwarding"
        };
        if let Some(error) = self.status_error() {
            return error;
        }
        match result {
            Ok(Err(error)) => error,
            Ok(Ok(())) => Error::network(format!("{name} stopped unexpectedly")),
            Err(error) => Error::network(format!("{name} task failed: {error}")),
        }
    }

    async fn shutdown(&mut self) {
        for task in [&mut self.replication_handle, &mut self.forwarding_handle] {
            if let Some(handle) = task.take() {
                handle.abort();
                let _ = handle.await;
            }
        }
        if let Some(applicator) = self.applicator.take() {
            let _ = applicator.stop().await;
        }
    }
}

#[cfg(feature = "ha-tier1")]
impl Drop for HAHandles {
    fn drop(&mut self) {
        // Also cover a later startup validation error before the serving select.
        if let Some(handle) = &self.replication_handle {
            handle.abort();
        }
        if let Some(handle) = &self.forwarding_handle {
            handle.abort();
        }
    }
}

/// What every replication role starts from: this node's operational ID, the
/// token both replication channels present, and the native sync mode.
#[cfg(feature = "ha-tier1")]
struct HaNode {
    node_id: uuid::Uuid,
    auth: Option<heliosdb_nano::replication::physical_wire::TransferToken>,
    sync_mode: heliosdb_nano::replication::SyncModeConfig,
}

/// Resolve [`HaNode`] and publish this node's configuration to `ha_state()`
/// (the replication system views read it). Shared by the primary/observer
/// start path and the physical standby's (`StandbyRuntime`).
#[cfg(feature = "ha-tier1")]
fn configure_ha_node(ha_config: &HAConfig, listen: &str, port: u16) -> Result<HaNode> {
    use heliosdb_nano::replication::{
        ha_state::{ha_state, HARole, NodeConfig, SyncMode as HASyncMode},
        SyncModeConfig,
    };
    use uuid::Uuid;

    let node_id = if let Some(ref id) = ha_config.node_id {
        Uuid::parse_str(id).map_err(|e| Error::config(format!("Invalid node ID: {e}")))?
    } else {
        Uuid::new_v4()
    };

    info!("Starting HA components with node ID: {}", node_id);

    // One token file authenticates both replication channels. Load it once here
    // (it is validated by the owner-only loader) and present the same bytes on
    // the native channel as the physical channel.
    let replication_auth = match &ha_config.token_file {
        Some(path) => Some(heliosdb_nano::replication::read_replication_token(path)?),
        None => None,
    };

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
    Ok(HaNode {
        node_id,
        auth: replication_auth,
        sync_mode,
    })
}

/// Start HA replication components of a primary or an observer. A standby's
/// replication is its `StandbyRuntime`'s (spec P1.3).
#[cfg(feature = "ha-tier1")]
async fn start_ha_components(
    ha_config: &HAConfig,
    listen: &str,
    port: u16,
    storage: std::sync::Arc<heliosdb_nano::storage::StorageEngine>,
) -> Result<HAHandles> {
    use heliosdb_nano::replication::{
        streaming::{StreamingServer, StreamingServerConfig},
        wal_store::{WalStore, WalStoreConfig},
    };
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    let HaNode {
        node_id,
        auth: replication_auth,
        sync_mode,
    } = configure_ha_node(ha_config, listen, port)?;
    let role = ha_config.role.to_lowercase();

    match role.as_str() {
        "primary" => {
            // RocksDB is already exclusively opened. Dataset history is durable
            // and deliberately independent of the configured operational node ID.
            let history_id = if storage.config().storage.memory_only {
                Uuid::new_v4()
            } else {
                let directory = storage
                    .config()
                    .storage
                    .path
                    .as_ref()
                    .ok_or_else(|| Error::config("persistent replication requires a data directory"))?;
                heliosdb_nano::replication::history::load_or_create_primary_history(directory)
                    .map_err(|error| Error::config(format!("Cannot load primary history: {error}")))?
            };
            // Start streaming server for primary
            let repl_addr = parse_listen_address(listen, Some(ha_config.replication_port), 5433)?;

            // Never let unrelated nodes share the process working directory's
            // default WAL path. In-memory nodes get an owned temporary lifetime.
            let ephemeral_wal = if storage.config().storage.memory_only {
                Some(
                    tempfile::tempdir()
                        .map_err(|e| Error::io(format!("Cannot create ephemeral replication WAL: {e}")))?,
                )
            } else {
                None
            };
            let wal_dir = if let Some(directory) = &ephemeral_wal {
                directory.path().to_path_buf()
            } else {
                storage
                    .config()
                    .storage
                    .path
                    .as_ref()
                    .ok_or_else(|| Error::config("persistent replication requires a data directory"))?
                    .join("replication_wal")
            };
            let wal_store = Arc::new(WalStore::new(WalStoreConfig {
                wal_dir,
                ..Default::default()
            }));
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
                auth: replication_auth,
                ..Default::default()
            };

            if ha_config.token_file.is_some() && !repl_addr.ip().is_loopback() {
                if !ha_config.allow_insecure_remote {
                    return Err(Error::config(
                        "native replication token is a plaintext bearer; bind --listen to a loopback \
                         address, or set --allow-insecure-replication-remote / \
                         [replication] allow_insecure_remote = true to acknowledge the documented \
                         plaintext risk on a protected tunnel",
                    ));
                }
                tracing::warn!(
                    "INSECURE REPLICATION OVERRIDE: the replication token crosses the network in \
                     PLAINTEXT to {} (--allow-insecure-replication-remote); use a loopback listener or \
                     an authenticated tunnel",
                    repl_addr
                );
            }
            let server = StreamingServer::new(server_config, node_id, history_id, wal_store)
                .map_err(|error| Error::config(format!("Cannot configure replication history: {error}")))?;
            // Catch-up is served from the persisted logical WAL, never from the
            // broadcast channel, so a standby that was down or lagged gets every
            // committed entry it missed (or a terminal full-resync refusal).
            let server = server.with_catchup_source(Arc::new(
                heliosdb_nano::replication::streaming::StorageCatchupSource(storage.clone()),
            ));
            info!("Streaming replication server starting on {}", repl_addr);

            let listener = server.bind_listener().await.map_err(|e| Error::network(format!(
                "Cannot bind native replication listener {repl_addr}: {e}. Use distinct PostgreSQL --port and --replication-port endpoints; --primary-host must point to the primary native replication port."
            )))?;
            // Only detach after binding has succeeded.
            let handle = tokio::spawn(async move {
                server
                    .start_with_listener(listener)
                    .await
                    .map_err(|error| Error::network(format!("Streaming server failed: {error}")))
            });

            Ok(HAHandles {
                replication_handle: Some(handle),
                _ephemeral_wal: ephemeral_wal,
                forwarding_handle: None,
                applicator: None,
                client_status: None,
            })
        }
        // Spec P1.3: a standby is a physical replica started by `StandbyRuntime`;
        // the native channel survives there as its control plane only.
        "standby" => Err(Error::config(
            "a standby is started by the physical standby start path (StandbyRuntime), not as a native HA component",
        )),

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

// ========== Physical standby start path (spec P1.3) ==========
//
// `start --replication-role standby` runs a physical replica
// (docs/plans/specs/P1_3_standby_start_posture_pgwire.md):
//
// 1. the data directory is classified ([`classify_standby_directory`]): an
//    installed physical standby is resumed at its durable cursor; an absent
//    (or empty) one is bootstrapped — `run_receiver(InstallOnce)` downloads a
//    verified copy from the primary's physical source and renames it into
//    place, retried while the primary is unreachable; anything else (a 4.41.x
//    logical standby, a primary's directory, unrelated data) is refused;
// 2. the installed copy is followed and served: `run_serving_receiver` reopens
//    it and, on its owner thread, builds `StorageEngine::open_physical_standby`
//    → `EmbeddedDatabase::with_physical_standby` → `PhysicalApplier` over the
//    stage's writer ([`standby_serving_attach`]); every batch is applied
//    through that applier. Transport failures are retried for as long as the
//    standby runs;
// 3. the posture (`standby_posture`) follows the receiver's progress and
//    decides readiness; the listeners open once it streams, or when
//    `replication.startup_timeout_secs` runs out, whatever the posture;
// 4. a refusal leaves the standby up in `needs_recovery` (or exits, with
//    `[replication] on_unrecoverable = "exit"`);
// 5. the native v3 channel stays as the control plane only — handshake and
//    history, heartbeats, acknowledgements — with the logical WAL batches the
//    primary still sends drained and dropped ([`run_native_control_plane`]).

/// The refusal a 4.41.x logical standby's directory gets (spec P1.3).
#[cfg(feature = "ha-tier1")]
const LEGACY_LOGICAL_STANDBY_REFUSAL: &str = "this directory is a logical standby from 4.41.x; physical standbys are \
     rebuilt once: start with a new, empty data directory (docs/guides/physical-standby.md)";

/// How long a failed bootstrap waits before it asks the primary again.
#[cfg(feature = "ha-tier1")]
const STANDBY_BOOTSTRAP_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
/// A serving standby's receiver cadence: poll interval, reconnect delay, and
/// the bytes one poll may carry.
#[cfg(feature = "ha-tier1")]
const STANDBY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
#[cfg(feature = "ha-tier1")]
const STANDBY_RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(feature = "ha-tier1")]
const STANDBY_POLL_BYTES: usize = 8 * 1024 * 1024;

/// What a standby's data directory is (see the section comment).
#[cfg(feature = "ha-tier1")]
#[derive(Debug)]
enum StandbyLayout {
    /// Absent, or an existing EMPTY directory: install a copy from the primary.
    Fresh { empty_existing: bool },
    /// An installed physical standby following `history`.
    Physical { history: uuid::Uuid },
    /// Never adopted; the reason names what was found.
    Refused(String),
}

/// Classify a standby's data directory without changing it.
///
/// An existing empty directory counts as fresh (the copy is renamed into its
/// place, so it is removed first — an operator who `mkdir`s the path, or a
/// volume's empty leaf, must not be refused); a non-empty one that is not a
/// physical standby is never adopted.
#[cfg(feature = "ha-tier1")]
fn classify_standby_directory(path: &std::path::Path) -> StandbyLayout {
    use heliosdb_nano::replication::checkpoint_transfer::{physical_stage_history, PHYSICAL_MARKER_FILE};
    use heliosdb_nano::replication::history::{PRIMARY_HISTORY_FILE, STANDBY_BINDING_FILE};
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return StandbyLayout::Fresh { empty_existing: false }
        }
        Err(error) => {
            return StandbyLayout::Refused(format!(
                "Cannot prepare standby history: cannot inspect {}: {error}",
                path.display()
            ))
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return StandbyLayout::Refused(format!(
            "Cannot prepare standby history: {} is not a directory",
            path.display()
        ));
    }
    let carries = |name: &str| std::fs::symlink_metadata(path.join(name)).is_ok();
    if carries(PHYSICAL_MARKER_FILE) {
        return match physical_stage_history(path) {
            Ok(history) => StandbyLayout::Physical { history },
            Err(error) => StandbyLayout::Refused(format!(
                "Cannot prepare standby history: the physical standby marker of {} cannot be read: {error}",
                path.display()
            )),
        };
    }
    if carries(STANDBY_BINDING_FILE) {
        return StandbyLayout::Refused(LEGACY_LOGICAL_STANDBY_REFUSAL.to_string());
    }
    if carries(PRIMARY_HISTORY_FILE) {
        return StandbyLayout::Refused(format!(
            "Cannot prepare standby history: {} carries a primary's dataset identity; a standby is never built \
             over a primary's data directory",
            path.display()
        ));
    }
    match std::fs::read_dir(path).map(|mut entries| entries.next().is_none()) {
        Ok(true) => StandbyLayout::Fresh { empty_existing: true },
        Ok(false) => StandbyLayout::Refused(format!(
            "Cannot prepare standby history: existing directory {} is not a physical standby; preserve it and \
             start the standby with a new, empty data directory",
            path.display()
        )),
        Err(error) => StandbyLayout::Refused(format!(
            "Cannot prepare standby history: cannot list {}: {error}",
            path.display()
        )),
    }
}

/// The native control channel's settings.
#[cfg(feature = "ha-tier1")]
#[derive(Clone)]
struct NativeControlPlan {
    primary_host: String,
    node_id: uuid::Uuid,
    sync_mode: heliosdb_nano::replication::SyncModeConfig,
    auth: Option<heliosdb_nano::replication::physical_wire::TransferToken>,
    connect_timeout: std::time::Duration,
}

/// Everything the standby's supervisor needs.
#[cfg(feature = "ha-tier1")]
struct StandbyPlan {
    data_dir: PathBuf,
    /// The node's configuration (storage path = `data_dir`): the receiver's
    /// RocksDB options and the serving database's.
    config: Config,
    /// The primary's physical source (`[replication.physical] listen`).
    source: std::net::SocketAddr,
    token: heliosdb_nano::replication::physical_wire::TransferToken,
    wire: heliosdb_nano::replication::physical_wire::WireLimits,
    native: Option<NativeControlPlan>,
    tracker: std::sync::Arc<heliosdb_nano::standby_posture::PostureTracker>,
}

#[cfg(feature = "ha-tier1")]
impl StandbyPlan {
    fn receiver_options(
        &self,
        start: heliosdb_nano::replication::physical_receiver::ReceiverStart,
        mode: heliosdb_nano::replication::physical_receiver::ReceiverMode,
        max_reconnects: u32,
    ) -> heliosdb_nano::replication::physical_receiver::ReceiverOptions {
        heliosdb_nano::replication::physical_receiver::ReceiverOptions {
            config: self.config.clone(),
            sources: vec![self.source],
            token: self.token.clone(),
            start,
            mode,
            wire: self.wire,
            poll_byte_limit: STANDBY_POLL_BYTES,
            poll_interval: STANDBY_POLL_INTERVAL,
            reconnect_delay: STANDBY_RECONNECT_DELAY,
            max_reconnects,
        }
    }

    /// Record a refusal: posture `needs_recovery`, logged once, and reported
    /// to the start path (which exits only with `on_unrecoverable = "exit"`).
    fn refuse(&self, refusal: &tokio::sync::watch::Sender<Option<String>>, reason: String) {
        tracing::error!("physical standby needs recovery: {reason}");
        self.tracker.needs_recovery(reason.clone());
        refusal.send_replace(Some(reason));
    }
}

/// How the start path's readiness gate ended.
#[cfg(feature = "ha-tier1")]
enum GateOutcome {
    /// Open the listeners (ready, refused-but-staying-up, or out of time).
    Open,
    /// A shutdown signal arrived first.
    Shutdown,
}

/// A physical standby's runtime, owned by `start_server` (see the section
/// comment). Dropping it asks the supervisor to stop.
#[cfg(feature = "ha-tier1")]
struct StandbyRuntime {
    posture: heliosdb_nano::standby_posture::PostureHandle,
    /// The serving database, once the store is attached.
    store: tokio::sync::watch::Receiver<Option<std::sync::Arc<EmbeddedDatabase>>>,
    /// A refusal the receiver cannot resolve by itself.
    refusal: tokio::sync::watch::Receiver<Option<String>>,
    /// The native control channel verified the primary's history (or is
    /// known not to come up); part of the readiness gate only.
    control_plane: tokio::sync::watch::Receiver<bool>,
    stop: tokio::sync::watch::Sender<bool>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    exit_on_unrecoverable: bool,
}

#[cfg(feature = "ha-tier1")]
impl StandbyRuntime {
    /// Install the standby guard and posture, and start the supervisor.
    fn start(
        ha_config: &HAConfig,
        data_dir: &std::path::Path,
        config: &Config,
        listen: &str,
        port: u16,
    ) -> Result<Self> {
        use heliosdb_nano::standby_posture::{install_standby, PostureTracker};
        // Every PostgreSQL session of this process is a standby session: the
        // write guard (25006) holds in every posture.
        heliosdb_nano::replication::ha_state::ha_state().disable_forwarding();
        let node = configure_ha_node(ha_config, listen, port)?;
        let token = node
            .auth
            .clone()
            .ok_or_else(|| Error::config("a physical standby requires --replication-token-file"))?;
        let source: std::net::SocketAddr = ha_config.physical.listen.parse().map_err(|_| {
            Error::config(format!(
                "replication.physical.listen '{}' must be a literal loopback IP:port naming the primary's physical source",
                ha_config.physical.listen
            ))
        })?;
        if !source.ip().is_loopback() {
            return Err(Error::config(
                "a physical standby follows a loopback physical source; reach a remote primary through an \
                 authenticated tunnel",
            ));
        }
        let tracker = std::sync::Arc::new(PostureTracker::new(
            source.to_string(),
            ha_config.sync_mode.clone(),
            ha_config.max_standby_lag,
        ));
        let posture = tracker.handle();
        install_standby(tracker.handle());
        let native = ha_config.primary_host.clone().map(|primary_host| NativeControlPlan {
            primary_host,
            node_id: node.node_id,
            sync_mode: node.sync_mode,
            auth: node.auth.clone(),
            connect_timeout: std::time::Duration::from_secs(ha_config.startup_timeout_secs),
        });
        info!(
            "Physical standby following {} (native control channel {})",
            source,
            ha_config.primary_host.as_deref().unwrap_or("none")
        );
        let plan = StandbyPlan {
            data_dir: data_dir.to_path_buf(),
            config: config.clone(),
            source,
            token,
            wire: physical_wire_limits(&ha_config.physical),
            native,
            tracker,
        };
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let (store_tx, store) = tokio::sync::watch::channel(None);
        let (refusal_tx, refusal) = tokio::sync::watch::channel(None);
        let (control_tx, control_plane) = tokio::sync::watch::channel(false);
        let supervisor = tokio::spawn(run_physical_standby(plan, store_tx, refusal_tx, control_tx, shutdown));
        Ok(Self {
            posture,
            store,
            refusal,
            control_plane,
            stop,
            supervisor: Some(supervisor),
            exit_on_unrecoverable: ha_config.exit_on_unrecoverable,
        })
    }

    /// Wait until the standby streams (with its control channel verified),
    /// a refusal arrives, `budget` runs out, or a shutdown signal arrives.
    /// Only a refusal with `on_unrecoverable = "exit"` is an error: in every
    /// other case the listeners open, and the posture decides what they serve.
    async fn open_gate(&self, budget: std::time::Duration) -> Result<GateOutcome> {
        use heliosdb_nano::standby_posture::format_lsn;
        let deadline = tokio::time::Instant::now() + budget;
        let signal = heliosdb_nano::wait_for_shutdown_signal();
        tokio::pin!(signal);
        loop {
            if let Some(reason) = self.refusal.borrow().clone() {
                if self.exit_on_unrecoverable {
                    return Err(unrecoverable_exit(&reason));
                }
                tracing::warn!(
                    "physical standby stays up in posture needs_recovery (on_unrecoverable = \"stay-up\"); its \
                     listeners open and refuse data reads with SQLSTATE 55000"
                );
                return Ok(GateOutcome::Open);
            }
            let snapshot = self.posture.snapshot();
            if snapshot.ready() && *self.control_plane.borrow() {
                info!(
                    "Physical standby streaming from {} at {} (primary at {})",
                    snapshot.primary,
                    format_lsn(snapshot.cursor),
                    format_lsn(snapshot.primary_cursor)
                );
                return Ok(GateOutcome::Open);
            }
            tokio::select! {
                signal = &mut signal => {
                    info!("{signal} received while the physical standby was starting");
                    return Ok(GateOutcome::Shutdown);
                }
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        "physical standby is not ready after {}s (replication.startup_timeout_secs): {}; its \
                         listeners open now and refuse data reads (SQLSTATE 55000) until it is",
                        budget.as_secs(),
                        self.posture.snapshot().not_ready_error()
                    );
                    return Ok(GateOutcome::Open);
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
    }

    /// The database the listeners open with: the serving store, or — while
    /// the standby has none — an empty in-memory placeholder whose sessions
    /// the guard keeps from reading anything (`reject_guarded_standby_statement`).
    fn listener_database(&self) -> Result<std::sync::Arc<EmbeddedDatabase>> {
        if let Some(database) = self.store.borrow().clone() {
            return Ok(database);
        }
        info!(
            "physical standby has no data store yet: its listeners serve an empty placeholder that refuses \
             data reads until the store is attached"
        );
        Ok(std::sync::Arc::new(EmbeddedDatabase::new_in_memory()?))
    }

    /// Hand the store to the listeners once it is attached (at most once).
    fn publish_store(
        &self,
        serving: tokio::sync::watch::Sender<std::sync::Arc<EmbeddedDatabase>>,
    ) -> tokio::task::JoinHandle<()> {
        let mut store = self.store.clone();
        tokio::spawn(async move {
            loop {
                let attached = store.borrow_and_update().clone();
                if let Some(database) = attached {
                    let replaced = !std::sync::Arc::ptr_eq(&serving.borrow(), &database);
                    serving.send_replace(database);
                    if replaced {
                        info!("physical standby store attached: new connections are served from it");
                    }
                    return;
                }
                if store.changed().await.is_err() {
                    return;
                }
            }
        })
    }

    /// Completes only when a refusal arrives and `on_unrecoverable = "exit"`;
    /// a standby that stays up never ends its serving loop this way.
    async fn unrecoverable(&self) -> Error {
        let mut refusal = self.refusal.clone();
        loop {
            let current = refusal.borrow_and_update().clone();
            if let Some(reason) = current {
                if self.exit_on_unrecoverable {
                    return unrecoverable_exit(&reason);
                }
                return std::future::pending().await;
            }
            if refusal.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    /// Stop the supervisor and wait for it: its receiver closes at a command
    /// boundary (an in-flight fsync is not interrupted).
    async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.supervisor.take() {
            if tokio::time::timeout(std::time::Duration::from_secs(60), task)
                .await
                .is_err()
            {
                tracing::warn!("the physical standby receiver did not stop within 60s; exiting anyway");
            }
        }
    }
}

#[cfg(feature = "ha-tier1")]
impl Drop for StandbyRuntime {
    fn drop(&mut self) {
        // An early startup error: ask the supervisor to stop; the process is
        // about to exit with that error.
        let _ = self.stop.send(true);
    }
}

#[cfg(feature = "ha-tier1")]
fn unrecoverable_exit(reason: &str) -> Error {
    Error::network(format!(
        "physical standby refused (on_unrecoverable = \"exit\"): {reason}"
    ))
}

/// Feed every receiver progress report into the posture.
#[cfg(feature = "ha-tier1")]
async fn forward_receiver_progress(
    mut progress: tokio::sync::watch::Receiver<heliosdb_nano::replication::physical_receiver::ReceiverStatus>,
    tracker: std::sync::Arc<heliosdb_nano::standby_posture::PostureTracker>,
) {
    loop {
        let status = progress.borrow_and_update().clone();
        tracker.observe(&status);
        if progress.changed().await.is_err() {
            let status = progress.borrow().clone();
            tracker.observe(&status);
            return;
        }
    }
}

/// Bootstrap failures worth asking the primary again for: it is unreachable,
/// timed out, or failed on its side. An authentication or protocol refusal,
/// and a local storage refusal, are not.
#[cfg(feature = "ha-tier1")]
fn bootstrap_retryable(error: &heliosdb_nano::replication::physical_receiver::ReceiverError) -> bool {
    use heliosdb_nano::replication::physical_receiver::ReceiverError;
    use heliosdb_nano::replication::physical_wire::WireError;
    matches!(
        error,
        ReceiverError::Wire(WireError::Io(_) | WireError::Timeout | WireError::Source(_) | WireError::Poisoned)
    )
}

/// Install a verified copy of the primary at `plan.data_dir`. `Ok(None)` on
/// shutdown; `Err` is a refusal.
#[cfg(feature = "ha-tier1")]
async fn bootstrap_physical_standby(
    plan: &StandbyPlan,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<Option<uuid::Uuid>, String> {
    use heliosdb_nano::replication::physical_receiver::{run_receiver, ReceiverMode, ReceiverStart, ReceiverStatus};
    info!(
        "Bootstrapping physical standby {} from {}: a verified copy is installed, then followed",
        plan.data_dir.display(),
        plan.source
    );
    // Doubles after each failed attempt, up to 30 s: a primary that stays
    // away is asked again, without a warning every second.
    let mut retry = STANDBY_BOOTSTRAP_RETRY;
    loop {
        if *shutdown.borrow() {
            return Ok(None);
        }
        let (progress, observed) = tokio::sync::watch::channel(ReceiverStatus::Connecting {
            history: None,
            after: None,
        });
        let forward = tokio::spawn(forward_receiver_progress(
            observed,
            std::sync::Arc::clone(&plan.tracker),
        ));
        let options = plan.receiver_options(
            ReceiverStart::Snapshot {
                target: plan.data_dir.clone(),
                expected_history: None,
            },
            ReceiverMode::InstallOnce,
            5,
        );
        let result = run_receiver(options, shutdown.clone(), progress).await;
        let _ = forward.await;
        match result {
            Ok(closed) => {
                info!(
                    "Installed physical standby copy at {} (snapshot {}, history {}, source sequence {})",
                    closed.path.display(),
                    closed.snapshot_id,
                    closed.cursor.history(),
                    closed.cursor.source_sequence()
                );
                return Ok(Some(closed.cursor.history()));
            }
            Err(_) if *shutdown.borrow() => return Ok(None),
            Err(error) if bootstrap_retryable(&error) => {
                tracing::warn!(
                    "physical standby bootstrap from {} did not complete ({error}); retrying in {:?}",
                    plan.source,
                    retry
                );
                let mut wait = shutdown.clone();
                tokio::select! {
                    () = tokio::time::sleep(retry) => {}
                    _ = wait.changed() => {}
                }
                retry = (retry * 2).min(std::time::Duration::from_secs(30));
            }
            Err(error) => {
                return Err(format!(
                    "the physical standby bootstrap from {} was refused: {error}",
                    plan.source
                ))
            }
        }
    }
}

/// The serving side of the follow receiver (`physical_receiver::ServingAttach`):
/// the engine over the stage's writer, the SQL layer over the engine (it
/// hydrates it), and the applier that reports to that SQL layer — in that
/// order, the one `EmbeddedDatabase::with_physical_standby` documents. The
/// database is published to the start path only once the applier exists.
#[cfg(feature = "ha-tier1")]
fn standby_serving_attach(
    config: Config,
    tracker: std::sync::Arc<heliosdb_nano::standby_posture::PostureTracker>,
    store: tokio::sync::watch::Sender<Option<std::sync::Arc<EmbeddedDatabase>>>,
) -> heliosdb_nano::replication::physical_receiver::ServingAttach {
    use heliosdb_nano::replication::physical::LocalWriter;
    use heliosdb_nano::replication::physical_applier::{DerivedStateListener, PhysicalApplier};
    use heliosdb_nano::storage::{StorageEngine, WriteGate};
    Box::new(
        move |path: &std::path::Path,
              writer: &std::sync::Arc<LocalWriter>|
              -> std::result::Result<std::sync::Arc<PhysicalApplier>, String> {
            let engine = std::sync::Arc::new(
                StorageEngine::open_physical_standby(
                    path,
                    &config,
                    std::sync::Arc::clone(writer),
                    WriteGate::closed("physical standby"),
                )
                .map_err(|error| error.to_string())?,
            );
            let database = std::sync::Arc::new(
                EmbeddedDatabase::with_physical_standby(config.clone(), std::sync::Arc::clone(&engine))
                    .map_err(|error| error.to_string())?,
            );
            let listener: std::sync::Arc<dyn DerivedStateListener> = database.clone();
            let applier = std::sync::Arc::new(
                PhysicalApplier::new(engine, std::sync::Arc::clone(writer), listener)
                    .map_err(|error| error.to_string())?,
            );
            let live = std::sync::Arc::downgrade(writer);
            tracker.attach_store(
                writer.cursor().source_sequence(),
                std::sync::Arc::new(move || live.upgrade().map(|writer| writer.cursor().source_sequence())),
            );
            info!(
                "Physical standby store at {} attached at source sequence {}",
                path.display(),
                writer.cursor().source_sequence()
            );
            store.send_replace(Some(database));
            Ok(applier)
        },
    )
}

/// The supervisor: classify, bootstrap if needed, then follow and serve until
/// shutdown or a refusal (see the section comment).
#[cfg(feature = "ha-tier1")]
async fn run_physical_standby(
    plan: StandbyPlan,
    store: tokio::sync::watch::Sender<Option<std::sync::Arc<EmbeddedDatabase>>>,
    refusal: tokio::sync::watch::Sender<Option<String>>,
    control_plane: tokio::sync::watch::Sender<bool>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use heliosdb_nano::replication::physical_receiver::{
        run_serving_receiver, ReceiverMode, ReceiverStart, ReceiverStatus,
    };
    let history = match classify_standby_directory(&plan.data_dir) {
        StandbyLayout::Refused(reason) => {
            control_plane.send_replace(true);
            plan.refuse(&refusal, reason);
            return;
        }
        StandbyLayout::Physical { history } => {
            info!(
                "Resuming physical standby {} at its durable cursor (history {history}); no copy is made",
                plan.data_dir.display()
            );
            history
        }
        StandbyLayout::Fresh { empty_existing } => {
            if empty_existing {
                // The verified copy is renamed into this path; an empty
                // directory holds nothing, and `remove_dir` refuses anything else.
                if let Err(error) = std::fs::remove_dir(&plan.data_dir) {
                    control_plane.send_replace(true);
                    plan.refuse(
                        &refusal,
                        format!(
                            "Cannot prepare standby history: the empty data directory {} cannot be replaced by \
                             the standby's verified copy ({error}); give the standby a path that does not exist yet",
                            plan.data_dir.display()
                        ),
                    );
                    return;
                }
            }
            match bootstrap_physical_standby(&plan, &shutdown).await {
                Ok(Some(history)) => history,
                Ok(None) => return,
                Err(reason) => {
                    control_plane.send_replace(true);
                    plan.refuse(&refusal, reason);
                    return;
                }
            }
        }
    };

    let native = match plan.native.clone() {
        Some(native) => {
            let tracker = std::sync::Arc::clone(&plan.tracker);
            let refused = refusal.clone();
            let refuse_history: std::sync::Arc<dyn Fn(String) + Send + Sync> =
                std::sync::Arc::new(move |reason: String| {
                    tracing::error!("physical standby needs recovery: {reason}");
                    tracker.needs_recovery(reason.clone());
                    refused.send_replace(Some(reason));
                });
            Some(tokio::spawn(run_native_control_plane(
                native,
                history,
                store.subscribe(),
                control_plane,
                refuse_history,
                std::sync::Arc::clone(&plan.tracker),
                shutdown.clone(),
            )))
        }
        None => {
            control_plane.send_replace(true);
            None
        }
    };

    let (progress, observed) = tokio::sync::watch::channel(ReceiverStatus::Connecting {
        history: Some(history),
        after: None,
    });
    let forward = tokio::spawn(forward_receiver_progress(
        observed,
        std::sync::Arc::clone(&plan.tracker),
    ));
    let options = plan.receiver_options(
        ReceiverStart::Resume {
            path: plan.data_dir.clone(),
            history,
        },
        ReceiverMode::Follow,
        1000,
    );
    let attach = standby_serving_attach(plan.config.clone(), std::sync::Arc::clone(&plan.tracker), store.clone());
    let result = run_serving_receiver(options, attach, shutdown.clone(), progress).await;
    let _ = forward.await;
    let stopping = *shutdown.borrow();
    match result {
        Ok(closed) => info!(
            "physical standby receiver stopped at source sequence {}",
            closed.cursor.source_sequence()
        ),
        // The serving database still shares the store at shutdown, so the
        // receiver cannot close it; it closes with the process (the receiver
        // fsynced its applied groups before it stopped).
        Err(error) if stopping => tracing::debug!("physical standby receiver stopped: {error}"),
        Err(error) => plan.refuse(&refusal, format!("the physical receiver stopped: {error}")),
    }
    if let Some(native) = native {
        if stopping {
            let _ = native.await;
        } else {
            // No acknowledgement may reach the primary for a standby that no
            // longer applies anything.
            native.abort();
        }
    }
}

/// The primary's logical-WAL position this store holds (`wal:last_lsn`, a
/// plaintext 8-byte counter in the replicated keyspace): what the native
/// handshake asks the primary to continue after, so it sends only what is
/// newer instead of its whole retained logical WAL.
#[cfg(feature = "ha-tier1")]
fn replicated_logical_position(storage: &std::sync::Weak<heliosdb_nano::storage::StorageEngine>) -> u64 {
    storage
        .upgrade()
        .and_then(|engine| engine.db().get(b"wal:last_lsn").ok().flatten())
        .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
        .map_or(0, u64::from_le_bytes)
}

/// The native v3 channel as a control plane only: it verifies the primary's
/// history against this standby's (bound in memory: the physical directory
/// holds no binding file), keeps the primary's heartbeat and acknowledgement
/// state, and drains the logical WAL batches a v3 primary still sends. It
/// never applies anything; the data arrives on the physical channel.
///
/// Its failure is logged and leaves the posture alone — with one exception:
/// a history mismatch, which says the primary is now a different dataset.
/// The physical channel cannot say so itself (its source drops a session it
/// refuses, which reads as a transport failure and is retried), so the
/// native handshake is what turns that case into `needs_recovery`
/// (`refuse_history`). The primary's position it learns is a LOGICAL WAL
/// LSN: it reaches the posture only as `native_primary_lsn`
/// (`PostureTracker::observe_native_handshake`), never as a physical cursor.
#[cfg(feature = "ha-tier1")]
async fn run_native_control_plane(
    native: NativeControlPlan,
    history: uuid::Uuid,
    mut store: tokio::sync::watch::Receiver<Option<std::sync::Arc<EmbeddedDatabase>>>,
    verified: tokio::sync::watch::Sender<bool>,
    refuse_history: std::sync::Arc<dyn Fn(String) + Send + Sync>,
    tracker: std::sync::Arc<heliosdb_nano::standby_posture::PostureTracker>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use heliosdb_nano::replication::history::ReceiverHistory;
    use heliosdb_nano::replication::streaming::{StreamingClient, StreamingClientConfig, StreamingClientStatus};
    let storage = loop {
        if let Some(database) = store.borrow_and_update().clone() {
            break std::sync::Arc::downgrade(&database.storage);
        }
        tokio::select! {
            changed = store.changed() => if changed.is_err() {
                verified.send_replace(true);
                return;
            },
            _ = shutdown.changed() => return,
        }
    };
    let resolved = tokio::time::timeout(native.connect_timeout, tokio::net::lookup_host(&native.primary_host)).await;
    let primary_addr = match resolved {
        Ok(Ok(mut addresses)) => addresses.next(),
        Ok(Err(error)) => {
            tracing::error!("cannot resolve --primary-host {}: {error}", native.primary_host);
            None
        }
        Err(_) => {
            tracing::error!("resolving --primary-host {} timed out", native.primary_host);
            None
        }
    };
    let Some(primary_addr) = primary_addr else {
        tracing::error!(
            "the standby follows its primary without the native control channel: no heartbeats or \
             acknowledgements reach the primary"
        );
        verified.send_replace(true);
        return;
    };
    let guard = std::sync::Arc::new(ReceiverHistory::in_memory());
    if let Err(error) = guard.verify_or_bind(history) {
        tracing::error!("the native control channel cannot bind history {history}: {error}");
        verified.send_replace(true);
        return;
    }
    let (client, mut entries) = StreamingClient::new(
        StreamingClientConfig {
            node_id: native.node_id,
            primary_addr,
            sync_mode: native.sync_mode,
            connect_timeout: native.connect_timeout,
            reconnect_delay: std::time::Duration::from_secs(5),
            max_reconnect_attempts: 0, // Unlimited
            auth: native.auth,
        },
        guard,
    );
    client.set_applied_source(std::sync::Arc::new(move || replicated_logical_position(&storage)));
    let client = std::sync::Arc::new(client);
    let mut status = client.subscribe_status();
    info!(
        "Native replication control channel connecting to {}",
        native.primary_host
    );
    let drain = tokio::spawn(async move {
        let mut announced = false;
        while let Some(entry) = entries.recv().await {
            if !announced {
                tracing::debug!(
                    lsn = entry.lsn,
                    "the primary sends logical WAL batches on the native channel; a physical standby drops them"
                );
                announced = true;
            }
        }
    });
    let running = std::sync::Arc::clone(&client);
    let task = tokio::spawn(async move { running.start().await });
    let mut announced = false;
    loop {
        let current = status.borrow_and_update().clone();
        match current {
            StreamingClientStatus::Verified { history_id } | StreamingClientStatus::CaughtUp { history_id } => {
                // Logical: kept apart from the physical posture.
                tracker.observe_native_handshake(client.primary_lsn());
                if !announced {
                    info!("Standby dataset history verified: {history_id}");
                    announced = true;
                }
                verified.send_replace(true);
            }
            StreamingClientStatus::Failed { error } => {
                if error.to_ascii_lowercase().contains("history mismatch") {
                    refuse_history(format!(
                        "the primary at {} serves a different dataset: {error}",
                        native.primary_host
                    ));
                } else {
                    tracing::error!(
                        "the native replication control channel stopped: {error}; heartbeats and \
                         acknowledgements to the primary are lost (the physical stream is unaffected)"
                    );
                }
                verified.send_replace(true);
                break;
            }
            StreamingClientStatus::Stopped => break,
            StreamingClientStatus::Connecting | StreamingClientStatus::Reconnecting => {}
        }
        tokio::select! {
            changed = status.changed() => if changed.is_err() { break },
            _ = shutdown.changed() => break,
        }
    }
    client.shutdown();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    drain.abort();
}

/// HTTP listener for Docker health checks, the BaaS HTTP surface, and (when
/// compiled with `mcp-endpoint`) MCP-over-HTTP routes.
///
/// P0-1: `http_auth` wraps the entire assembled router, so every non-health route
/// requires the wire credential when one is configured. The caller owns the
/// already-bound `TcpListener` so bind failures are reported up front
/// (see KanttBan bug #2 / v3.28.0 fix); the non-loopback bind *policy* is enforced
/// in `start_server` before this is reached.
#[cfg_attr(not(test), allow(dead_code))]
async fn run_http_listener(
    listener: tokio::net::TcpListener,
    db: std::sync::Arc<EmbeddedDatabase>,
    mcp_token: Option<String>,
    allow_remote_mcp: bool,
    api_config: heliosdb_nano::config::ApiConfig,
    http_auth: std::sync::Arc<heliosdb_nano::api::HttpAuthPolicy>,
    health_only: bool,
) -> std::io::Result<()> {
    run_http_listener_with_replication(
        listener,
        db,
        mcp_token,
        allow_remote_mcp,
        api_config,
        http_auth,
        health_only,
        None,
    )
    .await
}

/// [`run_http_listener`] for a replication node (spec P1.3): `/health`
/// carries its replication block and `/ready` answers 200 / 503 by
/// readiness (`api::attach_http_auth_with_replication`), on the guarded
/// standby's health-only router and the primary's full one alike.
#[allow(clippy::too_many_arguments)]
async fn run_http_listener_with_replication(
    listener: tokio::net::TcpListener,
    db: std::sync::Arc<EmbeddedDatabase>,
    mcp_token: Option<String>,
    allow_remote_mcp: bool,
    api_config: heliosdb_nano::config::ApiConfig,
    http_auth: std::sync::Arc<heliosdb_nano::api::HttpAuthPolicy>,
    health_only: bool,
    replication: Option<heliosdb_nano::api::ReplicationStatus>,
) -> std::io::Result<()> {
    use axum::{routing::get, Json, Router};

    async fn health() -> Json<serde_json::Value> {
        Json(serde_json::json!({ "status": "ok" }))
    }

    if health_only {
        async fn version() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }))
        }
        let app = Router::new()
            .route("/", get(health))
            .route("/health", get(health))
            .route("/version", get(version));
        // The configured policy still governs `/version` (which reveals the
        // build); only `/` and `/health` (and, on a replication node,
        // `/ready`) stay public because the middleware answers them with a
        // status body. REST/BaaS/MCP are never mounted here, so a valid
        // credential still lands on a 404.
        let app = heliosdb_nano::api::attach_http_auth_with_replication(app, http_auth, replication);
        return axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
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
    let app = heliosdb_nano::api::attach_http_auth_with_replication(app, http_auth, replication);

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
    #[test]
    fn daemon_readiness_includes_full_standby_window() {
        for seconds in [1, 30, 3600] {
            assert_eq!(daemon_readiness_timeout("standby", seconds).as_secs(), seconds + 5);
        }
        assert_eq!(daemon_readiness_timeout("STANDBY", 30).as_secs(), 35);
        for role in ["standalone", "primary", "observer"] {
            assert_eq!(daemon_readiness_timeout(role, 30).as_secs(), 5);
        }
    }

    #[test]
    fn http_listen_literals_ports_and_disable() {
        for (input, port, expected) in [
            ("127.0.0.1", None, "127.0.0.1:8080"),
            ("0.0.0.0:9010", None, "0.0.0.0:9010"),
            ("0.0.0.0:9010", Some(9020), "0.0.0.0:9020"),
            ("::1", None, "[::1]:8080"),
            ("[::1]", Some(9010), "[::1]:9010"),
            ("[::1]:9010", None, "[::1]:9010"),
            ("[::]:9010", Some(9020), "[::]:9020"),
        ] {
            assert_eq!(
                resolve_http_address(input, port).unwrap().unwrap().to_string(),
                expected
            );
        }
        for input in ["127.0.0.1:9010", "invalid", "[::1]:9010"] {
            assert_eq!(resolve_http_address(input, Some(0)).unwrap(), None);
        }
        assert_eq!(resolve_http_address("127.0.0.1:0", None).unwrap(), None);
        for input in ["localhost", "127.0.0.1:80:90", "[::1]:bad", "127.0.0.1:65536", ""] {
            assert!(resolve_http_address(input, None).is_err(), "{input}");
        }
    }

    /// `start_server` stores `http_listen` as a bare IP (`SocketAddr::ip()`
    /// `.to_string()` drops IPv6 brackets) with the port kept separately, then
    /// reassembles the bind. That reassembly must go through
    /// `parse_listen_address`, because `format!("{host}:{port}")` turns a bare
    /// `::1` plus `8080` into the IPv6 address `::1:8080`, not a socket.
    #[test]
    fn http_bind_reassembles_stored_ip_and_port_without_string_concatenation() {
        for (stored, port, expected) in [
            ("127.0.0.1", 8080, "127.0.0.1:8080"),
            ("0.0.0.0", 9010, "0.0.0.0:9010"),
            ("::1", 8080, "[::1]:8080"),
            ("::", 9010, "[::]:9010"),
        ] {
            assert_eq!(
                parse_listen_address(stored, Some(port), 8080).unwrap().to_string(),
                expected
            );
        }
        // The string concatenation this seam replaced is wrong for a bare IPv6
        // host: `::1:8080` is an address with no port, not a socket.
        assert!("::1:8080".parse::<std::net::SocketAddr>().is_err());
    }

    /// P0-3: the HTTP default is loopback and is independent of the database
    /// `--listen`. A deployment that binds PostgreSQL to `0.0.0.0` must not
    /// silently publish the HTTP surface; only an explicit `--http-listen` moves
    /// it off loopback.
    #[test]
    fn omitted_http_listen_defaults_to_loopback_not_database_listen() {
        let resolved = resolve_http_address(&heliosdb_nano::api::resolve_http_listen(None), None)
            .unwrap()
            .unwrap();
        assert_eq!(resolved, "127.0.0.1:8080".parse().unwrap());
        assert!(resolved.ip().is_loopback());
    }

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
    async fn guarded_standby_http_omits_all_writable_api_and_mcp_routes() {
        let db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = run_http_listener(
                listener,
                db,
                Some("fixture-token".into()),
                true,
                heliosdb_nano::config::ApiConfig::default(),
                std::sync::Arc::new(heliosdb_nano::api::HttpAuthPolicy::disabled()),
                true,
            )
            .await;
        });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        for path in ["/health", "/version"] {
            assert_eq!(
                client
                    .get(format!("http://{address}{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::OK
            );
        }
        for path in [
            "/rest/v1/test",
            "/auth/v1/signup",
            "/realtime/v1/websocket",
            "/mcp",
            "/mcp/info",
            "/mcp/sse",
            "/docs",
            "/openapi.json",
        ] {
            for method in [reqwest::Method::GET, reqwest::Method::POST] {
                assert_eq!(
                    client
                        .request(method, format!("http://{address}{path}"))
                        .bearer_auth("fixture-token")
                        .body("{}")
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    reqwest::StatusCode::NOT_FOUND,
                    "unexpected guarded standby route {path}"
                );
            }
        }
        handle.abort();
        let _ = handle.await;
    }

    /// The security HTTP policy has to survive BOTH daemon compositions at this
    /// seam: normal mode wraps the assembled REST/query/MCP surface in the wire
    /// credential middleware, while a history-guarded standby (`health_only`)
    /// mounts only status/health, so no writable or MCP route exists to reach
    /// even when the caller holds valid credentials. Unlike the pre-existing
    /// guarded-standby test, this drives a REQUIRED policy, which is what
    /// distinguishes "route not mounted" from "route unmounted behind auth".
    #[tokio::test]
    async fn required_http_policy_is_composed_into_normal_and_health_only_listeners() {
        fn required_policy() -> std::sync::Arc<heliosdb_nano::api::HttpAuthPolicy> {
            std::sync::Arc::new(heliosdb_nano::api::HttpAuthPolicy::from_wire(
                Some("wire-secret".to_string()),
                Some(std::sync::Arc::from("mcp-secret")),
                false,
            ))
        }

        async fn serve(
            db: std::sync::Arc<EmbeddedDatabase>,
            mcp_token: Option<String>,
            allow_remote_mcp: bool,
            health_only: bool,
        ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                let _ = run_http_listener(
                    listener,
                    db,
                    mcp_token,
                    allow_remote_mcp,
                    heliosdb_nano::config::ApiConfig::default(),
                    required_policy(),
                    health_only,
                )
                .await;
            });
            (address, handle)
        }

        let client = || {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap()
        };

        // Normal mode: `/` and `/health` are the fixed public status body, while
        // the REST surface sits behind the wire credential. Seed a real table so
        // a valid Basic request proves the policy clears AND the handler runs
        // against THIS listener's database (not a missing-table 404).
        let normal_db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        normal_db
            .execute("CREATE TABLE rest_probe (id INT, label TEXT)")
            .unwrap();
        normal_db
            .execute("INSERT INTO rest_probe VALUES (1, 'mounted')")
            .unwrap();
        let (address, handle) = serve(
            std::sync::Arc::clone(&normal_db),
            Some("mcp-secret".to_string()),
            false,
            false,
        )
        .await;
        let normal = client();
        for path in ["/", "/health"] {
            let response = normal.get(format!("http://{address}{path}")).send().await.unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::OK,
                "normal {path} must answer 200"
            );
            let body: Value = response.json().await.unwrap();
            assert_eq!(
                body,
                serde_json::json!({ "status": "ok" }),
                "normal {path} must be the fixed public status body"
            );
        }
        assert_eq!(
            normal
                .get(format!("http://{address}/rest/v1/rest_probe"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "the normal REST surface must require the wire credential"
        );
        let row_response = normal
            .get(format!("http://{address}/rest/v1/rest_probe"))
            .basic_auth("postgres", Some("wire-secret"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            row_response.status(),
            reqwest::StatusCode::OK,
            "a valid wire credential must clear the HTTP policy layer and reach the REST handler"
        );
        let row: Value = row_response.json().await.unwrap();
        assert_eq!(
            row,
            serde_json::json!([{ "id": 1, "label": "mounted" }]),
            "the REST handler must serve exactly the seeded row off this listener's database"
        );
        handle.abort();
        let _ = handle.await;

        // health_only standby: status/health only. `/` and `/health` stay public
        // with the fixed body, `/version` sits behind the configured wire policy,
        // and no writable/MCP route is mounted.
        let guarded_db = std::sync::Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let (address, handle) = serve(
            std::sync::Arc::clone(&guarded_db),
            Some("mcp-secret".to_string()),
            true,
            true,
        )
        .await;
        let guarded = client();
        for path in ["/", "/health"] {
            let response = guarded.get(format!("http://{address}{path}")).send().await.unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::OK,
                "health_only {path} must answer 200"
            );
            let body: Value = response.json().await.unwrap();
            assert_eq!(
                body,
                serde_json::json!({ "status": "ok" }),
                "health_only {path} must be the fixed public status body"
            );
        }
        let version_url = format!("http://{address}/version");
        assert_eq!(
            guarded.get(version_url.as_str()).send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "health_only /version must require the configured credential"
        );
        assert_eq!(
            guarded
                .get(version_url.as_str())
                .basic_auth("postgres", Some("wrong-secret"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "health_only /version must reject a wrong wire credential"
        );
        assert_eq!(
            guarded
                .get(version_url.as_str())
                .bearer_auth("mcp-secret")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "health_only /version must not accept the MCP bearer token, which is scoped to /mcp*"
        );
        assert_eq!(
            guarded
                .get(version_url.as_str())
                .basic_auth("postgres", Some("wire-secret"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK,
            "health_only /version must succeed with the valid wire credential"
        );
        // Every writable/MCP/query path is unmounted, so a policy-cleared Basic
        // request reaches the router and 404s. The query surface is the mounted
        // `POST /v1/branches/:name/query` route; `/v1/branches/main/query` is the
        // concrete instance exercised here.
        for path in [
            "/rest/v1/test",
            "/auth/v1/signup",
            "/realtime/v1/websocket",
            "/v1/branches/main/query",
            "/mcp",
            "/mcp/info",
            "/mcp/sse",
            "/docs",
            "/openapi.json",
        ] {
            for method in [reqwest::Method::GET, reqwest::Method::POST] {
                assert_eq!(
                    guarded
                        .request(method, format!("http://{address}{path}"))
                        .basic_auth("postgres", Some("wire-secret"))
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    reqwest::StatusCode::NOT_FOUND,
                    "health_only leaked {path} to a valid wire credential"
                );
            }
        }
        // The MCP bearer clears the policy ONLY on `/mcp*`; those routes are still
        // unmounted here, so they 404 rather than 401.
        for path in ["/mcp", "/mcp/info", "/mcp/sse"] {
            for method in [reqwest::Method::GET, reqwest::Method::POST] {
                assert_eq!(
                    guarded
                        .request(method, format!("http://{address}{path}"))
                        .bearer_auth("mcp-secret")
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    reqwest::StatusCode::NOT_FOUND,
                    "health_only leaked {path} to the MCP bearer token"
                );
            }
        }
        // On any non-`/mcp*` path the scoped bearer is refused by the policy
        // before routing (401). One representative check keeps the per-peer
        // failure budget (10/window) well clear of the rate limiter.
        assert_eq!(
            guarded
                .get(format!("http://{address}/rest/v1/test"))
                .bearer_auth("mcp-secret")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "the MCP bearer token must not authenticate a non-MCP route"
        );
        handle.abort();
        let _ = handle.await;
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
                false,
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
                false,
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
                false,
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
                false,
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

/// Transfer bounds and authentication shared by source and receiver.
#[derive(clap::Args, Debug, Default)]
struct PhysicalTransferArgs {
    /// Owner-only 32-byte token file authenticating both replication channels.
    #[arg(long)]
    replication_token_file: Option<PathBuf>,
    #[arg(long)]
    physical_max_files: Option<usize>,
    #[arg(long)]
    physical_max_file_bytes: Option<u64>,
    #[arg(long)]
    physical_max_total_bytes: Option<u64>,
    #[arg(long)]
    physical_max_chunk_bytes: Option<usize>,
    #[arg(long)]
    physical_frame_timeout_secs: Option<u64>,
    #[arg(long)]
    physical_operation_timeout_secs: Option<u64>,
}

impl PhysicalTransferArgs {
    fn apply(self, replication: &mut heliosdb_nano::config::ReplicationConfig) {
        if let Some(value) = self.replication_token_file {
            replication.token_file = Some(value);
        }
        let config = &mut replication.physical;
        if let Some(value) = self.physical_max_files {
            config.max_files = value;
        }
        if let Some(value) = self.physical_max_file_bytes {
            config.max_file_bytes = value;
        }
        if let Some(value) = self.physical_max_total_bytes {
            config.max_total_bytes = value;
        }
        if let Some(value) = self.physical_max_chunk_bytes {
            config.max_chunk_bytes = value;
        }
        if let Some(value) = self.physical_frame_timeout_secs {
            config.frame_timeout_secs = value;
        }
        if let Some(value) = self.physical_operation_timeout_secs {
            config.operation_timeout_secs = value;
        }
    }
}

/// CLI overrides are optional so explicit defaults still override the file.
#[derive(clap::Args, Debug, Default)]
struct PhysicalSourceArgs {
    #[command(flatten)]
    transfer: PhysicalTransferArgs,
    /// Loopback physical transfer source (persistent primary only), which a
    /// physical standby is built and followed from. On by default for a
    /// primary with a replication token; `--physical-replication=false` keeps
    /// it off.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    physical_replication: Option<bool>,
    /// Physical endpoint (literal loopback IP:port): where a primary serves its
    /// physical source, and where a standby finds its primary's. Default:
    /// 127.0.0.1:<native replication port + 7> (5440 for the default 5433) —
    /// the primary's own replication port, or the port of a standby's
    /// --primary-host.
    #[arg(long)]
    physical_replication_listen: Option<String>,
    #[arg(long)]
    physical_export_dir: Option<PathBuf>,
    #[arg(long)]
    physical_wal_retention_secs: Option<u64>,
    #[arg(long)]
    physical_wal_retention_size_mb: Option<u64>,
    #[arg(long)]
    physical_max_sessions: Option<usize>,
}

impl PhysicalSourceArgs {
    fn apply(self, replication: &mut heliosdb_nano::config::ReplicationConfig) {
        self.transfer.apply(replication);
        let config = &mut replication.physical;
        if let Some(value) = self.physical_replication {
            config.enabled = value;
        }
        if let Some(value) = self.physical_replication_listen {
            config.listen = value;
        }
        if let Some(value) = self.physical_export_dir {
            config.export_dir = Some(value);
        }
        if let Some(value) = self.physical_wal_retention_secs {
            config.wal_retention_secs = value;
        }
        if let Some(value) = self.physical_wal_retention_size_mb {
            config.wal_retention_size_mb = value;
        }
        if let Some(value) = self.physical_max_sessions {
            config.max_sessions = value;
        }
    }
}

#[derive(clap::Args, Debug)]
struct ResyncArgs {
    /// Physical source endpoint, reached through loopback or an authenticated tunnel.
    #[arg(long)]
    primary_host: String,
    /// Absent destination for a fresh snapshot. Existing data is never overwritten.
    #[arg(short, long)]
    data_dir: PathBuf,
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[command(flatten)]
    physical: PhysicalTransferArgs,
    /// Pin source dataset UUID; mandatory when resuming an installed physical replica.
    #[arg(long)]
    expected_history: Option<uuid::Uuid>,
    /// Resume ONLY an already installed, marked physical replica at its durable cursor.
    #[arg(long, requires = "expected_history")]
    resume_existing: bool,
    /// Install/close once instead of continuously following the source.
    #[arg(long)]
    once: bool,
    #[arg(long, default_value_t = 100)]
    poll_interval_ms: u64,
    #[arg(long, default_value_t = 1000)]
    reconnect_delay_ms: u64,
    #[arg(long, default_value_t = 5)]
    max_reconnects: u32,
    #[arg(long, default_value_t = 8_388_608)]
    poll_byte_limit: usize,
}

fn append_physical_daemon_args(
    args: &mut Vec<String>,
    config: &heliosdb_nano::config::PhysicalReplicationConfig,
    replication_token_file: Option<&std::path::Path>,
) {
    if let Some(path) = replication_token_file {
        args.extend(["--replication-token-file".into(), path.display().to_string()]);
    }
    // An empty listen is the derived default (`PhysicalReplicationConfig::listen`);
    // `start` resolves it before re-exec, so only a caller that did not reaches here.
    if !config.listen.trim().is_empty() {
        args.extend(["--physical-replication-listen".into(), config.listen.clone()]);
    }
    for (name, value) in [
        ("--physical-replication", config.enabled.to_string()),
        ("--physical-wal-retention-secs", config.wal_retention_secs.to_string()),
        (
            "--physical-wal-retention-size-mb",
            config.wal_retention_size_mb.to_string(),
        ),
        ("--physical-max-sessions", config.max_sessions.to_string()),
        ("--physical-max-files", config.max_files.to_string()),
        ("--physical-max-file-bytes", config.max_file_bytes.to_string()),
        ("--physical-max-total-bytes", config.max_total_bytes.to_string()),
        ("--physical-max-chunk-bytes", config.max_chunk_bytes.to_string()),
        ("--physical-frame-timeout-secs", config.frame_timeout_secs.to_string()),
        (
            "--physical-operation-timeout-secs",
            config.operation_timeout_secs.to_string(),
        ),
    ] {
        args.extend([name.into(), value]);
    }
    if let Some(path) = &config.export_dir {
        args.extend(["--physical-export-dir".into(), path.display().to_string()]);
    }
}

/// Spec P1.3: make `[replication.physical] listen` explicit. Empty means
/// "derived from the native endpoint" (`PhysicalReplicationConfig::resolved_listen`):
/// a primary derives it from its own replication port, a standby from the
/// port of `--primary-host`, so a pair configured with those two settings
/// alone meets on the same physical endpoint. Resolved once, before a daemon
/// re-exec forwards it.
fn resolve_physical_listen(replication: &mut heliosdb_nano::config::ReplicationConfig) -> Result<()> {
    if !replication.physical.listen.trim().is_empty() {
        return Ok(());
    }
    let native_port = if replication.role == "standby" {
        // A standby without --primary-host is refused later, with its own message.
        match replication
            .primary_host
            .as_deref()
            .and_then(|host| host.rsplit_once(':'))
            .and_then(|(_, port)| port.parse::<u16>().ok())
        {
            Some(port) => port,
            None => return Ok(()),
        }
    } else {
        replication.replication_port
    };
    replication.physical.listen = replication.physical.resolved_listen(native_port)?.to_string();
    Ok(())
}

/// Spec P1.3: a primary with a replication token serves its physical source
/// by default — a physical standby can be built from nothing else, and an HA
/// pair that worked with 4.41's logical standbys keeps working without a new
/// flag. `--physical-replication=false` keeps it off. A primary without a
/// token (or in memory) cannot serve one; that is said once at startup.
///
/// The source checkpoints into `export_dir`; when none is configured it is
/// `<data dir>.physical-export` beside the data directory (same filesystem,
/// so a checkpoint is hard links), created owner-only if absent.
#[cfg(feature = "ha-tier1")]
fn auto_enable_physical_source(
    replication: &mut heliosdb_nano::config::ReplicationConfig,
    explicit: Option<bool>,
    memory: bool,
    data_dir: &std::path::Path,
) -> Result<()> {
    if replication.role != "primary" || explicit == Some(false) {
        return Ok(());
    }
    if !replication.physical.enabled {
        if memory || replication.token_file.is_none() {
            tracing::warn!(
                "this primary serves no physical source ({}); physical standbys cannot follow it",
                if memory {
                    "in-memory storage"
                } else {
                    "no --replication-token-file"
                }
            );
            return Ok(());
        }
        replication.physical.enabled = true;
    }
    if replication.physical.export_dir.is_none() && !memory {
        replication.physical.export_dir = Some(default_physical_export_dir(data_dir)?);
    }
    Ok(())
}

/// `<data dir>.physical-export`, absolute, created mode 0700 if absent.
#[cfg(feature = "ha-tier1")]
fn default_physical_export_dir(data_dir: &std::path::Path) -> Result<PathBuf> {
    let absolute = if data_dir.is_absolute() {
        data_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| Error::io(format!("cannot resolve the data directory: {e}")))?
            .join(data_dir)
    };
    let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return Err(Error::config(
            "cannot derive a physical export directory from the data directory; set --physical-export-dir",
        ));
    };
    std::fs::create_dir_all(parent).map_err(|e| Error::io(format!("cannot create {}: {e}", parent.display())))?;
    let mut leaf = name.to_os_string();
    leaf.push(".physical-export");
    let export = parent.join(leaf);
    match std::fs::symlink_metadata(&export) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&export).map_err(|e| {
                Error::io(format!(
                    "cannot create the physical export directory {}: {e}; set --physical-export-dir or \
                     --physical-replication=false",
                    export.display()
                ))
            })?;
        }
        Err(error) => {
            return Err(Error::io(format!(
                "cannot inspect the physical export directory {}: {error}",
                export.display()
            )))
        }
    }
    Ok(export)
}

/// Validate before any persistent database is opened or receiver leaf is claimed.
fn validate_physical_source(
    config: &heliosdb_nano::config::PhysicalReplicationConfig,
    token_file: Option<&std::path::Path>,
    role: &str,
    memory: bool,
    data_dir: &std::path::Path,
) -> Result<()> {
    config.validate()?;
    if !config.enabled {
        return Ok(());
    }
    if role != "primary" || memory {
        return Err(Error::config(
            "physical source requires --replication-role primary and persistent storage",
        ));
    }
    #[cfg(not(feature = "ha-tier1"))]
    {
        let _ = (token_file, data_dir);
        return Err(Error::config("physical transfer requires the ha-tier1 feature"));
    }
    #[cfg(feature = "ha-tier1")]
    {
        let token_path = token_file.ok_or_else(|| {
            Error::config("physical source requires --replication-token-file or [replication] token_file")
        })?;
        let _token = heliosdb_nano::replication::read_replication_token(token_path)?;
        let export = config
            .export_dir
            .as_ref()
            .ok_or_else(|| Error::config("physical source requires an existing private export_dir"))?;
        validate_private_export(export)?;
        let export = export.canonicalize().map_err(|e| Error::io(e.to_string()))?;
        let source = if data_dir.exists() {
            data_dir.canonicalize()
        } else {
            let parent = data_dir
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            parent
                .canonicalize()
                .map(|p| p.join(data_dir.file_name().unwrap_or_default()))
        }
        .map_err(|e| Error::config(format!("physical source data directory parent must exist: {e}")))?;
        if export.starts_with(&source) || source.starts_with(&export) {
            return Err(Error::config(
                "physical export_dir and source data directory must be disjoint",
            ));
        }
        Ok(())
    }
}

#[cfg(feature = "ha-tier1")]
fn validate_private_export(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| Error::config(format!("cannot inspect physical export_dir: {e}")))?;
        // SAFETY: geteuid takes no pointers and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(Error::config(
                "physical export_dir must be an existing owned nonsymlink directory, mode 0700 or stricter",
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(Error::config("physical checkpoint export currently requires Unix"))
    }
}

#[cfg(feature = "ha-tier1")]
fn physical_wire_limits(
    config: &heliosdb_nano::config::PhysicalReplicationConfig,
) -> heliosdb_nano::replication::physical_wire::WireLimits {
    heliosdb_nano::replication::physical_wire::WireLimits {
        frame_timeout: std::time::Duration::from_secs(config.frame_timeout_secs),
        operation_timeout: std::time::Duration::from_secs(config.operation_timeout_secs),
        transfer: heliosdb_nano::replication::checkpoint_transfer::TransferLimits {
            max_files: config.max_files,
            max_file_bytes: config.max_file_bytes,
            max_total_bytes: config.max_total_bytes,
            max_chunk_bytes: config.max_chunk_bytes,
        },
    }
}

#[cfg(feature = "ha-tier1")]
struct PreparedPhysicalSource {
    listener: tokio::net::TcpListener,
    token: heliosdb_nano::replication::physical_wire::TransferToken,
    db: std::sync::Arc<rocksdb::DB>,
    history: uuid::Uuid,
    export_dir: PathBuf,
    config: heliosdb_nano::config::PhysicalReplicationConfig,
    work: heliosdb_nano::replication::physical_source::SourceWorkLimiter,
}

#[cfg(feature = "ha-tier1")]
async fn prepare_physical_source(
    config: &heliosdb_nano::config::PhysicalReplicationConfig,
    token_file: Option<&std::path::Path>,
    storage: &heliosdb_nano::storage::StorageEngine,
) -> Result<Option<PreparedPhysicalSource>> {
    if !config.enabled {
        return Ok(None);
    }
    let directory = storage
        .config()
        .storage
        .path
        .as_ref()
        .ok_or_else(|| Error::config("physical source requires persistent data"))?;
    validate_physical_source(
        config,
        token_file,
        "primary",
        storage.config().storage.memory_only,
        directory,
    )?;
    // Native primary startup has already opened/locked the DB and established this marker.
    let history = heliosdb_nano::replication::history::load_or_create_primary_history(directory)
        .map_err(|e| Error::config(e.to_string()))?;
    let token_path = token_file.ok_or_else(|| {
        Error::config("physical source requires --replication-token-file or [replication] token_file")
    })?;
    let token = heliosdb_nano::replication::read_replication_token(token_path)?;
    let listener = heliosdb_nano::replication::physical_wire::bind_loopback(
        config
            .listen
            .parse()
            .map_err(|e| Error::config(format!("physical listen: {e}")))?,
    )
    .await
    .map_err(|e| Error::network(format!("physical source bind failed: {e}")))?;
    Ok(Some(PreparedPhysicalSource {
        listener,
        token,
        db: storage.db(),
        history,
        export_dir: config.export_dir.clone().expect("validated export path"),
        config: config.clone(),
        work: heliosdb_nano::replication::physical_source::SourceWorkLimiter::new(config.max_sessions)
            .map_err(|e| Error::config(e.to_string()))?,
    }))
}

/// Directly owned by main, never detached. The listener is bound before readiness;
/// every authenticated session is joined after shutdown before source DB close.
#[cfg(feature = "ha-tier1")]
async fn serve_physical_source(
    source: Option<PreparedPhysicalSource>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let Some(source) = source else {
        while !*shutdown.borrow() {
            if shutdown.changed().await.is_err() {
                break;
            }
        }
        return Ok(());
    };
    let token = std::sync::Arc::new(source.token);
    let (session_stop, session_shutdown) = tokio::sync::watch::channel(false);
    let mut sessions = tokio::task::JoinSet::new();
    let limits = physical_wire_limits(&source.config);
    let result = loop {
        if *shutdown.borrow() {
            break Ok(());
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => { break Ok(()); }
            completed = sessions.join_next(), if !sessions.is_empty() => {
                if let Some(Err(error)) = completed { tracing::warn!("physical session task failed: {error}"); }
            }
            accepted = source.listener.accept(), if sessions.len() < source.config.max_sessions => {
                let (stream, peer) = match accepted { Ok(value) => value, Err(error) => break Err(Error::network(format!("physical accept failed: {error}"))) };
                if !peer.ip().is_loopback() { continue; }
                let db = source.db.clone();
                let export = source.export_dir.clone();
                let history = source.history;
                let work = source.work.clone();
                let token = token.clone();
                let stop = session_shutdown.clone();
                sessions.spawn(async move {
                    let result = heliosdb_nano::replication::physical_wire::serve_session(stream, &token, limits, stop,
                        move |request| heliosdb_nano::replication::physical_source::open_primary_session(db, history, export, limits.transfer, request, work)
                    ).await;
                    if let Err(error) = result { tracing::warn!("physical transfer session ended: {error}"); }
                });
            }
        }
    };
    let _ = session_stop.send(true);
    while let Some(completed) = sessions.join_next().await {
        if let Err(error) = completed {
            tracing::warn!("physical session cleanup failed: {error}");
        }
    }
    let drained = source
        .work
        .drain()
        .await
        .map_err(|e| Error::network(format!("physical source work drain failed: {e}")));
    match result {
        Err(error) => Err(error),
        Ok(()) => drained,
    }
}

async fn run_physical_resync(args: ResyncArgs) -> Result<()> {
    #[cfg(not(feature = "ha-tier1"))]
    {
        let _ = args;
        return Err(Error::config("resync requires the ha-tier1 feature"));
    }
    #[cfg(feature = "ha-tier1")]
    {
        use heliosdb_nano::replication::physical_receiver::{
            run_receiver, ReceiverMode, ReceiverOptions, ReceiverStart, ReceiverStatus,
        };
        use std::time::Duration;
        // The whole file, not only its replication section: the receiver opens
        // its store with the storage options the standby engine will serve it
        // with (`StorageEngine::build_db_options`).
        let mut config = if let Some(path) = args.config {
            Config::from_file(path)?
        } else {
            Config::default()
        };
        args.physical.apply(&mut config.replication);
        let replication = config.replication.clone();
        let mut physical = replication.physical;
        physical.validate()?;
        if !(1..=60_000).contains(&args.poll_interval_ms)
            || !(1..=60_000).contains(&args.reconnect_delay_ms)
            || args.max_reconnects > 1000
            || !(1..=67_108_864).contains(&args.poll_byte_limit)
        {
            return Err(Error::config(
                "resync requires poll/reconnect intervals 1..=60000 ms, retries 0..=1000, and poll bytes 1..=67108864",
            ));
        }
        if args.expected_history.is_some_and(|id| id.is_nil()) {
            return Err(Error::config("expected history must not be nil"));
        }
        if !args.resume_existing {
            match std::fs::symlink_metadata(&args.data_dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(Error::io(error.to_string())),
                Ok(_) => return Err(Error::config("fresh resync requires an absent destination; preserve existing data and choose a new path, or explicitly resume an installed physical receiver")),
            }
        }
        heliosdb_nano::config::validate_replication_endpoint(&args.primary_host)?;
        let resolved = tokio::time::timeout(
            Duration::from_secs(physical.operation_timeout_secs),
            tokio::net::lookup_host(&args.primary_host),
        )
        .await
        .map_err(|_| Error::network("physical source DNS lookup timed out"))?
        .map_err(|e| Error::network(e.to_string()))?
        .collect::<Vec<_>>();
        if resolved.is_empty() || resolved.iter().any(|address| !address.ip().is_loopback()) {
            return Err(Error::config("initial physical transfer supports loopback only; use an authenticated tunnel to the remote primary, not plaintext over a remote network"));
        }
        let mut sources = Vec::new();
        for address in resolved {
            if !sources.contains(&address) {
                sources.push(address);
            }
        }
        if sources.len() > 16 {
            return Err(Error::config(
                "physical source hostname resolves to more than 16 addresses",
            ));
        }
        let token =
            heliosdb_nano::replication::read_replication_token(replication.token_file.as_ref().ok_or_else(|| {
                Error::config("resync requires --replication-token-file or [replication] token_file")
            })?)?;
        let start = if args.resume_existing {
            ReceiverStart::Resume {
                path: args.data_dir,
                history: args
                    .expected_history
                    .ok_or_else(|| Error::config("--resume-existing requires --expected-history"))?,
            }
        } else {
            ReceiverStart::Snapshot {
                target: args.data_dir,
                expected_history: args.expected_history,
            }
        };
        let options = ReceiverOptions {
            config,
            sources,
            token,
            start,
            mode: if args.once {
                ReceiverMode::InstallOnce
            } else {
                ReceiverMode::Follow
            },
            wire: physical_wire_limits(&physical),
            poll_byte_limit: args.poll_byte_limit,
            poll_interval: Duration::from_millis(args.poll_interval_ms),
            reconnect_delay: Duration::from_millis(args.reconnect_delay_ms),
            max_reconnects: args.max_reconnects,
        };
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let (progress, mut statuses) = tokio::sync::watch::channel(ReceiverStatus::Connecting {
            history: None,
            after: None,
        });
        println!("Physical receiver only: no SQL listener or serving-readiness publication. Strict hydration/publication is a separate prerequisite.");
        let receiver = run_receiver(options, shutdown, progress);
        tokio::pin!(receiver);
        let signal = heliosdb_nano::wait_for_shutdown_signal();
        tokio::pin!(signal);
        let mut status_open = true;
        let result = loop {
            tokio::select! {
                biased;
                value = &mut receiver => break value,
                _ = &mut signal => {
                    let _ = stop.send(true);
                    // Owned worker closes at a command boundary and joins. An OS fsync
                    // cannot safely be force-cancelled; do not abort this future.
                    break (&mut receiver).await;
                }
                changed = statuses.changed(), if status_open => {
                    if changed.is_err() { status_open = false; }
                    else { eprintln!("Physical transfer: {:?}", &*statuses.borrow_and_update()); }
                }
            }
        };
        let closed = result.map_err(|e| {
            Error::network(format!(
                "physical resync stopped: {e}; retained files were not overwritten or auto-reseeded"
            ))
        })?;
        println!("Physical state retained closed at {} (snapshot {}, history {}, source sequence {}). This is not SQL readiness.",
            closed.path.display(), closed.snapshot_id, closed.cursor.history(), closed.cursor.source_sequence());
        Ok(())
    }
}

#[cfg(test)]
mod physical_cli_tests {
    use super::*;
    #[test]
    fn explicit_defaults_override_file_and_survive_daemon_reexec() {
        let cli = Cli::try_parse_from([
            "nano",
            "start",
            "--physical-replication=false",
            "--physical-wal-retention-secs",
            "3600",
            "--physical-wal-retention-size-mb",
            "1024",
            "--physical-max-files",
            "10",
            "--physical-max-file-bytes",
            "4096",
            "--physical-max-total-bytes",
            "8192",
            "--physical-max-chunk-bytes",
            "1024",
        ])
        .unwrap();
        let Commands::Start { physical, .. } = cli.command else {
            panic!("wrong command")
        };
        let mut config = heliosdb_nano::config::ReplicationConfig {
            physical: heliosdb_nano::config::PhysicalReplicationConfig {
                enabled: true,
                wal_retention_secs: 60,
                wal_retention_size_mb: 64,
                ..Default::default()
            },
            ..Default::default()
        };
        physical.apply(&mut config);
        assert!(!config.physical.enabled);
        assert_eq!(config.physical.wal_retention_secs, 3600);
        assert_eq!(config.physical.wal_retention_size_mb, 1024);
        let mut argv = vec!["nano".into(), "start".into()];
        append_physical_daemon_args(&mut argv, &config.physical, config.token_file.as_deref());
        let Commands::Start { physical, .. } = Cli::try_parse_from(argv).unwrap().command else {
            panic!("wrong command")
        };
        let mut child = heliosdb_nano::config::ReplicationConfig {
            physical: heliosdb_nano::config::PhysicalReplicationConfig {
                enabled: true,
                wal_retention_secs: 60,
                ..Default::default()
            },
            ..Default::default()
        };
        physical.apply(&mut child);
        assert!(!child.physical.enabled);
        assert_eq!(child.physical.wal_retention_secs, 3600);
        assert_eq!(child.physical.wal_retention_size_mb, 1024);
        assert_eq!(child.physical.max_files, 10);
        assert_eq!(child.physical.max_file_bytes, 4096);
        assert_eq!(child.physical.max_total_bytes, 8192);
        assert_eq!(child.physical.max_chunk_bytes, 1024);
    }
    #[test]
    fn source_requires_persistent_primary_and_resume_requires_history() {
        let config = heliosdb_nano::config::PhysicalReplicationConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(validate_physical_source(&config, None, "standalone", false, std::path::Path::new("missing")).is_err());
        assert!(validate_physical_source(&config, None, "primary", true, std::path::Path::new("missing")).is_err());
        assert!(Cli::try_parse_from([
            "nano",
            "resync",
            "--primary-host",
            "127.0.0.1:5440",
            "--data-dir",
            "new",
            "--resume-existing"
        ])
        .is_err());
        let Commands::Resync(args) = Cli::try_parse_from([
            "nano",
            "resync",
            "--primary-host",
            "127.0.0.1:5440",
            "--data-dir",
            "new",
        ])
        .unwrap()
        .command
        else {
            panic!("wrong command")
        };
        assert!(!args.once && !args.resume_existing);
    }
    #[cfg(all(feature = "ha-tier1", unix))]
    #[test]
    fn token_file_rejects_world_readable_links_and_wrong_length() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, [9u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(heliosdb_nano::replication::read_replication_token(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(heliosdb_nano::replication::read_replication_token(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(heliosdb_nano::replication::read_replication_token(&link).is_err());
        let hard = directory.path().join("hard");
        std::fs::hard_link(&path, &hard).unwrap();
        assert!(heliosdb_nano::replication::read_replication_token(&path).is_err());
        std::fs::remove_file(hard).unwrap();
        std::fs::write(&path, [9u8; 31]).unwrap();
        assert!(heliosdb_nano::replication::read_replication_token(&path).is_err());
    }
}
