//! Load Balancer - HeliosProxy
//!
//! Intelligent request routing with read/write splitting,
//! multiple routing strategies, and latency-aware selection.

use super::{NodeEndpoint, NodeId, NodeRole, ProxyError, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Load balancer configuration
#[derive(Debug, Clone)]
pub struct LoadBalancerConfig {
    /// Routing strategy for read queries
    pub read_strategy: RoutingStrategy,
    /// Routing strategy for write queries (usually Primary only)
    pub write_strategy: RoutingStrategy,
    /// Enable read/write splitting
    pub read_write_split: bool,
    /// Latency threshold for unhealthy marking (ms)
    pub latency_threshold_ms: u64,
    /// Minimum weight for a node to receive traffic
    pub min_weight: u32,
}

impl Default for LoadBalancerConfig {
    fn default() -> Self {
        Self {
            read_strategy: RoutingStrategy::RoundRobin,
            write_strategy: RoutingStrategy::PrimaryOnly,
            read_write_split: true,
            latency_threshold_ms: 100,
            min_weight: 1,
        }
    }
}

/// Routing strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingStrategy {
    /// Only route to primary (for writes)
    PrimaryOnly,
    /// Round-robin across all eligible nodes
    RoundRobin,
    /// Weighted round-robin based on node weights
    WeightedRoundRobin,
    /// Route to least connections
    LeastConnections,
    /// Route to lowest latency node
    LatencyBased,
    /// Random selection
    Random,
    /// Prefer local node (same rack/zone)
    PreferLocal,
}

/// Node health state for graceful degradation during failover
///
/// This enum enables the load balancer to handle intermediate states
/// during failover, allowing for graceful degradation rather than
/// binary healthy/unhealthy transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeHealth {
    /// Node is operating normally - can serve all traffic
    Healthy,
    /// Node is degraded (high latency or replication lag) but still usable for reads
    Degraded,
    /// Node is transitioning (failover in progress) - hold new requests
    Transitioning,
    /// Node is down or unreachable - do not route traffic
    Unhealthy,
}

impl NodeHealth {
    /// Check if node can serve read requests
    pub fn can_serve_reads(&self) -> bool {
        matches!(self, NodeHealth::Healthy | NodeHealth::Degraded)
    }

    /// Check if node can serve write requests
    pub fn can_serve_writes(&self) -> bool {
        matches!(self, NodeHealth::Healthy)
    }

    /// Check if node is in a usable state
    pub fn is_usable(&self) -> bool {
        !matches!(self, NodeHealth::Unhealthy)
    }
}

impl Default for NodeHealth {
    fn default() -> Self {
        NodeHealth::Healthy
    }
}

/// Node state for load balancing
#[derive(Debug, Clone)]
struct NodeState {
    /// Node endpoint
    endpoint: NodeEndpoint,
    /// Node health state (supports degraded/transitioning states)
    health: NodeHealth,
    /// Replication lag in milliseconds (for standby nodes)
    replication_lag_ms: u64,
    /// Current connection count
    connections: u64,
    /// Average latency (ms)
    avg_latency_ms: f64,
    /// Requests routed to this node
    requests: u64,
    /// Request failures
    failures: u64,
}

/// Load Balancer
pub struct LoadBalancer {
    /// Configuration
    config: LoadBalancerConfig,
    /// Node states
    nodes: Arc<RwLock<HashMap<NodeId, NodeState>>>,
    /// Round-robin counter
    rr_counter: AtomicU64,
    /// Total requests routed
    total_requests: AtomicU64,
    /// Weighted round-robin state
    wrr_state: Arc<RwLock<WeightedRRState>>,
}

/// Weighted round-robin state
#[derive(Debug, Default)]
struct WeightedRRState {
    /// Current index
    current_index: usize,
    /// Current weight
    current_weight: i32,
    /// GCD of all weights
    gcd_weight: u32,
    /// Maximum weight
    max_weight: u32,
}

impl LoadBalancer {
    /// Create a new load balancer
    pub fn new(config: LoadBalancerConfig) -> Self {
        Self {
            config,
            nodes: Arc::new(RwLock::new(HashMap::new())),
            rr_counter: AtomicU64::new(0),
            total_requests: AtomicU64::new(0),
            wrr_state: Arc::new(RwLock::new(WeightedRRState::default())),
        }
    }

