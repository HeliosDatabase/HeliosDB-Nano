//! Remote Target - Branch-to-Server Replication
//!
//! Manages connections to remote servers for branch replication.
//! Handles authentication, connection pooling, and retry logic.

use super::config::{AuthMethod, SyncMode};
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected
    Disconnected,
    /// Connecting
    Connecting,
    /// Connected and ready
    Connected,
    /// Authentication in progress
    Authenticating,
    /// Connection failed
    Failed,
    /// Connection closed by remote
    ClosedByRemote,
}

/// Remote server information
#[derive(Debug, Clone)]
pub struct RemoteServer {
    /// Server ID
    pub id: Uuid,
    /// Host address
    pub host: String,
    /// Port
    pub port: u16,
    /// Server name (for logging)
    pub name: Option<String>,
    /// Connection state
    pub state: ConnectionState,
    /// Authentication method
    pub auth: AuthMethod,
    /// Sync mode
    pub sync_mode: SyncMode,
    /// Last connection attempt
    pub last_connect_attempt: Option<chrono::DateTime<chrono::Utc>>,
    /// Last successful connection
    pub last_connected: Option<chrono::DateTime<chrono::Utc>>,
    /// Consecutive failure count
    pub failure_count: u32,
    /// Server capabilities (discovered after connect)
    pub capabilities: ServerCapabilities,
}

/// Server capabilities discovered during handshake
#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    /// Supports branch replication
    pub branch_replication: bool,
    /// Supports content-addressed dedup
    pub content_dedup: bool,
    /// Maximum message size
    pub max_message_size: usize,
    /// Protocol version
    pub protocol_version: u32,
    /// Server version string
    pub server_version: Option<String>,
}

/// Connection metrics
#[derive(Debug, Clone, Default)]
pub struct ConnectionMetrics {
    /// Total bytes sent
    pub bytes_sent: u64,
    /// Total bytes received
    pub bytes_received: u64,
    /// Total messages sent
    pub messages_sent: u64,
    /// Total messages received
    pub messages_received: u64,
    /// Average latency (ms)
    pub avg_latency_ms: f64,
    /// Connection uptime
    pub uptime_seconds: u64,
    /// Reconnection count
    pub reconnections: u32,
}

/// Remote Target Manager
pub struct RemoteTargetManager {
    /// Known remote servers
    servers: Arc<RwLock<HashMap<Uuid, RemoteServer>>>,
    /// Connection metrics per server
    metrics: Arc<RwLock<HashMap<Uuid, ConnectionMetrics>>>,
    /// Default connection timeout
    connect_timeout: Duration,
    /// Default read timeout
    read_timeout: Duration,
    /// Maximum retry attempts
    max_retries: u32,
    /// Retry delay
    retry_delay: Duration,
}

impl RemoteTargetManager {
    /// Create a new remote target manager
    pub fn new() -> Self {
        Self {
            servers: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(RwLock::new(HashMap::new())),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_secs(5),
        }
    }

    /// Configure connection timeout
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Configure read timeout
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Configure retry settings
    pub fn with_retries(mut self, max_retries: u32, delay: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_delay = delay;
        self
    }

    /// Register a remote server
    pub async fn register_server(&self, server: RemoteServer) -> Result<Uuid> {
        let id = server.id;
        self.servers.write().await.insert(id, server);
        self.metrics.write().await.insert(id, ConnectionMetrics::default());
        Ok(id)
    }

    /// Register from host string (host:port)
    pub async fn register_from_host(
        &self,
        host_str: &str,
        auth: AuthMethod,
        sync_mode: SyncMode,
    ) -> Result<Uuid> {
        let (host, port) = parse_host_port(host_str)?;

        let server = RemoteServer {
            id: Uuid::new_v4(),
            host,
            port,
            name: None,
            state: ConnectionState::Disconnected,
            auth,
            sync_mode,
            last_connect_attempt: None,
            last_connected: None,
            failure_count: 0,
            capabilities: ServerCapabilities::default(),
        };

        self.register_server(server).await
    }

    /// Remove a server
    pub async fn remove_server(&self, server_id: &Uuid) -> Result<()> {
        self.servers.write().await.remove(server_id);
        self.metrics.write().await.remove(server_id);
        Ok(())
    }

    /// Get server info
    pub async fn get_server(&self, server_id: &Uuid) -> Option<RemoteServer> {
        self.servers.read().await.get(server_id).cloned()
    }

