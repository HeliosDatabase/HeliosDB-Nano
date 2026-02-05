//! Connection Pool - HeliosProxy
//!
//! Manages connection pooling with configurable limits, idle timeout,
//! and health-aware connection management.

use super::{NodeId, ProxyError, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, Semaphore};
use uuid::Uuid;

/// Connection pool configuration
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Minimum connections per node
    pub min_connections: usize,
    /// Maximum connections per node
    pub max_connections: usize,
    /// Connection idle timeout
    pub idle_timeout: Duration,
    /// Connection lifetime (max age before recycling)
    pub max_lifetime: Duration,
    /// Acquire timeout
    pub acquire_timeout: Duration,
    /// Validate connection before use
    pub test_on_acquire: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_connections: 2,
            max_connections: 10,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(1800),
            acquire_timeout: Duration::from_secs(30),
            test_on_acquire: true,
        }
    }
}

/// A pooled connection
#[derive(Debug)]
pub struct PooledConnection {
    /// Connection ID
    pub id: Uuid,
    /// Node this connection belongs to
    pub node_id: NodeId,
    /// When the connection was created
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last used timestamp
    pub last_used: chrono::DateTime<chrono::Utc>,
    /// Connection state
    pub state: ConnectionState,
    /// Use count
    pub use_count: u64,
}

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Available for use
    Idle,
    /// Currently in use
    InUse,
    /// Being validated
    Validating,
    /// Closed/invalid
    Closed,
}

/// Per-node connection pool
struct NodePool {
    /// Node ID
    node_id: NodeId,
    /// Available connections
    connections: Vec<PooledConnection>,
    /// Semaphore for limiting connections
    semaphore: Arc<Semaphore>,
    /// Total created connections
    total_created: u64,
    /// Total closed connections
    total_closed: u64,
}

impl NodePool {
    fn new(node_id: NodeId, max_connections: usize) -> Self {
        Self {
            node_id,
            connections: Vec::new(),
            semaphore: Arc::new(Semaphore::new(max_connections)),
            total_created: 0,
            total_closed: 0,
        }
    }
}

/// Connection Pool Manager
pub struct ConnectionPool {
    /// Configuration
    config: PoolConfig,
    /// Per-node pools
    pools: Arc<RwLock<HashMap<NodeId, NodePool>>>,
    /// Total connections across all nodes
    total_connections: AtomicU64,
    /// Active (in-use) connections
    active_connections: AtomicU64,
    /// Metrics
    metrics: Arc<RwLock<PoolMetrics>>,
}

/// Pool metrics
#[derive(Debug, Clone, Default)]
pub struct PoolMetrics {
    /// Total connection acquires
    pub acquires: u64,
    /// Acquire failures
    pub acquire_failures: u64,
    /// Connections created
    pub connections_created: u64,
    /// Connections closed
    pub connections_closed: u64,
    /// Connections recycled (exceeded lifetime)
    pub connections_recycled: u64,
    /// Validation failures
    pub validation_failures: u64,
    /// Timeout waiting for connection
    pub acquire_timeouts: u64,
}

