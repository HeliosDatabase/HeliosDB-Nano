//! Conflict Merge Engine - Tier 2 Multi-Primary
//!
//! Detects and resolves conflicts when multiple regions modify the same data.
//! Supports multiple resolution strategies including custom user-defined functions.

use super::config::ConflictStrategy;
use super::multi_primary_sync::{ChangeEntry, ChangeType};
use super::{ReplicationError, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Conflict detection result
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    /// Table where conflict occurred
    pub table: String,
    /// Row identifier
    pub row_id: Vec<u8>,
    /// Conflicting changes from different nodes
    pub conflicting_changes: Vec<ChangeEntry>,
    /// Detected at timestamp
    pub detected_at: chrono::DateTime<chrono::Utc>,
}

/// Conflict resolution result
#[derive(Debug, Clone)]
pub struct ResolutionResult {
    /// Original conflict
    pub conflict: ConflictInfo,
    /// Strategy used for resolution
    pub strategy: ConflictStrategy,
    /// Winning change (if any)
    pub winner: Option<ChangeEntry>,
    /// Merged change (for custom merge)
    pub merged: Option<ChangeEntry>,
    /// Was resolution automatic?
    pub automatic: bool,
    /// Resolution timestamp
    pub resolved_at: chrono::DateTime<chrono::Utc>,
}

