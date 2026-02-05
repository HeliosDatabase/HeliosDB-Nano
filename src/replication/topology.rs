//! Cluster Topology Manager
//!
//! Tracks cluster membership, node states, and topology changes.
//! Used by HeliosProxy and switchover coordinator to route traffic.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::Result;
use super::role_manager::NodeRole;

/// Global topology manager instance
static TOPOLOGY_MANAGER: once_cell::sync::Lazy<TopologyManager> =
    once_cell::sync::Lazy::new(|| TopologyManager::new(None));

/// Get the global topology manager
pub fn topology_manager() -> &'static TopologyManager {
    &TOPOLOGY_MANAGER
}

/// Node information in the cluster
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Unique node identifier
    pub node_id: Uuid,
    /// Human-readable alias for the node (e.g., "primary-1", "standby-east")
    pub alias: Option<String>,
    /// Current role
    pub role: NodeRole,
    /// PostgreSQL protocol address (host:port)
    pub client_addr: String,
    /// Replication protocol address (host:port)
    pub replication_addr: String,
    /// Last known LSN (for standbys)
    pub last_lsn: u64,
    /// Replication lag in milliseconds
    pub replication_lag_ms: u64,
    /// Last health check time
    pub last_seen: Instant,
    /// Whether node is healthy
    pub is_healthy: bool,
    /// Node priority for failover (lower = higher priority)
    pub priority: u32,
    /// Node weight for load balancing (higher = more traffic)
    pub weight: u32,
    /// Tags for routing (e.g., "region=us-east", "tier=hot")
    pub tags: HashMap<String, String>,
    /// Last health check result message
    pub health_message: Option<String>,
    /// Number of consecutive health check failures
    pub health_failures: u32,
}

impl NodeInfo {
    pub fn new(
        node_id: Uuid,
        role: NodeRole,
        client_addr: String,
        replication_addr: String,
    ) -> Self {
        Self {
            node_id,
            alias: None,
            role,
            client_addr,
            replication_addr,
            last_lsn: 0,
            replication_lag_ms: 0,
            last_seen: Instant::now(),
            is_healthy: true,
            priority: 100,
            weight: 100,
            tags: HashMap::new(),
            health_message: None,
            health_failures: 0,
        }
    }

    /// Create a new node with an alias
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    /// Create a new node with priority
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    /// Create a new node with weight
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    /// Get the display name (alias or truncated UUID)
    pub fn display_name(&self) -> String {
        self.alias.clone().unwrap_or_else(|| {
            let uuid_str = self.node_id.to_string();
            format!("{}...", &uuid_str[..8])
        })
    }

    /// Check if this node can serve read queries
    pub fn can_read(&self) -> bool {
        self.is_healthy && self.role.can_read()
    }

    /// Check if this node can serve write queries
    pub fn can_write(&self) -> bool {
        self.is_healthy && self.role.can_write()
    }

    /// Time since last health check
    pub fn time_since_seen(&self) -> Duration {
        self.last_seen.elapsed()
    }
}

/// Topology change event
#[derive(Debug, Clone)]
pub enum TopologyEvent {
    /// Node joined the cluster
    NodeJoined(NodeInfo),
    /// Node left the cluster
    NodeLeft { node_id: Uuid },
    /// Node role changed
    RoleChanged {
        node_id: Uuid,
        old_role: NodeRole,
        new_role: NodeRole,
    },
    /// Node alias changed
    AliasChanged {
        node_id: Uuid,
        old_alias: Option<String>,
        new_alias: Option<String>,
    },
    /// Node health changed
    HealthChanged {
        node_id: Uuid,
        is_healthy: bool,
    },
    /// Primary changed
    PrimaryChanged {
        old_primary: Option<Uuid>,
        new_primary: Uuid,
    },
    /// Cluster topology refreshed
    Refreshed,
}

/// Cluster topology manager
pub struct TopologyManager {
    /// All known nodes
    nodes: RwLock<HashMap<Uuid, NodeInfo>>,
    /// Alias to node ID mapping for quick lookups
    aliases: RwLock<HashMap<String, Uuid>>,
    /// Current primary node ID
    primary_node: RwLock<Option<Uuid>>,
    /// Topology event broadcaster
    event_tx: broadcast::Sender<TopologyEvent>,
    /// Health check timeout
    health_timeout: Duration,
    /// This node's ID (if part of cluster)
    local_node_id: Option<Uuid>,
}

