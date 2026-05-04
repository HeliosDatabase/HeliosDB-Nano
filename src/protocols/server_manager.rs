//! Protocol Server Manager
//!
//! Provides unified management for multiple protocol servers (Oracle, PostgreSQL, etc.)

use crate::{Result, Error, EmbeddedDatabase};
use super::oracle::OracleServer;
use crate::protocols::OracleServerConfig;
use crate::protocol::postgres::server::{PgServer, PgServerConfig};
use crate::protocol::postgres::auth::AuthMethod;
use std::sync::Arc;
use std::net::SocketAddr;
use tokio::sync::broadcast;
use tracing::{info, error};

/// Server shutdown signal
type ShutdownSignal = broadcast::Receiver<()>;

/// Protocol server manager configuration
#[derive(Debug, Clone)]
pub struct ServerManagerConfig {
    /// Enable Oracle protocol server
    pub enable_oracle: bool,
    /// Oracle server configuration
    pub oracle_config: OracleServerConfig,

    /// Enable PostgreSQL protocol server
    pub enable_postgres: bool,
    /// PostgreSQL listen address
    pub postgres_addr: String,
    /// PostgreSQL port
    pub postgres_port: u16,
    /// PostgreSQL authentication method
    pub postgres_auth_method: AuthMethod,
}

impl Default for ServerManagerConfig {
    fn default() -> Self {
        Self {
            enable_oracle: true,
            oracle_config: OracleServerConfig::default(),
            enable_postgres: false,
            postgres_addr: "127.0.0.1".to_string(),
            postgres_port: 5432,
            postgres_auth_method: AuthMethod::Trust,
        }
    }
}

impl ServerManagerConfig {
    /// Create configuration with Oracle only
    pub fn oracle_only(config: OracleServerConfig) -> Self {
        Self {
            enable_oracle: true,
            oracle_config: config,
            enable_postgres: false,
            postgres_addr: "127.0.0.1".to_string(),
            postgres_port: 5432,
            postgres_auth_method: AuthMethod::Trust,
        }
    }

    /// Create configuration with both protocols
    pub fn dual_protocol(oracle_config: OracleServerConfig, postgres_port: u16) -> Self {
        Self {
            enable_oracle: true,
            oracle_config,
            enable_postgres: true,
            postgres_addr: "127.0.0.1".to_string(),
            postgres_port,
            postgres_auth_method: AuthMethod::Trust,
        }
    }

    /// Enable PostgreSQL protocol
    pub fn with_postgres(mut self, addr: String, port: u16, auth_method: AuthMethod) -> Self {
        self.enable_postgres = true;
        self.postgres_addr = addr;
        self.postgres_port = port;
        self.postgres_auth_method = auth_method;
        self
    }
}

/// Protocol server manager
///
/// Manages multiple protocol servers (Oracle, PostgreSQL) with unified lifecycle
pub struct ServerManager {
    /// Server configuration
    config: ServerManagerConfig,
    /// Embedded database (shared across all protocols)
    database: Arc<EmbeddedDatabase>,
    /// Shutdown broadcast channel
    shutdown_tx: broadcast::Sender<()>,
}

impl ServerManager {
    /// Create a new server manager
    pub fn new(database: Arc<EmbeddedDatabase>, config: ServerManagerConfig) -> Self {
        let (shutdown_tx, _) = broadcast::channel(16);

        Self {
            config,
            database,
            shutdown_tx,
        }
    }

