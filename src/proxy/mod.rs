//! HeliosProxy - Connection Router and Failover Manager
//!
//! Intelligent connection routing, failover, and load balancing for HeliosDB-Lite.
//! Provides Oracle-grade resilience with Transaction Replay (TR).
//!
//! # Features
//!
//! - **Connection Pooling**: Efficient connection reuse and management
//! - **Load Balancing**: Read/write splitting, round-robin, latency-based routing
//! - **Health Monitoring**: Continuous node health checking
//! - **TR (Transaction Replay)**: Resume transactions after node failure
//!
//! # Deployment Options
//!
//! - **Embedded**: Library component within application
//! - **Standalone**: Separate proxy process
//! - **Sidecar**: Kubernetes sidecar container
//!
//! # Feature Flags
//!
//! - `ha-proxy`: Base proxy functionality
//! - `ha-tr`: Transaction Replay (requires ha-proxy + ha-tier1)

pub mod connection_pool;
pub mod failover_controller;
pub mod load_balancer;
pub mod health_checker;

// Switchover support modules
pub mod switchover_buffer;
pub mod primary_tracker;

// TR (Transaction Replay) modules
#[cfg(feature = "ha-tr")]
pub mod transaction_journal;
#[cfg(feature = "ha-tr")]
pub mod failover_replay;
#[cfg(feature = "ha-tr")]
pub mod cursor_restore;
#[cfg(feature = "ha-tr")]
pub mod session_migrate;

// Re-exports
pub use connection_pool::{ConnectionPool, PoolConfig, PooledConnection};
pub use failover_controller::{FailoverController, FailoverConfig, FailoverMode};
#[cfg(feature = "ha-tr")]
pub use failover_controller::CoordinatedReplayResult;
pub use load_balancer::{LoadBalancer, LoadBalancerConfig, RoutingStrategy};
pub use health_checker::{HealthChecker, HealthConfig, NodeHealth};
pub use switchover_buffer::{SwitchoverBuffer, BufferConfig, BufferState};
pub use primary_tracker::{PrimaryTracker, PrimaryInfo};

#[cfg(feature = "ha-tr")]
pub use transaction_journal::{TransactionJournal, JournalEntry};
#[cfg(feature = "ha-tr")]
pub use failover_replay::{FailoverReplay, ReplayResult};
#[cfg(feature = "ha-tr")]
pub use cursor_restore::{CursorRestore, CursorState};
#[cfg(feature = "ha-tr")]
pub use session_migrate::{SessionMigrate, SessionState};

use thiserror::Error;
use uuid::Uuid;

/// Proxy errors
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Pool exhausted: {0}")]
    PoolExhausted(String),

    #[error("No healthy nodes available")]
    NoHealthyNodes,

    #[error("Failover failed: {0}")]
    FailoverFailed(String),

    #[error("Transaction replay failed: {0}")]
    ReplayFailed(String),

    #[error("Session migration failed: {0}")]
    SessionMigration(String),

    #[error("Cursor restore failed: {0}")]
    CursorRestore(String),

    #[error("Health check failed: {0}")]
    HealthCheck(String),

    #[error("Routing error: {0}")]
    Routing(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;

/// Node identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

/// Node role in the cluster
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    /// Primary node (accepts writes)
    Primary,
    /// Standby node (read-only, can be promoted)
    Standby,
    /// Read replica (read-only, cannot be promoted)
    ReadReplica,
    /// Unknown role (during discovery)
    Unknown,
}

/// Node endpoint information
#[derive(Debug, Clone)]
pub struct NodeEndpoint {
    /// Node identifier
    pub id: NodeId,
    /// Host address
    pub host: String,
    /// Port
    pub port: u16,
    /// Node role
    pub role: NodeRole,
    /// Weight for load balancing (higher = more traffic)
    pub weight: u32,
    /// Whether this node is enabled
    pub enabled: bool,
}

impl NodeEndpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            id: NodeId::new(),
            host: host.into(),
            port,
            role: NodeRole::Unknown,
            weight: 100,
            enabled: true,
        }
    }

    pub fn with_role(mut self, role: NodeRole) -> Self {
        self.role = role;
        self
    }

    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Proxy configuration
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Connection pool configuration
    pub pool: PoolConfig,
    /// Load balancer configuration
    pub load_balancer: LoadBalancerConfig,
    /// Health checker configuration
    pub health: HealthConfig,
    /// Failover configuration
    pub failover: FailoverConfig,
    /// TR mode (if ha-tr enabled)
    pub tr_mode: TrMode,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            pool: PoolConfig::default(),
            load_balancer: LoadBalancerConfig::default(),
            health: HealthConfig::default(),
            failover: FailoverConfig::default(),
            tr_mode: TrMode::Session,
        }
    }
}