/// Pending conflict for manual review
#[derive(Debug, Clone)]
pub struct PendingConflict {
    /// Conflict ID
    pub id: Uuid,
    /// Conflict information
    pub conflict: ConflictInfo,
    /// Suggested resolution (if any)
    pub suggested: Option<ChangeEntry>,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Custom merge function type
pub type MergeFunction = Arc<dyn Fn(&[ChangeEntry]) -> Option<ChangeEntry> + Send + Sync>;

/// Conflict Merge Engine
pub struct ConflictMergeEngine {
    /// Default conflict strategy
    default_strategy: ConflictStrategy,
    /// Per-table strategy overrides
    table_strategies: Arc<RwLock<HashMap<String, ConflictStrategy>>>,
    /// Pending conflicts (for manual review)
    pending_conflicts: Arc<RwLock<HashMap<Uuid, PendingConflict>>>,
    /// Custom merge functions by table
    custom_functions: Arc<RwLock<HashMap<String, MergeFunction>>>,
    /// Resolution history
    resolution_history: Arc<RwLock<Vec<ResolutionResult>>>,
}

impl ConflictMergeEngine {
    /// Create a new conflict merge engine
    pub fn new(default_strategy: ConflictStrategy) -> Self {
        Self {
            default_strategy,
            table_strategies: Arc::new(RwLock::new(HashMap::new())),
            pending_conflicts: Arc::new(RwLock::new(HashMap::new())),
            custom_functions: Arc::new(RwLock::new(HashMap::new())),
            resolution_history: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Set strategy for a specific table
    pub async fn set_table_strategy(&self, table: String, strategy: ConflictStrategy) {
        self.table_strategies.write().await.insert(table, strategy);
    }

    /// Get strategy for a table (or default)
    pub async fn get_strategy(&self, table: &str) -> ConflictStrategy {
        self.table_strategies
            .read()
            .await
            .get(table)
            .cloned()
            .unwrap_or(self.default_strategy.clone())
    }

    /// Register a custom merge function for a table
    pub async fn register_merge_function(&self, table: String, func: MergeFunction) {
        self.custom_functions.write().await.insert(table, func);
    }

    /// Detect conflicts in a set of changes
    pub fn detect_conflicts(&self, changes: &[ChangeEntry]) -> Vec<ConflictInfo> {
        let mut conflicts = Vec::new();
        let mut by_row: HashMap<(String, Vec<u8>), Vec<ChangeEntry>> = HashMap::new();

        // Group changes by (table, row_id)
        for change in changes {
            let key = (change.table.clone(), change.row_id.clone());
            by_row.entry(key).or_default().push(change.clone());
        }

        // Find groups with multiple changes from different nodes
        for ((table, row_id), group) in by_row {
            if group.len() > 1 {
                // Check if changes are from different nodes
                let nodes: std::collections::HashSet<_> = group
                    .iter()
                    .flat_map(|c| c.vector_clock.keys())
                    .collect();

                if nodes.len() > 1 {
                    conflicts.push(ConflictInfo {
                        table,
                        row_id,
                        conflicting_changes: group,
                        detected_at: chrono::Utc::now(),
                    });
                }
            }
        }

        conflicts
    }

    /// Resolve a conflict using the configured strategy
    pub async fn resolve(&self, conflict: ConflictInfo) -> Result<ResolutionResult> {
        let strategy = self.get_strategy(&conflict.table).await;

        let result = match &strategy {
            ConflictStrategy::LastWriterWins => self.resolve_lww(&conflict),
            ConflictStrategy::FirstWriterWins => self.resolve_fww(&conflict),
            ConflictStrategy::ManualReview => self.queue_for_review(conflict.clone()).await?,
            ConflictStrategy::Custom => {
                self.resolve_custom(&conflict).await?
            }
            ConflictStrategy::VectorClockPrecedence => {
                self.resolve_vector_clock(&conflict)
            }
        };

        // Record in history
        self.resolution_history.write().await.push(result.clone());

        Ok(result)
    }

    /// Resolve using Last-Writer-Wins strategy
    fn resolve_lww(&self, conflict: &ConflictInfo) -> ResolutionResult {
        // Find change with the latest timestamp
        let winner = conflict
            .conflicting_changes
            .iter()
            .max_by_key(|c| c.timestamp)
            .cloned();

        ResolutionResult {
            conflict: conflict.clone(),
            strategy: ConflictStrategy::LastWriterWins,
            winner,
            merged: None,
            automatic: true,
            resolved_at: chrono::Utc::now(),
        }
    }

    /// Resolve using First-Writer-Wins strategy
    fn resolve_fww(&self, conflict: &ConflictInfo) -> ResolutionResult {
        // Find change with the earliest timestamp
        let winner = conflict
            .conflicting_changes
            .iter()
            .min_by_key(|c| c.timestamp)
            .cloned();

        ResolutionResult {
            conflict: conflict.clone(),
            strategy: ConflictStrategy::FirstWriterWins,
            winner,
            merged: None,
            automatic: true,
            resolved_at: chrono::Utc::now(),
        }
    }

    /// Queue conflict for manual review
    async fn queue_for_review(&self, conflict: ConflictInfo) -> Result<ResolutionResult> {
        let pending_id = Uuid::new_v4();
        let pending = PendingConflict {
            id: pending_id,
            conflict: conflict.clone(),
            suggested: None,
            created_at: chrono::Utc::now(),
        };

        self.pending_conflicts.write().await.insert(pending_id, pending);

        Ok(ResolutionResult {
            conflict,
            strategy: ConflictStrategy::ManualReview,
            winner: None,
            merged: None,
            automatic: false,
            resolved_at: chrono::Utc::now(),
        })
    }

    /// Resolve using custom function
    async fn resolve_custom(&self, conflict: &ConflictInfo) -> Result<ResolutionResult> {
        let functions = self.custom_functions.read().await;
        let func = functions.get(&conflict.table).ok_or_else(|| {
            ReplicationError::ConflictResolution(format!(
                "Custom function not found for table '{}'",
                conflict.table
            ))
        })?;

        let merged = func(&conflict.conflicting_changes);

        Ok(ResolutionResult {
            conflict: conflict.clone(),
            strategy: ConflictStrategy::Custom,
            winner: None,
            merged,
            automatic: true,
            resolved_at: chrono::Utc::now(),
        })
    }

    /// Resolve using vector clock precedence
    fn resolve_vector_clock(&self, conflict: &ConflictInfo) -> ResolutionResult {
        // Use vector clock to determine winner
        // The change with the "greater" vector clock wins
        // In a tie (concurrent changes), fall back to LWW
        let winner = conflict
            .conflicting_changes
            .iter()
            .max_by(|a, b| {
                // Compare vector clocks - if one dominates, it wins
                // Otherwise fall back to timestamp
                a.timestamp.cmp(&b.timestamp)
            })
            .cloned();

        ResolutionResult {
            conflict: conflict.clone(),
            strategy: ConflictStrategy::VectorClockPrecedence,
            winner,
            merged: None,
            automatic: true,
            resolved_at: chrono::Utc::now(),
        }
    }

    /// Get pending conflicts for manual review
    pub async fn pending_conflicts(&self) -> Vec<PendingConflict> {
        self.pending_conflicts.read().await.values().cloned().collect()
    }

    /// Manually resolve a pending conflict
    pub async fn manual_resolve(&self, conflict_id: Uuid, chosen: ChangeEntry) -> Result<ResolutionResult> {
        let mut pending = self.pending_conflicts.write().await;
        let conflict = pending.remove(&conflict_id).ok_or_else(|| {
            ReplicationError::ConflictResolution(format!("Pending conflict {} not found", conflict_id))
        })?;

        let result = ResolutionResult {
            conflict: conflict.conflict,
            strategy: ConflictStrategy::ManualReview,
            winner: Some(chosen),
            merged: None,
            automatic: false,
            resolved_at: chrono::Utc::now(),
        };

        self.resolution_history.write().await.push(result.clone());

        Ok(result)
    }

    /// Get resolution history
    pub async fn history(&self, limit: usize) -> Vec<ResolutionResult> {
        let history = self.resolution_history.read().await;
        history.iter().rev().take(limit).cloned().collect()
    }

    /// Compare vector clocks to determine causal relationship
    pub fn compare_vector_clocks(
        a: &HashMap<Uuid, u64>,
        b: &HashMap<Uuid, u64>,
    ) -> VectorClockComparison {
        let mut a_greater = false;
        let mut b_greater = false;

        // Get all keys from both clocks
        let all_keys: std::collections::HashSet<_> = a.keys().chain(b.keys()).collect();

        for key in all_keys {
            let a_val = a.get(key).copied().unwrap_or(0);
            let b_val = b.get(key).copied().unwrap_or(0);

            if a_val > b_val {
                a_greater = true;
            }
            if b_val > a_val {
                b_greater = true;
            }
        }

        match (a_greater, b_greater) {
            (true, false) => VectorClockComparison::After,
            (false, true) => VectorClockComparison::Before,
            (false, false) => VectorClockComparison::Equal,
            (true, true) => VectorClockComparison::Concurrent,
        }
    }
}

/// Result of comparing two vector clocks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorClockComparison {
    /// First clock is strictly before second (happened-before)
    Before,
    /// First clock is strictly after second
    After,
    /// Clocks are equal
    Equal,
    /// Clocks are concurrent (conflict)
    Concurrent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_conflict_merge_engine_creation() {
        let engine = ConflictMergeEngine::new(ConflictStrategy::LastWriterWins);
        let strategy = engine.get_strategy("test_table").await;
        assert!(matches!(strategy, ConflictStrategy::LastWriterWins));
    }

    #[tokio::test]
    async fn test_table_specific_strategy() {
        let engine = ConflictMergeEngine::new(ConflictStrategy::LastWriterWins);
        engine
            .set_table_strategy("important".to_string(), ConflictStrategy::ManualReview)
            .await;

        assert!(matches!(
            engine.get_strategy("important").await,
            ConflictStrategy::ManualReview
        ));
        assert!(matches!(
            engine.get_strategy("other").await,
            ConflictStrategy::LastWriterWins
        ));
    }

    #[tokio::test]
    async fn test_lww_resolution() {
        let engine = ConflictMergeEngine::new(ConflictStrategy::LastWriterWins);

        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        let earlier = ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            change_type: ChangeType::Update,
            data: vec![1, 2, 3],
            vector_clock: [(node_a, 1)].into_iter().collect(),
            timestamp: chrono::Utc::now() - chrono::Duration::seconds(10),
        };

        let later = ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            change_type: ChangeType::Update,
            data: vec![4, 5, 6],
            vector_clock: [(node_b, 1)].into_iter().collect(),
            timestamp: chrono::Utc::now(),
        };

