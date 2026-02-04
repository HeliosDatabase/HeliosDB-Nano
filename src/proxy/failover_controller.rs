//! Failover Controller - HeliosProxy
//!
//! Orchestrates failover operations including primary detection,
//! automatic rerouting, and transaction replay coordination.

use super::{NodeEndpoint, NodeId, NodeRole, ProxyError, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

// TR (Transaction Replay) imports
#[cfg(feature = "ha-tr")]
use super::failover_replay::{FailoverReplay, ReplayConfig, ReplayResult};
#[cfg(feature = "ha-tr")]
use super::transaction_journal::TransactionJournal;

/// Failover configuration
#[derive(Debug, Clone)]
pub struct FailoverConfig {
    /// Time to wait before initiating failover
    pub detection_time: Duration,
    /// Maximum time to wait for failover completion
    pub failover_timeout: Duration,
    /// Automatic failover (vs manual confirmation)
    pub auto_failover: bool,
    /// Prefer synchronous standbys for failover
    pub prefer_sync_standby: bool,
    /// Maximum LSN lag allowed for standby promotion (bytes)
    pub max_lag_bytes: u64,
    /// Retry failed failovers
    pub retry_failed: bool,
    /// Max retry attempts
    pub max_retries: u32,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            detection_time: Duration::from_secs(10),
            failover_timeout: Duration::from_secs(60),
            auto_failover: true,
            prefer_sync_standby: true,
            max_lag_bytes: 16 * 1024 * 1024, // 16MB
            retry_failed: true,
            max_retries: 3,
        }
    }
}

/// Failover mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverMode {
    /// Automatic failover on primary failure
    Automatic,
    /// Manual failover (require confirmation)
    Manual,
    /// Disabled (no failover)
    Disabled,
}

/// Failover state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverState {
    /// Normal operation
    Normal,
    /// Primary failure detected
    PrimaryFailed,
    /// Failover in progress
    InProgress,
    /// Waiting for standby to catch up
    WaitingForSync,
    /// Failover completed
    Completed,
    /// Failover failed
    Failed,
}

/// Failover event
#[derive(Debug, Clone)]
pub enum FailoverEvent {
    /// Primary failure detected
    PrimaryFailed { node_id: NodeId },
    /// Failover started
    FailoverStarted { from: NodeId, to: NodeId },
    /// Waiting for standby sync
    WaitingForSync { standby: NodeId, lag_bytes: u64 },
    /// Standby promoted
    StandbyPromoted { new_primary: NodeId },
    /// Failover completed
    FailoverCompleted { duration_ms: u64 },
    /// Failover failed
    FailoverFailed { reason: String },
    /// Old primary recovered (split-brain prevention)
    OldPrimaryRecovered { node_id: NodeId },
}

/// Failover candidate information
#[derive(Debug, Clone)]
pub struct FailoverCandidate {
    /// Node ID
    pub node_id: NodeId,
    /// Node endpoint
    pub endpoint: NodeEndpoint,
    /// Is synchronous standby
    pub is_sync: bool,
    /// Replication lag (bytes)
    pub lag_bytes: u64,
    /// Priority (lower = better)
    pub priority: u32,
    /// Last heartbeat
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
}

/// Failover history entry
#[derive(Debug, Clone)]
pub struct FailoverHistoryEntry {
    /// Failover ID
    pub id: uuid::Uuid,
    /// Start time
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// End time
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Old primary
    pub old_primary: NodeId,
    /// New primary
    pub new_primary: Option<NodeId>,
    /// Result
    pub success: bool,
    /// Error message (if failed)
    pub error: Option<String>,
}

/// Failover Controller
pub struct FailoverController {
    /// Configuration
    config: FailoverConfig,
    /// Current state
    state: Arc<RwLock<FailoverState>>,
    /// Current primary node
    current_primary: Arc<RwLock<Option<NodeId>>>,
    /// Failover candidates (standbys)
    candidates: Arc<RwLock<HashMap<NodeId, FailoverCandidate>>>,
    /// Event channel sender
    event_tx: mpsc::Sender<FailoverEvent>,
    /// Event channel receiver
    event_rx: Option<mpsc::Receiver<FailoverEvent>>,
    /// Failover count
    failover_count: AtomicU64,
    /// Failover history
    history: Arc<RwLock<Vec<FailoverHistoryEntry>>>,
    /// Running flag
    running: Arc<RwLock<bool>>,
}

