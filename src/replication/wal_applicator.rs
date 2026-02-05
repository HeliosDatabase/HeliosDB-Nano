//! WAL Applicator - Tier 1 Warm Standby
//!
//! Receives and applies WAL segments on standby nodes.
//! Maintains synchronization with primary and supports failover.
//!
//! The WAL Applicator deserializes replicated WAL entries and applies them
//! to the local storage engine, keeping the standby synchronized with the primary.

use super::config::PrimaryConfig;
use super::wal_replicator::{Lsn, WalEntry};
use super::ha_state::ha_state;
use super::{ReplicationError, Result};
use crate::storage::WalOperation;
use crate::storage::StorageEngine;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock, broadcast};
use tracing::{debug, info, error};

/// Application result for a WAL entry
#[derive(Debug, Clone)]
pub struct ApplyResult {
    /// Applied LSN
    pub lsn: Lsn,
    /// Whether the entry was applied (vs skipped as duplicate)
    pub applied: bool,
    /// Any warnings during application
    pub warnings: Vec<String>,
}

/// WAL Applicator state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicatorState {
    /// Not connected to primary
    Disconnected,
    /// Connecting to primary
    Connecting,
    /// Catching up with primary (initial sync)
    CatchingUp,
    /// Streaming in real-time
    Streaming,
    /// Paused (manual intervention)
    Paused,
    /// Error state
    Error,
}

/// WAL Applicator - receives and applies WAL on standby
pub struct WalApplicator {
    /// Primary configuration
    primary_config: PrimaryConfig,
    /// Current applied LSN
    applied_lsn: Arc<RwLock<Lsn>>,
    /// Applicator state
    state: Arc<RwLock<ApplicatorState>>,
    /// Queue sender (for external use)
    queue_tx: mpsc::Sender<WalEntry>,
    /// Queue receiver (moved to run loop)
    queue_rx: Arc<RwLock<Option<mpsc::Receiver<WalEntry>>>>,
    /// Shutdown signal sender
    shutdown_tx: broadcast::Sender<()>,
    /// Statistics
    entries_applied: Arc<RwLock<u64>>,
    entries_skipped: Arc<RwLock<u64>>,
    errors_count: Arc<RwLock<u64>>,
}

impl WalApplicator {
    /// Create a new WAL applicator
    pub fn new(primary_config: PrimaryConfig) -> Self {
        let (queue_tx, queue_rx) = mpsc::channel(10000);
        let (shutdown_tx, _) = broadcast::channel(1);

        Self {
            primary_config,
            applied_lsn: Arc::new(RwLock::new(0)),
            state: Arc::new(RwLock::new(ApplicatorState::Disconnected)),
            queue_tx,
            queue_rx: Arc::new(RwLock::new(Some(queue_rx))),
            shutdown_tx,
            entries_applied: Arc::new(RwLock::new(0)),
            entries_skipped: Arc::new(RwLock::new(0)),
            errors_count: Arc::new(RwLock::new(0)),
        }
    }