impl ConnectionPool {
    /// Create a new connection pool
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            pools: Arc::new(RwLock::new(HashMap::new())),
            total_connections: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            metrics: Arc::new(RwLock::new(PoolMetrics::default())),
        }
    }

    /// Add a node to the pool
    pub async fn add_node(&self, node_id: NodeId) {
        let mut pools = self.pools.write().await;
        if !pools.contains_key(&node_id) {
            pools.insert(
                node_id,
                NodePool::new(node_id, self.config.max_connections),
            );
            tracing::debug!("Added node {:?} to connection pool", node_id);
        }
    }

    /// Remove a node from the pool
    pub async fn remove_node(&self, node_id: &NodeId) {
        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.remove(node_id) {
            let count = pool.connections.len() as u64;
            self.total_connections.fetch_sub(count, Ordering::SeqCst);
            tracing::debug!("Removed node {:?} from connection pool", node_id);
        }
    }

    /// Get a connection from the pool
    pub async fn get_connection(&self, node_id: &NodeId) -> Result<PooledConnection> {
        // Update metrics
        {
            let mut metrics = self.metrics.write().await;
            metrics.acquires += 1;
        }

        let mut pools = self.pools.write().await;
        let pool = pools.get_mut(node_id).ok_or_else(|| {
            ProxyError::Connection(format!("Node {:?} not found in pool", node_id))
        })?;

        // Try to acquire a semaphore permit (with timeout)
        let permit_result = tokio::time::timeout(
            self.config.acquire_timeout,
            pool.semaphore.clone().acquire_owned(),
        )
        .await;

        match permit_result {
            Ok(Ok(_permit)) => {
                // Try to get an existing idle connection
                if let Some(idx) = pool
                    .connections
                    .iter()
                    .position(|c| c.state == ConnectionState::Idle)
                {
                    let mut conn = pool.connections.remove(idx);

                    // Check lifetime
                    let age = chrono::Utc::now()
                        .signed_duration_since(conn.created_at)
                        .to_std()
                        .unwrap_or(Duration::ZERO);

                    if age > self.config.max_lifetime {
                        // Recycle old connection
                        self.metrics.write().await.connections_recycled += 1;
                        // Create new connection instead
                        conn = self.create_connection(*node_id).await?;
                    }

                    conn.state = ConnectionState::InUse;
                    conn.last_used = chrono::Utc::now();
                    conn.use_count += 1;

                    self.active_connections.fetch_add(1, Ordering::SeqCst);

                    return Ok(conn);
                }

                // Create new connection
                let conn = self.create_connection(*node_id).await?;
                self.active_connections.fetch_add(1, Ordering::SeqCst);
                self.total_connections.fetch_add(1, Ordering::SeqCst);
                pool.total_created += 1;

                Ok(conn)
            }
            Ok(Err(_)) => {
                self.metrics.write().await.acquire_failures += 1;
                Err(ProxyError::PoolExhausted(format!(
                    "Failed to acquire semaphore for node {:?}",
                    node_id
                )))
            }
            Err(_) => {
                self.metrics.write().await.acquire_timeouts += 1;
                Err(ProxyError::Timeout(format!(
                    "Timeout acquiring connection for node {:?}",
                    node_id
                )))
            }
        }
    }

    /// Return a connection to the pool
    pub async fn return_connection(&self, mut conn: PooledConnection) {
        self.active_connections.fetch_sub(1, Ordering::SeqCst);

        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get_mut(&conn.node_id) {
            conn.state = ConnectionState::Idle;
            conn.last_used = chrono::Utc::now();
            pool.connections.push(conn);
        }
    }

    /// Close a connection (don't return to pool)
    pub async fn close_connection(&self, conn: PooledConnection) {
        self.active_connections.fetch_sub(1, Ordering::SeqCst);
        self.total_connections.fetch_sub(1, Ordering::SeqCst);

        let mut metrics = self.metrics.write().await;
        metrics.connections_closed += 1;

        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get_mut(&conn.node_id) {
            pool.total_closed += 1;
        }

        tracing::debug!("Closed connection {:?}", conn.id);
    }

    /// Create a new connection
    ///
    /// In a full implementation, this would:
    /// 1. Look up the node address from topology manager
    /// 2. Create a TCP connection to the node
    /// 3. Perform connection handshake/authentication
    /// 4. Store the TcpStream in the PooledConnection
    async fn create_connection(&self, node_id: NodeId) -> Result<PooledConnection> {
        // In production, this would use the topology manager to get the node address:
        // let node = topology_manager().get_node(node_id.0)?;
        // let addr = format!("{}:{}", node.client_addr);
        // let stream = TcpStream::connect(&addr).await?;

        let now = chrono::Utc::now();
        let conn = PooledConnection {
            id: Uuid::new_v4(),
            node_id,
            created_at: now,
            last_used: now,
            state: ConnectionState::InUse,
            use_count: 1,
        };

        self.metrics.write().await.connections_created += 1;

        tracing::debug!("Created connection {:?} for node {:?}", conn.id, node_id);

        Ok(conn)
    }

    /// Validate a connection is still usable
    ///
    /// In a full implementation, this would:
    /// 1. Send a ping query (e.g., "SELECT 1")
    /// 2. Check for response within timeout
    /// 3. Mark connection as invalid if ping fails
    pub async fn validate_connection(&self, conn: &PooledConnection) -> Result<bool> {
        // Check connection state
        if conn.state == ConnectionState::Closed {
            self.metrics.write().await.validation_failures += 1;
            return Ok(false);
        }

        // Check connection age - recycle if too old
        let age = chrono::Utc::now() - conn.created_at;
        if age > chrono::Duration::from_std(self.config.max_lifetime).unwrap_or_default() {
            self.metrics.write().await.connections_recycled += 1;
            return Ok(false);
        }

        // Check idle timeout
        let idle_time = chrono::Utc::now() - conn.last_used;
        if idle_time > chrono::Duration::from_std(self.config.idle_timeout).unwrap_or_default() {
            self.metrics.write().await.validation_failures += 1;
            return Ok(false);
        }

        // In production: send ping query and check response
        // let result = conn.stream.query("SELECT 1").await;
        // if result.is_err() { return Ok(false); }

        Ok(true)
    }

    /// Close all connections
    pub async fn close_all(&self) -> Result<()> {
        let mut pools = self.pools.write().await;
        for (_, pool) in pools.iter_mut() {
            pool.connections.clear();
        }
        self.total_connections.store(0, Ordering::SeqCst);
        self.active_connections.store(0, Ordering::SeqCst);
        tracing::info!("Closed all connections");
        Ok(())
    }

    /// Evict idle connections that have exceeded idle timeout
    pub async fn evict_idle(&self) {
        let mut pools = self.pools.write().await;
        let mut evicted = 0;

        for (_, pool) in pools.iter_mut() {
            let before = pool.connections.len();
            pool.connections.retain(|conn| {
                let idle_time = chrono::Utc::now()
                    .signed_duration_since(conn.last_used)
                    .to_std()
                    .unwrap_or(Duration::ZERO);

                idle_time < self.config.idle_timeout
            });
            evicted += before - pool.connections.len();
        }

        if evicted > 0 {
            self.total_connections
                .fetch_sub(evicted as u64, Ordering::SeqCst);
            tracing::debug!("Evicted {} idle connections", evicted);
        }
    }

    /// Get total connections
    pub async fn total_connections(&self) -> usize {
        self.total_connections.load(Ordering::SeqCst) as usize
    }

    /// Get active connections
    pub async fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst) as usize
    }

    /// Get pool metrics
    pub async fn metrics(&self) -> PoolMetrics {
        self.metrics.read().await.clone()
    }

    /// Get per-node statistics
    pub async fn node_stats(&self, node_id: &NodeId) -> Option<NodePoolStats> {
        let pools = self.pools.read().await;
        pools.get(node_id).map(|pool| NodePoolStats {
            idle_connections: pool
                .connections
                .iter()
                .filter(|c| c.state == ConnectionState::Idle)
                .count(),
            total_created: pool.total_created,
            total_closed: pool.total_closed,
        })
    }
}