impl FailoverController {
    /// Create a new failover controller
    pub fn new(config: FailoverConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);

        Self {
            config,
            state: Arc::new(RwLock::new(FailoverState::Normal)),
            current_primary: Arc::new(RwLock::new(None)),
            candidates: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            event_rx: Some(event_rx),
            failover_count: AtomicU64::new(0),
            history: Arc::new(RwLock::new(Vec::new())),
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Set the current primary
    pub async fn set_primary(&self, node_id: NodeId) {
        *self.current_primary.write().await = Some(node_id);
        tracing::info!("Primary set to {:?}", node_id);
    }

    /// Get the current primary
    pub async fn get_primary(&self) -> Option<NodeId> {
        *self.current_primary.read().await
    }

    /// Register a failover candidate (standby)
    pub async fn register_candidate(&self, candidate: FailoverCandidate) {
        let node_id = candidate.node_id;
        self.candidates.write().await.insert(node_id, candidate);
        tracing::debug!("Registered failover candidate {:?}", node_id);
    }

    /// Remove a failover candidate
    pub async fn remove_candidate(&self, node_id: &NodeId) {
        self.candidates.write().await.remove(node_id);
    }

    /// Update candidate lag
    pub async fn update_candidate_lag(&self, node_id: &NodeId, lag_bytes: u64) {
        if let Some(candidate) = self.candidates.write().await.get_mut(node_id) {
            candidate.lag_bytes = lag_bytes;
            candidate.last_heartbeat = Some(chrono::Utc::now());
        }
    }

    /// Get current state
    pub async fn state(&self) -> FailoverState {
        *self.state.read().await
    }

    /// Handle primary failure
    pub async fn on_primary_failed(&self, node_id: NodeId) -> Result<()> {
        let current_primary = self.current_primary.read().await;
        if *current_primary != Some(node_id) {
            return Ok(()); // Not the current primary
        }
        drop(current_primary);

        *self.state.write().await = FailoverState::PrimaryFailed;

        let _ = self
            .event_tx
            .send(FailoverEvent::PrimaryFailed { node_id })
            .await;

        tracing::warn!("Primary node {:?} failed", node_id);

        if self.config.auto_failover {
            self.initiate_failover().await?;
        }

        Ok(())
    }

    /// Initiate failover to best candidate
    pub async fn initiate_failover(&self) -> Result<()> {
        let old_primary = self
            .current_primary
            .read()
            .await
            .ok_or_else(|| ProxyError::FailoverFailed("No primary to failover from".to_string()))?;

        // Select best candidate
        let candidate = self.select_best_candidate().await?;
        let new_primary = candidate.node_id;

        *self.state.write().await = FailoverState::InProgress;

        let _ = self
            .event_tx
            .send(FailoverEvent::FailoverStarted {
                from: old_primary,
                to: new_primary,
            })
            .await;

        let start = chrono::Utc::now();

        // Record history entry
        let history_entry = FailoverHistoryEntry {
            id: uuid::Uuid::new_v4(),
            started_at: start,
            ended_at: None,
            old_primary,
            new_primary: Some(new_primary),
            success: false,
            error: None,
        };
        self.history.write().await.push(history_entry);

        // Check lag
        if candidate.lag_bytes > self.config.max_lag_bytes {
            *self.state.write().await = FailoverState::WaitingForSync;

            let _ = self
                .event_tx
                .send(FailoverEvent::WaitingForSync {
                    standby: new_primary,
                    lag_bytes: candidate.lag_bytes,
                })
                .await;

            // Wait for sync (with timeout)
            let sync_result = self.wait_for_sync(new_primary).await;
            if let Err(e) = sync_result {
                self.fail_failover(&e.to_string()).await;
                return Err(e);
            }
        }

        // Promote standby
        self.promote_standby(new_primary).await?;

        // Complete failover
        *self.current_primary.write().await = Some(new_primary);
        *self.state.write().await = FailoverState::Completed;
        self.failover_count.fetch_add(1, Ordering::SeqCst);

        let duration = chrono::Utc::now()
            .signed_duration_since(start)
            .num_milliseconds() as u64;

        // Update history
        if let Some(entry) = self.history.write().await.last_mut() {
            entry.ended_at = Some(chrono::Utc::now());
            entry.success = true;
        }

        let _ = self
            .event_tx
            .send(FailoverEvent::StandbyPromoted {
                new_primary,
            })
            .await;

        let _ = self
            .event_tx
            .send(FailoverEvent::FailoverCompleted { duration_ms: duration })
            .await;

        tracing::info!(
            "Failover completed: {:?} -> {:?} in {}ms",
            old_primary,
            new_primary,
            duration
        );

        // Reset state after a moment
        tokio::spawn({
            let state = self.state.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                *state.write().await = FailoverState::Normal;
            }
        });

        Ok(())
    }