/// TR (Transaction Replay) modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrMode {
    /// No failover, return error on failure
    None,
    /// Re-establish session, lose transaction
    Session,
    /// Re-execute SELECT after failover
    Select,
    /// Replay entire transaction (full TAC)
    Transaction,
}

/// HeliosProxy - Main proxy instance
pub struct HeliosProxy {
    /// Configuration
    config: ProxyConfig,
    /// Node endpoints
    nodes: Vec<NodeEndpoint>,
    /// Connection pool
    pool: ConnectionPool,
    /// Load balancer
    load_balancer: LoadBalancer,
    /// Health checker
    health_checker: HealthChecker,
    /// Failover controller
    failover_controller: FailoverController,
}

impl HeliosProxy {
    /// Create a new proxy instance
    pub fn new(config: ProxyConfig) -> Self {
        Self {
            pool: ConnectionPool::new(config.pool.clone()),
            load_balancer: LoadBalancer::new(config.load_balancer.clone()),
            health_checker: HealthChecker::new(config.health.clone()),
            failover_controller: FailoverController::new(config.failover.clone()),
            config,
            nodes: Vec::new(),
        }
    }

    /// Add a node to the proxy
    pub fn add_node(&mut self, node: NodeEndpoint) {
        self.nodes.push(node.clone());
        self.load_balancer.add_node(node.clone());
        self.health_checker.add_node(node);
    }

    /// Remove a node from the proxy
    pub fn remove_node(&mut self, node_id: &NodeId) {
        self.nodes.retain(|n| &n.id != node_id);
        self.load_balancer.remove_node(node_id);
        self.health_checker.remove_node(node_id);
    }

    /// Get a connection for a read query
    pub async fn get_read_connection(&self) -> Result<PooledConnection> {
        let node = self.load_balancer.select_for_read()?;
        self.pool.get_connection(&node.id).await
    }

    /// Get a connection for a write query
    pub async fn get_write_connection(&self) -> Result<PooledConnection> {
        let node = self.load_balancer.select_for_write()?;
        self.pool.get_connection(&node.id).await
    }

    /// Start the proxy (health checks, etc.)
    pub async fn start(&self) -> Result<()> {
        self.health_checker.start().await?;
        tracing::info!("HeliosProxy started with {} nodes", self.nodes.len());
        Ok(())
    }

    /// Stop the proxy
    pub async fn stop(&self) -> Result<()> {
        self.health_checker.stop().await?;
        self.pool.close_all().await?;
        tracing::info!("HeliosProxy stopped");
        Ok(())
    }

    /// Get proxy statistics
    pub async fn stats(&self) -> ProxyStats {
        ProxyStats {
            total_nodes: self.nodes.len(),
            healthy_nodes: self.health_checker.healthy_count().await,
            total_connections: self.pool.total_connections().await,
            active_connections: self.pool.active_connections().await,
            requests_routed: self.load_balancer.requests_routed(),
            failovers_triggered: self.failover_controller.failover_count(),
        }
    }

    /// Get TR mode
    pub fn tr_mode(&self) -> TrMode {
        self.config.tr_mode
    }
}

/// Proxy statistics
#[derive(Debug, Clone)]
pub struct ProxyStats {
    /// Total registered nodes
    pub total_nodes: usize,
    /// Currently healthy nodes
    pub healthy_nodes: usize,
    /// Total connections in pool
    pub total_connections: usize,
    /// Active (in-use) connections
    pub active_connections: usize,
    /// Total requests routed
    pub requests_routed: u64,
    /// Total failovers triggered
    pub failovers_triggered: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_endpoint() {
        let node = NodeEndpoint::new("localhost", 5432)
            .with_role(NodeRole::Primary)
            .with_weight(150);

        assert_eq!(node.host, "localhost");
        assert_eq!(node.port, 5432);
        assert_eq!(node.role, NodeRole::Primary);
        assert_eq!(node.weight, 150);
        assert_eq!(node.address(), "localhost:5432");
    }

    #[test]
    fn test_proxy_config_default() {
        let config = ProxyConfig::default();
        assert_eq!(config.tr_mode, TrMode::Session);
    }

    #[test]
    fn test_error_display() {
        let err = ProxyError::NoHealthyNodes;
        assert_eq!(err.to_string(), "No healthy nodes available");
    }
}
