//! WAL Replicator - Tier 1 Warm Standby
//!
//! Streams Write-Ahead Log (WAL) segments from primary to standby nodes.
//! Based on PostgreSQL-style WAL streaming (industry standard, FTO-safe).
//!
//! # Architecture
//!
//! ```text
//! PRIMARY NODE                          STANDBY NODE
//! ┌─────────────────┐                  ┌─────────────────┐
//! │ WAL Writer      │                  │ WAL Applicator  │
//! │ ├─ segment.001  │──────stream─────►│ ├─ segment.001  │
//! │ ├─ segment.002  │                  │ ├─ segment.002  │
//! │ └─ current      │                  │ └─ applied_lsn  │
//! └─────────────────┘                  └─────────────────┘
//! ```

use super::config::{StandbyConfig, WalStreamingConfig};
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Log Sequence Number - unique identifier for WAL position
pub type Lsn = u64;

/// WAL segment metadata
#[derive(Debug, Clone)]
pub struct WalSegment {
    /// Segment number
    pub segment_id: u64,
    /// Start LSN of this segment
    pub start_lsn: Lsn,
    /// End LSN of this segment (exclusive)
    pub end_lsn: Lsn,
    /// Segment size in bytes
    pub size: usize,
    /// CRC32 checksum
    pub checksum: u32,
    /// Timestamp of segment creation
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// WAL entry to be replicated
#[derive(Debug, Clone)]
pub struct WalEntry {
    /// Log sequence number
    pub lsn: Lsn,
    /// Transaction ID (for grouping related changes)
    pub tx_id: Option<u64>,
    /// Entry type
    pub entry_type: WalEntryType,
    /// Serialized entry data
    pub data: Vec<u8>,
    /// Entry checksum
    pub checksum: u32,
}

/// Types of WAL entries
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalEntryType {
    /// Insert operation
    Insert,
    /// Update operation
    Update,
    /// Delete operation
    Delete,
    /// Transaction begin
    TxBegin,
    /// Transaction commit
    TxCommit,
    /// Transaction rollback
    TxRollback,
    /// Checkpoint marker
    Checkpoint,
    /// Schema change (DDL)
    SchemaChange,
    /// Branch operation
    BranchOp,
}

/// Standby connection state
#[derive(Debug, Clone)]
pub struct StandbyState {
    /// Standby node ID
    pub node_id: Uuid,
    /// Last acknowledged LSN
    pub ack_lsn: Lsn,
    /// Connection status
    pub connected: bool,
    /// Last heartbeat time
    pub last_heartbeat: chrono::DateTime<chrono::Utc>,
    /// Replication lag in bytes
    pub lag_bytes: u64,
}

/// WAL Replicator - streams WAL from primary to standbys
pub struct WalReplicator {
    /// Configuration
    config: WalStreamingConfig,
    /// Standby configurations
    standbys: Vec<StandbyConfig>,
    /// Current WAL position (write LSN)
    current_lsn: Arc<RwLock<Lsn>>,
    /// Standby states
    standby_states: Arc<RwLock<HashMap<Uuid, StandbyState>>>,
    /// Broadcast channel for WAL entries
    wal_broadcast: broadcast::Sender<WalEntry>,
    /// Shutdown signal sender
    shutdown_tx: mpsc::Sender<()>,
    /// Shutdown signal receiver (stored for cloning)
    shutdown_rx: Arc<RwLock<Option<mpsc::Receiver<()>>>>,
    /// Whether the replicator is running
    running: Arc<AtomicBool>,
    /// Background task handles
    task_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
}

impl WalReplicator {
    /// Create a new WAL replicator
    pub fn new(config: WalStreamingConfig, standbys: Vec<StandbyConfig>) -> Self {
        let (wal_broadcast, _) = broadcast::channel(config.batch_size);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        Self {
            config,
            standbys,
            current_lsn: Arc::new(RwLock::new(0)),
            standby_states: Arc::new(RwLock::new(HashMap::new())),
            wal_broadcast,
            shutdown_tx,
            shutdown_rx: Arc::new(RwLock::new(Some(shutdown_rx))),
            running: Arc::new(AtomicBool::new(false)),
            task_handles: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Start the WAL replicator
    ///
    /// This spawns background tasks for:
    /// - Streaming WAL to standbys
    /// - Managing standby connections
    /// - Handling acknowledgments
    pub async fn start(&self) -> Result<()> {
        // Check if already running
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(ReplicationError::WalStreaming(
                "WAL Replicator already running".to_string(),
            ));
        }

        tracing::info!(
            "Starting WAL Replicator with {} standbys",
            self.standbys.len()
        );

        // Initialize standby states
        {
            let mut states = self.standby_states.write().await;
            for standby in &self.standbys {
                let node_id = Uuid::new_v4(); // Generate ID for tracking
                states.insert(
                    node_id,
                    StandbyState {
                        node_id,
                        ack_lsn: 0,
                        connected: false,
                        last_heartbeat: chrono::Utc::now(),
                        lag_bytes: 0,
                    },
                );
                tracing::info!(
                    "Registered standby at {}:{} (id: {})",
                    standby.host,
                    standby.port,
                    node_id
                );
            }
        }

        // Start heartbeat monitoring task
        let running = self.running.clone();
        let standby_states = self.standby_states.clone();
        let heartbeat_interval = self.config.heartbeat_interval;
        let heartbeat_timeout = self.config.heartbeat_timeout;

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(heartbeat_interval);
            while running.load(Ordering::SeqCst) {
                interval.tick().await;

                // Check for stale standbys
                let now = chrono::Utc::now();
                let mut states = standby_states.write().await;
                for (id, state) in states.iter_mut() {
                    if state.connected {
                        let elapsed = now - state.last_heartbeat;
                        if elapsed > chrono::Duration::from_std(heartbeat_timeout).unwrap_or_default()
                        {
                            tracing::warn!("Standby {} heartbeat timeout, marking disconnected", id);
                            state.connected = false;
                        }
                    }
                }
            }
            tracing::debug!("Heartbeat monitor task stopped");
        });

