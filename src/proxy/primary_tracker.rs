//! Primary Tracker - Tracks current primary node for query routing
//!
//! Monitors cluster topology and maintains the current primary node
//! information. During switchover, updates are received from the
//! switchover coordinator to ensure queries are routed correctly.

use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::replication::topology::{TopologyManager, TopologyEvent, NodeInfo};

/// Primary node information
#[derive(Debug, Clone)]
pub struct PrimaryInfo {
    /// Node ID
    pub node_id: Uuid,
    /// Client address (host:port)
    pub address: String,
    /// Time when this node became primary
    pub became_primary_at: Instant,
    /// Whether this is confirmed (vs pending switchover)
    pub is_confirmed: bool,
}

/// Primary change event
#[derive(Debug, Clone)]
pub enum PrimaryChangeEvent {
    /// Primary changed to new node
    Changed {
        old: Option<Uuid>,
        new: Uuid,
        address: String,
    },
    /// Primary lost (no healthy primary)
    Lost {
        old: Uuid,
    },
    /// Primary confirmed (after switchover completes)
    Confirmed {
        node_id: Uuid,
    },
}

/// Primary Tracker
pub struct PrimaryTracker {
    /// Topology manager reference
    topology: Arc<TopologyManager>,
    /// Current primary info
    current_primary: RwLock<Option<PrimaryInfo>>,
    /// Event broadcaster
    event_tx: broadcast::Sender<PrimaryChangeEvent>,
    /// Tracking interval
    tracking_interval: Duration,
}

impl PrimaryTracker {
    /// Create a new primary tracker
    pub fn new(topology: Arc<TopologyManager>) -> Self {
        let (event_tx, _) = broadcast::channel(16);

        Self {
            topology,
            current_primary: RwLock::new(None),
            event_tx,
            tracking_interval: Duration::from_millis(500),
        }
    }

    /// Set tracking interval
    pub fn with_tracking_interval(mut self, interval: Duration) -> Self {
        self.tracking_interval = interval;
        self
    }

    /// Subscribe to primary change events
    pub fn subscribe(&self) -> broadcast::Receiver<PrimaryChangeEvent> {
        self.event_tx.subscribe()
    }

    /// Get current primary info
    pub fn get_primary(&self) -> Option<PrimaryInfo> {
        self.current_primary.read().clone()
    }

    /// Get current primary node ID
    pub fn get_primary_id(&self) -> Option<Uuid> {
        self.current_primary.read().as_ref().map(|p| p.node_id)
    }

    /// Get current primary address
    pub fn get_primary_address(&self) -> Option<String> {
        self.current_primary.read().as_ref().map(|p| p.address.clone())
    }

    /// Check if we have a healthy primary
    pub fn has_primary(&self) -> bool {
        self.current_primary.read().is_some()
    }

    /// Set primary (called during switchover)
    pub fn set_primary(&self, node_id: Uuid, address: String) {
        let old_primary = self.current_primary.read().as_ref().map(|p| p.node_id);

        let new_info = PrimaryInfo {
            node_id,
            address: address.clone(),
            became_primary_at: Instant::now(),
            is_confirmed: false, // Will be confirmed after switchover completes
        };

        *self.current_primary.write() = Some(new_info);

        let _ = self.event_tx.send(PrimaryChangeEvent::Changed {
            old: old_primary,
            new: node_id,
            address,
        });

        tracing::info!("Primary tracker: set primary to {} (pending confirmation)", node_id);
    }

    /// Confirm the current primary (called after switchover completes)
    pub fn confirm_primary(&self) {
        let mut guard = self.current_primary.write();
        if let Some(ref mut info) = *guard {
            info.is_confirmed = true;
            let node_id = info.node_id;
            drop(guard);

            let _ = self.event_tx.send(PrimaryChangeEvent::Confirmed { node_id });
            tracing::info!("Primary tracker: confirmed primary {}", node_id);
        }
    }

    /// Clear primary (called when primary is lost)
    pub fn clear_primary(&self) {
        let old_primary = self.current_primary.write().take();

        if let Some(info) = old_primary {
            let _ = self.event_tx.send(PrimaryChangeEvent::Lost { old: info.node_id });
            tracing::warn!("Primary tracker: lost primary {}", info.node_id);
        }
    }