    /// Add a node to the load balancer
    pub fn add_node(&mut self, endpoint: NodeEndpoint) {
        let node_id = endpoint.id;
        let state = NodeState {
            endpoint,
            health: NodeHealth::Healthy,
            replication_lag_ms: 0,
            connections: 0,
            avg_latency_ms: 0.0,
            requests: 0,
            failures: 0,
        };

        // Use blocking lock for simplicity in sync context
        // In production, this should be async
        let nodes = self.nodes.clone();
        tokio::spawn(async move {
            nodes.write().await.insert(node_id, state);
        });
    }

    /// Remove a node from the load balancer
    pub fn remove_node(&mut self, node_id: &NodeId) {
        let id = *node_id;
        let nodes = self.nodes.clone();
        tokio::spawn(async move {
            nodes.write().await.remove(&id);
        });
    }

    /// Select a node for a read query
    pub fn select_for_read(&self) -> Result<NodeEndpoint> {
        self.total_requests.fetch_add(1, Ordering::SeqCst);

        // Use blocking for sync compatibility
        let rt = tokio::runtime::Handle::try_current();
        let nodes_guard = match rt {
            Ok(handle) => {
                handle.block_on(async { self.nodes.read().await })
            }
            Err(_) => {
                // Fallback: return error if no runtime
                return Err(ProxyError::Routing("No async runtime available".to_string()));
            }
        };

        // First, filter for healthy or degraded nodes (can serve reads)
        let mut eligible: Vec<_> = nodes_guard
            .values()
            .filter(|n| n.health.can_serve_reads() && n.endpoint.enabled)
            .filter(|n| {
                self.config.read_write_split
                    || n.endpoint.role == NodeRole::Primary
                    || n.endpoint.role == NodeRole::Standby
                    || n.endpoint.role == NodeRole::ReadReplica
            })
            .collect();

        // If no healthy/degraded nodes, try transitioning nodes as last resort
        if eligible.is_empty() {
            eligible = nodes_guard
                .values()
                .filter(|n| n.health == NodeHealth::Transitioning && n.endpoint.enabled)
                .collect();
        }

        if eligible.is_empty() {
            return Err(ProxyError::NoHealthyNodes);
        }

        // Sort by health preference: Healthy first, then Degraded, then Transitioning
        eligible.sort_by_key(|n| match n.health {
            NodeHealth::Healthy => 0,
            NodeHealth::Degraded => 1,
            NodeHealth::Transitioning => 2,
            NodeHealth::Unhealthy => 3,
        });

        let selected = self.select_by_strategy(&eligible, self.config.read_strategy)?;
        Ok(selected.endpoint.clone())
    }

    /// Select a node for a write query
    pub fn select_for_write(&self) -> Result<NodeEndpoint> {
        self.total_requests.fetch_add(1, Ordering::SeqCst);

        let rt = tokio::runtime::Handle::try_current();
        let nodes_guard = match rt {
            Ok(handle) => {
                handle.block_on(async { self.nodes.read().await })
            }
            Err(_) => {
                return Err(ProxyError::Routing("No async runtime available".to_string()));
            }
        };

        // For writes, require fully healthy primary (not degraded)
        let primary = nodes_guard
            .values()
            .find(|n| n.endpoint.role == NodeRole::Primary && n.health.can_serve_writes() && n.endpoint.enabled);

        match primary {
            Some(node) => Ok(node.endpoint.clone()),
            None => Err(ProxyError::NoHealthyNodes),
        }
    }

