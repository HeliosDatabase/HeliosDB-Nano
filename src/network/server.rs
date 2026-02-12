//! PostgreSQL wire protocol TCP server
//!
//! Listens for client connections and spawns session handlers

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::net::TcpListener;
use tracing::{error, info};

use super::session::Session;
use crate::{EmbeddedDatabase, Error};

/// PostgreSQL wire protocol server
pub struct PgServer {
    /// Bind address
    address: String,
    /// Database instance
    db: Arc<EmbeddedDatabase>,
    /// Next session ID
    next_session_id: Arc<AtomicU32>,
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
    /// use heliosdb_lite::{EmbeddedDatabase, network::PgServer};
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
        }
    }

    /// Run the server
    ///
    /// This will bind to the configured address and start accepting connections.
    /// The function will run until an error occurs or the server is shut down.
    pub async fn run(self) -> Result<(), Error> {
        let listener = TcpListener::bind(&self.address)
            .await
            .map_err(|e| Error::protocol(format!("Failed to bind to {}: {}", self.address, e)))?;

        info!("HeliosDB PostgreSQL server listening on {}", self.address);
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

            // Generate session ID
            let session_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);

            info!("New connection from {} (session {})", addr, session_id);

            // Create session
            let session = Session::new(Arc::clone(&self.db), session_id);

            // Spawn handler task
            tokio::spawn(async move {
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

        info!("HeliosDB PostgreSQL server listening on {}", self.address);
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

                    // Generate session ID
                    let session_id = self.next_session_id.fetch_add(1, Ordering::SeqCst);

                    info!("New connection from {} (session {})", addr, session_id);

                    // Create session
                    let session = Session::new(Arc::clone(&self.db), session_id);

                    // Spawn handler task
                    tokio::spawn(async move {
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
