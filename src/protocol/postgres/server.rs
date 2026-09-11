//! PostgreSQL TCP server
//!
//! This module implements a TCP server that listens for PostgreSQL protocol
//! connections and spawns handlers for each connection.

use super::auth::{AuthManager, AuthMethod};
use super::handler::{ConnectionPolicy, PgConnectionHandler};
use super::ssl::{SecureConnection, SslConfig, SslMode, SslNegotiator};
use super::timeouts::ConnectionTimeouts;
use crate::{EmbeddedDatabase, Error, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::BufWriter;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// Default PostgreSQL listen address (0.0.0.0:5432)
const DEFAULT_PG_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5432);

/// PostgreSQL server configuration
#[derive(Debug, Clone)]
pub struct PgServerConfig {
    /// Listen address
    pub address: SocketAddr,
    /// Authentication method
    pub auth_method: AuthMethod,
    /// Maximum concurrent connections
    pub max_connections: usize,
    /// SSL/TLS configuration (optional)
    pub ssl_config: Option<SslConfig>,
    /// GH#28: connection-lifetime policy (`authentication_timeout`,
    /// `idle_session_timeout`, `idle_in_transaction_session_timeout`, TCP
    /// keepalive, utilisation warning). PostgreSQL defaults.
    pub timeouts: ConnectionTimeouts,
}

impl Default for PgServerConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_PG_ADDRESS,
            auth_method: AuthMethod::Trust,
            max_connections: 100,
            ssl_config: None,
            timeouts: ConnectionTimeouts::default(),
        }
    }
}

impl PgServerConfig {
    /// Create with custom address
    pub fn with_address(address: SocketAddr) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    /// Set authentication method
    pub fn with_auth_method(mut self, method: AuthMethod) -> Self {
        self.auth_method = method;
        self
    }

    /// Set maximum connections
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// GH#28: set the connection-lifetime policy.
    pub fn with_timeouts(mut self, timeouts: ConnectionTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Set SSL configuration
    pub fn with_ssl(mut self, ssl_config: SslConfig) -> Self {
        self.ssl_config = Some(ssl_config);
        self
    }

    /// Enable SSL with default test certificates
    pub fn with_ssl_test(mut self) -> Result<Self> {
        let ssl_config = SslConfig::new(SslMode::Allow, "certs/server.crt", "certs/server.key");
        self.ssl_config = Some(ssl_config);
        Ok(self)
    }
}

/// PostgreSQL server
pub struct PgServer {
    config: PgServerConfig,
    database: Arc<EmbeddedDatabase>,
    auth_manager: Arc<AuthManager>,
    ssl_negotiator: Option<Arc<SslNegotiator>>,
    connection_limiter: Arc<Semaphore>,
    /// GH#28: edge trigger for the utilisation WARN — `true` while in-use
    /// connections are at or above `max_connections_warn_percent`, so the
    /// line is logged once per crossing, never once per accept.
    utilisation_warned: AtomicBool,
}

impl PgServer {
    /// Refuse `AuthMethod::Trust` on a non-loopback listener. v3.26.0
    /// safety gate: silently accepting any client on a public interface
    /// is a footgun, so the server refuses to start in that
    /// configuration. SCRAM-SHA-256 and CleartextPassword stay available
    /// for non-loopback deployments.
    fn enforce_trust_loopback_only(config: &PgServerConfig) -> Result<()> {
        if matches!(config.auth_method, AuthMethod::Trust) && !config.address.ip().is_loopback() {
            return Err(Error::authentication(format!(
                "AuthMethod::Trust is only permitted on loopback (127.0.0.1, ::1) listeners; \
                 binding to {} requires a non-trust auth method (password, scram-sha-256). \
                 To start anyway on a non-loopback address, switch the auth method or bind to 127.0.0.1.",
                config.address
            )));
        }
        Ok(())
    }

    /// Create a new PostgreSQL server
    pub fn new(config: PgServerConfig, database: Arc<EmbeddedDatabase>) -> Result<Self> {
        Self::enforce_trust_loopback_only(&config)?;

        let auth_manager = Arc::new(AuthManager::new(config.auth_method).with_default_users());

        // Initialize SSL negotiator if SSL is configured
        let ssl_negotiator = if let Some(ref ssl_config) = config.ssl_config {
            Some(Arc::new(SslNegotiator::new(ssl_config.clone())?))
        } else {
            None
        };

        let connection_limiter = Arc::new(Semaphore::new(config.max_connections));

        Ok(Self {
            config,
            database,
            auth_manager,
            ssl_negotiator,
            connection_limiter,
            utilisation_warned: AtomicBool::new(false),
        })
    }

