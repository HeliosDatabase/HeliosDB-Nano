//! Reshard Manager - Tier 3 Sharding
//!
//! Handles dynamic shard splitting, merging, and rebalancing operations.
//! Ensures minimal disruption during resharding with online migration support.

use super::hash_ring::ShardNode;
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

/// Resharding operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReshardOperation {
    /// Split a shard into multiple shards
    Split,
    /// Merge multiple shards into one
    Merge,
    /// Rebalance data across existing shards
    Rebalance,
    /// Move data between shards
    Move,
}

/// Resharding operation state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReshardState {
    /// Operation planned but not started
    Planned,
    /// Preparing for resharding
    Preparing,
    /// Copying data
    Copying,
    /// Verifying data integrity
    Verifying,
    /// Switching traffic
    Switching,
    /// Cleaning up old data
    Cleanup,
    /// Operation completed
    Completed,
    /// Operation failed
    Failed,
    /// Operation cancelled
    Cancelled,
}

/// Resharding operation plan
#[derive(Debug, Clone)]
pub struct ReshardPlan {
    /// Plan ID
    pub id: Uuid,
    /// Operation type
    pub operation: ReshardOperation,
    /// Source shard(s)
    pub source_shards: Vec<Uuid>,
    /// Target shard(s)
    pub target_shards: Vec<Uuid>,
    /// Tables to reshard (empty = all)
    pub tables: Vec<String>,
    /// Estimated data size to move (bytes)
    pub estimated_size: u64,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Progress of a resharding operation
#[derive(Debug, Clone)]
pub struct ReshardProgress {
    /// Plan ID
    pub plan_id: Uuid,
    /// Current state
    pub state: ReshardState,
    /// Bytes copied so far
    pub bytes_copied: u64,
    /// Total bytes to copy
    pub bytes_total: u64,
    /// Rows copied so far
    pub rows_copied: u64,
    /// Total rows to copy
    pub rows_total: u64,
    /// Current table being processed
    pub current_table: Option<String>,
    /// Start timestamp
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// End timestamp (if completed)
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Error message (if failed)
    pub error: Option<String>,
}

/// Resharding event
#[derive(Debug, Clone)]
pub enum ReshardEvent {
    /// Operation started
    Started { plan_id: Uuid },
    /// Progress update
    Progress { plan_id: Uuid, progress: ReshardProgress },
    /// State changed
    StateChanged { plan_id: Uuid, old: ReshardState, new: ReshardState },
    /// Operation completed
    Completed { plan_id: Uuid },
    /// Operation failed
    Failed { plan_id: Uuid, error: String },
    /// Operation cancelled
    Cancelled { plan_id: Uuid },
}

/// Migration batch for incremental data movement
#[derive(Debug, Clone)]
pub struct MigrationBatch {
    /// Batch ID
    pub id: Uuid,
    /// Source shard
    pub source: Uuid,
    /// Target shard
    pub target: Uuid,
    /// Table name
    pub table: String,
    /// Key range (start, end)
    pub key_range: (Vec<u8>, Vec<u8>),
    /// Rows in this batch
    pub rows: Vec<Vec<u8>>,
    /// Batch checksum
    pub checksum: u32,
}

/// Reshard Manager
pub struct ReshardManager {
    /// Active operations
    active_operations: Arc<RwLock<HashMap<Uuid, ReshardProgress>>>,
    /// Completed operations history
    history: Arc<RwLock<Vec<ReshardProgress>>>,
    /// Event channel sender
    event_tx: mpsc::Sender<ReshardEvent>,
    /// Event channel receiver
    event_rx: Option<mpsc::Receiver<ReshardEvent>>,
    /// Maximum concurrent migrations
    max_concurrent_migrations: usize,
    /// Batch size for data migration
    batch_size: usize,
}

impl ReshardManager {
    /// Create a new reshard manager
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);