    /// Start the WAL applicator with a storage engine
    ///
    /// This spawns a background task that processes incoming WAL entries
    /// and applies them to the storage engine.
    pub async fn start_with_storage(&self, storage: Arc<StorageEngine>) -> Result<()> {
        // Take ownership of the receiver
        let queue_rx = {
            let mut rx_guard = self.queue_rx.write().await;
            rx_guard.take()
        };

        let Some(mut queue_rx) = queue_rx else {
            return Err(ReplicationError::WalStreaming("Applicator already started".to_string()));
        };

        *self.state.write().await = ApplicatorState::Streaming;
        info!("WAL Applicator started, ready to receive entries");

        let applied_lsn = self.applied_lsn.clone();
        let state = self.state.clone();
        let entries_applied = self.entries_applied.clone();
        let entries_skipped = self.entries_skipped.clone();
        let errors_count = self.errors_count.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Spawn the applicator task
        tokio::spawn(async move {
            info!("WAL Applicator background task started");

            loop {
                tokio::select! {
                    // Check for shutdown signal - only break on Ok, ignore errors
                    result = shutdown_rx.recv() => {
                        match result {
                            Ok(()) => {
                                info!("WAL Applicator received shutdown signal");
                                break;
                            }
                            Err(e) => {
                                debug!("Shutdown channel error (ignoring): {:?}", e);
                                // Re-subscribe and continue - this can happen with Lagged errors
                                shutdown_rx = shutdown_rx.resubscribe();
                            }
                        }
                    }

                    // Process incoming entries
                    entry = queue_rx.recv() => {
                        let Some(entry) = entry else {
                            info!("WAL Applicator queue closed");
                            break;
                        };

                        info!("WAL Applicator: Received entry LSN={} from queue", entry.lsn);

                        // Check if we're paused
                        if *state.read().await == ApplicatorState::Paused {
                            debug!("WAL Applicator paused, skipping entry {}", entry.lsn);
                            continue;
                        }

                        // Apply the entry
                        info!("WAL Applicator: Applying entry LSN={}", entry.lsn);
                        match Self::apply_entry_to_storage(&storage, &entry, &applied_lsn).await {
                            Ok(result) => {
                                if result.applied {
                                    *entries_applied.write().await += 1;
                                    info!("WAL Applicator: Applied entry LSN={} successfully", entry.lsn);

                                    // Update HA state
                                    ha_state().update_primary(|p| {
                                        p.local_lsn = entry.lsn;
                                        p.lag_bytes = p.primary_lsn.saturating_sub(entry.lsn);
                                    });
                                } else {
                                    *entries_skipped.write().await += 1;
                                    debug!("Skipped WAL entry LSN {} (already applied)", entry.lsn);
                                }
                            }
                            Err(e) => {
                                *errors_count.write().await += 1;
                                error!("Failed to apply WAL entry {}: {}", entry.lsn, e);
                            }
                        }
                    }
                }
            }

            *state.write().await = ApplicatorState::Disconnected;
            info!("WAL Applicator background task stopped");
        });

        Ok(())
    }

    /// Apply a single WAL entry to storage
    async fn apply_entry_to_storage(
        storage: &Arc<StorageEngine>,
        entry: &WalEntry,
        applied_lsn: &Arc<RwLock<Lsn>>,
    ) -> Result<ApplyResult> {
        let current_lsn = *applied_lsn.read().await;

        // Skip already-applied entries (idempotency)
        if entry.lsn <= current_lsn {
            return Ok(ApplyResult {
                lsn: entry.lsn,
                applied: false,
                warnings: vec!["Entry already applied".to_string()],
            });
        }

        // Deserialize the operation from entry data
        let operation: WalOperation = bincode::deserialize(&entry.data)
            .map_err(|e| ReplicationError::WalStreaming(
                format!("Failed to deserialize WAL operation: {}", e)
            ))?;

        // Apply the operation to storage
        // The storage engine's apply_wal_operation handles all operation types
        storage.apply_replicated_operation(operation)
            .map_err(|e| ReplicationError::Storage(
                format!("Failed to apply WAL operation: {}", e)
            ))?;

        // Update applied LSN
        *applied_lsn.write().await = entry.lsn;

        // Update HA state LSN
        ha_state().set_lsn(entry.lsn);

        Ok(ApplyResult {
            lsn: entry.lsn,
            applied: true,
            warnings: vec![],
        })
    }

    /// Start the WAL applicator (legacy method - use start_with_storage instead)
    pub async fn start(&self) -> Result<()> {
        *self.state.write().await = ApplicatorState::Connecting;
        info!("WAL Applicator started (no storage engine - entries will be queued only)");
        Ok(())
    }

    /// Stop the WAL applicator
    pub async fn stop(&self) -> Result<()> {
        let _ = self.shutdown_tx.send(());
        *self.state.write().await = ApplicatorState::Disconnected;
        info!("WAL Applicator stopped");
        Ok(())
    }

    /// Pause WAL application
    pub async fn pause(&self) -> Result<()> {
        *self.state.write().await = ApplicatorState::Paused;
        info!("WAL Applicator paused");
        Ok(())
    }

    /// Resume WAL application
    pub async fn resume(&self) -> Result<()> {
        *self.state.write().await = ApplicatorState::Streaming;
        info!("WAL Applicator resumed");
        Ok(())
    }

    /// Apply a single WAL entry (for direct application without queue)
    pub async fn apply(&self, entry: WalEntry) -> Result<ApplyResult> {
        let current_lsn = *self.applied_lsn.read().await;

        // Skip already-applied entries (idempotency)
        if entry.lsn <= current_lsn {
            return Ok(ApplyResult {
                lsn: entry.lsn,
                applied: false,
                warnings: vec!["Entry already applied".to_string()],
            });
        }

        // Without a storage engine, we just update the LSN
        // This is used for testing or when entries are applied externally
        *self.applied_lsn.write().await = entry.lsn;

        Ok(ApplyResult {
            lsn: entry.lsn,
            applied: true,
            warnings: vec!["No storage engine - entry not persisted".to_string()],
        })
    }