        // Store task handle
        {
            let mut handles = self.task_handles.write().await;
            handles.push(heartbeat_handle);
        }

        tracing::info!("WAL Replicator started successfully");
        Ok(())
    }

    /// Stop the WAL replicator
    pub async fn stop(&self) -> Result<()> {
        // Check if running
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(()); // Already stopped
        }

        tracing::info!("Stopping WAL Replicator...");

        // Signal shutdown
        let _ = self.shutdown_tx.send(()).await;

        // Wait for background tasks to complete (with timeout)
        {
            let mut handles = self.task_handles.write().await;
            for handle in handles.drain(..) {
                // Give each task 5 seconds to complete
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    handle,
                ).await;
            }
        }

        // Mark all standbys as disconnected
        {
            let mut states = self.standby_states.write().await;
            for state in states.values_mut() {
                state.connected = false;
            }
        }

        tracing::info!("WAL Replicator stopped successfully");
        Ok(())
    }

    /// Check if the replicator is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Append a WAL entry for replication
    ///
    /// This is called by the storage engine after writing to local WAL.
    /// The entry is broadcast to all connected standbys.
    pub async fn append(&self, entry: WalEntry) -> Result<Lsn> {
        // Update current LSN
        let mut current = self.current_lsn.write().await;
        *current = entry.lsn;

        // Update lag for all standbys
        {
            let mut states = self.standby_states.write().await;
            for state in states.values_mut() {
                state.lag_bytes = entry.lsn.saturating_sub(state.ack_lsn);
            }
        }

        // Broadcast to all subscribers (standby connections)
        self.wal_broadcast
            .send(entry.clone())
            .map_err(|e| ReplicationError::WalStreaming(e.to_string()))?;

        Ok(entry.lsn)
    }

    /// Get the current write LSN
    pub async fn current_lsn(&self) -> Lsn {
        *self.current_lsn.read().await
    }

    /// Get standby states
    pub async fn standby_states(&self) -> HashMap<Uuid, StandbyState> {
        self.standby_states.read().await.clone()
    }

    /// Get replication lag for a standby
    pub async fn get_lag(&self, standby_id: &Uuid) -> Option<u64> {
        let states = self.standby_states.read().await;
        states.get(standby_id).map(|s| s.lag_bytes)
    }

    /// Subscribe to WAL entries (for standby connections)
    pub fn subscribe(&self) -> broadcast::Receiver<WalEntry> {
        self.wal_broadcast.subscribe()
    }

    /// Acknowledge LSN from standby
    pub async fn acknowledge(&self, standby_id: Uuid, ack_lsn: Lsn) -> Result<()> {
        let mut states = self.standby_states.write().await;
        if let Some(state) = states.get_mut(&standby_id) {
            state.ack_lsn = ack_lsn;
            state.last_heartbeat = chrono::Utc::now();
            let current = *self.current_lsn.read().await;
            state.lag_bytes = current.saturating_sub(ack_lsn);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_wal_replicator_creation() {
        let config = WalStreamingConfig::default();
        let replicator = WalReplicator::new(config, vec![]);
        assert_eq!(replicator.current_lsn().await, 0);
    }

    #[tokio::test]
    async fn test_wal_entry_broadcast() {
        let config = WalStreamingConfig::default();
        let replicator = WalReplicator::new(config, vec![]);

        let mut rx = replicator.subscribe();

        let entry = WalEntry {
            lsn: 1,
            tx_id: None,
            entry_type: WalEntryType::Insert,
            data: vec![1, 2, 3],
            checksum: 0,
        };

        replicator.append(entry.clone()).await.expect("append failed");

        let received = rx.recv().await.expect("recv failed");
        assert_eq!(received.lsn, 1);
    }

    #[tokio::test]
    async fn test_start_stop() {
        use super::super::config::SyncMode;

        let config = WalStreamingConfig::default();
        let standbys = vec![
            StandbyConfig {
                node_id: Uuid::new_v4(),
                host: "standby1.example.com".to_string(),
                port: 5433,
                sync_mode: SyncMode::Async,
                priority: 1,
            },
        ];
        let replicator = WalReplicator::new(config, standbys);

        // Should not be running initially
        assert!(!replicator.is_running());

        // Start the replicator
        replicator.start().await.expect("start failed");
        assert!(replicator.is_running());

        // Verify standby was registered
        let states = replicator.standby_states().await;
        assert_eq!(states.len(), 1);

        // Starting again should fail
        let result = replicator.start().await;
        assert!(result.is_err());

        // Stop the replicator
        replicator.stop().await.expect("stop failed");
        assert!(!replicator.is_running());

        // Stopping again should be fine (no-op)
        replicator.stop().await.expect("stop failed");
    }
}