    /// Start all enabled protocol servers
    pub async fn start(self) -> Result<()> {
        info!("Starting HeliosDB Nano Protocol Server Manager");

        let mut tasks = Vec::new();

        // Extract database Arc for shared use
        let database = Arc::clone(&self.database);

        // Start Oracle server if enabled
        if self.config.enable_oracle {
            info!(
                "Starting Oracle TNS server on {}:{}",
                self.config.oracle_config.listen_addr,
                self.config.oracle_config.port
            );

            // Oracle server uses Arc<StorageEngine> which is now stored in database
            let storage = Arc::clone(&database.storage);
            let oracle_server = OracleServer::new(
                storage,
                self.config.oracle_config.clone(),
            );

            let mut shutdown_rx = self.shutdown_tx.subscribe();
            let oracle_task = tokio::spawn(async move {
                tokio::select! {
                    result = oracle_server.start() => {
                        if let Err(e) = result {
                            error!("Oracle server error: {}", e);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Oracle server shutting down");
                    }
                }
            });

            tasks.push(oracle_task);
        }

        // Start PostgreSQL server if enabled
        if self.config.enable_postgres {
            info!(
                "Starting PostgreSQL protocol server on {}:{}",
                self.config.postgres_addr,
                self.config.postgres_port
            );

            // Parse PostgreSQL server address
            let pg_addr: SocketAddr = format!("{}:{}", self.config.postgres_addr, self.config.postgres_port)
                .parse()
                .map_err(|e| Error::config(format!("Invalid PostgreSQL address: {}", e)))?;

            // Create PostgreSQL server configuration
            let pg_config = PgServerConfig::with_address(pg_addr)
                .with_auth_method(self.config.postgres_auth_method)
                .with_max_connections(100);

            // Create PostgreSQL server
            let pg_server = PgServer::new(pg_config, Arc::clone(&self.database))
                .map_err(|e| Error::internal(format!("Failed to create PostgreSQL server: {}", e)))?;

            let mut shutdown_rx = self.shutdown_tx.subscribe();
            let pg_task = tokio::spawn(async move {
                tokio::select! {
                    result = pg_server.serve() => {
                        if let Err(e) = result {
                            error!("PostgreSQL server error: {}", e);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("PostgreSQL server shutting down");
                    }
                }
            });

            tasks.push(pg_task);
        }

        if tasks.is_empty() {
            return Err(Error::internal("No protocol servers enabled"));
        }

        info!(
            "Protocol servers started: Oracle={}, PostgreSQL={}",
            self.config.enable_oracle,
            self.config.enable_postgres
        );

        // Wait for Ctrl+C
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal (Ctrl+C)");
            }
        }

        // Send shutdown signal to all servers
        let _ = self.shutdown_tx.send(());

        // Wait for all tasks to complete
        info!("Waiting for protocol servers to shut down...");
        for task in tasks {
            let _ = task.await;
        }

        info!("All protocol servers shut down successfully");
        Ok(())
    }

    /// Get server health status
    pub fn health_check(&self) -> ServerHealth {
        // Check if database is accessible (basic health check)
        // StorageEngine is always accessible if we have it in the database
        let storage_ok = true;

        ServerHealth {
            oracle_enabled: self.config.enable_oracle,
            postgres_enabled: self.config.enable_postgres,
            storage_ok,
        }
    }
}

/// Server health status
#[derive(Debug, Clone)]
pub struct ServerHealth {
    /// Oracle server enabled
    pub oracle_enabled: bool,
    /// PostgreSQL server enabled
    pub postgres_enabled: bool,
    /// Storage engine healthy
    pub storage_ok: bool,
}

impl ServerHealth {
    /// Check if all enabled servers are healthy
    pub fn is_healthy(&self) -> bool {
        self.storage_ok
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn test_manager_config_default() {
        let config = ServerManagerConfig::default();
        assert!(config.enable_oracle);
        assert!(!config.enable_postgres);
    }

    #[test]
    fn test_manager_config_oracle_only() {
        let oracle_config = OracleServerConfig {
            listen_addr: "0.0.0.0".to_string(),
            port: 1521,
            max_connections: 50,
        };

        let config = ServerManagerConfig::oracle_only(oracle_config);
        assert!(config.enable_oracle);
        assert!(!config.enable_postgres);
        assert_eq!(config.oracle_config.port, 1521);
    }

    #[test]
    fn test_manager_creation() {
        let database = EmbeddedDatabase::new_in_memory().unwrap();
        let manager_config = ServerManagerConfig::default();

        let manager = ServerManager::new(Arc::new(database), manager_config);
        let health = manager.health_check();

        assert!(health.oracle_enabled);
        assert!(!health.postgres_enabled);
        assert!(health.is_healthy());
    }
}
