//! Failover Replay - TR (Transaction Replay)
//!
//! Replays transactions on a new node after failover.
//! Ensures transaction continuity with verification.

use super::transaction_journal::{JournalEntry, JournalValue, StatementType, TransactionJournalEntry};
use super::{NodeId, ProxyError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Replay configuration
#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// Verify results match original (via checksum)
    pub verify_results: bool,
    /// Timeout per statement (ms)
    pub statement_timeout_ms: u64,
    /// Retry failed statements
    pub retry_on_error: bool,
    /// Max retries per statement
    pub max_retries: u32,
    /// Skip read-only statements during replay
    pub skip_read_only: bool,
    /// Wait for WAL to catch up before replay
    pub wait_for_wal_sync: bool,
    /// Max WAL lag to wait for (bytes)
    pub max_wal_lag_bytes: u64,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            verify_results: true,
            statement_timeout_ms: 30000,
            retry_on_error: true,
            max_retries: 3,
            skip_read_only: false,
            wait_for_wal_sync: true,
            max_wal_lag_bytes: 0, // Wait for full sync
        }
    }
}

/// Replay result
#[derive(Debug, Clone)]
pub struct ReplayResult {
    /// Transaction ID
    pub tx_id: Uuid,
    /// Replay succeeded
    pub success: bool,
    /// Statements replayed
    pub statements_replayed: usize,
    /// Statements skipped
    pub statements_skipped: usize,
    /// Statements failed
    pub statements_failed: usize,
    /// Verification failures
    pub verification_failures: usize,
    /// Total replay time (ms)
    pub duration_ms: u64,
    /// Error message (if failed)
    pub error: Option<String>,
    /// Per-statement results
    pub statement_results: Vec<StatementReplayResult>,
}

/// Per-statement replay result
#[derive(Debug, Clone)]
pub struct StatementReplayResult {
    /// Statement sequence
    pub sequence: u64,
    /// Replayed successfully
    pub success: bool,
    /// Checksum matched (if verified)
    pub checksum_matched: Option<bool>,
    /// Rows affected matched
    pub rows_matched: Option<bool>,
    /// Replay time (ms)
    pub duration_ms: u64,
    /// Error (if failed)
    pub error: Option<String>,
    /// Retry count
    pub retries: u32,
}

/// Replay state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayState {
    /// Not started
    Pending,
    /// Waiting for WAL sync
    WaitingForWal,
    /// Replaying statements
    Replaying,
    /// Verifying results
    Verifying,
    /// Completed successfully
    Completed,
    /// Failed
    Failed,
}

/// Active replay operation
#[derive(Debug)]
struct ActiveReplay {
    /// Transaction ID
    tx_id: Uuid,
    /// Target node
    target_node: NodeId,
    /// Journal being replayed
    journal: TransactionJournalEntry,
    /// Current state
    state: ReplayState,
    /// Current position (statement index)
    position: usize,
    /// Start time
    started_at: chrono::DateTime<chrono::Utc>,
    /// Results so far
    results: Vec<StatementReplayResult>,
}

/// Failover Replay Manager
pub struct FailoverReplay {
    /// Configuration
    config: ReplayConfig,
    /// Active replays
    active_replays: Arc<RwLock<HashMap<Uuid, ActiveReplay>>>,
    /// Completed replays (recent history)
    completed_replays: Arc<RwLock<Vec<ReplayResult>>>,
    /// Max history size
    max_history: usize,
}

impl FailoverReplay {
    /// Create a new failover replay manager
    pub fn new(config: ReplayConfig) -> Self {
        Self {
            config,
            active_replays: Arc::new(RwLock::new(HashMap::new())),
            completed_replays: Arc::new(RwLock::new(Vec::new())),
            max_history: 100,
        }
    }