    /// Create server with custom authentication manager
    pub fn with_auth_manager(
        config: PgServerConfig,
        database: Arc<EmbeddedDatabase>,
        auth_manager: AuthManager,
    ) -> Result<Self> {
        // Apply the trust-loopback gate using the AuthManager's method
        // (the user may have constructed it with a different method
        // than `config.auth_method`).
        let effective_method = auth_manager.method();
        if matches!(effective_method, AuthMethod::Trust) && !config.address.ip().is_loopback() {
            return Err(Error::authentication(format!(
                "AuthMethod::Trust is only permitted on loopback (127.0.0.1, ::1) listeners; \
                 binding to {} requires a non-trust auth method (password, scram-sha-256).",
                config.address
            )));
        }

        // Initialize SSL negotiator if SSL is configured
        let ssl_negotiator = if let Some(ref ssl_config) = config.ssl_config {
            Some(Arc::new(SslNegotiator::new(ssl_config.clone())?))
        } else {
            None
        };

        let connection_limiter = Arc::new(Semaphore::new(config.max_connections));

        Ok(Self {
            config,
            database,
            auth_manager: Arc::new(auth_manager),
            ssl_negotiator,
            connection_limiter,
            utilisation_warned: AtomicBool::new(false),
        })
    }

    /// GH#28: WARN once when in-use connections cross
    /// `max_connections_warn_percent` of `max_connections`, and re-arm when
    /// utilisation drops back below it. `Semaphore::available_permits()` is
    /// the single source of truth — no side counter.
    fn maybe_warn_utilisation(&self) {
        let max = self.config.max_connections;
        let in_use = max.saturating_sub(self.connection_limiter.available_permits());
        let over = self.config.timeouts.should_warn_utilisation(in_use, max);
        if self.utilisation_warned.swap(over, Ordering::Relaxed) != over && over {
            tracing::warn!(
                "connection utilisation {}/{} ({}%) is at or above [server] max_connections_warn_percent = {}; \
                 new connections are refused at {} (raise --max-connections / [server] max_connections)",
                in_use,
                max,
                in_use.saturating_mul(100) / max.max(1),
                self.config.timeouts.connection_warn_threshold_percent,
                max
            );
        }
    }

    /// Start the server and listen for connections
    ///
    /// This method runs the server loop and does not return unless an error occurs.
    /// Use `tokio::spawn()` to run it in the background.
    pub async fn serve(&self) -> Result<()> {
        let listener = TcpListener::bind(self.config.address)
            .await
            .map_err(|e| Error::network(format!("Failed to bind to {}: {}", self.config.address, e)))?;

        let ssl_enabled = self.ssl_negotiator.is_some();
        tracing::info!(
            "PostgreSQL server listening on {} (auth: {:?}, ssl: {})",
            self.config.address,
            self.config.auth_method,
            if ssl_enabled { "enabled" } else { "disabled" }
        );

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    // TCP_NODELAY (low-latency responses) + SO_KEEPALIVE (GH#28:
                    // half-open peers are reaped by the kernel). Never fatal.
                    if let Err(e) = super::timeouts::apply_socket_options(&stream, &self.config.timeouts) {
                        tracing::warn!("Failed to apply socket options for {}: {}", addr, e);
                    }

                    // Enforce max_connections via semaphore
                    let permit = match Arc::clone(&self.connection_limiter).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::warn!(
                                "Connection limit reached ({}), rejecting {}",
                                self.config.max_connections,
                                addr
                            );
                            drop(stream);
                            continue;
                        }
                    };

                    tracing::debug!("Accepted connection from {}", addr);
                    self.maybe_warn_utilisation();

                    let database = Arc::clone(&self.database);
                    let auth_manager = Arc::clone(&self.auth_manager);
                    let ssl_negotiator = self.ssl_negotiator.clone();
                    // GH#28: ONE absolute authentication deadline, computed here
                    // and shared by the pre-startup reads, the TLS accept and the
                    // handler's startup / password / SCRAM round trips.
                    let policy = ConnectionPolicy::at_accept(self.config.timeouts.clone(), self.config.max_connections);