    /// Get current applied LSN
    pub async fn applied_lsn(&self) -> Lsn {
        *self.applied_lsn.read().await
    }

    /// Get applicator state
    pub async fn state(&self) -> ApplicatorState {
        *self.state.read().await
    }

    /// Get replication lag (current_primary_lsn - applied_lsn)
    pub async fn lag(&self, primary_lsn: Lsn) -> u64 {
        let applied = *self.applied_lsn.read().await;
        primary_lsn.saturating_sub(applied)
    }

    /// Queue an entry for application
    pub async fn queue_entry(&self, entry: WalEntry) -> Result<()> {
        self.queue_tx
            .send(entry)
            .await
            .map_err(|e| ReplicationError::WalStreaming(e.to_string()))
    }

    /// Get the queue sender for direct entry submission
    pub fn get_queue_sender(&self) -> mpsc::Sender<WalEntry> {
        self.queue_tx.clone()
    }

    /// Get statistics
    pub async fn stats(&self) -> (u64, u64, u64) {
        (
            *self.entries_applied.read().await,
            *self.entries_skipped.read().await,
            *self.errors_count.read().await,
        )
    }

    /// Promote this standby to primary
    ///
    /// Called during failover to make this node the new primary.
    pub async fn promote(&self) -> Result<()> {
        // Stop accepting WAL from old primary
        let _ = self.shutdown_tx.send(());

        // Update state
        *self.state.write().await = ApplicatorState::Disconnected;

        let final_lsn = self.applied_lsn().await;
        info!("Standby promoted to primary at LSN {}", final_lsn);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::wal_replicator::WalEntryType;
    use std::time::Duration;

    #[tokio::test]
    async fn test_wal_applicator_creation() {
        let config = PrimaryConfig {
            host: "localhost".to_string(),
            port: 5432,
            connect_timeout: Duration::from_secs(10),
            use_tls: false,
        };
        let applicator = WalApplicator::new(config);
        assert_eq!(applicator.applied_lsn().await, 0);
        assert_eq!(applicator.state().await, ApplicatorState::Disconnected);
    }

    #[tokio::test]
    async fn test_apply_entry() {
        let config = PrimaryConfig {
            host: "localhost".to_string(),
            port: 5432,
            connect_timeout: Duration::from_secs(10),
            use_tls: false,
        };
        let applicator = WalApplicator::new(config);

        let entry = WalEntry {
            lsn: 1,
            tx_id: None,
            entry_type: WalEntryType::Insert,
            data: vec![1, 2, 3],
            checksum: 0,
        };

        let result = applicator.apply(entry).await.expect("apply failed");
        assert!(result.applied);
        assert_eq!(result.lsn, 1);
        assert_eq!(applicator.applied_lsn().await, 1);
    }

    #[tokio::test]
    async fn test_idempotent_apply() {
        let config = PrimaryConfig {
            host: "localhost".to_string(),
            port: 5432,
            connect_timeout: Duration::from_secs(10),
            use_tls: false,
        };
        let applicator = WalApplicator::new(config);

        let entry = WalEntry {
            lsn: 1,
            tx_id: None,
            entry_type: WalEntryType::Insert,
            data: vec![1, 2, 3],
            checksum: 0,
        };

        // Apply once
        applicator.apply(entry.clone()).await.expect("apply failed");

        // Apply again - should be idempotent
        let result = applicator.apply(entry).await.expect("apply failed");
        assert!(!result.applied);
    }

    #[tokio::test]
    async fn test_queue_entry() {
        let config = PrimaryConfig {
            host: "localhost".to_string(),
            port: 5432,
            connect_timeout: Duration::from_secs(10),
            use_tls: false,
        };
        let applicator = WalApplicator::new(config);

        let entry = WalEntry {
            lsn: 1,
            tx_id: None,
            entry_type: WalEntryType::Insert,
            data: vec![1, 2, 3],
            checksum: 0,
        };

        // Queue should succeed
        applicator.queue_entry(entry).await.expect("queue failed");
    }
}