impl TopologyManager {
    /// Create a new topology manager
    pub fn new(local_node_id: Option<Uuid>) -> Self {
        let (event_tx, _) = broadcast::channel(64);

        Self {
            nodes: RwLock::new(HashMap::new()),
            aliases: RwLock::new(HashMap::new()),
            primary_node: RwLock::new(None),
            event_tx,
            health_timeout: Duration::from_secs(10),
            local_node_id,
        }
    }

    /// Create topology manager with custom health timeout
    pub fn with_health_timeout(mut self, timeout: Duration) -> Self {
        self.health_timeout = timeout;
        self
    }

    /// Subscribe to topology events
    pub fn subscribe(&self) -> broadcast::Receiver<TopologyEvent> {
        self.event_tx.subscribe()
    }

    /// Register or update a node
    pub fn register_node(&self, info: NodeInfo) {
        let is_new;
        let old_role;
        let old_alias;
        let is_primary = info.role == NodeRole::Primary;
        let node_id = info.node_id;

        {
            let mut nodes = self.nodes.write();
            is_new = !nodes.contains_key(&info.node_id);
            old_role = nodes.get(&info.node_id).map(|n| n.role);
            old_alias = nodes.get(&info.node_id).and_then(|n| n.alias.clone());
            nodes.insert(info.node_id, info.clone());
        }

        // Update alias mapping
        if info.alias != old_alias {
            let mut aliases = self.aliases.write();
            // Remove old alias if it existed
            if let Some(ref old) = old_alias {
                aliases.remove(old);
            }
            // Add new alias if provided
            if let Some(ref new_alias) = info.alias {
                aliases.insert(new_alias.clone(), node_id);
            }
            // Emit alias change event
            if !is_new {
                let _ = self.event_tx.send(TopologyEvent::AliasChanged {
                    node_id,
                    old_alias,
                    new_alias: info.alias.clone(),
                });
            }
        }

        if is_new {
            let _ = self.event_tx.send(TopologyEvent::NodeJoined(info.clone()));
        } else if let Some(old) = old_role {
            if old != info.role {
                let _ = self.event_tx.send(TopologyEvent::RoleChanged {
                    node_id,
                    old_role: old,
                    new_role: info.role,
                });
            }
        }

        // Update primary if needed
        if is_primary {
            let old_primary = *self.primary_node.read();
            if old_primary != Some(node_id) {
                *self.primary_node.write() = Some(node_id);
                let _ = self.event_tx.send(TopologyEvent::PrimaryChanged {
                    old_primary,
                    new_primary: node_id,
                });
            }
        }
    }

    /// Remove a node from the cluster
    pub fn remove_node(&self, node_id: Uuid) {
        let removed = self.nodes.write().remove(&node_id);

        if let Some(ref node) = removed {
            // Remove alias if it existed
            if let Some(ref alias) = node.alias {
                self.aliases.write().remove(alias);
            }
            let _ = self.event_tx.send(TopologyEvent::NodeLeft { node_id });

            // Clear primary if it was this node
            let mut primary = self.primary_node.write();
            if *primary == Some(node_id) {
                *primary = None;
            }
        }
    }

    /// Update node health status
    pub fn update_health(&self, node_id: Uuid, is_healthy: bool) {
        let changed;
        {
            let mut nodes = self.nodes.write();
            if let Some(node) = nodes.get_mut(&node_id) {
                changed = node.is_healthy != is_healthy;
                node.is_healthy = is_healthy;
                node.last_seen = Instant::now();
            } else {
                return;
            }
        }

        if changed {
            let _ = self.event_tx.send(TopologyEvent::HealthChanged { node_id, is_healthy });
        }
    }

    /// Update node LSN
    pub fn update_lsn(&self, node_id: Uuid, lsn: u64, lag_ms: u64) {
        let mut nodes = self.nodes.write();
        if let Some(node) = nodes.get_mut(&node_id) {
            node.last_lsn = lsn;
            node.replication_lag_ms = lag_ms;
            node.last_seen = Instant::now();
        }
    }