    /// Get all servers
    pub async fn list_servers(&self) -> Vec<RemoteServer> {
        self.servers.read().await.values().cloned().collect()
    }

    /// Get connected servers
    pub async fn connected_servers(&self) -> Vec<RemoteServer> {
        self.servers
            .read()
            .await
            .values()
            .filter(|s| s.state == ConnectionState::Connected)
            .cloned()
            .collect()
    }

    /// Connect to a server
    pub async fn connect(&self, server_id: &Uuid) -> Result<()> {
        let mut servers = self.servers.write().await;
        let server = servers.get_mut(server_id).ok_or_else(|| {
            ReplicationError::RemoteTarget(format!("Server {} not found", server_id))
        })?;

        server.state = ConnectionState::Connecting;
        server.last_connect_attempt = Some(chrono::Utc::now());

        // TODO: Implement actual connection
        // 1. Open TCP connection
        // 2. TLS handshake (if configured)
        // 3. Authentication
        // 4. Capability exchange

        tracing::info!(
            "Connecting to remote server {}:{} ({})",
            server.host,
            server.port,
            server_id
        );

        // For skeleton, simulate successful connection
        server.state = ConnectionState::Connected;
        server.last_connected = Some(chrono::Utc::now());
        server.failure_count = 0;
        server.capabilities = ServerCapabilities {
            branch_replication: true,
            content_dedup: true,
            max_message_size: 16 * 1024 * 1024,
            protocol_version: 1,
            server_version: Some("HeliosDB-Lite 1.0".to_string()),
        };

        Ok(())
    }

    /// Disconnect from a server
    pub async fn disconnect(&self, server_id: &Uuid) -> Result<()> {
        let mut servers = self.servers.write().await;
        let server = servers.get_mut(server_id).ok_or_else(|| {
            ReplicationError::RemoteTarget(format!("Server {} not found", server_id))
        })?;

        server.state = ConnectionState::Disconnected;

        tracing::info!("Disconnected from remote server {}", server_id);

        Ok(())
    }

    /// Record connection failure
    pub async fn record_failure(&self, server_id: &Uuid, error: &str) {
        if let Some(server) = self.servers.write().await.get_mut(server_id) {
            server.state = ConnectionState::Failed;
            server.failure_count += 1;
            tracing::warn!(
                "Connection to {} failed (attempt {}): {}",
                server_id,
                server.failure_count,
                error
            );
        }
    }

    /// Check if should retry connection
    pub async fn should_retry(&self, server_id: &Uuid) -> bool {
        if let Some(server) = self.servers.read().await.get(server_id) {
            if server.failure_count >= self.max_retries {
                return false;
            }

            // Check if enough time has passed
            if let Some(last_attempt) = server.last_connect_attempt {
                let elapsed = chrono::Utc::now()
                    .signed_duration_since(last_attempt)
                    .to_std()
                    .unwrap_or(Duration::ZERO);

                return elapsed >= self.retry_delay;
            }

            true
        } else {
            false
        }
    }

    /// Get connection metrics
    pub async fn get_metrics(&self, server_id: &Uuid) -> Option<ConnectionMetrics> {
        self.metrics.read().await.get(server_id).cloned()
    }

    /// Update metrics after send
    pub async fn record_send(&self, server_id: &Uuid, bytes: usize) {
        if let Some(metrics) = self.metrics.write().await.get_mut(server_id) {
            metrics.bytes_sent += bytes as u64;
            metrics.messages_sent += 1;
        }
    }

    /// Update metrics after receive
    pub async fn record_receive(&self, server_id: &Uuid, bytes: usize) {
        if let Some(metrics) = self.metrics.write().await.get_mut(server_id) {
            metrics.bytes_received += bytes as u64;
            metrics.messages_received += 1;
        }
    }

    /// Record latency sample
    pub async fn record_latency(&self, server_id: &Uuid, latency_ms: f64) {
        if let Some(metrics) = self.metrics.write().await.get_mut(server_id) {
            // Simple exponential moving average
            let alpha = 0.1;
            metrics.avg_latency_ms = alpha * latency_ms + (1.0 - alpha) * metrics.avg_latency_ms;
        }
    }

