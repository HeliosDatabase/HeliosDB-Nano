//! PostgreSQL TCP server
//!
//! This module implements a TCP server that listens for PostgreSQL protocol
//! connections and spawns handlers for each connection.

use crate::{Result, Error, EmbeddedDatabase};
use super::handler::PgConnectionHandler;
use super::auth::{AuthManager, AuthMethod};
use super::ssl::{SslConfig, SslNegotiator, SslMode, SecureConnection};
use tokio::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Default PostgreSQL listen address (0.0.0.0:5432)
const DEFAULT_PG_ADDRESS: SocketAddr = SocketAddr::new(
    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    5432
);

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
}

impl Default for PgServerConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_PG_ADDRESS,
            auth_method: AuthMethod::Trust,
            max_connections: 100,
            ssl_config: None,
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

    /// Set SSL configuration
    pub fn with_ssl(mut self, ssl_config: SslConfig) -> Self {
        self.ssl_config = Some(ssl_config);
        self
    }

    /// Enable SSL with default test certificates
    pub fn with_ssl_test(mut self) -> Result<Self> {
        let ssl_config = SslConfig::new(
            SslMode::Allow,
            "certs/server.crt",
            "certs/server.key",
        );
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
}

impl PgServer {
    /// Create a new PostgreSQL server
    pub fn new(config: PgServerConfig, database: Arc<EmbeddedDatabase>) -> Result<Self> {
        let auth_manager = Arc::new(
            AuthManager::new(config.auth_method)
                .with_default_users()
        );

        // Initialize SSL negotiator if SSL is configured
        let ssl_negotiator = if let Some(ref ssl_config) = config.ssl_config {
            Some(Arc::new(SslNegotiator::new(ssl_config.clone())?))
        } else {
            None
        };

        Ok(Self {
            config,
            database,
            auth_manager,
            ssl_negotiator,
        })
    }

    /// Create server with custom authentication manager
    pub fn with_auth_manager(
        config: PgServerConfig,
        database: Arc<EmbeddedDatabase>,
        auth_manager: AuthManager,
    ) -> Result<Self> {
        // Initialize SSL negotiator if SSL is configured
        let ssl_negotiator = if let Some(ref ssl_config) = config.ssl_config {
            Some(Arc::new(SslNegotiator::new(ssl_config.clone())?))
        } else {
            None
        };

        Ok(Self {
            config,
            database,
            auth_manager: Arc::new(auth_manager),
            ssl_negotiator,
        })
    }

    /// Start the server and listen for connections
    ///
    /// This method runs the server loop and does not return unless an error occurs.
    /// Use `tokio::spawn()` to run it in the background.
    pub async fn serve(&self) -> Result<()> {
        let listener = TcpListener::bind(self.config.address).await
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
                    tracing::debug!("Accepted connection from {}", addr);

                    let database = Arc::clone(&self.database);
                    let auth_manager = Arc::clone(&self.auth_manager);
                    let ssl_negotiator = self.ssl_negotiator.clone();

                    // Spawn a new task for each connection
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(stream, database, auth_manager, ssl_negotiator).await {
                            tracing::error!("Connection error from {}: {}", addr, e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    /// Handle a single connection with optional SSL/TLS
    async fn handle_connection(
        mut stream: TcpStream,
        database: Arc<EmbeddedDatabase>,
        auth_manager: Arc<AuthManager>,
        ssl_negotiator: Option<Arc<SslNegotiator>>,
    ) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Read message length
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await
            .map_err(|e| Error::network(format!("Failed to read message length: {}", e)))?;

        // Read request code
        let mut code_buf = [0u8; 4];
        stream.read_exact(&mut code_buf).await
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
                        let tls_stream = acceptor.accept(stream).await
                            .map_err(|e| Error::network(format!("TLS handshake failed: {}", e)))?;

                        let secure_conn = SecureConnection::Tls(tls_stream);
                        let mut handler = PgConnectionHandler::new_with_stream(
                            secure_conn,
                            database,
                            auth_manager,
                            None // TLS stream starts fresh
                        );
                        return handler.handle().await;
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
            stream.write_all(b"N").await
                .map_err(|e| Error::network(format!("Failed to send SSL rejection: {}", e)))?;
            stream.flush().await
                .map_err(|e| Error::network(format!("Failed to flush stream: {}", e)))?;
            
            // After rejection, client will send startup message.
            // We haven't consumed any of THAT message yet.
            // So initial_data should be None for the handler.
            let secure_conn = SecureConnection::Plain(stream);
            let mut handler = PgConnectionHandler::new_with_stream(
                secure_conn,
                database,
                auth_manager,
                None
            );
            return handler.handle().await;
        }

        // Plain connection with potentially consumed startup header
        let mut initial_data = Vec::with_capacity(8);
        initial_data.extend_from_slice(&len_buf);
        initial_data.extend_from_slice(&code_buf);

        let secure_conn = SecureConnection::Plain(stream);
        let mut handler = PgConnectionHandler::new_with_stream(
            secure_conn,
            database,
            auth_manager,
            Some(&initial_data)
        );
        handler.handle().await
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
        self.config.ssl_config = Some(SslConfig::new(
            SslMode::Allow,
            "certs/server.crt",
            "certs/server.key",
        ));
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

        let ssl_config = SslConfig::new(
            SslMode::Require,
            "cert.pem",
            "key.pem",
        );
        let config_with_ssl = PgServerConfig::default().with_ssl(ssl_config);
        assert!(config_with_ssl.ssl_config.is_some());
    }
}