    /// Get the current primary node
    pub fn get_primary(&self) -> Option<NodeInfo> {
        let primary_id = *self.primary_node.read();
        primary_id.and_then(|id| self.nodes.read().get(&id).cloned())
    }

    /// Get the current primary node ID
    pub fn get_primary_id(&self) -> Option<Uuid> {
        *self.primary_node.read()
    }

    /// Get a specific node
    pub fn get_node(&self, node_id: Uuid) -> Option<NodeInfo> {
        self.nodes.read().get(&node_id).cloned()
    }

    /// Get a node by alias
    pub fn get_node_by_alias(&self, alias: &str) -> Option<NodeInfo> {
        let node_id = self.aliases.read().get(alias).copied()?;
        self.nodes.read().get(&node_id).cloned()
    }

    /// Resolve a node identifier (alias or UUID string) to a node ID
    ///
    /// Tries to:
    /// 1. Look up by alias first
    /// 2. Parse as UUID if alias lookup fails
    /// 3. Return None if neither works
    pub fn resolve_node_id(&self, identifier: &str) -> Option<Uuid> {
        // First try alias lookup
        if let Some(node_id) = self.aliases.read().get(identifier).copied() {
            return Some(node_id);
        }

        // Then try parsing as UUID
        if let Ok(uuid) = Uuid::parse_str(identifier) {
            if self.nodes.read().contains_key(&uuid) {
                return Some(uuid);
            }
        }

        None
    }

    /// Set or update a node's alias
    pub fn set_alias(&self, node_id: Uuid, alias: Option<String>) -> bool {
        let mut nodes = self.nodes.write();
        if let Some(node) = nodes.get_mut(&node_id) {
            let old_alias = node.alias.clone();

            // Update alias mapping
            {
                let mut aliases = self.aliases.write();
                // Remove old alias
                if let Some(ref old) = old_alias {
                    aliases.remove(old);
                }
                // Add new alias
                if let Some(ref new_alias) = alias {
                    // Check if alias is already in use by another node
                    if let Some(&existing_id) = aliases.get(new_alias) {
                        if existing_id != node_id {
                            return false; // Alias already in use
                        }
                    }
                    aliases.insert(new_alias.clone(), node_id);
                }
            }

            node.alias = alias.clone();

            // Emit event
            let _ = self.event_tx.send(TopologyEvent::AliasChanged {
                node_id,
                old_alias,
                new_alias: alias,
            });

            true
        } else {
            false
        }
    }

    /// Get all registered aliases
    pub fn get_all_aliases(&self) -> HashMap<String, Uuid> {
        self.aliases.read().clone()
    }

    /// Get all healthy standbys
    pub fn get_healthy_standbys(&self) -> Vec<NodeInfo> {
        self.nodes
            .read()
            .values()
            .filter(|n| n.is_healthy && n.role == NodeRole::Standby)
            .cloned()
            .collect()
    }

    /// Get all nodes
    pub fn get_all_nodes(&self) -> Vec<NodeInfo> {
        self.nodes.read().values().cloned().collect()
    }

    /// Get nodes that can handle reads
    pub fn get_read_nodes(&self) -> Vec<NodeInfo> {
        self.nodes
            .read()
            .values()
            .filter(|n| n.can_read())
            .cloned()
            .collect()
    }