    /// Select by strategy
    fn select_by_strategy<'a>(
        &self,
        nodes: &[&'a NodeState],
        strategy: RoutingStrategy,
    ) -> Result<&'a NodeState> {
        match strategy {
            RoutingStrategy::PrimaryOnly => {
                nodes
                    .iter()
                    .find(|n| n.endpoint.role == NodeRole::Primary)
                    .copied()
                    .ok_or(ProxyError::NoHealthyNodes)
            }
            RoutingStrategy::RoundRobin => {
                let idx = self.rr_counter.fetch_add(1, Ordering::SeqCst) as usize;
                Ok(nodes[idx % nodes.len()])
            }
            RoutingStrategy::WeightedRoundRobin => {
                // Simplified weighted selection
                let total_weight: u32 = nodes.iter().map(|n| n.endpoint.weight).sum();
                if total_weight == 0 {
                    return Err(ProxyError::NoHealthyNodes);
                }

                let idx = self.rr_counter.fetch_add(1, Ordering::SeqCst);
                let mut target = (idx % total_weight as u64) as u32;

                for node in nodes {
                    if target < node.endpoint.weight {
                        return Ok(node);
                    }
                    target -= node.endpoint.weight;
                }

                Ok(nodes[0])
            }
            RoutingStrategy::LeastConnections => {
                nodes
                    .iter()
                    .min_by_key(|n| n.connections)
                    .copied()
                    .ok_or(ProxyError::NoHealthyNodes)
            }
            RoutingStrategy::LatencyBased => {
                nodes
                    .iter()
                    .min_by(|a, b| {
                        a.avg_latency_ms
                            .partial_cmp(&b.avg_latency_ms)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .copied()
                    .ok_or(ProxyError::NoHealthyNodes)
            }
            RoutingStrategy::Random => {
                use std::time::{SystemTime, UNIX_EPOCH};
                let seed = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as usize;
                Ok(nodes[seed % nodes.len()])
            }
            RoutingStrategy::PreferLocal => {
                // For skeleton, just return first node
                // In production, would check rack/zone affinity
                nodes.first().copied().ok_or(ProxyError::NoHealthyNodes)
            }
        }
    }

    /// Set node health state
    ///
    /// Supports granular health states for graceful degradation:
    /// - Healthy: Normal operation
    /// - Degraded: High latency/lag but still usable for reads
    /// - Transitioning: Failover in progress
    /// - Unhealthy: Do not route traffic
    pub async fn set_node_health(&self, node_id: &NodeId, health: NodeHealth) {
        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            let old_health = node.health;
            node.health = health;
            tracing::debug!("Node {:?} health changed: {:?} -> {:?}", node_id, old_health, health);
        }
    }

    /// Legacy method for backward compatibility
    pub async fn set_node_healthy(&self, node_id: &NodeId, healthy: bool) {
        let health = if healthy { NodeHealth::Healthy } else { NodeHealth::Unhealthy };
        self.set_node_health(node_id, health).await;
    }

    /// Mark node as transitioning (failover in progress)
    pub async fn set_node_transitioning(&self, node_id: &NodeId) {
        self.set_node_health(node_id, NodeHealth::Transitioning).await;
    }

