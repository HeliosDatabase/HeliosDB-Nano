//! Region Coordinator - Tier 2 Multi-Primary
//!
//! Coordinates state across multiple regions in an active-active setup.
//! Manages region health, convergence tracking, and partition handling.

use super::config::MultiPrimaryConfig;
use super::multi_primary_sync::PeerSyncState;
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Region status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionStatus {
    /// Region is online and healthy
    Online,
    /// Region is online but degraded (some peers unreachable)
    Degraded,
    /// Region is partitioned (majority of peers unreachable)
    Partitioned,
    /// Region is offline
    Offline,
}

/// Region information
#[derive(Debug, Clone)]
pub struct RegionInfo {
    /// Region ID
    pub id: Uuid,
    /// Region name
    pub name: String,
    /// Region status
    pub status: RegionStatus,
    /// Primary node in this region (for failover)
    pub primary_node: Option<Uuid>,
    /// Nodes in this region
    pub nodes: Vec<Uuid>,
    /// Last health check
    pub last_health_check: chrono::DateTime<chrono::Utc>,
    /// Convergence lag (max vector clock difference)
    pub convergence_lag: u64,
}

/// Partition event
#[derive(Debug, Clone)]
pub struct PartitionEvent {
    /// Event ID
    pub id: Uuid,
    /// Regions involved in partition
    pub regions: Vec<Uuid>,
    /// Partition detected at
    pub detected_at: chrono::DateTime<chrono::Utc>,
    /// Partition resolved at (if resolved)
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Divergence count (changes made during partition)
    pub divergence_count: usize,
}

/// Coordinator events
#[derive(Debug, Clone)]
pub enum CoordinatorEvent {
    /// Region came online
    RegionOnline { region_id: Uuid },
    /// Region went offline
    RegionOffline { region_id: Uuid },
    /// Region status changed
    RegionStatusChanged { region_id: Uuid, old: RegionStatus, new: RegionStatus },
    /// Partition detected
    PartitionDetected { event: PartitionEvent },
    /// Partition healed
    PartitionHealed { event: PartitionEvent },
    /// Convergence achieved across all regions
    GlobalConvergence { timestamp: chrono::DateTime<chrono::Utc> },
}

/// Region Coordinator
pub struct RegionCoordinator {
    /// Configuration
    config: MultiPrimaryConfig,
    /// This region's ID
    local_region_id: Uuid,
    /// This region's name
    local_region_name: String,
    /// Known regions
    regions: Arc<RwLock<HashMap<Uuid, RegionInfo>>>,
    /// Active partitions
    partitions: Arc<RwLock<Vec<PartitionEvent>>>,
    /// Global vector clock (merged from all regions)
    global_clock: Arc<RwLock<HashMap<Uuid, u64>>>,
    /// Event channel sender
    event_tx: mpsc::Sender<CoordinatorEvent>,
    /// Event channel receiver
    event_rx: Option<mpsc::Receiver<CoordinatorEvent>>,
    /// Health check interval
    health_check_interval: std::time::Duration,
}

impl RegionCoordinator {
    /// Create a new region coordinator
    pub fn new(
        config: MultiPrimaryConfig,
        local_region_id: Uuid,
        local_region_name: String,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);

        // Create local region entry
        let local_region = RegionInfo {
            id: local_region_id,
            name: local_region_name.clone(),
            status: RegionStatus::Online,
            primary_node: None,
            nodes: vec![],
            last_health_check: chrono::Utc::now(),
            convergence_lag: 0,
        };

        let mut regions = HashMap::new();
        regions.insert(local_region_id, local_region);