    /// Run the primary tracker (monitors topology for changes)
    pub async fn run(&self) {
        let mut topology_rx = self.topology.subscribe();
        let mut interval = tokio::time::interval(self.tracking_interval);

        // Initial detection
        self.detect_primary_from_topology();

        loop {
            tokio::select! {
                // Handle topology events
                event = topology_rx.recv() => {
                    match event {
                        Ok(TopologyEvent::PrimaryChanged { old_primary, new_primary }) => {
                            self.handle_primary_changed(old_primary, new_primary);
                        }
                        Ok(TopologyEvent::NodeLeft { node_id }) => {
                            self.handle_node_left(node_id);
                        }
                        Ok(TopologyEvent::HealthChanged { node_id, is_healthy }) => {
                            self.handle_health_changed(node_id, is_healthy);
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Primary tracker lagged {} events", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                // Periodic check
                _ = interval.tick() => {
                    self.periodic_check();
                }
            }
        }
    }

    /// Detect primary from topology (initial detection)
    fn detect_primary_from_topology(&self) {
        if let Some(primary) = self.topology.get_primary() {
            let info = PrimaryInfo {
                node_id: primary.node_id,
                address: primary.client_addr.clone(),
                became_primary_at: Instant::now(),
                is_confirmed: true,
            };

            *self.current_primary.write() = Some(info);
            tracing::info!("Primary tracker: detected primary {}", primary.node_id);
        }
    }

    /// Handle primary changed event from topology
    fn handle_primary_changed(&self, old: Option<Uuid>, new: Uuid) {
        // Get address from topology
        let address = self.topology
            .get_node(new)
            .map(|n| n.client_addr)
            .unwrap_or_else(|| format!("{}:5432", new));

        let info = PrimaryInfo {
            node_id: new,
            address: address.clone(),
            became_primary_at: Instant::now(),
            is_confirmed: true,
        };

        *self.current_primary.write() = Some(info);

        let _ = self.event_tx.send(PrimaryChangeEvent::Changed {
            old,
            new,
            address,
        });

        tracing::info!("Primary tracker: primary changed from {:?} to {}", old, new);
    }

    /// Handle node left event
    fn handle_node_left(&self, node_id: Uuid) {
        let current = self.current_primary.read().as_ref().map(|p| p.node_id);

        if current == Some(node_id) {
            self.clear_primary();
        }
    }

    /// Handle health changed event
    fn handle_health_changed(&self, node_id: Uuid, is_healthy: bool) {
        if !is_healthy {
            let current = self.current_primary.read().as_ref().map(|p| p.node_id);

            if current == Some(node_id) {
                tracing::warn!("Primary {} became unhealthy", node_id);
                // Don't immediately clear - might recover
                // Let failover mechanism handle this
            }
        }
    }

    /// Periodic health check
    fn periodic_check(&self) {
        // Verify current primary is still in topology and healthy
        let current_id = self.current_primary.read().as_ref().map(|p| p.node_id);

        if let Some(id) = current_id {
            if let Some(node) = self.topology.get_node(id) {
                if !node.is_healthy {
                    tracing::warn!("Primary {} is unhealthy in periodic check", id);
                }
            } else {
                // Node not in topology anymore
                self.clear_primary();
            }
        } else {
            // No primary, try to detect one
            self.detect_primary_from_topology();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::role_manager::NodeRole;

    #[test]
    fn test_primary_tracker() {
        let topology = Arc::new(TopologyManager::new(None));
        let tracker = PrimaryTracker::new(Arc::clone(&topology));

        // Initially no primary
        assert!(!tracker.has_primary());

        // Set primary manually
        let node_id = Uuid::new_v4();
        tracker.set_primary(node_id, "localhost:5432".to_string());

        assert!(tracker.has_primary());
        assert_eq!(tracker.get_primary_id(), Some(node_id));
        assert_eq!(tracker.get_primary_address(), Some("localhost:5432".to_string()));

        // Not confirmed yet
        let info = tracker.get_primary().unwrap();
        assert!(!info.is_confirmed);

        // Confirm
        tracker.confirm_primary();
        let info = tracker.get_primary().unwrap();
        assert!(info.is_confirmed);

        // Clear
        tracker.clear_primary();
        assert!(!tracker.has_primary());
    }

    #[test]
    fn test_detect_from_topology() {
        let topology = Arc::new(TopologyManager::new(None));

        // Add a primary to topology
        let node_id = Uuid::new_v4();
        let info = NodeInfo::new(
            node_id,
            NodeRole::Primary,
            "primary:5432".to_string(),
            "primary:5433".to_string(),
        );
        topology.register_node(info);

        // Create tracker - should detect primary
        let tracker = PrimaryTracker::new(Arc::clone(&topology));
        tracker.detect_primary_from_topology();

        assert!(tracker.has_primary());
        assert_eq!(tracker.get_primary_id(), Some(node_id));
    }
}