    /// Get the best standby for promotion (lowest priority value, then lowest lag)
    pub fn get_best_promotion_candidate(&self) -> Option<NodeInfo> {
        self.nodes
            .read()
            .values()
            .filter(|n| n.is_healthy && n.role == NodeRole::Standby)
            .min_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then(a.replication_lag_ms.cmp(&b.replication_lag_ms))
            })
            .cloned()
    }

    /// Check and mark unhealthy nodes that haven't been seen recently
    pub fn check_health_timeouts(&self) -> Vec<Uuid> {
        let mut timed_out = Vec::new();

        {
            let mut nodes = self.nodes.write();
            for node in nodes.values_mut() {
                if node.is_healthy && node.time_since_seen() > self.health_timeout {
                    node.is_healthy = false;
                    timed_out.push(node.node_id);
                }
            }
        }

        for node_id in &timed_out {
            let _ = self.event_tx.send(TopologyEvent::HealthChanged {
                node_id: *node_id,
                is_healthy: false,
            });
        }

        timed_out
    }

    /// Select a standby for read query (weighted round-robin)
    pub fn select_read_standby(&self) -> Option<NodeInfo> {
        let standbys = self.get_healthy_standbys();
        if standbys.is_empty() {
            return None;
        }

        // Simple weighted random selection
        let total_weight: u32 = standbys.iter().map(|s| s.weight).sum();
        if total_weight == 0 {
            return standbys.first().cloned();
        }

        let random_point = rand::random::<u32>() % total_weight;
        let mut cumulative = 0;

        for standby in standbys {
            cumulative += standby.weight;
            if random_point < cumulative {
                return Some(standby);
            }
        }

        None
    }

    /// Get cluster summary for monitoring
    pub fn get_cluster_summary(&self) -> ClusterSummary {
        let nodes = self.nodes.read();
        let mut summary = ClusterSummary::default();

        for node in nodes.values() {
            summary.total_nodes += 1;
            if node.is_healthy {
                summary.healthy_nodes += 1;
            }
            match node.role {
                NodeRole::Primary => summary.primary_count += 1,
                NodeRole::Standby => summary.standby_count += 1,
                _ => summary.transitioning_count += 1,
            }
            if node.role == NodeRole::Standby {
                summary.max_lag_ms = summary.max_lag_ms.max(node.replication_lag_ms);
            }
        }

        summary.primary_id = *self.primary_node.read();

        summary
    }
}

/// Cluster summary for monitoring
#[derive(Debug, Default, Clone)]
pub struct ClusterSummary {
    pub total_nodes: usize,
    pub healthy_nodes: usize,
    pub primary_count: usize,
    pub standby_count: usize,
    pub transitioning_count: usize,
    pub primary_id: Option<Uuid>,
    pub max_lag_ms: u64,
}

impl ClusterSummary {
    /// Check if cluster is healthy (has exactly one primary and at least one healthy standby)
    pub fn is_healthy(&self) -> bool {
        self.primary_count == 1 && self.healthy_nodes > 1
    }
}

/// Detailed node status for topology display
#[derive(Debug, Clone)]
pub struct NodeStatus {
    /// Node UUID
    pub node_id: Uuid,
    /// Human-readable alias (if set)
    pub alias: Option<String>,
    /// Display name (alias or truncated UUID)
    pub display_name: String,
    /// Current role as string
    pub role: String,
    /// Client address
    pub client_addr: String,
    /// Replication address
    pub replication_addr: String,
    /// Is node healthy
    pub is_healthy: bool,
    /// Health check message
    pub health_message: Option<String>,
    /// Consecutive health failures
    pub health_failures: u32,
    /// Seconds since last seen
    pub last_seen_secs: u64,
    /// Current LSN
    pub lsn: u64,
    /// Replication lag in milliseconds
    pub lag_ms: u64,
    /// Failover priority (lower = higher priority)
    pub priority: u32,
    /// Load balancing weight
    pub weight: u32,
    /// Node tags
    pub tags: HashMap<String, String>,
}

/// Full topology description for monitoring/display
#[derive(Debug, Clone)]
pub struct TopologyDescription {
    /// All nodes with detailed status
    pub nodes: Vec<NodeStatus>,
    /// Current primary node ID
    pub primary_id: Option<Uuid>,
    /// Current primary alias/name
    pub primary_name: Option<String>,
    /// Cluster health status
    pub cluster_healthy: bool,
    /// Health summary message
    pub health_summary: String,
    /// Total number of nodes
    pub total_nodes: usize,
    /// Number of healthy nodes
    pub healthy_nodes: usize,
    /// Maximum replication lag across all standbys
    pub max_lag_ms: u64,
}