/// Per-node pool statistics
#[derive(Debug, Clone)]
pub struct NodePoolStats {
    /// Number of idle connections
    pub idle_connections: usize,
    /// Total connections created for this node
    pub total_created: u64,
    /// Total connections closed for this node
    pub total_closed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_config_default() {
        let config = PoolConfig::default();
        assert_eq!(config.min_connections, 2);
        assert_eq!(config.max_connections, 10);
        assert!(config.test_on_acquire);
    }

    #[tokio::test]
    async fn test_add_remove_node() {
        let pool = ConnectionPool::new(PoolConfig::default());
        let node_id = NodeId::new();

        pool.add_node(node_id).await;
        assert!(pool.node_stats(&node_id).await.is_some());

        pool.remove_node(&node_id).await;
        assert!(pool.node_stats(&node_id).await.is_none());
    }

    #[tokio::test]
    async fn test_get_return_connection() {
        let pool = ConnectionPool::new(PoolConfig::default());
        let node_id = NodeId::new();

        pool.add_node(node_id).await;

        // Get connection
        let conn = pool.get_connection(&node_id).await.expect("get failed");
        assert_eq!(conn.node_id, node_id);
        assert_eq!(conn.state, ConnectionState::InUse);
        assert_eq!(pool.active_connections().await, 1);

        // Return connection
        pool.return_connection(conn).await;
        assert_eq!(pool.active_connections().await, 0);
    }

    #[tokio::test]
    async fn test_metrics() {
        let pool = ConnectionPool::new(PoolConfig::default());
        let node_id = NodeId::new();

        pool.add_node(node_id).await;

        let conn = pool.get_connection(&node_id).await.expect("get failed");
        pool.return_connection(conn).await;

        let metrics = pool.metrics().await;
        assert_eq!(metrics.acquires, 1);
        assert_eq!(metrics.connections_created, 1);
    }
}
