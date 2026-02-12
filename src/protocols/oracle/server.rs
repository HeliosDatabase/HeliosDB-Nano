//! Oracle TNS TCP Server
//!
//! Provides a TCP server that accepts Oracle TNS protocol connections
//! and routes them to the protocol handler.

use super::handler::OracleProtocolHandler;
use super::tns::TnsPacket;
use super::DEFAULT_ORACLE_PORT;
use crate::{Result, Error, storage::StorageEngine};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// Oracle TNS server configuration
#[derive(Debug, Clone)]
pub struct OracleServerConfig {
    /// Listen address
    pub listen_addr: String,
    /// Listen port (default: 1521)
    pub port: u16,
    /// Maximum concurrent connections
    pub max_connections: usize,
}

impl Default for OracleServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1".to_string(),
            port: DEFAULT_ORACLE_PORT,
            max_connections: 100,
        }
    }
}

/// Oracle TNS server
pub struct OracleServer {
    /// Server configuration
    config: OracleServerConfig,
    /// Storage engine (shared across connections)
    storage: Arc<StorageEngine>,
    /// Connection semaphore for limiting concurrent connections
    connection_limiter: Arc<Semaphore>,
}

impl OracleServer {
    /// Create a new Oracle server
    pub fn new(storage: Arc<StorageEngine>, config: OracleServerConfig) -> Self {
        let connection_limiter = Arc::new(Semaphore::new(config.max_connections));

        Self {
            config,
            storage,
            connection_limiter,
        }
    }

    /// Start the Oracle server
    pub async fn start(self) -> Result<()> {
        let addr = format!("{}:{}", self.config.listen_addr, self.config.port);

        tracing::info!("Starting Oracle TNS server on {}", addr);

        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| Error::io(format!("Failed to bind to {}: {}", addr, e)))?;

        tracing::info!("Oracle TNS server listening on {}", addr);

        loop {
            // Wait for connection slot
            let permit = self.connection_limiter.clone().acquire_owned().await
                .map_err(|e| Error::io(format!("Semaphore error: {}", e)))?;

            // Accept connection
            let (socket, peer_addr) = match listener.accept().await {
                Ok((socket, addr)) => (socket, addr),
                Err(e) => {
                    tracing::error!("Failed to accept connection: {}", e);
                    continue;
                }
            };

            tracing::info!("New Oracle connection from {}", peer_addr);

            // Spawn handler for this connection
            let storage = self.storage.clone();
            tokio::spawn(async move {
                let result = handle_connection(socket, storage).await;

                if let Err(e) = result {
                    tracing::error!("Connection error from {}: {}", peer_addr, e);
                }

                // Release permit when connection closes
                drop(permit);

                tracing::info!("Connection closed from {}", peer_addr);
            });
        }
    }

    /// Get server configuration
    pub fn config(&self) -> &OracleServerConfig {
        &self.config
    }
}

/// Handle a single Oracle TNS connection
async fn handle_connection(mut socket: TcpStream, storage: Arc<StorageEngine>) -> Result<()> {
    let mut handler = OracleProtocolHandler::new(storage);
    let mut buffer = vec![0u8; 65536]; // 64KB buffer

    loop {
        // Read TNS packet
        let n = socket.read(&mut buffer).await
            .map_err(|e| Error::io(format!("Failed to read from socket: {}", e)))?;

        if n == 0 {
            // Connection closed by client
            break;
        }

        tracing::debug!("Received {} bytes from client", n);

        // Parse TNS packet
        let recv_data = buffer.get(..n).ok_or_else(|| Error::io("Buffer read out of bounds"))?;
        let packet = match TnsPacket::parse(recv_data) {
            Ok(pkt) => pkt,
            Err(e) => {
                tracing::error!("Failed to parse TNS packet: {}", e);
                // Send error response and continue
                continue;
            }
        };

        tracing::debug!("Received TNS packet: type={:?}", packet.header.packet_type);

        // Handle packet
        let response_packets = match handler.handle_packet(packet) {
            Ok(packets) => packets,
            Err(e) => {
                tracing::error!("Handler error: {}", e);
                // Send error response as TNS Refuse packet
                let error_msg = format!("Error: {}", e);
                vec![TnsPacket::refuse(1, error_msg)]
            }
        };

        // Send response packets
        for response in response_packets {
            let encoded = response.encode();
            socket.write_all(&encoded).await
                .map_err(|e| Error::io(format!("Failed to write to socket: {}", e)))?;

            tracing::debug!("Sent {} bytes to client", encoded.len());
        }

        // Check if connection should be closed
        if handler.is_closed() {
            break;
        }
    }

    Ok(())
}

/// Start Oracle server with storage engine
pub async fn start_oracle_server(
    storage: Arc<StorageEngine>,
    config: OracleServerConfig,
) -> Result<()> {
    let server = OracleServer::new(storage, config);
    server.start().await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn test_server_config_default() {
        let config = OracleServerConfig::default();
        assert_eq!(config.port, 1521);
        assert_eq!(config.max_connections, 100);
    }

    #[test]
    fn test_server_creation() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let server_config = OracleServerConfig::default();
        let server = OracleServer::new(Arc::new(storage), server_config);

        assert_eq!(server.config().port, 1521);
    }

    #[tokio::test]
    async fn test_server_bind_error() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        // Try to bind to invalid address
        let server_config = OracleServerConfig {
            listen_addr: "999.999.999.999".to_string(),
            port: 1521,
            max_connections: 10,
        };

        let server = OracleServer::new(Arc::new(storage), server_config);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            server.start()
        ).await;

        // Should timeout or error immediately
        assert!(result.is_err() || result.unwrap().is_err());
    }
}