                    // Spawn a new task for each connection. `_permit` is declared
                    // FIRST so it drops LAST — after the handler's `Drop` has
                    // rolled back and released the session — and it is never
                    // cloned or moved: the slot is released exactly once.
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) =
                            Self::handle_connection(stream, database, auth_manager, ssl_negotiator, policy).await
                        {
                            tracing::error!("Connection error from {}: {}", addr, e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept connection: {}", e);
                    // Accept errors (e.g. EMFILE at fd exhaustion) return
                    // immediately — without a pause this loop busy-spins at
                    // 100% CPU exactly when the process is resource-starved.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Handle a single connection with optional SSL/TLS
    ///
    /// GH#28: the whole pre-authentication region — both header reads, the
    /// SSL answer and the TLS handshake — runs under the ONE absolute
    /// `authentication_timeout` deadline computed at accept; the same instant
    /// then bounds startup / password / SCRAM inside `handler.handle()`. On
    /// expiry NOTHING is written to the unauthenticated peer (fail closed;
    /// PostgreSQL `_exit(1)`s), the half-built stream is dropped and the
    /// permit goes with the task. No code path resumes the stream after an
    /// expiry: `read_exact` / TLS accept / `write_all` are not cancel-safe.
    async fn handle_connection(
        stream: TcpStream,
        database: Arc<EmbeddedDatabase>,
        auth_manager: Arc<AuthManager>,
        ssl_negotiator: Option<Arc<SslNegotiator>>,
        policy: ConnectionPolicy,
    ) -> Result<()> {
        let auth_deadline = policy.auth_deadline;
        let negotiate = Self::negotiate(stream, database, auth_manager, ssl_negotiator, policy);
        let mut handler = match auth_deadline {
            Some(at) => match tokio::time::timeout_at(at, negotiate).await {
                Ok(negotiated) => negotiated?,
                Err(_elapsed) => {
                    // DEBUG, not higher: internet scanners produce this at volume.
                    tracing::debug!("authentication_timeout expired before startup completed; closing");
                    return Ok(());
                }
            },
            None => negotiate.await?,
        };
        handler.handle().await
    }

    /// Pre-startup negotiation: read the 8-byte request header, answer /
    /// perform SSL, and build the handler for the resulting stream (plain or
    /// TLS). All three outcomes yield the same handler type.
    async fn negotiate(
        mut stream: TcpStream,
        database: Arc<EmbeddedDatabase>,
        auth_manager: Arc<AuthManager>,
        ssl_negotiator: Option<Arc<SslNegotiator>>,
        policy: ConnectionPolicy,
    ) -> Result<PgConnectionHandler<BufWriter<SecureConnection<TcpStream>>>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Read message length
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| Error::network(format!("Failed to read message length: {}", e)))?;

        // Read request code
        let mut code_buf = [0u8; 4];
        stream
            .read_exact(&mut code_buf)
            .await
            .map_err(|e| Error::network(format!("Failed to read request code: {}", e)))?;

        let code = i32::from_be_bytes(code_buf);
        let is_ssl_request = code == super::ssl::SSL_REQUEST_CODE;

        // Handle SSL negotiation based on configuration
        if let Some(negotiator) = ssl_negotiator {
            if is_ssl_request {
                // Negotiate SSL
                let ssl_accepted = negotiator.negotiate(&mut stream, true).await?;

                if ssl_accepted {
                    // Upgrade connection to TLS
                    if let Some(acceptor) = negotiator.acceptor() {
                        tracing::debug!("Upgrading connection to TLS");
                        let tls_stream = acceptor
                            .accept(stream)
                            .await
                            .map_err(|e| Error::network(format!("TLS handshake failed: {}", e)))?;

                        let secure_conn = SecureConnection::Tls(tls_stream);
                        let handler = PgConnectionHandler::new_with_stream(
                            secure_conn,
                            database,
                            auth_manager,
                            None, // TLS stream starts fresh
                        );
                        return Ok(handler.with_connection_policy(policy));
                    }
                } else if negotiator.is_required() {
                    return Err(Error::network("SSL is required but was rejected"));
                }
            } else if negotiator.is_required() {
                return Err(Error::network("SSL is required but no SSL request was received"));
            }
        } else if is_ssl_request {
            // SSL is not configured, but client requested it - reject with 'N'
            tracing::debug!("SSL request received but SSL is not configured, sending rejection");
            stream
                .write_all(b"N")
                .await
                .map_err(|e| Error::network(format!("Failed to send SSL rejection: {}", e)))?;
            stream
                .flush()
                .await
                .map_err(|e| Error::network(format!("Failed to flush stream: {}", e)))?;

            // After rejection, client will send startup message.
            // We haven't consumed any of THAT message yet.
            // So initial_data should be None for the handler.
            let secure_conn = SecureConnection::Plain(stream);
            let handler = PgConnectionHandler::new_with_stream(secure_conn, database, auth_manager, None);
            return Ok(handler.with_connection_policy(policy));
        }

        // Plain connection with potentially consumed startup header
        let mut initial_data = Vec::with_capacity(8);
        initial_data.extend_from_slice(&len_buf);
        initial_data.extend_from_slice(&code_buf);

        let secure_conn = SecureConnection::Plain(stream);
        let handler = PgConnectionHandler::new_with_stream(secure_conn, database, auth_manager, Some(&initial_data));
        Ok(handler.with_connection_policy(policy))
    }

    /// Get server configuration
    pub fn config(&self) -> &PgServerConfig {
        &self.config
    }
}