        Self {
            active_operations: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(Vec::new())),
            event_tx,
            event_rx: Some(event_rx),
            max_concurrent_migrations: 4,
            batch_size: 10000,
        }
    }

    /// Configure batch size for migrations
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Configure max concurrent migrations
    pub fn with_max_concurrent(mut self, count: usize) -> Self {
        self.max_concurrent_migrations = count;
        self
    }

    /// Plan a shard split operation
    pub async fn plan_split(
        &self,
        source_shard: Uuid,
        target_count: usize,
        target_nodes: Vec<ShardNode>,
    ) -> Result<ReshardPlan> {
        if target_count < 2 {
            return Err(ReplicationError::Resharding(
                "Split requires at least 2 target shards".to_string(),
            ));
        }

        if target_nodes.len() != target_count {
            return Err(ReplicationError::Resharding(format!(
                "Expected {} target nodes, got {}",
                target_count,
                target_nodes.len()
            )));
        }

        let plan = ReshardPlan {
            id: Uuid::new_v4(),
            operation: ReshardOperation::Split,
            source_shards: vec![source_shard],
            target_shards: target_nodes.iter().map(|n| n.id).collect(),
            tables: vec![],
            estimated_size: 0, // TODO: Calculate from source shard
            created_at: chrono::Utc::now(),
        };

        Ok(plan)
    }

    /// Plan a shard merge operation
    pub async fn plan_merge(
        &self,
        source_shards: Vec<Uuid>,
        target_node: ShardNode,
    ) -> Result<ReshardPlan> {
        if source_shards.len() < 2 {
            return Err(ReplicationError::Resharding(
                "Merge requires at least 2 source shards".to_string(),
            ));
        }

        let plan = ReshardPlan {
            id: Uuid::new_v4(),
            operation: ReshardOperation::Merge,
            source_shards,
            target_shards: vec![target_node.id],
            tables: vec![],
            estimated_size: 0, // TODO: Calculate from source shards
            created_at: chrono::Utc::now(),
        };

        Ok(plan)
    }

    /// Plan a rebalance operation
    pub async fn plan_rebalance(&self, shards: Vec<Uuid>) -> Result<ReshardPlan> {
        if shards.is_empty() {
            return Err(ReplicationError::Resharding(
                "Rebalance requires at least one shard".to_string(),
            ));
        }

        let plan = ReshardPlan {
            id: Uuid::new_v4(),
            operation: ReshardOperation::Rebalance,
            source_shards: shards.clone(),
            target_shards: shards,
            tables: vec![],
            estimated_size: 0,
            created_at: chrono::Utc::now(),
        };

        Ok(plan)
    }

    /// Execute a resharding plan
    pub async fn execute(&self, plan: ReshardPlan) -> Result<()> {
        // Check for conflicting operations
        {
            let operations = self.active_operations.read().await;
            for (_, progress) in operations.iter() {
                if progress.state != ReshardState::Completed
                    && progress.state != ReshardState::Failed
                    && progress.state != ReshardState::Cancelled
                {
                    // Check for overlapping shards
                    // For simplicity, just check if any operation is in progress
                    return Err(ReplicationError::Resharding(
                        "Another resharding operation is in progress".to_string(),
                    ));
                }
            }
        }

        // Initialize progress
        let progress = ReshardProgress {
            plan_id: plan.id,
            state: ReshardState::Preparing,
            bytes_copied: 0,
            bytes_total: plan.estimated_size,
            rows_copied: 0,
            rows_total: 0,
            current_table: None,
            started_at: Some(chrono::Utc::now()),
            ended_at: None,
            error: None,
        };

        self.active_operations.write().await.insert(plan.id, progress.clone());

        let _ = self.event_tx.send(ReshardEvent::Started { plan_id: plan.id }).await;

        // TODO: Implement actual resharding logic
        // 1. Preparing: Create target shards, set up replication
        // 2. Copying: Stream data from source to target
        // 3. Verifying: Checksum verification
        // 4. Switching: Update hash ring, redirect traffic
        // 5. Cleanup: Remove old data

        tracing::info!("Started resharding operation {} ({:?})", plan.id, plan.operation);

        Ok(())
    }

    /// Cancel a resharding operation
    pub async fn cancel(&self, plan_id: &Uuid) -> Result<()> {
        let mut operations = self.active_operations.write().await;
        let progress = operations.get_mut(plan_id).ok_or_else(|| {
            ReplicationError::Resharding(format!("Operation {} not found", plan_id))
        })?;

        if progress.state == ReshardState::Completed {
            return Err(ReplicationError::Resharding(
                "Cannot cancel completed operation".to_string(),
            ));
        }

        let old_state = progress.state;
        progress.state = ReshardState::Cancelled;
        progress.ended_at = Some(chrono::Utc::now());

        let _ = self.event_tx.send(ReshardEvent::StateChanged {
            plan_id: *plan_id,
            old: old_state,
            new: ReshardState::Cancelled,
        }).await;

        let _ = self.event_tx.send(ReshardEvent::Cancelled { plan_id: *plan_id }).await;

        Ok(())
    }

    /// Get progress of an operation
    pub async fn get_progress(&self, plan_id: &Uuid) -> Option<ReshardProgress> {
        self.active_operations.read().await.get(plan_id).cloned()
    }

    /// Get all active operations
    pub async fn active_operations(&self) -> Vec<ReshardProgress> {
        self.active_operations
            .read()
            .await
            .values()
            .filter(|p| {
                p.state != ReshardState::Completed
                    && p.state != ReshardState::Failed
                    && p.state != ReshardState::Cancelled
            })
            .cloned()
            .collect()
    }

    /// Get operation history
    pub async fn history(&self, limit: usize) -> Vec<ReshardProgress> {
        self.history.read().await.iter().rev().take(limit).cloned().collect()
    }

    /// Update progress (internal)
    async fn update_progress(&self, plan_id: &Uuid, update: impl FnOnce(&mut ReshardProgress)) -> Result<()> {
        let mut operations = self.active_operations.write().await;
        let progress = operations.get_mut(plan_id).ok_or_else(|| {
            ReplicationError::Resharding(format!("Operation {} not found", plan_id))
        })?;

        update(progress);

        let _ = self.event_tx.send(ReshardEvent::Progress {
            plan_id: *plan_id,
            progress: progress.clone(),
        }).await;

        Ok(())
    }

    /// Mark operation as completed (internal)
    async fn complete_operation(&self, plan_id: &Uuid) -> Result<()> {
        let mut operations = self.active_operations.write().await;
        let progress = operations.remove(plan_id).ok_or_else(|| {
            ReplicationError::Resharding(format!("Operation {} not found", plan_id))
        })?;

        let mut completed = progress;
        completed.state = ReshardState::Completed;
        completed.ended_at = Some(chrono::Utc::now());

        self.history.write().await.push(completed);

        let _ = self.event_tx.send(ReshardEvent::Completed { plan_id: *plan_id }).await;

        Ok(())
    }

    /// Mark operation as failed (internal)
    async fn fail_operation(&self, plan_id: &Uuid, error: String) -> Result<()> {
        let mut operations = self.active_operations.write().await;
        let progress = operations.remove(plan_id).ok_or_else(|| {
            ReplicationError::Resharding(format!("Operation {} not found", plan_id))
        })?;

        let mut failed = progress;
        failed.state = ReshardState::Failed;
        failed.ended_at = Some(chrono::Utc::now());
        failed.error = Some(error.clone());

        self.history.write().await.push(failed);

        let _ = self.event_tx.send(ReshardEvent::Failed {
            plan_id: *plan_id,
            error,
        }).await;

        Ok(())
    }

    /// Take the event receiver
    pub fn take_event_receiver(&mut self) -> Option<mpsc::Receiver<ReshardEvent>> {
        self.event_rx.take()
    }
}