    /// Update node latency and adjust health state accordingly
    pub async fn update_latency(&self, node_id: &NodeId, latency_ms: f64) {
        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            // Exponential moving average
            let alpha = 0.2;
            node.avg_latency_ms = alpha * latency_ms + (1.0 - alpha) * node.avg_latency_ms;

            // Adjust health based on latency thresholds
            let threshold = self.config.latency_threshold_ms as f64;
            let degraded_threshold = threshold * 0.7; // 70% of threshold = degraded

            // Only adjust health if not transitioning (preserve failover state)
            if node.health != NodeHealth::Transitioning {
                if latency_ms > threshold {
                    node.health = NodeHealth::Unhealthy;
                    tracing::warn!(
                        "Node {:?} marked unhealthy due to high latency: {}ms",
                        node_id,
                        latency_ms
                    );
                } else if latency_ms > degraded_threshold {
                    node.health = NodeHealth::Degraded;
                    tracing::debug!(
                        "Node {:?} marked degraded due to elevated latency: {}ms",
                        node_id,
                        latency_ms
                    );
                } else if node.health == NodeHealth::Degraded || node.health == NodeHealth::Unhealthy {
                    // Recovery: if latency is back to normal, restore to healthy
                    node.health = NodeHealth::Healthy;
                    tracing::info!("Node {:?} recovered, marked healthy", node_id);
                }
            }
        }
    }

    /// Update node replication lag and adjust health state
    pub async fn update_replication_lag(&self, node_id: &NodeId, lag_ms: u64) {
        // Thresholds for replication lag (configurable in production)
        const DEGRADED_LAG_MS: u64 = 5000;   // 5 seconds = degraded
        const UNHEALTHY_LAG_MS: u64 = 30000; // 30 seconds = unhealthy

        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            node.replication_lag_ms = lag_ms;

            // Only adjust health if not transitioning
            if node.health != NodeHealth::Transitioning {
                if lag_ms > UNHEALTHY_LAG_MS {
                    node.health = NodeHealth::Unhealthy;
                    tracing::warn!(
                        "Node {:?} marked unhealthy due to high replication lag: {}ms",
                        node_id,
                        lag_ms
                    );
                } else if lag_ms > DEGRADED_LAG_MS {
                    node.health = NodeHealth::Degraded;
                    tracing::debug!(
                        "Node {:?} marked degraded due to replication lag: {}ms",
                        node_id,
                        lag_ms
                    );
                } else if node.health == NodeHealth::Degraded && node.avg_latency_ms < self.config.latency_threshold_ms as f64 * 0.7 {
                    // Recovery: lag is acceptable and latency is good
                    node.health = NodeHealth::Healthy;
                    tracing::info!("Node {:?} recovered from lag, marked healthy", node_id);
                }
            }
        }
    }

    /// Update node health based on combined metrics
    pub async fn update_node_metrics(&self, node_id: &NodeId, latency_ms: f64, replication_lag_ms: u64, failure_rate: f64) {
        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            // Update metrics
            node.avg_latency_ms = 0.2 * latency_ms + 0.8 * node.avg_latency_ms;
            node.replication_lag_ms = replication_lag_ms;

            // Only adjust health if not transitioning
            if node.health != NodeHealth::Transitioning {
                // Determine health based on all factors
                let new_health = if !Self::is_responsive(latency_ms) {
                    NodeHealth::Unhealthy
                } else if replication_lag_ms > 30000 {
                    NodeHealth::Unhealthy
                } else if replication_lag_ms > 5000 || failure_rate > 0.5 || latency_ms > self.config.latency_threshold_ms as f64 {
                    NodeHealth::Degraded
                } else {
                    NodeHealth::Healthy
                };

                if new_health != node.health {
                    tracing::debug!("Node {:?} health: {:?} -> {:?}", node_id, node.health, new_health);
                    node.health = new_health;
                }
            }
        }
    }

    /// Check if latency indicates node is responsive
    fn is_responsive(latency_ms: f64) -> bool {
        // Consider non-responsive if latency exceeds 5 seconds or is negative (timeout)
        latency_ms >= 0.0 && latency_ms < 5000.0
    }

    /// Increment connection count for a node
    pub async fn increment_connections(&self, node_id: &NodeId) {
        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            node.connections += 1;
            node.requests += 1;
        }
    }

    /// Decrement connection count for a node
    pub async fn decrement_connections(&self, node_id: &NodeId) {
        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            node.connections = node.connections.saturating_sub(1);
        }
    }

    /// Record a failure for a node
    pub async fn record_failure(&self, node_id: &NodeId) {
        if let Some(node) = self.nodes.write().await.get_mut(node_id) {
            node.failures += 1;
        }
    }

    /// Get total requests routed
    pub fn requests_routed(&self) -> u64 {
        self.total_requests.load(Ordering::SeqCst)
    }

    /// Get node statistics
    pub async fn node_stats(&self, node_id: &NodeId) -> Option<NodeStats> {
        self.nodes.read().await.get(node_id).map(|n| NodeStats {
            health: n.health,
            replication_lag_ms: n.replication_lag_ms,
            connections: n.connections,
            avg_latency_ms: n.avg_latency_ms,
            requests: n.requests,
            failures: n.failures,
        })
    }

    /// Get all node statistics
    pub async fn all_stats(&self) -> HashMap<NodeId, NodeStats> {
        self.nodes
            .read()
            .await
            .iter()
            .map(|(id, n)| {
                (
                    *id,
                    NodeStats {
                        health: n.health,
                        replication_lag_ms: n.replication_lag_ms,
                        connections: n.connections,
                        avg_latency_ms: n.avg_latency_ms,
                        requests: n.requests,
                        failures: n.failures,
                    },
                )
            })
            .collect()
    }
}

/// Node statistics
#[derive(Debug, Clone)]
pub struct NodeStats {
    /// Node health state
    pub health: NodeHealth,
    /// Replication lag (ms)
    pub replication_lag_ms: u64,
    /// Current connections
    pub connections: u64,
    /// Average latency (ms)
    pub avg_latency_ms: f64,
    /// Total requests
    pub requests: u64,
    /// Total failures
    pub failures: u64,
}

impl NodeStats {
    /// Check if node is healthy (backward compatibility)
    pub fn is_healthy(&self) -> bool {
        self.health == NodeHealth::Healthy
    }

    /// Check if node can serve reads
    pub fn can_serve_reads(&self) -> bool {
        self.health.can_serve_reads()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = LoadBalancerConfig::default();
        assert_eq!(config.read_strategy, RoutingStrategy::RoundRobin);
        assert_eq!(config.write_strategy, RoutingStrategy::PrimaryOnly);
        assert!(config.read_write_split);
    }

