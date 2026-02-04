//! LSN Manager - Tier 1 Warm Standby
//!
//! Tracks Log Sequence Numbers (LSN) across the cluster:
//! - Primary write LSN
//! - Standby applied LSNs
//! - Replication lag calculation
//! - Checkpoint tracking

use super::wal_replicator::Lsn;
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// LSN watermark types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsnWatermark {
    /// Write LSN - latest written to WAL
    Write,
    /// Flush LSN - latest flushed to disk
    Flush,
    /// Replay LSN - latest replayed on standby
    Replay,
    /// Checkpoint LSN - latest checkpoint
    Checkpoint,
}

/// LSN tracking entry
#[derive(Debug, Clone)]
pub struct LsnEntry {
    /// Node ID
    pub node_id: Uuid,
    /// Write LSN
    pub write_lsn: Lsn,
    /// Flush LSN
    pub flush_lsn: Lsn,
    /// Replay LSN (for standbys)
    pub replay_lsn: Option<Lsn>,
    /// Last checkpoint LSN
    pub checkpoint_lsn: Lsn,
    /// Last updated timestamp
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Default for LsnEntry {
    fn default() -> Self {
        Self {
            node_id: Uuid::nil(),
            write_lsn: 0,
            flush_lsn: 0,
            replay_lsn: None,
            checkpoint_lsn: 0,
            updated_at: chrono::Utc::now(),
        }
    }
}

/// Replication slot information
#[derive(Debug, Clone)]
pub struct ReplicationSlot {
    /// Slot name
    pub name: String,
    /// Associated standby node ID
    pub standby_id: Uuid,
    /// Confirmed flush LSN
    pub confirmed_flush_lsn: Lsn,
    /// Restart LSN (where to start streaming from)
    pub restart_lsn: Lsn,
    /// Is the slot active?
    pub active: bool,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// LSN Manager - tracks LSN positions across cluster
pub struct LsnManager {
    /// This node's ID
    node_id: Uuid,
    /// This node's LSN entry
    local_lsn: Arc<RwLock<LsnEntry>>,
    /// Remote node LSN entries
    remote_lsns: Arc<RwLock<HashMap<Uuid, LsnEntry>>>,
    /// Replication slots
    slots: Arc<RwLock<HashMap<String, ReplicationSlot>>>,
}

impl LsnManager {
    /// Create a new LSN manager
    pub fn new(node_id: Uuid) -> Self {
        let mut local = LsnEntry::default();
        local.node_id = node_id;

        Self {
            node_id,
            local_lsn: Arc::new(RwLock::new(local)),
            remote_lsns: Arc::new(RwLock::new(HashMap::new())),
            slots: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the local write LSN
    pub async fn write_lsn(&self) -> Lsn {
        self.local_lsn.read().await.write_lsn
    }

    /// Get the local flush LSN
    pub async fn flush_lsn(&self) -> Lsn {
        self.local_lsn.read().await.flush_lsn
    }

    /// Get the local checkpoint LSN
    pub async fn checkpoint_lsn(&self) -> Lsn {
        self.local_lsn.read().await.checkpoint_lsn
    }

    /// Advance the write LSN
    pub async fn advance_write(&self, new_lsn: Lsn) -> Result<()> {
        let mut entry = self.local_lsn.write().await;
        if new_lsn <= entry.write_lsn {
            return Err(ReplicationError::LsnTracking(format!(
                "New LSN {} is not greater than current {}",
                new_lsn, entry.write_lsn
            )));
        }
        entry.write_lsn = new_lsn;
        entry.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Advance the flush LSN
    pub async fn advance_flush(&self, new_lsn: Lsn) -> Result<()> {
        let mut entry = self.local_lsn.write().await;
        if new_lsn > entry.write_lsn {
            return Err(ReplicationError::LsnTracking(format!(
                "Flush LSN {} cannot exceed write LSN {}",
                new_lsn, entry.write_lsn
            )));
        }
        entry.flush_lsn = new_lsn;
        entry.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Set checkpoint LSN
    pub async fn set_checkpoint(&self, lsn: Lsn) -> Result<()> {
        let mut entry = self.local_lsn.write().await;
        entry.checkpoint_lsn = lsn;
        entry.updated_at = chrono::Utc::now();
        Ok(())
    }

    /// Update a remote node's LSN
    pub async fn update_remote(&self, node_id: Uuid, lsn_entry: LsnEntry) {
        self.remote_lsns.write().await.insert(node_id, lsn_entry);
    }

    /// Get a remote node's LSN entry
    pub async fn get_remote(&self, node_id: &Uuid) -> Option<LsnEntry> {
        self.remote_lsns.read().await.get(node_id).cloned()
    }

    /// Calculate replication lag for a standby
    pub async fn replication_lag(&self, standby_id: &Uuid) -> Option<u64> {
        let local = self.local_lsn.read().await;
        let remotes = self.remote_lsns.read().await;

        remotes.get(standby_id).map(|remote| {
            let standby_lsn = remote.replay_lsn.unwrap_or(remote.flush_lsn);
            local.write_lsn.saturating_sub(standby_lsn)
        })
    }

    /// Get minimum confirmed flush LSN across all standbys
    ///
    /// Used to determine safe WAL retention point.
    pub async fn min_confirmed_flush(&self) -> Lsn {
        let remotes = self.remote_lsns.read().await;
        remotes
            .values()
            .map(|e| e.flush_lsn)
            .min()
            .unwrap_or(0)
    }

    /// Create a replication slot
    pub async fn create_slot(&self, name: String, standby_id: Uuid) -> Result<ReplicationSlot> {
        let mut slots = self.slots.write().await;

        if slots.contains_key(&name) {
            return Err(ReplicationError::LsnTracking(format!(
                "Slot '{}' already exists",
                name
            )));
        }

        let local = self.local_lsn.read().await;
        let slot = ReplicationSlot {
            name: name.clone(),
            standby_id,
            confirmed_flush_lsn: 0,
            restart_lsn: local.flush_lsn,
            active: false,
            created_at: chrono::Utc::now(),
        };

        slots.insert(name, slot.clone());
        Ok(slot)
    }

    /// Drop a replication slot
    pub async fn drop_slot(&self, name: &str) -> Result<()> {
        let mut slots = self.slots.write().await;
        slots.remove(name).ok_or_else(|| {
            ReplicationError::LsnTracking(format!("Slot '{}' not found", name))
        })?;
        Ok(())
    }

    /// Activate a replication slot
    pub async fn activate_slot(&self, name: &str) -> Result<()> {
        let mut slots = self.slots.write().await;
        let slot = slots.get_mut(name).ok_or_else(|| {
            ReplicationError::LsnTracking(format!("Slot '{}' not found", name))
        })?;
        slot.active = true;
        Ok(())
    }

    /// Deactivate a replication slot
    pub async fn deactivate_slot(&self, name: &str) -> Result<()> {
        let mut slots = self.slots.write().await;
        let slot = slots.get_mut(name).ok_or_else(|| {
            ReplicationError::LsnTracking(format!("Slot '{}' not found", name))
        })?;
        slot.active = false;
        Ok(())
    }

    /// Update slot's confirmed flush LSN
    pub async fn update_slot_flush(&self, name: &str, lsn: Lsn) -> Result<()> {
        let mut slots = self.slots.write().await;
        let slot = slots.get_mut(name).ok_or_else(|| {
            ReplicationError::LsnTracking(format!("Slot '{}' not found", name))
        })?;
        slot.confirmed_flush_lsn = lsn;
        Ok(())
    }

    /// Get slot information
    pub async fn get_slot(&self, name: &str) -> Option<ReplicationSlot> {
        self.slots.read().await.get(name).cloned()
    }

    /// List all slots
    pub async fn list_slots(&self) -> Vec<ReplicationSlot> {
        self.slots.read().await.values().cloned().collect()
    }

    /// Get local LSN entry
    pub async fn local_entry(&self) -> LsnEntry {
        self.local_lsn.read().await.clone()
    }

    /// Get all remote LSN entries
    pub async fn remote_entries(&self) -> HashMap<Uuid, LsnEntry> {
        self.remote_lsns.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_lsn_manager_creation() {
        let node_id = Uuid::new_v4();
        let manager = LsnManager::new(node_id);
        assert_eq!(manager.write_lsn().await, 0);
        assert_eq!(manager.flush_lsn().await, 0);
    }

    #[tokio::test]
    async fn test_advance_write_lsn() {
        let manager = LsnManager::new(Uuid::new_v4());

        manager.advance_write(100).await.expect("advance failed");
        assert_eq!(manager.write_lsn().await, 100);

        manager.advance_write(200).await.expect("advance failed");
        assert_eq!(manager.write_lsn().await, 200);

        // Should fail if LSN doesn't advance
        assert!(manager.advance_write(200).await.is_err());
        assert!(manager.advance_write(150).await.is_err());
    }

    #[tokio::test]
    async fn test_flush_lsn_constraint() {
        let manager = LsnManager::new(Uuid::new_v4());

        manager.advance_write(100).await.expect("advance failed");

        // Should succeed
        manager.advance_flush(50).await.expect("flush failed");
        manager.advance_flush(100).await.expect("flush failed");

        // Should fail - flush can't exceed write
        assert!(manager.advance_flush(150).await.is_err());
    }

    #[tokio::test]
    async fn test_replication_lag() {
        let manager = LsnManager::new(Uuid::new_v4());
        manager.advance_write(1000).await.expect("advance failed");

        let standby_id = Uuid::new_v4();
        let standby_entry = LsnEntry {
            node_id: standby_id,
            write_lsn: 0,
            flush_lsn: 500,
            replay_lsn: Some(500),
            checkpoint_lsn: 0,
            updated_at: chrono::Utc::now(),
        };

        manager.update_remote(standby_id, standby_entry).await;

        let lag = manager.replication_lag(&standby_id).await;
        assert_eq!(lag, Some(500)); // 1000 - 500
    }

    #[tokio::test]
    async fn test_replication_slots() {
        let manager = LsnManager::new(Uuid::new_v4());
        let standby_id = Uuid::new_v4();

        // Create slot
        let slot = manager
            .create_slot("standby_slot".to_string(), standby_id)
            .await
            .expect("create failed");
        assert!(!slot.active);

        // Activate slot
        manager.activate_slot("standby_slot").await.expect("activate failed");
        let slot = manager.get_slot("standby_slot").await.expect("get failed");
        assert!(slot.active);

        // Update flush
        manager.update_slot_flush("standby_slot", 100).await.expect("update failed");
        let slot = manager.get_slot("standby_slot").await.expect("get failed");
        assert_eq!(slot.confirmed_flush_lsn, 100);

        // List slots
        let slots = manager.list_slots().await;
        assert_eq!(slots.len(), 1);

        // Drop slot
        manager.drop_slot("standby_slot").await.expect("drop failed");
        assert!(manager.get_slot("standby_slot").await.is_none());
    }
}