impl Default for ReshardManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_plan_split() {
        let manager = ReshardManager::new();

        let source = Uuid::new_v4();
        let targets = vec![
            ShardNode::new("target1", "localhost", 5433),
            ShardNode::new("target2", "localhost", 5434),
        ];

        let plan = manager.plan_split(source, 2, targets).await.expect("plan failed");
        assert_eq!(plan.operation, ReshardOperation::Split);
        assert_eq!(plan.source_shards.len(), 1);
        assert_eq!(plan.target_shards.len(), 2);
    }

    #[tokio::test]
    async fn test_plan_merge() {
        let manager = ReshardManager::new();

        let sources = vec![Uuid::new_v4(), Uuid::new_v4()];
        let target = ShardNode::new("target", "localhost", 5433);

        let plan = manager.plan_merge(sources, target).await.expect("plan failed");
        assert_eq!(plan.operation, ReshardOperation::Merge);
        assert_eq!(plan.source_shards.len(), 2);
        assert_eq!(plan.target_shards.len(), 1);
    }

    #[tokio::test]
    async fn test_plan_rebalance() {
        let manager = ReshardManager::new();

        let shards = vec![Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];

        let plan = manager.plan_rebalance(shards.clone()).await.expect("plan failed");
        assert_eq!(plan.operation, ReshardOperation::Rebalance);
        assert_eq!(plan.source_shards.len(), 3);
        assert_eq!(plan.target_shards.len(), 3);
    }

    #[tokio::test]
    async fn test_execute_and_cancel() {
        let manager = ReshardManager::new();

        let plan = ReshardPlan {
            id: Uuid::new_v4(),
            operation: ReshardOperation::Rebalance,
            source_shards: vec![Uuid::new_v4()],
            target_shards: vec![Uuid::new_v4()],
            tables: vec![],
            estimated_size: 1000,
            created_at: chrono::Utc::now(),
        };

        manager.execute(plan.clone()).await.expect("execute failed");

        // Should have active operation
        let active = manager.active_operations().await;
        assert_eq!(active.len(), 1);

        // Cancel it
        manager.cancel(&plan.id).await.expect("cancel failed");

        // Should have no active operations
        let active = manager.active_operations().await;
        assert_eq!(active.len(), 0);
    }
}
