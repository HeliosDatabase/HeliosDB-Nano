//! PostgreSQL wire protocol TCP server
//!
//! Listens for client connections and spawns session handlers

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use super::session::Session;
use crate::{EmbeddedDatabase, Error};

/// Default maximum connections for the wire protocol server
const DEFAULT_MAX_CONNECTIONS: usize = 256;

/// PostgreSQL wire protocol server
pub struct PgServer {
    /// Bind address
    address: String,
    /// Database instance
    db: Arc<EmbeddedDatabase>,
    /// Next session ID
    next_session_id: Arc<AtomicU32>,
    /// Connection limiter
    connection_limiter: Arc<Semaphore>,
    /// Max connections (for logging)
    max_connections: usize,
    /// Idle connection timeout in seconds (0 = no timeout)
    idle_timeout_secs: u64,
}

impl PgServer {
    /// Create a new PostgreSQL server
    ///
    /// # Arguments
    ///
    /// * `address` - Bind address (e.g., "127.0.0.1:5432")
    /// * `db` - Database instance
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_nano::{EmbeddedDatabase, network::PgServer};
    /// use std::sync::Arc;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    /// let server = PgServer::new("127.0.0.1:5432", db);
    /// server.run().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(address: impl Into<String>, db: Arc<EmbeddedDatabase>) -> Self {
        Self {
            address: address.into(),
            db,
            next_session_id: Arc::new(AtomicU32::new(1)),
            connection_limiter: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout_secs: 300, // Default 5 minutes
        }
    }

    /// Create a new server with a custom connection limit
    pub fn with_max_connections(address: impl Into<String>, db: Arc<EmbeddedDatabase>, max_connections: usize) -> Self {
        Self {
            address: address.into(),
            db,
            next_session_id: Arc::new(AtomicU32::new(1)),
            connection_limiter: Arc::new(Semaphore::new(max_connections)),
            max_connections,
            idle_timeout_secs: 300,
        }
    }

    /// Set idle connection timeout in seconds (0 = no timeout)
    pub fn with_idle_timeout(mut self, secs: u64) -> Self {
        self.idle_timeout_secs = secs;
        self
    }

    /// Run the server
    ///
    /// This will bind to the configured address and start accepting connections.
    /// The function will run until an error occurs or the server is shut down.
    pub async fn run(self) -> Result<(), Error> {
        let listener = TcpListener::bind(&self.address)
            .await
            .map_err(|e| Error::protocol(format!("Failed to bind to {}: {}", self.address, e)))?;

        info!("HeliosDB PostgreSQL server listening on {} (max_connections: {})", self.address, self.max_connections);
        let parts: Vec<&str> = self.address.split(':').collect();
        let host = parts.first().unwrap_or(&"localhost");
        let port = parts.get(1).unwrap_or(&"5432");
        info!("Connect with: psql -h {} -p {}", host, port);

        loop {
            // Accept new connection
            let (stream, addr) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                    continue;
                }
            };

            // Enforce connection limit
            let permit = match Arc::clone(&self.connection_limiter).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    warn!("Connection limit reached ({}), rejecting {}", self.max_connections, addr);
                    drop(stream);
                    continue;
                }
            };

            // Generate session ID
            let session_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);

            info!("New connection from {} (session {})", addr, session_id);

            // Create session
            let session = Session::new(Arc::clone(&self.db), session_id)
                        .with_idle_timeout(self.idle_timeout_secs);

            // Spawn handler task (permit released when task completes)
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = session.handle_connection(stream).await {
                    error!("Session {} error: {}", session_id, e);
                }
                info!("Session {} ended", session_id);
            });
        }
    }

    /// Run the server with graceful shutdown
    ///
    /// This will run the server until the shutdown signal is received.
    ///
    /// # Arguments
    ///
    /// * `shutdown` - A future that resolves when shutdown is requested
    pub async fn run_with_shutdown<F>(self, shutdown: F) -> Result<(), Error>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind(&self.address)
            .await
            .map_err(|e| Error::protocol(format!("Failed to bind to {}: {}", self.address, e)))?;

        info!("HeliosDB PostgreSQL server listening on {} (max_connections: {})", self.address, self.max_connections);
        let parts: Vec<&str> = self.address.split(':').collect();
        let host = parts.first().unwrap_or(&"localhost");
        let port = parts.get(1).unwrap_or(&"5432");
        info!("Connect with: psql -h {} -p {}", host, port);

        // Pin the shutdown future
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                // Accept new connection
                result = listener.accept() => {
                    let (stream, addr) = match result {
                        Ok(conn) => conn,
                        Err(e) => {
                            error!("Failed to accept connection: {}", e);
                            continue;
                        }
                    };

                    // Enforce connection limit
                    let permit = match Arc::clone(&self.connection_limiter).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            warn!("Connection limit reached ({}), rejecting {}", self.max_connections, addr);
                            drop(stream);
                            continue;
                        }
                    };

                    // Generate session ID
                    let session_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);

                    info!("New connection from {} (session {})", addr, session_id);

                    // Create session
                    let session = Session::new(Arc::clone(&self.db), session_id)
                        .with_idle_timeout(self.idle_timeout_secs);

                    // Spawn handler task (permit released when task completes)
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = session.handle_connection(stream).await {
                            error!("Session {} error: {}", session_id, e);
                        }
                        info!("Session {} ended", session_id);
                    });
                }

                // Shutdown signal received
                () = &mut shutdown => {
                    info!("Shutdown signal received, stopping server");
                    break;
                }
            }
        }

        info!("Server stopped");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_creation() {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let _server = PgServer::new("127.0.0.1:0", db);
        // Server created successfully
    }
}