        Self {
            config,
            local_region_id,
            local_region_name,
            regions: Arc::new(RwLock::new(regions)),
            partitions: Arc::new(RwLock::new(Vec::new())),
            global_clock: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            event_rx: Some(event_rx),
            health_check_interval: std::time::Duration::from_secs(5),
        }
    }

    /// Start the coordinator
    pub async fn start(&self) -> Result<()> {
        // TODO: Implement coordinator startup
        // 1. Start health check loop
        // 2. Connect to peer regions
        // 3. Initialize global clock from peers

        tracing::info!(
            "Region Coordinator started for region '{}' ({})",
            self.local_region_name,
            self.local_region_id
        );
        Ok(())
    }

    /// Stop the coordinator
    pub async fn stop(&self) -> Result<()> {
        tracing::info!("Region Coordinator stopped");
        Ok(())
    }

    /// Register a remote region
    pub async fn register_region(&self, region: RegionInfo) -> Result<()> {
        let mut regions = self.regions.write().await;
        if regions.contains_key(&region.id) {
            return Err(ReplicationError::MultiPrimary(format!(
                "Region {} already registered",
                region.id
            )));
        }

        let _ = self.event_tx.send(CoordinatorEvent::RegionOnline {
            region_id: region.id,
        }).await;

        regions.insert(region.id, region);
        Ok(())
    }

    /// Unregister a region
    pub async fn unregister_region(&self, region_id: &Uuid) -> Result<()> {
        let mut regions = self.regions.write().await;
        regions.remove(region_id).ok_or_else(|| {
            ReplicationError::MultiPrimary(format!("Region {} not found", region_id))
        })?;

        let _ = self.event_tx.send(CoordinatorEvent::RegionOffline {
            region_id: *region_id,
        }).await;

        Ok(())
    }

    /// Update region status
    pub async fn update_region_status(&self, region_id: &Uuid, status: RegionStatus) -> Result<()> {
        let mut regions = self.regions.write().await;
        let region = regions.get_mut(region_id).ok_or_else(|| {
            ReplicationError::MultiPrimary(format!("Region {} not found", region_id))
        })?;

        if region.status != status {
            let old = region.status;
            region.status = status;
            region.last_health_check = chrono::Utc::now();

            let _ = self.event_tx.send(CoordinatorEvent::RegionStatusChanged {
                region_id: *region_id,
                old,
                new: status,
            }).await;
        }

        Ok(())
    }

    /// Get region info
    pub async fn get_region(&self, region_id: &Uuid) -> Option<RegionInfo> {
        self.regions.read().await.get(region_id).cloned()
    }

    /// List all regions
    pub async fn list_regions(&self) -> Vec<RegionInfo> {
        self.regions.read().await.values().cloned().collect()
    }

    /// Get online regions
    pub async fn online_regions(&self) -> Vec<RegionInfo> {
        self.regions
            .read()
            .await
            .values()
            .filter(|r| r.status == RegionStatus::Online || r.status == RegionStatus::Degraded)
            .cloned()
            .collect()
    }

    /// Check for partition
    pub async fn detect_partition(&self) -> Option<PartitionEvent> {
        let regions = self.regions.read().await;
        let offline: Vec<Uuid> = regions
            .values()
            .filter(|r| r.status == RegionStatus::Offline || r.status == RegionStatus::Partitioned)
            .map(|r| r.id)
            .collect();

        if offline.is_empty() {
            return None;
        }

        // Check if this is a new partition
        let partitions = self.partitions.read().await;
        for partition in partitions.iter() {
            if partition.resolved_at.is_none() {
                // Already tracking this partition
                return None;
            }
        }

        let event = PartitionEvent {
            id: Uuid::new_v4(),
            regions: offline,
            detected_at: chrono::Utc::now(),
            resolved_at: None,
            divergence_count: 0,
        };

        Some(event)
    }

    /// Record a partition event
    pub async fn record_partition(&self, event: PartitionEvent) {
        let _ = self.event_tx.send(CoordinatorEvent::PartitionDetected {
            event: event.clone(),
        }).await;

        self.partitions.write().await.push(event);
    }

    /// Mark partition as healed
    pub async fn heal_partition(&self, partition_id: &Uuid) -> Result<()> {
        let mut partitions = self.partitions.write().await;
        let partition = partitions.iter_mut().find(|p| &p.id == partition_id).ok_or_else(|| {
            ReplicationError::MultiPrimary(format!("Partition {} not found", partition_id))
        })?;

        partition.resolved_at = Some(chrono::Utc::now());

        let _ = self.event_tx.send(CoordinatorEvent::PartitionHealed {
            event: partition.clone(),
        }).await;

        Ok(())
    }

    /// Update global vector clock
    pub async fn update_global_clock(&self, node_id: Uuid, timestamp: u64) {
        let mut clock = self.global_clock.write().await;
        let entry = clock.entry(node_id).or_insert(0);
        *entry = (*entry).max(timestamp);
    }

    /// Get global vector clock
    pub async fn global_clock(&self) -> HashMap<Uuid, u64> {
        self.global_clock.read().await.clone()
    }

    /// Calculate convergence lag for a region
    pub async fn calculate_convergence_lag(&self, region_clock: &HashMap<Uuid, u64>) -> u64 {
        let global = self.global_clock.read().await;
        let mut max_lag = 0u64;

        for (node_id, &global_ts) in global.iter() {
            let region_ts = region_clock.get(node_id).copied().unwrap_or(0);
            let lag = global_ts.saturating_sub(region_ts);
            max_lag = max_lag.max(lag);
        }

        max_lag
    }

    /// Check if all regions have converged
    pub async fn is_globally_converged(&self) -> bool {
        let regions = self.regions.read().await;
        regions.values().all(|r| r.convergence_lag == 0 && r.status == RegionStatus::Online)
    }

    /// Get active partitions
    pub async fn active_partitions(&self) -> Vec<PartitionEvent> {
        self.partitions
            .read()
            .await
            .iter()
            .filter(|p| p.resolved_at.is_none())
            .cloned()
            .collect()
    }

    /// Get partition history
    pub async fn partition_history(&self, limit: usize) -> Vec<PartitionEvent> {
        self.partitions
            .read()
            .await
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Take the event receiver
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<CoordinatorEvent>> {
        self.event_rx.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_coordinator_creation() {
        let config = MultiPrimaryConfig::default();
        let region_id = Uuid::new_v4();
        let coordinator = RegionCoordinator::new(config, region_id, "us-east".to_string());

        let regions = coordinator.list_regions().await;
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].name, "us-east");
    }

    #[tokio::test]
    async fn test_region_registration() {
        let config = MultiPrimaryConfig::default();
        let local_id = Uuid::new_v4();
        let coordinator = RegionCoordinator::new(config, local_id, "us-east".to_string());

        let remote = RegionInfo {
            id: Uuid::new_v4(),
            name: "eu-west".to_string(),
            status: RegionStatus::Online,
            primary_node: None,
            nodes: vec![],
            last_health_check: chrono::Utc::now(),
            convergence_lag: 0,
        };

        coordinator.register_region(remote.clone()).await.expect("register failed");

        let regions = coordinator.list_regions().await;
        assert_eq!(regions.len(), 2);

        let retrieved = coordinator.get_region(&remote.id).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "eu-west");
    }

    #[tokio::test]
    async fn test_status_update() {
        let config = MultiPrimaryConfig::default();
        let local_id = Uuid::new_v4();
        let coordinator = RegionCoordinator::new(config, local_id, "us-east".to_string());

        coordinator
            .update_region_status(&local_id, RegionStatus::Degraded)
            .await
            .expect("update failed");

        let region = coordinator.get_region(&local_id).await.unwrap();
        assert_eq!(region.status, RegionStatus::Degraded);
    }

    #[tokio::test]
    async fn test_convergence_lag() {
        let config = MultiPrimaryConfig::default();
        let local_id = Uuid::new_v4();
        let coordinator = RegionCoordinator::new(config, local_id, "us-east".to_string());

        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        // Update global clock
        coordinator.update_global_clock(node_a, 100).await;
        coordinator.update_global_clock(node_b, 200).await;

        // Region clock is behind
        let region_clock: HashMap<Uuid, u64> = [(node_a, 100), (node_b, 150)].into_iter().collect();
        let lag = coordinator.calculate_convergence_lag(&region_clock).await;
        assert_eq!(lag, 50); // node_b: 200 - 150 = 50
    }
}