    /// Select the best failover candidate
    async fn select_best_candidate(&self) -> Result<FailoverCandidate> {
        let candidates = self.candidates.read().await;

        if candidates.is_empty() {
            return Err(ProxyError::FailoverFailed(
                "No failover candidates available".to_string(),
            ));
        }

        // Sort by: sync status, lag, priority
        let mut sorted: Vec<_> = candidates.values().cloned().collect();
        sorted.sort_by(|a, b| {
            // Prefer sync standbys
            if self.config.prefer_sync_standby {
                if a.is_sync != b.is_sync {
                    return b.is_sync.cmp(&a.is_sync);
                }
            }
            // Then by lag
            if a.lag_bytes != b.lag_bytes {
                return a.lag_bytes.cmp(&b.lag_bytes);
            }
            // Then by priority
            a.priority.cmp(&b.priority)
        });

        sorted
            .first()
            .cloned()
            .ok_or_else(|| ProxyError::FailoverFailed("No eligible candidates".to_string()))
    }

    /// Wait for standby to catch up
    async fn wait_for_sync(&self, _standby: NodeId) -> Result<()> {
        // TODO: Implement actual sync waiting
        // 1. Monitor standby lag
        // 2. Wait until lag is below threshold
        // 3. Timeout if too slow

        tokio::time::timeout(self.config.failover_timeout, async {
            // Simulate waiting
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<(), ProxyError>(())
        })
        .await
        .map_err(|_| ProxyError::Timeout("Standby sync timeout".to_string()))?
    }

    /// Promote a standby to primary
    async fn promote_standby(&self, standby: NodeId) -> Result<()> {
        // TODO: Implement actual promotion
        // 1. Tell standby to promote
        // 2. Verify promotion succeeded
        // 3. Update routing

        tracing::info!("Promoting standby {:?} to primary", standby);
        Ok(())
    }

    /// Fail the failover
    async fn fail_failover(&self, reason: &str) {
        *self.state.write().await = FailoverState::Failed;

        if let Some(entry) = self.history.write().await.last_mut() {
            entry.ended_at = Some(chrono::Utc::now());
            entry.success = false;
            entry.error = Some(reason.to_string());
        }

        let _ = self
            .event_tx
            .send(FailoverEvent::FailoverFailed {
                reason: reason.to_string(),
            })
            .await;

        tracing::error!("Failover failed: {}", reason);
    }

    /// Handle old primary recovery (split-brain prevention)
    pub async fn on_old_primary_recovered(&self, node_id: NodeId) {
        // The old primary should be demoted to standby
        let _ = self
            .event_tx
            .send(FailoverEvent::OldPrimaryRecovered { node_id })
            .await;

        tracing::warn!(
            "Old primary {:?} recovered - must be demoted to prevent split-brain",
            node_id
        );

        // TODO: Implement demotion logic
    }

    /// Manual failover to specific node
    pub async fn manual_failover(&self, target: NodeId) -> Result<()> {
        // Verify target is a valid candidate
        let candidates = self.candidates.read().await;
        if !candidates.contains_key(&target) {
            return Err(ProxyError::FailoverFailed(format!(
                "Node {:?} is not a valid failover candidate",
                target
            )));
        }
        drop(candidates);

        // Force failover to specific node
        *self.state.write().await = FailoverState::InProgress;

        let old_primary = self.current_primary.read().await.unwrap_or(NodeId::new());

        let _ = self
            .event_tx
            .send(FailoverEvent::FailoverStarted {
                from: old_primary,
                to: target,
            })
            .await;

        self.promote_standby(target).await?;

        *self.current_primary.write().await = Some(target);
        *self.state.write().await = FailoverState::Completed;
        self.failover_count.fetch_add(1, Ordering::SeqCst);

        Ok(())
    }

