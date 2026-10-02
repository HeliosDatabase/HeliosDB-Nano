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
    allow_insecure_trust: bool,
    node_id: Option<String>,
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

        /// Offer the X25519MLKEM768 hybrid post-quantum key-exchange group
        /// (draft-ietf-tls-ecdhe-mlkem) alongside classical groups on the
        /// PostgreSQL TLS listener, in addition to a classical fallback for
        /// clients that don't support it. Only meaningful with --tls-cert /
        /// --tls-key. Default: on.
        #[arg(long, default_value = "true", default_missing_value = "true", num_args = 0..=1, action = clap::ArgAction::Set)]
        tls_post_quantum: bool,

        /// Authentication method: trust, password, md5, scram-sha-256
        #[arg(long, default_value = "trust")]
        auth: String,

        /// Permit unauthenticated non-loopback PostgreSQL access for isolated development only.
        #[arg(long)]
        allow_insecure_trust: bool,

        /// Password for authentication (required for password/md5/scram-sha-256 auth)
        #[arg(long)]
        password: Option<String>,

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

        /// HTTP API listen address. Defaults to --listen; use 127.0.0.1 for local MCP while PG listens remotely.
        #[arg(long)]
        http_listen: Option<String>,

        /// Bearer token required for MCP-over-HTTP routes. Requires the mcp-endpoint feature.
        #[arg(long)]
        mcp_token: Option<String>,

        /// Mount MCP-over-HTTP routes on a non-loopback HTTP listener without MCP auth. Requires mcp-endpoint; unsafe.
        #[arg(long)]
        allow_remote_mcp: bool,

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
        #[arg(long, default_value = "true", default_missing_value = "true", num_args = 0..=1, action = clap::ArgAction::Set)]
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