    /// Authenticate with a server
    pub async fn authenticate(&self, server_id: &Uuid) -> Result<()> {
        let mut servers = self.servers.write().await;
        let server = servers.get_mut(server_id).ok_or_else(|| {
            ReplicationError::RemoteTarget(format!("Server {} not found", server_id))
        })?;

        server.state = ConnectionState::Authenticating;

        // TODO: Implement actual authentication based on method
        match &server.auth {
            AuthMethod::Token { token } => {
                tracing::debug!("Authenticating with token (length: {})", token.len());
                // Send token to server
            }
            AuthMethod::Tls { cert_path, key_path: _, ca_path: _ } => {
                tracing::debug!("Authenticating with TLS cert: {}", cert_path.display());
                // TLS handled during connection
            }
            AuthMethod::SecurePairing { pairing_key } => {
                tracing::debug!("Authenticating with secure pairing (key length: {})", pairing_key.len());
                // Exchange pairing keys
            }
            AuthMethod::OAuth2 { client_id, .. } => {
                tracing::debug!("Authenticating with OAuth2 client: {}", client_id);
                // OAuth2 flow would be implemented here
            }
        }

        server.state = ConnectionState::Connected;
        Ok(())
    }
}

impl Default for RemoteTargetManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse host:port string
fn parse_host_port(host_str: &str) -> Result<(String, u16)> {
    let parts: Vec<&str> = host_str.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(ReplicationError::RemoteTarget(format!(
            "Invalid host:port format: {}",
            host_str
        )));
    }

    let port: u16 = parts[0].parse().map_err(|_| {
        ReplicationError::RemoteTarget(format!("Invalid port: {}", parts[0]))
    })?;

    Ok((parts[1].to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_host_port() {
        let (host, port) = parse_host_port("localhost:5432").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);

        let (host, port) = parse_host_port("192.168.1.100:9000").unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(port, 9000);

        // IPv6
        let (host, port) = parse_host_port("[::1]:5432").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 5432);

        // Invalid
        assert!(parse_host_port("localhost").is_err());
        assert!(parse_host_port("localhost:abc").is_err());
    }

    #[tokio::test]
    async fn test_register_server() {
        let manager = RemoteTargetManager::new();

        let id = manager
            .register_from_host(
                "localhost:5432",
                AuthMethod::Token { token: "test".to_string() },
                SyncMode::Async { max_lag_ms: 1000 },
            )
            .await
            .expect("register failed");

        let server = manager.get_server(&id).await.expect("server not found");
        assert_eq!(server.host, "localhost");
        assert_eq!(server.port, 5432);
        assert_eq!(server.state, ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn test_connect() {
        let manager = RemoteTargetManager::new();

        let id = manager
            .register_from_host(
                "localhost:5432",
                AuthMethod::Token { token: "test".to_string() },
                SyncMode::Sync,
            )
            .await
            .expect("register failed");

        manager.connect(&id).await.expect("connect failed");

        let server = manager.get_server(&id).await.unwrap();
        assert_eq!(server.state, ConnectionState::Connected);
        assert!(server.capabilities.branch_replication);
    }

    #[tokio::test]
    async fn test_metrics() {
        let manager = RemoteTargetManager::new();

        let id = manager
            .register_from_host(
                "localhost:5432",
                AuthMethod::Token { token: "test".to_string() },
                SyncMode::Sync,
            )
            .await
            .expect("register failed");

        manager.record_send(&id, 1024).await;
        manager.record_receive(&id, 512).await;
        manager.record_latency(&id, 10.0).await;

        let metrics = manager.get_metrics(&id).await.unwrap();
        assert_eq!(metrics.bytes_sent, 1024);
        assert_eq!(metrics.bytes_received, 512);
        assert_eq!(metrics.messages_sent, 1);
        assert_eq!(metrics.messages_received, 1);
    }

    #[tokio::test]
    async fn test_failure_tracking() {
        let manager = RemoteTargetManager::new().with_retries(3, Duration::from_millis(10));

        let id = manager
            .register_from_host(
                "localhost:5432",
                AuthMethod::Token { token: "test".to_string() },
                SyncMode::Sync,
            )
            .await
            .expect("register failed");

        // Record failures
        manager.record_failure(&id, "Connection refused").await;
        manager.record_failure(&id, "Connection refused").await;

        let server = manager.get_server(&id).await.unwrap();
        assert_eq!(server.failure_count, 2);
        assert!(manager.should_retry(&id).await);

        // Third failure
        manager.record_failure(&id, "Connection refused").await;
        assert!(!manager.should_retry(&id).await); // Max retries reached
    }
}