impl TopologyManager {
    /// Get detailed topology description for display
    pub fn get_topology_description(&self) -> TopologyDescription {
        let nodes_map = self.nodes.read();
        let primary_id = *self.primary_node.read();

        let mut nodes: Vec<NodeStatus> = nodes_map.values().map(|n| {
            NodeStatus {
                node_id: n.node_id,
                alias: n.alias.clone(),
                display_name: n.display_name(),
                role: format!("{:?}", n.role),
                client_addr: n.client_addr.clone(),
                replication_addr: n.replication_addr.clone(),
                is_healthy: n.is_healthy,
                health_message: n.health_message.clone(),
                health_failures: n.health_failures,
                last_seen_secs: n.time_since_seen().as_secs(),
                lsn: n.last_lsn,
                lag_ms: n.replication_lag_ms,
                priority: n.priority,
                weight: n.weight,
                tags: n.tags.clone(),
            }
        }).collect();

        // Sort: primary first, then by priority, then by alias/name
        nodes.sort_by(|a, b| {
            let a_is_primary = Some(a.node_id) == primary_id;
            let b_is_primary = Some(b.node_id) == primary_id;
            b_is_primary.cmp(&a_is_primary)
                .then(a.priority.cmp(&b.priority))
                .then(a.display_name.cmp(&b.display_name))
        });

        let total_nodes = nodes.len();
        let healthy_nodes = nodes.iter().filter(|n| n.is_healthy).count();
        let max_lag_ms = nodes.iter()
            .filter(|n| n.role == "Standby")
            .map(|n| n.lag_ms)
            .max()
            .unwrap_or(0);

        let primary_count = nodes.iter().filter(|n| n.role == "Primary").count();
        let standby_count = nodes.iter().filter(|n| n.role == "Standby").count();

        let cluster_healthy = primary_count == 1 && healthy_nodes > 1;

        let health_summary = if cluster_healthy {
            format!("Healthy: 1 primary, {} standbys, {} nodes total", standby_count, total_nodes)
        } else if primary_count == 0 {
            "CRITICAL: No primary node".to_string()
        } else if primary_count > 1 {
            format!("CRITICAL: Multiple primaries detected ({})", primary_count)
        } else if healthy_nodes <= 1 {
            format!("WARNING: Insufficient healthy nodes ({}/{})", healthy_nodes, total_nodes)
        } else {
            format!("WARNING: {} healthy nodes out of {}", healthy_nodes, total_nodes)
        };

        let primary_name = primary_id.and_then(|id| {
            nodes_map.get(&id).map(|n| n.display_name())
        });

        TopologyDescription {
            nodes,
            primary_id,
            primary_name,
            cluster_healthy,
            health_summary,
            total_nodes,
            healthy_nodes,
            max_lag_ms,
        }
    }
}

/// Topology discovery configuration
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Seed nodes to connect to for discovery
    pub seed_nodes: Vec<String>,
    /// Discovery refresh interval
    pub refresh_interval: Duration,
    /// Whether to use DNS discovery
    pub dns_discovery: bool,
    /// DNS discovery hostname (e.g., "heliosdb-cluster.local")
    pub dns_hostname: Option<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            seed_nodes: vec![],
            refresh_interval: Duration::from_secs(5),
            dns_discovery: false,
            dns_hostname: None,
        }
    }
}

/// Topology discovery service
pub struct TopologyDiscovery {
    topology: Arc<TopologyManager>,
    config: DiscoveryConfig,
}

impl TopologyDiscovery {
    pub fn new(topology: Arc<TopologyManager>, config: DiscoveryConfig) -> Self {
        Self { topology, config }
    }

    /// Run the discovery service
    pub async fn run(&self) {
        let mut interval = tokio::time::interval(self.config.refresh_interval);

        loop {
            interval.tick().await;

            // Check for timed out nodes
            let timed_out = self.topology.check_health_timeouts();
            for node_id in timed_out {
                tracing::warn!("Node {} health timeout", node_id);
            }

            // Refresh topology from seed nodes
            if let Err(e) = self.refresh_from_seeds().await {
                tracing::debug!("Topology refresh from seeds failed: {}", e);
            }

            // DNS discovery if enabled
            if self.config.dns_discovery {
                if let Err(e) = self.discover_from_dns().await {
                    tracing::debug!("DNS discovery failed: {}", e);
                }
            }
        }
    }