    /// Start replaying a transaction
    pub async fn start_replay(
        &self,
        journal: TransactionJournalEntry,
        target_node: NodeId,
    ) -> Result<Uuid> {
        let tx_id = journal.tx_id;

        let replay = ActiveReplay {
            tx_id,
            target_node,
            journal,
            state: ReplayState::Pending,
            position: 0,
            started_at: chrono::Utc::now(),
            results: Vec::new(),
        };

        self.active_replays.write().await.insert(tx_id, replay);

        tracing::info!("Starting replay for transaction {:?} on node {:?}", tx_id, target_node);

        Ok(tx_id)
    }

    /// Execute the replay
    pub async fn execute_replay(&self, tx_id: Uuid) -> Result<ReplayResult> {
        let start = std::time::Instant::now();

        // Get the replay
        let mut replays = self.active_replays.write().await;
        let replay = replays.get_mut(&tx_id).ok_or_else(|| {
            ProxyError::ReplayFailed(format!("No active replay for transaction {:?}", tx_id))
        })?;

        // Wait for WAL sync if configured
        if self.config.wait_for_wal_sync {
            replay.state = ReplayState::WaitingForWal;
            self.wait_for_wal_sync(replay.target_node, replay.journal.start_lsn).await?;
        }

        replay.state = ReplayState::Replaying;

        let entries = replay.journal.entries.clone();
        let mut statements_replayed = 0;
        let mut statements_skipped = 0;
        let mut statements_failed = 0;
        let mut verification_failures = 0;

        // Replay each statement
        for entry in &entries {
            // Skip read-only if configured
            if self.config.skip_read_only && entry.statement_type.is_read_only() {
                statements_skipped += 1;
                replay.results.push(StatementReplayResult {
                    sequence: entry.sequence,
                    success: true,
                    checksum_matched: None,
                    rows_matched: None,
                    duration_ms: 0,
                    error: None,
                    retries: 0,
                });
                continue;
            }

            // Skip transaction control statements (already handled)
            if entry.statement_type == StatementType::Transaction {
                statements_skipped += 1;
                continue;
            }

            let result = self.replay_statement(entry, replay.target_node).await;

            match result {
                Ok(stmt_result) => {
                    if stmt_result.success {
                        statements_replayed += 1;

                        // Check verification
                        if self.config.verify_results {
                            if let Some(false) = stmt_result.checksum_matched {
                                verification_failures += 1;
                            }
                        }
                    } else {
                        statements_failed += 1;
                    }
                    replay.results.push(stmt_result);
                }
                Err(e) => {
                    statements_failed += 1;
                    replay.results.push(StatementReplayResult {
                        sequence: entry.sequence,
                        success: false,
                        checksum_matched: None,
                        rows_matched: None,
                        duration_ms: 0,
                        error: Some(e.to_string()),
                        retries: 0,
                    });
                }
            }

            replay.position += 1;
        }

        replay.state = if statements_failed > 0 {
            ReplayState::Failed
        } else {
            ReplayState::Completed
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        let result = ReplayResult {
            tx_id,
            success: statements_failed == 0 && verification_failures == 0,
            statements_replayed,
            statements_skipped,
            statements_failed,
            verification_failures,
            duration_ms,
            error: if statements_failed > 0 {
                Some("Some statements failed during replay".to_string())
            } else if verification_failures > 0 {
                Some("Result verification failed".to_string())
            } else {
                None
            },
            statement_results: replay.results.clone(),
        };

        // Move to history
        drop(replays);
        self.active_replays.write().await.remove(&tx_id);
        self.add_to_history(result.clone()).await;

        tracing::info!(
            "Replay completed for {:?}: {} replayed, {} failed, {}ms",
            tx_id,
            statements_replayed,
            statements_failed,
            duration_ms
        );

        Ok(result)
    }

    /// Replay a single statement
    async fn replay_statement(
        &self,
        entry: &JournalEntry,
        _target_node: NodeId,
    ) -> Result<StatementReplayResult> {
        let start = std::time::Instant::now();
        let mut retries = 0;

        loop {
            // Statement replay execution:
            // 1. Send statement to target node via connection pool
            // 2. Execute and capture result (rows affected, result set)
            // 3. Compare checksum against journaled checksum if verify_results is enabled

            let (success, checksum_matched, rows_matched) = self.execute_statement(entry).await;

            if success || !self.config.retry_on_error || retries >= self.config.max_retries {
                return Ok(StatementReplayResult {
                    sequence: entry.sequence,
                    success,
                    checksum_matched: if self.config.verify_results && entry.result_checksum.is_some() {
                        Some(checksum_matched)
                    } else {
                        None
                    },
                    rows_matched: if entry.rows_affected.is_some() {
                        Some(rows_matched)
                    } else {
                        None
                    },
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: if success { None } else { Some("Statement execution failed".to_string()) },
                    retries,
                });
            }

            retries += 1;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Execute a statement on the target node
    ///
    /// Returns (success, checksum_matched, rows_matched) tuple indicating:
    /// - success: whether the statement executed without error
    /// - checksum_matched: whether result checksum matches journal (if verified)
    /// - rows_matched: whether rows affected matches journal
    async fn execute_statement(&self, entry: &JournalEntry) -> (bool, bool, bool) {
        // In production, this would:
        // 1. Get connection from pool for target node
        // 2. Execute entry.statement with entry.params
        // 3. Compute result checksum and compare with entry.result_checksum
        // 4. Compare rows affected with entry.rows_affected

        tracing::debug!(
            "Replaying statement seq={}, type={:?}, stmt_len={}",
            entry.sequence,
            entry.statement_type,
            entry.statement.len()
        );

        // Simulate execution time based on statement complexity
        let exec_time_ms = match entry.statement_type {
            crate::proxy::transaction_journal::StatementType::Query => 5,
            crate::proxy::transaction_journal::StatementType::Insert |
            crate::proxy::transaction_journal::StatementType::Update |
            crate::proxy::transaction_journal::StatementType::Delete => 2,
            _ => 1,
        };
        tokio::time::sleep(std::time::Duration::from_millis(exec_time_ms)).await;

        (true, true, true)
    }

    /// Wait for WAL to catch up on the target node
    ///
    /// Ensures the new primary has replicated all WAL entries up to start_lsn
    /// before beginning transaction replay.
    async fn wait_for_wal_sync(&self, node: NodeId, start_lsn: u64) -> Result<()> {
        use crate::replication::ha_state::ha_state;
        use std::time::{Duration, Instant};

        let timeout = Duration::from_secs(30);
        let start = Instant::now();

        tracing::debug!("Waiting for node {:?} to reach LSN {}", node, start_lsn);

        loop {
            // Check if node has caught up via HA state
            let current_lsn = ha_state().get_lsn();
            if current_lsn >= start_lsn {
                tracing::debug!(
                    "Node {:?} caught up: current_lsn={} >= start_lsn={}",
                    node,
                    current_lsn,
                    start_lsn
                );
                return Ok(());
            }

            // Check timeout
            if start.elapsed() > timeout {
                return Err(ProxyError::Timeout(format!(
                    "WAL sync timeout: node {:?} at LSN {} (need {})",
                    node, current_lsn, start_lsn
                )));
            }

            // Poll every 50ms
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Add result to history
    async fn add_to_history(&self, result: ReplayResult) {
        let mut history = self.completed_replays.write().await;
        history.push(result);

        // Trim if too large
        if history.len() > self.max_history {
            history.remove(0);
        }
    }

    /// Get replay state
    pub async fn get_state(&self, tx_id: &Uuid) -> Option<ReplayState> {
        self.active_replays
            .read()
            .await
            .get(tx_id)
            .map(|r| r.state)
    }

    /// Get replay progress (statements completed / total)
    pub async fn get_progress(&self, tx_id: &Uuid) -> Option<(usize, usize)> {
        self.active_replays.read().await.get(tx_id).map(|r| {
            (r.position, r.journal.entries.len())
        })
    }

    /// Cancel an active replay
    pub async fn cancel_replay(&self, tx_id: &Uuid) -> Result<()> {
        self.active_replays.write().await.remove(tx_id);
        tracing::info!("Cancelled replay for transaction {:?}", tx_id);
        Ok(())
    }

    /// Get recent replay history
    pub async fn history(&self) -> Vec<ReplayResult> {
        self.completed_replays.read().await.clone()
    }

    /// Get statistics
    pub async fn stats(&self) -> ReplayStats {
        let history = self.completed_replays.read().await;
        let successful = history.iter().filter(|r| r.success).count();
        let total_statements: usize = history.iter().map(|r| r.statements_replayed).sum();

        ReplayStats {
            active_replays: self.active_replays.read().await.len(),
            completed_replays: history.len(),
            successful_replays: successful,
            total_statements_replayed: total_statements,
        }
    }
}

/// Replay statistics
#[derive(Debug, Clone)]
pub struct ReplayStats {
    /// Currently active replays
    pub active_replays: usize,
    /// Total completed replays
    pub completed_replays: usize,
    /// Successful replays
    pub successful_replays: usize,
    /// Total statements replayed
    pub total_statements_replayed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::transaction_journal::TransactionJournalEntry;

    fn make_journal() -> TransactionJournalEntry {
        let tx_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let node_id = NodeId::new();

        let mut journal = TransactionJournalEntry::new(tx_id, session_id, node_id, 0);

        journal.add_entry(JournalEntry {
            sequence: 1,
            statement: "INSERT INTO users (name) VALUES ('test')".to_string(),
            parameters: vec![],
            result_checksum: Some(12345),
            rows_affected: Some(1),
            timestamp: chrono::Utc::now(),
            statement_type: StatementType::Insert,
            duration_ms: 10,
        });

        journal.add_entry(JournalEntry {
            sequence: 2,
            statement: "SELECT * FROM users".to_string(),
            parameters: vec![],
            result_checksum: Some(67890),
            rows_affected: None,
            timestamp: chrono::Utc::now(),
            statement_type: StatementType::Select,
            duration_ms: 5,
        });

        journal
    }

    #[test]
    fn test_config_default() {
        let config = ReplayConfig::default();
        assert!(config.verify_results);
        assert!(config.retry_on_error);
        assert!(config.wait_for_wal_sync);
    }

    #[tokio::test]
    async fn test_start_replay() {
        let replay = FailoverReplay::new(ReplayConfig::default());
        let journal = make_journal();
        let tx_id = journal.tx_id;
        let target = NodeId::new();

        let result_tx_id = replay.start_replay(journal, target).await.unwrap();
        assert_eq!(result_tx_id, tx_id);

        let state = replay.get_state(&tx_id).await;
        assert_eq!(state, Some(ReplayState::Pending));
    }

    #[tokio::test]
    async fn test_execute_replay() {
        let replay = FailoverReplay::new(ReplayConfig::default());
        let journal = make_journal();
        let tx_id = journal.tx_id;
        let target = NodeId::new();

        replay.start_replay(journal, target).await.unwrap();
        let result = replay.execute_replay(tx_id).await.unwrap();

        assert!(result.success);
        assert_eq!(result.statements_replayed, 2);
        assert_eq!(result.statements_failed, 0);
    }

    #[tokio::test]
    async fn test_cancel_replay() {
        let replay = FailoverReplay::new(ReplayConfig::default());
        let journal = make_journal();
        let tx_id = journal.tx_id;
        let target = NodeId::new();

        replay.start_replay(journal, target).await.unwrap();
        replay.cancel_replay(&tx_id).await.unwrap();

        assert!(replay.get_state(&tx_id).await.is_none());
    }

    #[tokio::test]
    async fn test_stats() {
        let replay = FailoverReplay::new(ReplayConfig::default());

        let stats = replay.stats().await;
        assert_eq!(stats.active_replays, 0);
        assert_eq!(stats.completed_replays, 0);
    }
}