    #[tokio::test]
    async fn test_set_node_health() {
        let lb = LoadBalancer::new(LoadBalancerConfig::default());
        let node_id = NodeId::new();

        // Add node
        {
            let mut nodes = lb.nodes.write().await;
            nodes.insert(
                node_id,
                NodeState {
                    endpoint: NodeEndpoint::new("localhost", 5432).with_role(NodeRole::Primary),
                    health: NodeHealth::Healthy,
                    replication_lag_ms: 0,
                    connections: 0,
                    avg_latency_ms: 0.0,
                    requests: 0,
                    failures: 0,
                },
            );
        }

        lb.set_node_health(&node_id, NodeHealth::Unhealthy).await;

        let stats = lb.node_stats(&node_id).await.unwrap();
        assert_eq!(stats.health, NodeHealth::Unhealthy);
        assert!(!stats.is_healthy());
    }

    #[tokio::test]
    async fn test_degraded_state() {
        let lb = LoadBalancer::new(LoadBalancerConfig::default());
        let node_id = NodeId::new();

        {
            let mut nodes = lb.nodes.write().await;
            nodes.insert(
                node_id,
                NodeState {
                    endpoint: NodeEndpoint::new("localhost", 5432).with_role(NodeRole::Standby),
                    health: NodeHealth::Healthy,
                    replication_lag_ms: 0,
                    connections: 0,
                    avg_latency_ms: 0.0,
                    requests: 0,
                    failures: 0,
                },
            );
        }

        // Set to degraded
        lb.set_node_health(&node_id, NodeHealth::Degraded).await;

        let stats = lb.node_stats(&node_id).await.unwrap();
        assert_eq!(stats.health, NodeHealth::Degraded);
        assert!(stats.can_serve_reads()); // Degraded can still serve reads
        assert!(!stats.is_healthy()); // But not considered fully healthy
    }

    #[tokio::test]
    async fn test_update_latency() {
        let lb = LoadBalancer::new(LoadBalancerConfig::default());
        let node_id = NodeId::new();

        {
            let mut nodes = lb.nodes.write().await;
            nodes.insert(
                node_id,
                NodeState {
                    endpoint: NodeEndpoint::new("localhost", 5432),
                    health: NodeHealth::Healthy,
                    replication_lag_ms: 0,
                    connections: 0,
                    avg_latency_ms: 0.0,
                    requests: 0,
                    failures: 0,
                },
            );
        }

        lb.update_latency(&node_id, 50.0).await;

        let stats = lb.node_stats(&node_id).await.unwrap();
        assert!(stats.avg_latency_ms > 0.0);
    }

    #[tokio::test]
    async fn test_replication_lag_degrades_health() {
        let lb = LoadBalancer::new(LoadBalancerConfig::default());
        let node_id = NodeId::new();

        {
            let mut nodes = lb.nodes.write().await;
            nodes.insert(
                node_id,
                NodeState {
                    endpoint: NodeEndpoint::new("localhost", 5432).with_role(NodeRole::Standby),
                    health: NodeHealth::Healthy,
                    replication_lag_ms: 0,
                    connections: 0,
                    avg_latency_ms: 0.0,
                    requests: 0,
                    failures: 0,
                },
            );
        }

        // Update with high replication lag
        lb.update_replication_lag(&node_id, 10000).await; // 10 seconds

        let stats = lb.node_stats(&node_id).await.unwrap();
        assert_eq!(stats.health, NodeHealth::Degraded);
        assert_eq!(stats.replication_lag_ms, 10000);
    }

    #[tokio::test]
    async fn test_connection_tracking() {
        let lb = LoadBalancer::new(LoadBalancerConfig::default());
        let node_id = NodeId::new();

        {
            let mut nodes = lb.nodes.write().await;
            nodes.insert(
                node_id,
                NodeState {
                    endpoint: NodeEndpoint::new("localhost", 5432),
                    health: NodeHealth::Healthy,
                    replication_lag_ms: 0,
                    connections: 0,
                    avg_latency_ms: 0.0,
                    requests: 0,
                    failures: 0,
                },
            );
        }

        lb.increment_connections(&node_id).await;
        lb.increment_connections(&node_id).await;

        let stats = lb.node_stats(&node_id).await.unwrap();
        assert_eq!(stats.connections, 2);

        lb.decrement_connections(&node_id).await;
        let stats = lb.node_stats(&node_id).await.unwrap();
        assert_eq!(stats.connections, 1);
    }
}