#[tokio::main]
async fn main() -> Result<()> {
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
            tls_post_quantum,
            auth,
            allow_insecure_trust,
            password,
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
            if mysql_tls_cert.is_some() != mysql_tls_key.is_some() {
                return Err(Error::config(
                    "Both --mysql-tls-cert and --mysql-tls-key must be specified together for MySQL TLS.".to_string(),
                ));
            }

            // Validate auth options
            let auth_lower = auth.to_lowercase();
            if auth_lower != "trust" && password.is_none() {
                return Err(Error::config(format!(
                    "Authentication method '{}' requires --password to be set.",
                    auth
                )));
            }

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
            physical.apply(&mut replication.physical);
            replication.role = replication.role.to_ascii_lowercase();
            replication.sync_mode = replication.sync_mode.to_ascii_lowercase();
            replication.validate()?;
            validate_physical_source(&replication.physical, &replication.role, memory, &resolved_data_dir)?;
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
            let http_address = resolve_http_address(http_listen.as_deref().unwrap_or(&listen), http_port)?;
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
                allow_insecure_trust,
                node_id: replication.node_id,
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
                    tls_post_quantum,
                    auth,
                    password,
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
                    tls_post_quantum,
                    auth,
                    password,
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
    use heliosdb_nano::protocol::postgres::ssl::{SslConfig, SslMode};
    use heliosdb_nano::protocol::postgres::timeouts::ConnectionTimeouts;
    use heliosdb_nano::protocol::postgres::{InMemoryPasswordStore, PasswordStore, SharedPasswordStore};
    use heliosdb_nano::storage::{DumpCompressionType, DumpManager, DumpMode, DumpOptions};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Instant;

    validate_physical_source(&ha_config.physical, &ha_config.role, memory_mode, &data_dir)?;
    let startup_time = Instant::now();
    let guarded_standby = ha_config.role.eq_ignore_ascii_case("standby");
    if guarded_standby && (mysql_enabled || mysql_socket.is_some()) {
        return Err(Error::config(
            "history-guarded standby does not support MySQL listeners; remove --mysql/--mysql-socket and connect to the primary for writes",
        ));
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

    // Claim/validate receiver history BEFORE opening RocksDB. Existing unbound
    // directories are never silently adopted as a live replication receiver.
    #[cfg(feature = "ha-tier1")]
    let receiver_history = if ha_config.role.eq_ignore_ascii_case("standby") {
        use heliosdb_nano::replication::history::{prepare_receiver_history, ReceiverHistory};
        heliosdb_nano::replication::ha_state::ha_state().disable_forwarding();
        Some(Arc::new(if memory_mode {
            ReceiverHistory::in_memory()
        } else {
            prepare_receiver_history(&data_dir)
                .map_err(|error| Error::config(format!("Cannot prepare standby history: {error}")))?
        }))
    } else {
        None
    };

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
    let pg_addr = parse_listen_address(&listen, Some(port), port)?;

    // Parse authentication method
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

    // Build server config
    let mut pg_config = PgServerConfig::with_address(pg_addr)
        .with_auth_method(auth_method)
        .with_allow_insecure_trust(ha_config.allow_insecure_trust)
        .with_max_connections(max_connections)
        .with_timeouts(connection_policy.clone());

    // Configure TLS if specified
    let tls_enabled = tls_cert.is_some();
    if let (Some(cert_path), Some(key_path)) = (&tls_cert, &tls_key) {
        let ssl_config = SslConfig::new(SslMode::Prefer, cert_path, key_path).with_post_quantum(tls_post_quantum);
        pg_config = pg_config.with_ssl(ssl_config);
    }

    println!("[3/4] Configuring server...");
    println!("      - Listen address: {pg_addr}");
    if ha_config.http_port != 0 {
        let http_listen = ha_config.http_listen.as_deref().unwrap_or(&listen);
        println!("      - HTTP address: {http_listen}:{}", ha_config.http_port);
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
        println!("      - HTTP health port: {}", ha_config.http_port);
    }

    // Create PostgreSQL server with appropriate auth configuration
    let pg_server = if auth_method == AuthMethod::Md5 {
        // MD5 uses the legacy `AuthManager` users map, which stores the
        // PostgreSQL md5 shadow — NOT the SCRAM password store, which cannot
        // serve an md5 challenge. With a --password, seed the default users
        // (their shadows are computed by `add_user` under AuthMethod::Md5);
        // without one, fall back to PgServer::new, which provisions md5-shadow
        // default users. Both paths are fail-closed: the correct password is
        // always required.
        if let Some(ref pwd) = password {
            let mut mgr = AuthManager::new(AuthMethod::Md5);
            mgr.add_user("postgres".to_string(), pwd.clone());
            mgr.add_user("helios".to_string(), pwd.clone());
            PgServer::with_auth_manager(pg_config, Arc::clone(&db), mgr)?
        } else {
            PgServer::new(pg_config, Arc::clone(&db))?
        }
    } else if matches!(auth_method, AuthMethod::CleartextPassword | AuthMethod::ScramSha256) {
        if let Some(ref pwd) = password {
            // Create password store with default users
            let mut store = InMemoryPasswordStore::new();
            store
                .add_user("postgres", pwd)
                .map_err(|e| Error::config(format!("Failed to add user: {e}")))?;
            store
                .add_user("helios", pwd)
                .map_err(|e| Error::config(format!("Failed to add user: {e}")))?;
            let shared_store = SharedPasswordStore::new(store);
            let auth_manager = AuthManager::with_password_store(auth_method, shared_store);
            PgServer::with_auth_manager(pg_config, Arc::clone(&db), auth_manager)?
        } else {
            PgServer::new(pg_config, Arc::clone(&db))?
        }
    } else {
        PgServer::new(pg_config, Arc::clone(&db))?
    };

    // Bind PostgreSQL before HA: an overlapping replication endpoint now fails
    // synchronously instead of reporting readiness with one listener missing.
    let pg_listener = tokio::net::TcpListener::bind(pg_addr)
        .await
        .map_err(|e| Error::network(format!("Failed to bind PostgreSQL listener {pg_addr}: {e}")))?;

    // Start HA components if enabled
    #[cfg(feature = "ha-tier1")]
    let mut ha_handles = if ha_role != "standalone" {
        start_ha_components(
            &ha_config,
            &pg_addr.ip().to_string(),
            pg_addr.port(),
            db.storage.clone(),
            receiver_history,
        )
        .await?
    } else {
        HAHandles::default()
    };

    // Bind physical source before publishing any readiness. Its directly owned
    // future is polled alongside PostgreSQL only after startup succeeds.
    #[cfg(feature = "ha-tier1")]
    let physical_source = prepare_physical_source(&ha_config.physical, &db.storage).await?;

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
    let http_health_disabled = ha_config.http_port == 0;
    // The join handle is kept (rather than fully detaching the task) so the
    // shutdown arm can abort it alongside the MySQL/Unix-socket listeners.
    // This task holds an `Arc::clone(&db)`; leaving it running past shutdown
    // pins the refcount above zero, so `EmbeddedDatabase::drop` — and with it
    // the ordered close-time row-counter flush and R4.2 snapshot checkpoint —
    // never runs at all under the default `--http-port 8080`. Aborting it is
    // what lets that work happen on the default `start` invocation, not just
    // under `--http-port 0`. The comment above still holds: the main
    // `select!` never *observes* this task, so a late-life HTTP failure
    // cannot tear the database listener down.
    let http_handle: Option<tokio::task::JoinHandle<()>> = if !http_health_disabled {
        let http_listen = ha_config.http_listen.as_deref().unwrap_or(&listen);
        let http_addr = parse_listen_address(http_listen, Some(ha_config.http_port), 8080)?;
        match tokio::net::TcpListener::bind(http_addr).await {
            Ok(listener) => {
                info!("HTTP endpoint at http://{http_addr} (history-guarded standby health-only: {guarded_standby})");
                let http_db = Arc::clone(&db);
                let mcp_token = ha_config.mcp_token.clone();
                let allow_remote_mcp = ha_config.allow_remote_mcp;
                let http_api_config = api_config.clone();
                Some(tokio::spawn(async move {
                    if let Err(e) = run_http_listener(
                        listener,
                        http_db,
                        mcp_token,
                        allow_remote_mcp,
                        http_api_config,
                        guarded_standby,
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
        error = async {
            #[cfg(feature = "ha-tier1")]
            { ha_handles.failure().await }
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
        result = pg_server.serve_with_listener(pg_listener) => result,
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
        let cleanup = if !physical_finished {
            (&mut physical_service).await
        } else {
            Ok(())
        };
        ha_handles.shutdown().await;
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
    tls_post_quantum: bool,
    auth: String,
    password: Option<String>,
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

    // Add auth options
    if ha_config.allow_insecure_trust {
        args.push("--allow-insecure-trust".to_string());
    }
    args.push("--auth".to_string());
    args.push(auth.clone());
    if let Some(pwd) = password {
        args.push("--password".to_string());
        args.push(pwd);
    }

    append_physical_daemon_args(&mut args, &ha_config.physical);

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
    if let Some(mcp_token) = ha_config.mcp_token {
        args.push("--mcp-token".to_string());
        args.push(mcp_token);
    }
    if ha_config.allow_remote_mcp {
        args.push("--allow-remote-mcp".to_string());
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
        let mut child = Command::new(&exe)
            .args(&args)
            .env("HELIOSDB_NANO_READY_FILE", &ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
        let _ = &pid_file; // only the unix path reads/writes the PID file
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

/// Start HA replication components based on role
#[cfg(feature = "ha-tier1")]
async fn start_ha_components(
    ha_config: &HAConfig,
    listen: &str,
    port: u16,
    storage: std::sync::Arc<heliosdb_nano::storage::StorageEngine>,
    receiver_history: Option<std::sync::Arc<heliosdb_nano::replication::history::ReceiverHistory>>,
) -> Result<HAHandles> {
    use heliosdb_nano::replication::{
        config::PrimaryConfig,
        ha_state::{ha_state, HARole, NodeConfig, SyncMode as HASyncMode},
        streaming::{
            StreamingClient, StreamingClientConfig, StreamingClientStatus, StreamingServer, StreamingServerConfig,
        },
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
                ..Default::default()
            };

            let server = StreamingServer::new(server_config, node_id, history_id, wal_store)
                .map_err(|error| Error::config(format!("Cannot configure replication history: {error}")))?;
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
        "standby" => {
            // Start streaming client for standby
            let primary_host = ha_config
                .primary_host
                .as_ref()
                .ok_or_else(|| Error::config("--primary-host required for standby role".to_string()))?;

            let startup_timeout = Duration::from_secs(ha_config.startup_timeout_secs);
            let verification_deadline = tokio::time::Instant::now() + startup_timeout;
            // DNS and initial history verification share the same configured budget.
            let primary_addr = tokio::time::timeout_at(verification_deadline, tokio::net::lookup_host(primary_host))
                .await
                .map_err(|_| {
                    Error::network(format!(
                        "Standby primary resolution exceeded replication.startup_timeout_secs={}s",
                        ha_config.startup_timeout_secs
                    ))
                })?
                .map_err(|e| Error::config(format!("Cannot resolve primary host '{}': {}", primary_host, e)))?
                .next()
                .ok_or_else(|| Error::config(format!("No address found for primary host '{}'", primary_host)))?;

            // Native history verification cannot authenticate a separate PG SQL
            // endpoint. Transparent forwarding remains disabled for every mode.
            ha_state().disable_forwarding();
            let receiver_history = receiver_history
                .ok_or_else(|| Error::config("standby history must be prepared before database open"))?;

            let client_config = StreamingClientConfig {
                node_id,
                primary_addr,
                sync_mode,
                connect_timeout: startup_timeout,
                reconnect_delay: Duration::from_secs(5),
                max_reconnect_attempts: 0, // Unlimited
            };

            let (client, entry_rx) = StreamingClient::new(client_config, receiver_history);
            let mut status = client.subscribe_status();
            info!("Streaming client connecting to primary at {}", primary_host);

            // Create WAL Applicator for applying replicated entries
            let primary_hostname = primary_host
                .rsplit_once(':')
                .map(|(host, _)| host.trim_start_matches('[').trim_end_matches(']'))
                .unwrap_or(primary_host);
            let primary_pg_port = std::env::var("HELIOSDB_PRIMARY_PG_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432u16);

            let applicator_config = PrimaryConfig {
                host: primary_hostname.to_string(),
                port: primary_pg_port,
                connect_timeout: startup_timeout,
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

            let forward_handle = tokio::spawn(async move {
                let mut entry_rx = entry_rx;
                loop {
                    tokio::select! {
                        _ = queue_tx.closed() => return Err(Error::network(
                            "WAL applicator stopped or refused an entry; receiver serving is terminated")),
                        entry = entry_rx.recv() => match entry {
                            Some(entry) => queue_tx.send(entry).await.map_err(|error|
                                Error::network(format!("WAL applicator queue closed: {error}")))?,
                            None => return Err(Error::network("replication entry stream closed")),
                        }
                    }
                }
            });
            let client_handle = tokio::spawn(async move {
                client
                    .start()
                    .await
                    .map_err(|error| Error::network(format!("Streaming client failed: {error}")))
            });
            let mut handles = HAHandles {
                replication_handle: Some(client_handle),
                forwarding_handle: Some(forward_handle),
                applicator: Some(applicator),
                client_status: Some(status.clone()),
                _ephemeral_wal: None,
            };
            // Identity verification is a startup prerequisite, not proof of
            // catchup. No PG accept task, HTTP/MCP route or daemon ready marker
            // is started until the transport durably verifies/binds history.
            let verified = tokio::time::timeout_at(verification_deadline, async {
                loop {
                    let current = status.borrow().clone();
                    match current {
                        StreamingClientStatus::Verified { history_id } => {
                            info!("Standby dataset history verified: {history_id}");
                            return Ok(());
                        }
                        StreamingClientStatus::Failed { error } => {
                            return Err(Error::network(format!("Standby history verification failed: {error}")))
                        }
                        StreamingClientStatus::Stopped => {
                            return Err(Error::network("Streaming client stopped before history verification"))
                        }
                        _ => (),
                    }
                    tokio::select! {
                        error = handles.failure() => return Err(error),
                        changed = status.changed() => changed.map_err(|_| Error::network(
                            "Streaming client status closed before history verification"))?,
                    }
                }
            })
            .await
            .map_err(|_| {
                Error::network(format!(
                    "Standby did not verify primary dataset history within {}s (replication.startup_timeout_secs)",
                    ha_config.startup_timeout_secs
                ))
            })
            .and_then(|result| result);
            if let Err(error) = verified {
                handles.shutdown().await;
                return Err(error);
            }
            Ok(handles)
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

/// HTTP listener for Docker health checks and, when compiled with
/// `mcp-endpoint`, MCP-over-HTTP routes.
/// Caller owns the
/// already-bound `TcpListener` so bind failures are reported up front
/// (see KanttBan bug #2 / v3.28.0 fix).
async fn run_http_listener(
    listener: tokio::net::TcpListener,
    db: std::sync::Arc<EmbeddedDatabase>,
    mcp_token: Option<String>,
    allow_remote_mcp: bool,
    api_config: heliosdb_nano::config::ApiConfig,
    health_only: bool,
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
        return axum::serve(listener, app).await;
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

    axum::serve(listener, app).await
}

#[cfg(test)]
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

    use super::*;
    #[cfg(feature = "mcp-endpoint")]
    use serde_json::json;
    use serde_json::Value;

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
}

/// Transfer bounds and authentication shared by source and receiver.
#[derive(clap::Args, Debug, Default)]
struct PhysicalTransferArgs {
    #[arg(long)]
    physical_replication_token_file: Option<PathBuf>,
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
    fn apply(self, config: &mut heliosdb_nano::config::PhysicalReplicationConfig) {
        if let Some(value) = self.physical_replication_token_file {
            config.token_file = Some(value);
        }
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
    /// Enable default-off loopback physical transfer source (persistent primary only).
    #[arg(long, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    physical_replication: Option<bool>,
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
    fn apply(self, config: &mut heliosdb_nano::config::PhysicalReplicationConfig) {
        self.transfer.apply(config);
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

fn append_physical_daemon_args(args: &mut Vec<String>, config: &heliosdb_nano::config::PhysicalReplicationConfig) {
    for (name, value) in [
        ("--physical-replication", config.enabled.to_string()),
        ("--physical-replication-listen", config.listen.clone()),
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
    for (name, path) in [
        ("--physical-replication-token-file", &config.token_file),
        ("--physical-export-dir", &config.export_dir),
    ] {
        if let Some(path) = path {
            args.extend([name.into(), path.display().to_string()]);
        }
    }
}

/// Validate before any persistent database is opened or receiver leaf is claimed.
fn validate_physical_source(
    config: &heliosdb_nano::config::PhysicalReplicationConfig,
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
        let _ = data_dir;
        return Err(Error::config("physical transfer requires the ha-tier1 feature"));
    }
    #[cfg(feature = "ha-tier1")]
    {
        let token_path = config
            .token_file
            .as_ref()
            .ok_or_else(|| Error::config("physical source requires a token_file"))?;
        let _token = read_physical_token(token_path)?;
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
fn read_physical_token(path: &std::path::Path) -> Result<heliosdb_nano::replication::physical_wire::TransferToken> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| Error::config(format!("cannot open physical token file: {e}")))?;
        let metadata = file.metadata().map_err(|e| Error::io(e.to_string()))?;
        // SAFETY: geteuid takes no pointers and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() != 32
        {
            return Err(Error::config("physical token must be an owned, nonsymlink, single-link regular file, mode 0600 or stricter, containing exactly 32 raw bytes"));
        }
        let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
        file.read_exact(&mut *bytes).map_err(|e| Error::io(e.to_string()))?;
        let mut extra = [0u8; 1];
        if file.read(&mut extra).map_err(|e| Error::io(e.to_string()))? != 0 {
            return Err(Error::config("physical token file changed size while reading"));
        }
        heliosdb_nano::replication::physical_wire::TransferToken::new(*bytes).map_err(|e| Error::config(e.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(Error::config(
            "physical transfer token ownership validation currently requires Unix",
        ))
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
    validate_physical_source(config, "primary", storage.config().storage.memory_only, directory)?;
    // Native primary startup has already opened/locked the DB and established this marker.
    let history = heliosdb_nano::replication::history::load_or_create_primary_history(directory)
        .map_err(|e| Error::config(e.to_string()))?;
    let token = read_physical_token(config.token_file.as_ref().expect("validated token path"))?;
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
        let mut physical = if let Some(path) = args.config {
            Config::from_file(path)?.replication.physical
        } else {
            Default::default()
        };
        args.physical.apply(&mut physical);
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
        let token = read_physical_token(physical.token_file.as_ref().ok_or_else(|| {
            Error::config("resync requires --physical-replication-token-file or [replication.physical] token_file")
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
        let mut config = heliosdb_nano::config::PhysicalReplicationConfig {
            enabled: true,
            wal_retention_secs: 60,
            wal_retention_size_mb: 64,
            ..Default::default()
        };
        physical.apply(&mut config);
        assert!(!config.enabled);
        assert_eq!(config.wal_retention_secs, 3600);
        assert_eq!(config.wal_retention_size_mb, 1024);
        let mut argv = vec!["nano".into(), "start".into()];
        append_physical_daemon_args(&mut argv, &config);
        let Commands::Start { physical, .. } = Cli::try_parse_from(argv).unwrap().command else {
            panic!("wrong command")
        };
        let mut child = heliosdb_nano::config::PhysicalReplicationConfig {
            enabled: true,
            wal_retention_secs: 60,
            ..Default::default()
        };
        physical.apply(&mut child);
        assert!(!child.enabled);
        assert_eq!(child.wal_retention_secs, 3600);
        assert_eq!(child.wal_retention_size_mb, 1024);
        assert_eq!(child.max_files, 10);
        assert_eq!(child.max_file_bytes, 4096);
        assert_eq!(child.max_total_bytes, 8192);
        assert_eq!(child.max_chunk_bytes, 1024);
    }
    #[test]
    fn source_requires_persistent_primary_and_resume_requires_history() {
        let config = heliosdb_nano::config::PhysicalReplicationConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(validate_physical_source(&config, "standalone", false, std::path::Path::new("missing")).is_err());
        assert!(validate_physical_source(&config, "primary", true, std::path::Path::new("missing")).is_err());
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
        assert!(read_physical_token(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_physical_token(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(read_physical_token(&link).is_err());
        let hard = directory.path().join("hard");
        std::fs::hard_link(&path, &hard).unwrap();
        assert!(read_physical_token(&path).is_err());
        std::fs::remove_file(hard).unwrap();
        std::fs::write(&path, [9u8; 31]).unwrap();
        assert!(read_physical_token(&path).is_err());
    }
}
