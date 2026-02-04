//! Hash Ring - Tier 3 Sharding
//!
//! Implements consistent hashing with virtual nodes for distributed data placement.
//! Provides stable key distribution with minimal remapping during cluster changes.
//!
//! # Algorithm
//!
//! Uses Karger et al.'s consistent hashing (1997, patent expired):
//! - Hash both keys and nodes to a ring (0 to 2^64-1)
//! - Keys are assigned to the first node clockwise from their hash
//! - Virtual nodes provide better distribution and load balancing

use super::{ReplicationError, Result};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use uuid::Uuid;

/// Default number of virtual nodes per physical node
const DEFAULT_VIRTUAL_NODES: usize = 150;

/// A position on the hash ring
pub type RingPosition = u64;

/// Physical shard node
#[derive(Debug, Clone)]
pub struct ShardNode {
    /// Node ID
    pub id: Uuid,
    /// Node name (for logging)
    pub name: String,
    /// Host address
    pub host: String,
    /// Port
    pub port: u16,
    /// Weight (affects virtual node count)
    pub weight: u32,
    /// Is the node healthy?
    pub healthy: bool,
    /// Node metadata
    pub metadata: HashMap<String, String>,
}

impl ShardNode {
    /// Create a new shard node
    pub fn new(name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            host: host.into(),
            port,
            weight: 100,
            healthy: true,
            metadata: HashMap::new(),
        }
    }

    /// Set node weight (affects virtual node distribution)
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }
}

/// Virtual node on the hash ring
#[derive(Debug, Clone)]
struct VirtualNode {
    /// Physical node ID
    node_id: Uuid,
    /// Virtual node index
    index: usize,
    /// Position on ring
    position: RingPosition,
}

/// Consistent Hash Ring
pub struct HashRing {
    /// Virtual nodes per physical node (base count, modified by weight)
    virtual_nodes_count: usize,
    /// Ring of virtual nodes (position -> virtual node)
    ring: BTreeMap<RingPosition, VirtualNode>,
    /// Physical nodes
    nodes: HashMap<Uuid, ShardNode>,
    /// Total virtual node count (for statistics)
    total_virtual_nodes: usize,
}

impl HashRing {
    /// Create a new hash ring
    pub fn new() -> Self {
        Self::with_virtual_nodes(DEFAULT_VIRTUAL_NODES)
    }

    /// Create a hash ring with custom virtual node count
    pub fn with_virtual_nodes(count: usize) -> Self {
        Self {
            virtual_nodes_count: count,
            ring: BTreeMap::new(),
            nodes: HashMap::new(),
            total_virtual_nodes: 0,
        }
    }

    /// Add a node to the ring
    pub fn add_node(&mut self, node: ShardNode) -> Result<()> {
        if self.nodes.contains_key(&node.id) {
            return Err(ReplicationError::Sharding(format!(
                "Node {} already exists in ring",
                node.id
            )));
        }

        // Calculate virtual node count based on weight
        let vnode_count = (self.virtual_nodes_count as u32 * node.weight / 100) as usize;
        let vnode_count = vnode_count.max(1);

        // Create virtual nodes
        for i in 0..vnode_count {
            let position = self.hash_virtual_node(&node.id, i);
            let vnode = VirtualNode {
                node_id: node.id,
                index: i,
                position,
            };
            self.ring.insert(position, vnode);
        }

        self.total_virtual_nodes += vnode_count;
        self.nodes.insert(node.id, node);

        Ok(())
    }

    /// Remove a node from the ring
    pub fn remove_node(&mut self, node_id: &Uuid) -> Result<ShardNode> {
        let node = self.nodes.remove(node_id).ok_or_else(|| {
            ReplicationError::Sharding(format!("Node {} not found in ring", node_id))
        })?;

        // Remove all virtual nodes for this physical node
        let positions_to_remove: Vec<RingPosition> = self
            .ring
            .iter()
            .filter(|(_, v)| &v.node_id == node_id)
            .map(|(pos, _)| *pos)
            .collect();

        for pos in &positions_to_remove {
            self.ring.remove(pos);
        }

        self.total_virtual_nodes -= positions_to_remove.len();

        Ok(node)
    }