    async fn refresh_from_seeds(&self) -> Result<()> {
        use tokio::net::TcpStream;
        use tokio::time::timeout;

        // For each seed node, try to fetch cluster topology via TCP probe
        for seed in &self.config.seed_nodes {
            let addr = match seed.parse::<std::net::SocketAddr>() {
                Ok(a) => a,
                Err(_) => {
                    // Try to resolve hostname:port
                    match tokio::net::lookup_host(seed).await {
                        Ok(mut addrs) => match addrs.next() {
                            Some(a) => a,
                            None => continue,
                        },
                        Err(_) => continue,
                    }
                }
            };

            // Try to connect with timeout to verify node is alive
            let connect_timeout = Duration::from_secs(2);
            match timeout(connect_timeout, TcpStream::connect(addr)).await {
                Ok(Ok(_stream)) => {
                    // Node is reachable - in a full implementation we would:
                    // 1. Perform replication protocol handshake
                    // 2. Request cluster topology list
                    // 3. Update topology manager with returned nodes
                    // For now, we just mark that the seed is reachable
                    tracing::trace!("Seed node {} is reachable", seed);

                    // Update last_seen for any node matching this address
                    let nodes = self.topology.get_all_nodes();
                    for node in nodes {
                        if node.client_addr.contains(&addr.ip().to_string())
                            || node.replication_addr.contains(&addr.ip().to_string())
                        {
                            self.topology.update_health(node.node_id, true);
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::trace!("Seed node {} connection failed: {}", seed, e);
                }
                Err(_) => {
                    tracing::trace!("Seed node {} connection timeout", seed);
                }
            }
        }
        Ok(())
    }

    async fn discover_from_dns(&self) -> Result<()> {
        use std::net::ToSocketAddrs;

        if let Some(hostname) = &self.config.dns_hostname {
            // Perform A/AAAA record lookup for the hostname
            // In production, this would use DNS SRV records for proper service discovery
            // SRV records contain priority, weight, port, and target information
            match tokio::task::spawn_blocking({
                let hostname = hostname.clone();
                move || {
                    // Standard DNS lookup - resolves A/AAAA records
                    // Format: hostname:port for ToSocketAddrs
                    let lookup_addr = format!("{}:5433", hostname); // Default replication port
                    lookup_addr.to_socket_addrs()
                }
            })
            .await
            {
                Ok(Ok(addrs)) => {
                    for addr in addrs {
                        tracing::trace!("DNS discovery found address: {}", addr);

                        // In production, we would connect and verify each discovered node
                        // For now, just log the discovery
                        // The node would need to be properly registered via the replication protocol
                    }
                }
                Ok(Err(e)) => {
                    tracing::trace!("DNS lookup for {} failed: {}", hostname, e);
                }
                Err(e) => {
                    tracing::trace!("DNS lookup task failed: {}", e);
                }
            }

            // SRV record lookup would be done like:
            // _heliosdb._tcp.{hostname} -> returns priority, weight, port, target
            // This requires a DNS library like trust-dns-resolver
            tracing::trace!(
                "SRV record lookup for _heliosdb._tcp.{} would be performed with DNS resolver",
                hostname
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topology_manager() {
        let manager = TopologyManager::new(None);

        let primary = NodeInfo::new(
            Uuid::new_v4(),
            NodeRole::Primary,
            "primary:5432".to_string(),
            "primary:5433".to_string(),
        );

        let standby = NodeInfo::new(
            Uuid::new_v4(),
            NodeRole::Standby,
            "standby:5432".to_string(),
            "standby:5433".to_string(),
        );

        manager.register_node(primary.clone());
        manager.register_node(standby.clone());

        assert_eq!(manager.get_primary_id(), Some(primary.node_id));
        assert_eq!(manager.get_healthy_standbys().len(), 1);
        assert_eq!(manager.get_read_nodes().len(), 2);

        let summary = manager.get_cluster_summary();
        assert_eq!(summary.total_nodes, 2);
        assert_eq!(summary.primary_count, 1);
        assert_eq!(summary.standby_count, 1);
        assert!(summary.is_healthy());
    }

    #[test]
    fn test_health_timeout() {
        let manager = TopologyManager::new(None)
            .with_health_timeout(Duration::from_millis(10));

        let node = NodeInfo::new(
            Uuid::new_v4(),
            NodeRole::Standby,
            "standby:5432".to_string(),
            "standby:5433".to_string(),
        );

        manager.register_node(node.clone());

        // Node should be healthy initially
        assert!(manager.get_node(node.node_id).unwrap().is_healthy);

        // Wait for timeout
        std::thread::sleep(Duration::from_millis(20));

        // Check should mark node unhealthy
        let timed_out = manager.check_health_timeouts();
        assert!(timed_out.contains(&node.node_id));
        assert!(!manager.get_node(node.node_id).unwrap().is_healthy);
    }
}
