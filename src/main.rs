//! `HeliosDB` Lite server binary

#![allow(
    clippy::uninlined_format_args,
    clippy::unused_async,
    clippy::too_many_lines,
    clippy::single_match_else,
    clippy::match_wildcard_for_single_variants,
    clippy::manual_string_new,
)]

use heliosdb_nano::{Config, EmbeddedDatabase, Result, Error};
use std::path::PathBuf;
use tracing::info;
use clap::{Parser, Subcommand};

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
    node_id: Option<String>,
}

#[derive(Parser)]
#[command(name = "heliosdb-nano")]
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

        /// Authentication method: trust, password, md5, scram-sha-256
        #[arg(long, default_value = "trust")]
        auth: String,

        /// Password for authentication (required for password/md5/scram-sha-256 auth)
        #[arg(long)]
        password: Option<String>,

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

        /// HTTP API port for health checks (default: 8080)
        #[arg(long, default_value = "8080")]
        http_port: u16,

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

        /// PostgreSQL Unix domain socket directory. If set, listens at
        /// `<dir>/.s.PGSQL.<port>` — the libpq default. Use with psql `-h /tmp`.
        #[arg(long)]
        pg_socket_dir: Option<PathBuf>,
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
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Start { data_dir, memory, port, listen, config, daemon, pid_file, dump_on_shutdown, dump_schedule, tls_cert, tls_key, auth, password, replication_role, replication_port, primary_host, standby_hosts, observer_hosts, sync_mode, http_port, node_id, mysql, mysql_listen, mysql_socket, pg_socket_dir } => {
            // Validate that either --data-dir or --memory is specified
            if !memory && data_dir.is_none() {
                return Err(Error::config(
                    "Either --data-dir or --memory must be specified. Use --help for more information.".to_string()
                ));
            }

            // Validate TLS options
            if tls_cert.is_some() != tls_key.is_some() {
                return Err(Error::config(
                    "Both --tls-cert and --tls-key must be specified together for TLS.".to_string()
                ));
            }

            // Validate auth options
            let auth_lower = auth.to_lowercase();
            if auth_lower != "trust" && password.is_none() {
                return Err(Error::config(format!(
                    "Authentication method '{}' requires --password to be set.", auth
                )));
            }

            // Note on dump_schedule: scheduled dumps are not yet implemented
            if dump_schedule.is_some() {
                info!("Note: --dump-schedule is not yet implemented");
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
                node_id,
            };

            if daemon {
                start_server_daemon(resolved_data_dir, port, listen, config, pid_file, tls_cert, tls_key, auth, password, ha_config).await
            } else {
                start_server(resolved_data_dir, port, listen, config, memory, dump_on_shutdown, tls_cert, tls_key, auth, password, ha_config, mysql, mysql_listen, mysql_socket, pg_socket_dir).await
            }
        }
        Commands::Stop { ref pid_file } => {
            stop_server(pid_file)
        }
        Commands::Status { ref pid_file } => {
            check_server_status(pid_file)
        }
        Commands::Init { ref data_dir } => {
            init_database(data_dir)
        }
        Commands::Repl { data_dir, memory, dump_on_shutdown, dump_file } => {
            run_repl(data_dir, memory, dump_on_shutdown, dump_file)
        }
        Commands::Dump { output, data_dir, append, compression, connection, verbose } => {
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
        Commands::Restore { input, target, verify, connection, verbose } => {
            use heliosdb_nano::cli::RestoreCommand;
            let cmd = RestoreCommand {
                input,
                target,
                verify,
                connection,
                verbose,
            };
            cmd.execute()
        }

        #[cfg(feature = "code-graph")]
        Commands::CodeGraph { action } => match action {
            CodeGraphAction::Hook { data_dir, repo_root, source_table } => {
                let stats = heliosdb_nano::code_graph::git_hook::run_from_stdin(
                    &data_dir, &repo_root, &source_table,
                )?;
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
    auth: String,
    password: Option<String>,
    ha_config: HAConfig,
    mysql_enabled: bool,
    mysql_listen: String,
    mysql_socket: Option<PathBuf>,
    pg_socket_dir: Option<PathBuf>,
) -> Result<()> {
    use heliosdb_nano::protocol::postgres::server::{PgServer, PgServerConfig};
    use heliosdb_nano::protocol::postgres::auth::{AuthMethod, AuthManager};
    use heliosdb_nano::protocol::postgres::ssl::{SslConfig, SslMode};
    use heliosdb_nano::protocol::postgres::{InMemoryPasswordStore, SharedPasswordStore, PasswordStore};
    use heliosdb_nano::storage::{DumpManager, DumpOptions, DumpMode, DumpCompressionType};
    use std::sync::Arc;
    use std::net::SocketAddr;
    use std::time::Instant;
    use colored::Colorize;

    let startup_time = Instant::now();

    // Print startup banner
    println!();
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║           HeliosDB-Lite v{:<32} ║", env!("CARGO_PKG_VERSION"));
    println!("║   PostgreSQL-compatible database with enterprise features     ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");
    println!();

    // Load config
    let _db_config = if let Some(ref path) = config_path {
        println!("[1/4] Loading configuration from {}...", path.display());
        Config::from_file(path.clone())?
    } else {
        println!("[1/4] Using default configuration...");
        Config::default()
    };

    // Open database (in-memory mode avoids disk I/O for all operations)
    let db = if memory_mode {
        println!("[2/4] Initializing in-memory database...");
        Arc::new(EmbeddedDatabase::new_in_memory()?)
    } else {
        println!("[2/4] Initializing database at {}...", data_dir.display());
        Arc::new(EmbeddedDatabase::new(&data_dir)?)
    };
    println!("      Database initialized successfully");

    // Configure PostgreSQL server
    let pg_addr: SocketAddr = format!("{listen}:{port}").parse()
        .map_err(|e| Error::config(format!("Invalid listen address: {e}")))?;

    // Parse authentication method
    let auth_method = match auth.to_lowercase().as_str() {
        "trust" => AuthMethod::Trust,
        "password" => AuthMethod::CleartextPassword,
        "md5" => AuthMethod::Md5,
        "scram-sha-256" | "scram" => AuthMethod::ScramSha256,
        other => return Err(Error::config(format!(
            "Unknown authentication method: '{}'. Use: trust, password, md5, scram-sha-256", other
        ))),
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
        .with_max_connections(100);

    // Configure TLS if specified
    let tls_enabled = tls_cert.is_some();
    if let (Some(cert_path), Some(key_path)) = (&tls_cert, &tls_key) {
        let ssl_config = SslConfig::new(SslMode::Prefer, cert_path, key_path);
        pg_config = pg_config.with_ssl(ssl_config);
    }

    println!("[3/4] Configuring server...");
    println!("      - Listen address: {pg_addr}");
    println!("      - Max connections: 100");
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
    let pg_server = if matches!(auth_method, AuthMethod::CleartextPassword | AuthMethod::Md5 | AuthMethod::ScramSha256) {
        if let Some(ref pwd) = password {
            // Create password store with default users
            let mut store = InMemoryPasswordStore::new();
            store.add_user("postgres", pwd).map_err(|e| Error::config(format!("Failed to add user: {e}")))?;
            store.add_user("helios", pwd).map_err(|e| Error::config(format!("Failed to add user: {e}")))?;
            let shared_store = SharedPasswordStore::new(store);
            let auth_manager = AuthManager::with_password_store(auth_method, shared_store);
            PgServer::with_auth_manager(pg_config, Arc::clone(&db), auth_manager)?
        } else {
            PgServer::new(pg_config, Arc::clone(&db))?
        }
    } else {
        PgServer::new(pg_config, Arc::clone(&db))?
    };

    println!("[4/4] Starting server...");
    println!();
    println!("════════════════════════════════════════════════════════════════");
    println!("  Server ready! Started in {:.2}s", startup_time.elapsed().as_secs_f64());
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
    println!("      ORM matrix:  https://github.com/Dimensigon/HDB-HeliosDB-Nano/blob/main/docs/compatibility/orm.md");
    println!("      Known gaps:  SELECT heliosdb_capability_report();");
    if mysql_enabled {
        println!();
        println!("    mysql:      mysql -h {} -P {}", mysql_listen.split(':').next().unwrap_or("127.0.0.1"),
            mysql_listen.split(':').nth(1).unwrap_or("3306"));
        println!("    PyMySQL:    pymysql.connect(host='{}', port={})",
            mysql_listen.split(':').next().unwrap_or("127.0.0.1"),
            mysql_listen.split(':').nth(1).unwrap_or("3306"));
    }
    println!();
    println!("  For REPL mode (single-user):  heliosdb-nano repl -d {}", data_dir.display());
    println!();
    println!("  Press Ctrl+C to shut down");
    println!("────────────────────────────────────────────────────────────────");
    println!();

    // Log for tracing subscribers
    info!("HeliosDB-Lite server listening on {}", pg_addr);

    // Start HA components if enabled
    #[cfg(feature = "ha-tier1")]
    let _ha_handles = if ha_role != "standalone" {
        start_ha_components(&ha_config, &listen, port, db.storage.clone()).await?
    } else {
        HAHandles::default()
    };

    // Start HTTP health endpoint (for Docker health checks)
    let http_addr: SocketAddr = format!("{}:{}", listen, ha_config.http_port).parse()
        .map_err(|e| Error::config(format!("Invalid HTTP address: {e}")))?;
    let health_server = start_health_server(http_addr);
    info!("Health endpoint at http://{}/health", http_addr);

    // Start MySQL listener if enabled
    let mysql_handle = if mysql_enabled {
        let mysql_addr: SocketAddr = mysql_listen.parse()
            .map_err(|e| Error::config(format!("Invalid MySQL listen address '{}': {}", mysql_listen, e)))?;
        let mysql_db = Arc::clone(&db);
        info!("MySQL protocol listening on {}", mysql_addr);
        let conn_counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(1));
        Some(tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(mysql_addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("Failed to bind MySQL listener on {}: {}", mysql_addr, e);
                    return;
                }
            };
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        tracing::debug!("MySQL connection from {}", addr);
                        let db_clone = Arc::clone(&mysql_db);
                        let conn_id = conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(async move {
                            if let Err(e) = heliosdb_nano::protocol::mysql::handle_mysql_connection(
                                db_clone, stream, conn_id
                            ).await {
                                tracing::debug!("MySQL connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("MySQL accept error: {}", e);
                    }
                }
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
            let _ = std::fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(0o777),
            );
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let db_clone = Arc::clone(&mysql_db);
                        let conn_id = conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(async move {
                            if let Err(e) = heliosdb_nano::protocol::mysql::handler::handle_mysql_connection_unix(
                                db_clone, stream, conn_id,
                            ).await {
                                tracing::debug!("MySQL UDS connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("MySQL UDS accept error: {}", e);
                    }
                }
            }
        }))
    } else {
        None
    };
    #[cfg(not(unix))]
    let mysql_unix_handle: Option<tokio::task::JoinHandle<()>> = None;
    let _ = &mysql_socket; // silence unused on non-unix

    // Start PostgreSQL Unix domain socket listener if requested.
    // libpq uses `<dir>/.s.PGSQL.<port>` when host starts with `/`.
    #[cfg(unix)]
    let pg_unix_handle = if let Some(ref sock_dir) = pg_socket_dir {
        let sock_path = sock_dir.join(format!(".s.PGSQL.{}", port));
        let _ = std::fs::create_dir_all(sock_dir);
        let _ = std::fs::remove_file(&sock_path);
        let pg_db = Arc::clone(&db);
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
            let _ = std::fs::set_permissions(
                &sock_path,
                std::os::unix::fs::PermissionsExt::from_mode(0o777),
            );
            let conn_counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(2_000_000));
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let db_clone = Arc::clone(&pg_db);
                        let conn_id = conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(async move {
                            if let Err(e) = heliosdb_nano::protocol::postgres::handler::handle_connection_unix(
                                db_clone, stream, conn_id,
                            ).await {
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
    #[cfg(not(unix))]
    let pg_unix_handle: Option<tokio::task::JoinHandle<()>> = None;
    let _ = &pg_socket_dir;

    // Start server with graceful shutdown handling
    tokio::select! {
        result = pg_server.serve() => {
            // Server stopped (error or normal shutdown)
            if let Err(ref e) = result {
                tracing::error!("Server error: {e}");
            }
            result?;
        }
        _ = health_server => {
            info!("Health server stopped");
        }
        _ = tokio::signal::ctrl_c() => {
            println!();
            println!("{}", "Received shutdown signal...".yellow());
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
    println!("Initializing new HeliosDB-Lite database...");
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
    use heliosdb_nano::repl::{ReplShell, ReplConfig};
    use heliosdb_nano::storage::{DumpManager, DumpOptions, DumpMode, DumpCompressionType};
    use colored::Colorize;

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
    pid_file: PathBuf,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    auth: String,
    password: Option<String>,
    ha_config: HAConfig,
) -> Result<()> {
    use std::process::{Command, Stdio};

    // Check if server is already running
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

    if let Some(cfg) = config_path {
        args.push("--config".to_string());
        args.push(cfg.display().to_string());
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

    // Add auth options
    args.push("--auth".to_string());
    args.push(auth);
    if let Some(pwd) = password {
        args.push("--password".to_string());
        args.push(pwd);
    }

    // Add HA options
    args.push("--replication-role".to_string());
    args.push(ha_config.role.clone());
    args.push("--replication-port".to_string());
    args.push(ha_config.replication_port.to_string());
    args.push("--sync-mode".to_string());
    args.push(ha_config.sync_mode.clone());
    args.push("--http-port".to_string());
    args.push(ha_config.http_port.to_string());
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
        let exe = std::env::current_exe()
            .map_err(|e| Error::io(format!("Failed to get current executable: {e}")))?;

        let child = Command::new(&exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::io(format!("Failed to spawn daemon process: {e}")))?;

        let pid = child.id();

        // Write PID file
        std::fs::write(&pid_file, pid.to_string())
            .map_err(|e| Error::io(format!("Failed to write PID file: {e}")))?;

        println!();
        println!("╔═══════════════════════════════════════════════════════════════╗");
        println!("║              HeliosDB-Lite Daemon Started                      ║");
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

    #[cfg(not(unix))]
    {
        Err(Error::io("Daemon mode is only supported on Unix systems"))
    }
}

fn stop_server(pid_file: &PathBuf) -> Result<()> {
    if !pid_file.exists() {
        return Err(Error::io(format!(
            "PID file not found: {}. Server may not be running.",
            pid_file.display()
        )));
    }

    let pid_str = std::fs::read_to_string(pid_file)
        .map_err(|e| Error::io(format!("Failed to read PID file: {e}")))?;

    let pid = pid_str.trim().parse::<i32>()
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
            std::fs::remove_file(pid_file)
                .map_err(|e| Error::io(format!("Failed to remove PID file: {e}")))?;

            println!("Server stopped successfully");
            Ok(())
        } else {
            Err(Error::io(format!("Failed to stop server. Process {pid} may not exist.")))
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

    let pid_str = std::fs::read_to_string(pid_file)
        .map_err(|e| Error::io(format!("Failed to read PID file: {e}")))?;

    let pid = pid_str.trim().parse::<i32>()
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
        streaming::{StreamingServer, StreamingServerConfig, StreamingClient, StreamingClientConfig},
        wal_store::{WalStore, WalStoreConfig},
        wal_applicator::WalApplicator,
        config::PrimaryConfig,
        SyncModeConfig,
        ha_state::{ha_state, HARole, SyncMode as HASyncMode, NodeConfig},
    };
    use std::sync::Arc;
    use uuid::Uuid;
    use std::time::Duration;

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
        other => return Err(Error::config(format!(
            "Unknown sync mode: '{}'. Use: async, semi-sync, sync", other
        ))),
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
            let repl_addr = format!("{}:{}", listen, ha_config.replication_port).parse()
                .map_err(|e| Error::config(format!("Invalid replication address: {e}")))?;

            let wal_store = Arc::new(WalStore::new(WalStoreConfig::default()));
            wal_store.init().await.map_err(|e| Error::io(format!("WAL store init failed: {e}")))?;

            let server_config = StreamingServerConfig {
                listen_addr: repl_addr,
                sync_mode,
                max_standbys: 10,
                heartbeat_interval: Duration::from_secs(1),
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
            let primary_host = ha_config.primary_host.as_ref()
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
            applicator.start_with_storage(storage.clone()).await
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
        _ => {
            Err(Error::config(format!(
                "Unknown replication role: '{}'. Use: standalone, primary, standby, observer",
                ha_config.role
            )))
        }
    }
}

/// Simple HTTP health server for Docker health checks
async fn start_health_server(addr: std::net::SocketAddr) -> std::io::Result<()> {
    use tokio::net::TcpListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind(addr).await?;
    info!("Health server listening on {}", addr);

    loop {
        let (mut socket, _) = listener.accept().await?;

        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            if socket.read(&mut buf).await.is_ok() {
                // Proper HTTP response with correct formatting
                let body = r#"{"status":"ok"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });
    }
}