    /// Get the node responsible for a key
    pub fn get_node(&self, key: &[u8]) -> Option<&ShardNode> {
        if self.ring.is_empty() {
            return None;
        }

        let hash = self.hash_key(key);
        self.get_node_for_position(hash)
    }

    /// Get the node responsible for a position on the ring
    fn get_node_for_position(&self, position: RingPosition) -> Option<&ShardNode> {
        // Find first node clockwise from position
        let vnode = self
            .ring
            .range(position..)
            .next()
            .or_else(|| self.ring.iter().next())
            .map(|(_, v)| v)?;

        self.nodes.get(&vnode.node_id)
    }

    /// Get N nodes responsible for a key (for replication)
    pub fn get_nodes(&self, key: &[u8], count: usize) -> Vec<&ShardNode> {
        if self.ring.is_empty() || count == 0 {
            return vec![];
        }

        let hash = self.hash_key(key);
        let mut result = Vec::with_capacity(count);
        let mut seen_nodes = std::collections::HashSet::new();

        // Walk the ring clockwise
        let iter = self.ring.range(hash..).chain(self.ring.iter());
        for (_, vnode) in iter {
            if seen_nodes.insert(vnode.node_id) {
                if let Some(node) = self.nodes.get(&vnode.node_id) {
                    result.push(node);
                    if result.len() >= count {
                        break;
                    }
                }
            }
        }

        result
    }

    /// Get all healthy nodes for a key
    pub fn get_healthy_nodes(&self, key: &[u8], count: usize) -> Vec<&ShardNode> {
        self.get_nodes(key, count * 2)
            .into_iter()
            .filter(|n| n.healthy)
            .take(count)
            .collect()
    }

    /// Mark a node as healthy or unhealthy
    pub fn set_node_health(&mut self, node_id: &Uuid, healthy: bool) -> Result<()> {
        let node = self.nodes.get_mut(node_id).ok_or_else(|| {
            ReplicationError::Sharding(format!("Node {} not found", node_id))
        })?;
        node.healthy = healthy;
        Ok(())
    }

    /// Get a physical node by ID
    pub fn get_node_by_id(&self, node_id: &Uuid) -> Option<&ShardNode> {
        self.nodes.get(node_id)
    }

    /// Get all physical nodes
    pub fn nodes(&self) -> impl Iterator<Item = &ShardNode> {
        self.nodes.values()
    }

    /// Get the number of physical nodes
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get total virtual node count
    pub fn virtual_node_count(&self) -> usize {
        self.total_virtual_nodes
    }

    /// Hash a key to a ring position
    fn hash_key(&self, key: &[u8]) -> RingPosition {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Hash a virtual node to a ring position
    fn hash_virtual_node(&self, node_id: &Uuid, index: usize) -> RingPosition {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        node_id.hash(&mut hasher);
        index.hash(&mut hasher);
        hasher.finish()
    }

    /// Get distribution statistics
    pub fn distribution_stats(&self) -> DistributionStats {
        let mut key_counts: HashMap<Uuid, usize> = HashMap::new();

        // Sample distribution by hashing test keys
        for i in 0..10000u64 {
            let key = i.to_le_bytes();
            if let Some(node) = self.get_node(&key) {
                *key_counts.entry(node.id).or_insert(0) += 1;
            }
        }

        let counts: Vec<usize> = key_counts.values().copied().collect();
        let total: usize = counts.iter().sum();
        let mean = if counts.is_empty() { 0.0 } else { total as f64 / counts.len() as f64 };

        let variance = if counts.is_empty() {
            0.0
        } else {
            counts.iter().map(|&c| (c as f64 - mean).powi(2)).sum::<f64>() / counts.len() as f64
        };

        DistributionStats {
            node_count: self.nodes.len(),
            virtual_node_count: self.total_virtual_nodes,
            mean_keys_per_node: mean,
            std_dev: variance.sqrt(),
            min_keys: counts.iter().copied().min().unwrap_or(0),
            max_keys: counts.iter().copied().max().unwrap_or(0),
        }
    }
}

impl Default for HashRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Distribution statistics for the hash ring
#[derive(Debug, Clone)]
pub struct DistributionStats {
    /// Number of physical nodes
    pub node_count: usize,
    /// Number of virtual nodes
    pub virtual_node_count: usize,
    /// Mean keys per node (from sample)
    pub mean_keys_per_node: f64,
    /// Standard deviation (from sample)
    pub std_dev: f64,
    /// Minimum keys assigned to any node
    pub min_keys: usize,
    /// Maximum keys assigned to any node
    pub max_keys: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_ring() {
        let ring = HashRing::new();
        assert!(ring.get_node(b"key").is_none());
        assert_eq!(ring.node_count(), 0);
    }