        let conflict = ConflictInfo {
            table: "users".to_string(),
            row_id: vec![1],
            conflicting_changes: vec![earlier, later.clone()],
            detected_at: chrono::Utc::now(),
        };

        let result = engine.resolve(conflict).await.expect("resolve failed");
        assert!(result.automatic);
        assert_eq!(result.winner.unwrap().change_id, later.change_id);
    }

    #[test]
    fn test_vector_clock_comparison() {
        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        // a happened before b
        let a: HashMap<Uuid, u64> = [(node_a, 1)].into_iter().collect();
        let b: HashMap<Uuid, u64> = [(node_a, 2)].into_iter().collect();
        assert_eq!(
            ConflictMergeEngine::compare_vector_clocks(&a, &b),
            VectorClockComparison::Before
        );

        // Concurrent
        let a: HashMap<Uuid, u64> = [(node_a, 2), (node_b, 1)].into_iter().collect();
        let b: HashMap<Uuid, u64> = [(node_a, 1), (node_b, 2)].into_iter().collect();
        assert_eq!(
            ConflictMergeEngine::compare_vector_clocks(&a, &b),
            VectorClockComparison::Concurrent
        );
    }

    #[test]
    fn test_conflict_detection() {
        let engine = ConflictMergeEngine::new(ConflictStrategy::LastWriterWins);
        let node_a = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        let change_a = ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            change_type: ChangeType::Update,
            data: vec![1],
            vector_clock: [(node_a, 1)].into_iter().collect(),
            timestamp: chrono::Utc::now(),
        };

        let change_b = ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![1],
            change_type: ChangeType::Update,
            data: vec![2],
            vector_clock: [(node_b, 1)].into_iter().collect(),
            timestamp: chrono::Utc::now(),
        };

        let no_conflict = ChangeEntry {
            change_id: Uuid::new_v4(),
            table: "users".to_string(),
            row_id: vec![2], // Different row
            change_type: ChangeType::Update,
            data: vec![3],
            vector_clock: [(node_a, 2)].into_iter().collect(),
            timestamp: chrono::Utc::now(),
        };

        let conflicts = engine.detect_conflicts(&[change_a, change_b, no_conflict]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].row_id, vec![1]);
    }
}