    /// Get failover count
    pub fn failover_count(&self) -> u64 {
        self.failover_count.load(Ordering::SeqCst)
    }

    /// Get failover history
    pub async fn history(&self) -> Vec<FailoverHistoryEntry> {
        self.history.read().await.clone()
    }

    /// Take the event receiver
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<FailoverEvent>> {
        self.event_rx.take()
    }

    /// Coordinate transaction replay after failover (TR integration)
    ///
    /// This method orchestrates the replay of in-flight transactions on a new primary
    /// after a failover event. It ensures transaction atomicity by:
    /// 1. Getting all active transactions from the journal that were on the failed node
    /// 2. Waiting for the new primary to catch up to the required LSN
    /// 3. Replaying each transaction's statements on the new primary
    /// 4. Verifying results match the original execution (via checksums)
    #[cfg(feature = "ha-tr")]
    pub async fn coordinate_failover_replay(
        &self,
        journal: &TransactionJournal,
        failed_node: NodeId,
        new_primary_endpoint: &NodeEndpoint,
    ) -> Result<CoordinatedReplayResult> {
        let start = std::time::Instant::now();

        tracing::info!(
            "Starting coordinated replay: failed_node={:?}, new_primary={:?}",
            failed_node,
            new_primary_endpoint.id
        );

        // 1. Get all active transactions that were on the failed node
        let affected_txs = journal.get_transactions_for_node(failed_node).await;

        if affected_txs.is_empty() {
            tracing::info!("No active transactions to replay");
            return Ok(CoordinatedReplayResult {
                total_transactions: 0,
                successful_replays: 0,
                failed_replays: 0,
                transaction_results: vec![],
                duration_ms: start.elapsed().as_millis() as u64,
                new_primary: new_primary_endpoint.id,
            });
        }

        tracing::info!("Found {} active transactions to replay", affected_txs.len());

        // 2. Get the maximum LSN we need to wait for
        let max_lsn = affected_txs.iter().map(|tx| tx.start_lsn).max().unwrap_or(0);

        // 3. Wait for the new primary to catch up to this LSN
        self.wait_for_lsn_catchup(new_primary_endpoint.id, max_lsn).await?;

        // 4. Create replay manager and replay each transaction
        let replay_manager = FailoverReplay::new(ReplayConfig {
            verify_results: true,
            statement_timeout_ms: 30000,
            retry_on_error: true,
            max_retries: 3,
            skip_read_only: false,
            wait_for_wal_sync: false, // Already waited above
            max_wal_lag_bytes: 0,
        });

        let mut transaction_results = Vec::new();
        let mut successful_replays = 0;
        let mut failed_replays = 0;

        for tx_journal in affected_txs {
            let tx_id = tx_journal.tx_id;

            tracing::debug!("Replaying transaction {:?} with {} entries", tx_id, tx_journal.entries.len());

            // Start and execute replay
            match replay_manager.start_replay(tx_journal, new_primary_endpoint.id).await {
                Ok(_) => {
                    match replay_manager.execute_replay(tx_id).await {
                        Ok(result) => {
                            if result.success {
                                successful_replays += 1;
                                tracing::debug!("Transaction {:?} replayed successfully", tx_id);
                            } else {
                                failed_replays += 1;
                                tracing::warn!(
                                    "Transaction {:?} replay failed: {:?}",
                                    tx_id,
                                    result.error
                                );
                            }
                            transaction_results.push(result);
                        }
                        Err(e) => {
                            failed_replays += 1;
                            tracing::error!("Failed to execute replay for {:?}: {}", tx_id, e);
                            transaction_results.push(ReplayResult {
                                tx_id,
                                success: false,
                                statements_replayed: 0,
                                statements_skipped: 0,
                                statements_failed: 0,
                                verification_failures: 0,
                                duration_ms: 0,
                                error: Some(e.to_string()),
                                statement_results: vec![],
                            });
                        }
                    }
                }
                Err(e) => {
                    failed_replays += 1;
                    tracing::error!("Failed to start replay for {:?}: {}", tx_id, e);
                    transaction_results.push(ReplayResult {
                        tx_id,
                        success: false,
                        statements_replayed: 0,
                        statements_skipped: 0,
                        statements_failed: 0,
                        verification_failures: 0,
                        duration_ms: 0,
                        error: Some(e.to_string()),
                        statement_results: vec![],
                    });
                }
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        tracing::info!(
            "Coordinated replay completed: {}/{} successful in {}ms",
            successful_replays,
            successful_replays + failed_replays,
            duration_ms
        );

        Ok(CoordinatedReplayResult {
            total_transactions: successful_replays + failed_replays,
            successful_replays,
            failed_replays,
            transaction_results,
            duration_ms,
            new_primary: new_primary_endpoint.id,
        })
    }

    /// Wait for a node to catch up to a specific LSN
    #[cfg(feature = "ha-tr")]
    async fn wait_for_lsn_catchup(&self, node: NodeId, target_lsn: u64) -> Result<()> {
        if target_lsn == 0 {
            return Ok(());
        }

        tracing::debug!("Waiting for node {:?} to catch up to LSN {}", node, target_lsn);

        // Use configured timeout
        let timeout = self.config.failover_timeout;
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() >= timeout {
                return Err(ProxyError::Timeout(format!(
                    "Timeout waiting for node {:?} to catch up to LSN {}",
                    node, target_lsn
                )));
            }

            // Check if candidate has caught up
            let candidates = self.candidates.read().await;
            if let Some(candidate) = candidates.get(&node) {
                // In a real implementation, we'd query the node's current LSN
                // For now, we check if lag is acceptable
                if candidate.lag_bytes == 0 {
                    tracing::debug!("Node {:?} has caught up", node);
                    return Ok(());
                }
            }
            drop(candidates);

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Result of coordinated transaction replay after failover
#[cfg(feature = "ha-tr")]
#[derive(Debug, Clone)]
pub struct CoordinatedReplayResult {
    /// Total number of transactions replayed
    pub total_transactions: usize,
    /// Number of successful replays
    pub successful_replays: usize,
    /// Number of failed replays
    pub failed_replays: usize,
    /// Per-transaction replay results
    pub transaction_results: Vec<ReplayResult>,
    /// Total duration (ms)
    pub duration_ms: u64,
    /// New primary node ID
    pub new_primary: NodeId,
}

#[cfg(feature = "ha-tr")]
impl CoordinatedReplayResult {
    /// Check if all transactions were replayed successfully
    pub fn all_successful(&self) -> bool {
        self.failed_replays == 0
    }

    /// Get the success rate as a percentage
    pub fn success_rate(&self) -> f64 {
        if self.total_transactions == 0 {
            100.0
        } else {
            (self.successful_replays as f64 / self.total_transactions as f64) * 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = FailoverConfig::default();
        assert!(config.auto_failover);
        assert!(config.prefer_sync_standby);
        assert_eq!(config.max_retries, 3);
    }

    #[tokio::test]
    async fn test_set_get_primary() {
        let controller = FailoverController::new(FailoverConfig::default());
        let node_id = NodeId::new();

        controller.set_primary(node_id).await;
        assert_eq!(controller.get_primary().await, Some(node_id));
    }

    #[tokio::test]
    async fn test_register_candidate() {
        let controller = FailoverController::new(FailoverConfig::default());
        let node_id = NodeId::new();

        let candidate = FailoverCandidate {
            node_id,
            endpoint: NodeEndpoint::new("localhost", 5432).with_role(NodeRole::Standby),
            is_sync: true,
            lag_bytes: 0,
            priority: 1,
            last_heartbeat: None,
        };

        controller.register_candidate(candidate).await;

        let candidates = controller.candidates.read().await;
        assert!(candidates.contains_key(&node_id));
    }

    #[tokio::test]
    async fn test_state_transitions() {
        let controller = FailoverController::new(FailoverConfig::default());

        assert_eq!(controller.state().await, FailoverState::Normal);

        *controller.state.write().await = FailoverState::PrimaryFailed;
        assert_eq!(controller.state().await, FailoverState::PrimaryFailed);
    }

    #[tokio::test]
    async fn test_select_best_candidate() {
        let controller = FailoverController::new(FailoverConfig::default());

        let sync_node = NodeId::new();
        let async_node = NodeId::new();

        controller
            .register_candidate(FailoverCandidate {
                node_id: async_node,
                endpoint: NodeEndpoint::new("async", 5432),
                is_sync: false,
                lag_bytes: 100,
                priority: 1,
                last_heartbeat: None,
            })
            .await;

        controller
            .register_candidate(FailoverCandidate {
                node_id: sync_node,
                endpoint: NodeEndpoint::new("sync", 5432),
                is_sync: true,
                lag_bytes: 50,
                priority: 2,
                last_heartbeat: None,
            })
            .await;

        let best = controller.select_best_candidate().await.unwrap();
        // Sync standby should be preferred
        assert_eq!(best.node_id, sync_node);
    }

    #[cfg(feature = "ha-tr")]
    #[tokio::test]
    async fn test_coordinate_failover_replay_empty() {
        use super::super::transaction_journal::TransactionJournal;

        let controller = FailoverController::new(FailoverConfig::default());
        let journal = TransactionJournal::new();
        let failed_node = NodeId::new();
        let new_primary = NodeEndpoint::new("new-primary", 5432).with_role(NodeRole::Primary);

        // With no transactions, should succeed immediately
        let result = controller
            .coordinate_failover_replay(&journal, failed_node, &new_primary)
            .await
            .unwrap();

        assert_eq!(result.total_transactions, 0);
        assert_eq!(result.successful_replays, 0);
        assert_eq!(result.failed_replays, 0);
        assert!(result.all_successful());
        assert_eq!(result.success_rate(), 100.0);
    }

    #[cfg(feature = "ha-tr")]
    #[tokio::test]
    async fn test_coordinate_failover_replay_with_transactions() {
        use super::super::transaction_journal::{TransactionJournal, JournalEntry, JournalValue, StatementType};
        use uuid::Uuid;

        let controller = FailoverController::new(FailoverConfig::default());
        let journal = TransactionJournal::new();
        let failed_node = NodeId::new();
        let new_primary_id = NodeId::new();
        let new_primary = NodeEndpoint::new("new-primary", 5432)
            .with_role(NodeRole::Primary);

        // Register the new primary as a candidate with zero lag
        controller.register_candidate(FailoverCandidate {
            node_id: new_primary.id,
            endpoint: new_primary.clone(),
            is_sync: true,
            lag_bytes: 0,
            priority: 1,
            last_heartbeat: None,
        }).await;

        // Create a transaction on the failed node
        let tx_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        journal.begin_transaction(tx_id, session_id, failed_node, 100).await.unwrap();
        journal.log_statement(
            tx_id,
            "INSERT INTO users (name) VALUES ('test')".to_string(),
            vec![JournalValue::Text("test".to_string())],
            Some(12345),
            Some(1),
            10,
        ).await.unwrap();

        // Coordinate replay
        let result = controller
            .coordinate_failover_replay(&journal, failed_node, &new_primary)
            .await
            .unwrap();

        assert_eq!(result.total_transactions, 1);
        assert_eq!(result.successful_replays, 1);
        assert_eq!(result.failed_replays, 0);
        assert!(result.all_successful());
    }

    #[cfg(feature = "ha-tr")]
    #[test]
    fn test_coordinated_replay_result_methods() {
        let result = CoordinatedReplayResult {
            total_transactions: 10,
            successful_replays: 8,
            failed_replays: 2,
            transaction_results: vec![],
            duration_ms: 1000,
            new_primary: NodeId::new(),
        };

        assert!(!result.all_successful());
        assert_eq!(result.success_rate(), 80.0);

        let perfect = CoordinatedReplayResult {
            total_transactions: 5,
            successful_replays: 5,
            failed_replays: 0,
            transaction_results: vec![],
            duration_ms: 500,
            new_primary: NodeId::new(),
        };

        assert!(perfect.all_successful());
        assert_eq!(perfect.success_rate(), 100.0);
    }
}