    #[test]
    fn test_single_node() {
        let mut ring = HashRing::new();
        let node = ShardNode::new("node1", "localhost", 5432);
        ring.add_node(node).expect("add failed");

        // All keys should go to the single node
        assert!(ring.get_node(b"key1").is_some());
        assert!(ring.get_node(b"key2").is_some());
        assert_eq!(ring.get_node(b"key1").unwrap().name, "node1");
    }

    #[test]
    fn test_multiple_nodes() {
        let mut ring = HashRing::new();

        for i in 0..3 {
            let node = ShardNode::new(format!("node{}", i), "localhost", 5432 + i);
            ring.add_node(node).expect("add failed");
        }

        assert_eq!(ring.node_count(), 3);

        // Keys should be distributed
        let mut distribution: HashMap<String, usize> = HashMap::new();
        for i in 0..1000 {
            let key = format!("key{}", i);
            if let Some(node) = ring.get_node(key.as_bytes()) {
                *distribution.entry(node.name.clone()).or_insert(0) += 1;
            }
        }

        // All nodes should have some keys
        assert_eq!(distribution.len(), 3);
        for count in distribution.values() {
            assert!(*count > 0);
        }
    }

    #[test]
    fn test_node_removal() {
        let mut ring = HashRing::new();
        let node1 = ShardNode::new("node1", "localhost", 5432);
        let node2 = ShardNode::new("node2", "localhost", 5433);
        let node1_id = node1.id;

        ring.add_node(node1).expect("add failed");
        ring.add_node(node2).expect("add failed");

        assert_eq!(ring.node_count(), 2);

        ring.remove_node(&node1_id).expect("remove failed");
        assert_eq!(ring.node_count(), 1);

        // All keys should now go to node2
        for i in 0..100 {
            let key = format!("key{}", i);
            assert_eq!(ring.get_node(key.as_bytes()).unwrap().name, "node2");
        }
    }

    #[test]
    fn test_replication_nodes() {
        let mut ring = HashRing::new();

        for i in 0..5 {
            let node = ShardNode::new(format!("node{}", i), "localhost", 5432 + i);
            ring.add_node(node).expect("add failed");
        }

        // Get 3 nodes for replication
        let nodes = ring.get_nodes(b"key", 3);
        assert_eq!(nodes.len(), 3);

        // All should be different
        let ids: std::collections::HashSet<_> = nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn test_weighted_nodes() {
        let mut ring = HashRing::with_virtual_nodes(100);

        let heavy = ShardNode::new("heavy", "localhost", 5432).with_weight(200);
        let light = ShardNode::new("light", "localhost", 5433).with_weight(50);

        ring.add_node(heavy).expect("add failed");
        ring.add_node(light).expect("add failed");

        // Heavy node should get more keys
        let mut heavy_count = 0;
        let mut light_count = 0;

        for i in 0..10000 {
            let key = format!("key{}", i);
            if let Some(node) = ring.get_node(key.as_bytes()) {
                if node.name == "heavy" {
                    heavy_count += 1;
                } else {
                    light_count += 1;
                }
            }
        }

        // Heavy should have significantly more (roughly 4x)
        assert!(heavy_count > light_count * 2);
    }

    #[test]
    fn test_consistent_hashing() {
        let mut ring = HashRing::new();

        for i in 0..3 {
            let node = ShardNode::new(format!("node{}", i), "localhost", 5432 + i);
            ring.add_node(node).expect("add failed");
        }

        // Record which node gets key "test"
        let original_node = ring.get_node(b"test").unwrap().name.clone();

        // Add another node
        let new_node = ShardNode::new("node3", "localhost", 5435);
        ring.add_node(new_node).expect("add failed");

        // "test" should still go to the same node OR the new node
        // but not to a different existing node
        let new_assignment = ring.get_node(b"test").unwrap().name.clone();
        assert!(new_assignment == original_node || new_assignment == "node3");
    }
}