/// Builder for PostgreSQL server
pub struct PgServerBuilder {
    config: PgServerConfig,
    auth_manager: Option<AuthManager>,
}

impl PgServerBuilder {
    /// Create a new server builder
    pub fn new() -> Self {
        Self {
            config: PgServerConfig::default(),
            auth_manager: None,
        }
    }

    /// Set listen address
    pub fn address(mut self, addr: SocketAddr) -> Self {
        self.config.address = addr;
        self
    }

    /// Set authentication method
    pub fn auth_method(mut self, method: AuthMethod) -> Self {
        self.config.auth_method = method;
        self
    }

    /// Set maximum connections
    pub fn max_connections(mut self, max: usize) -> Self {
        self.config.max_connections = max;
        self
    }

    /// GH#28: set the connection-lifetime policy.
    pub fn timeouts(mut self, timeouts: ConnectionTimeouts) -> Self {
        self.config.timeouts = timeouts;
        self
    }

    /// Set custom authentication manager
    pub fn auth_manager(mut self, manager: AuthManager) -> Self {
        self.auth_manager = Some(manager);
        self
    }

    /// Set SSL configuration
    pub fn ssl_config(mut self, ssl_config: SslConfig) -> Self {
        self.config.ssl_config = Some(ssl_config);
        self
    }

    /// Enable SSL with test certificates
    pub fn ssl_test(mut self) -> Self {
        self.config.ssl_config = Some(SslConfig::new(SslMode::Allow, "certs/server.crt", "certs/server.key"));
        self
    }

    /// Build the server
    pub fn build(self, database: Arc<EmbeddedDatabase>) -> Result<PgServer> {
        if let Some(auth_manager) = self.auth_manager {
            PgServer::with_auth_manager(self.config, database, auth_manager)
        } else {
            PgServer::new(self.config, database)
        }
    }
}

impl Default for PgServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = PgServerConfig::default();
        assert_eq!(config.address.port(), 5432);
        assert_eq!(config.max_connections, 100);
    }

    #[test]
    fn test_config_builder() {
        let addr: SocketAddr = "127.0.0.1:15432".parse().unwrap();
        let config = PgServerConfig::with_address(addr)
            .with_auth_method(AuthMethod::CleartextPassword)
            .with_max_connections(50);

        assert_eq!(config.address, addr);
        assert_eq!(config.auth_method, AuthMethod::CleartextPassword);
        assert_eq!(config.max_connections, 50);
    }

    #[test]
    fn test_server_builder() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let addr: SocketAddr = "127.0.0.1:15432".parse().unwrap();

        let server = PgServerBuilder::new()
            .address(addr)
            .auth_method(AuthMethod::Trust)
            .max_connections(25)
            .build(db)
            .unwrap();

        assert_eq!(server.config().address, addr);
        assert_eq!(server.config().max_connections, 25);
    }

    #[test]
    fn test_ssl_config() {
        let config = PgServerConfig::default();
        assert!(config.ssl_config.is_none());

        let ssl_config = SslConfig::new(SslMode::Require, "cert.pem", "key.pem");
        let config_with_ssl = PgServerConfig::default().with_ssl(ssl_config);
        assert!(config_with_ssl.ssl_config.is_some());
    }
}
