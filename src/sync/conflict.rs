//! Conflict Detection and Resolution for HeliosDB-Lite v2.3.0
//!
//! Implements comprehensive conflict detection and resolution strategies
//! for multi-replica synchronization scenarios.
//!
//! # Features
//!
//! - Multiple conflict types (Update-Update, Update-Delete, Delete-Update, Insert-Insert)
//! - Multiple resolution strategies (Last-Write-Wins, Vector Clock Causal, Custom)
//! - Deterministic conflict resolution with tie-breaking
//! - Thread-safe conflict logging and reporting
//! - Sub-millisecond conflict detection performance

use super::{RowDelta, RowId, VectorClock};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;
use uuid::Uuid;

/// Conflict types that can occur during synchronization
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ConflictType {
    /// Two replicas updated the same row concurrently
    UpdateUpdate,
    /// One replica updated while another deleted
    UpdateDelete,
    /// One replica deleted while another updated
    DeleteUpdate,
    /// Same row_id inserted on multiple replicas (rare but possible with manual ID assignment)
    InsertInsert,
}

/// Represents a detected conflict between local and remote changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    /// Unique conflict identifier
    pub id: Uuid,
    /// Type of conflict detected
    pub conflict_type: ConflictType,
    /// Table where conflict occurred
    pub table: String,
    /// Row identifier
    pub row_id: RowId,
    /// Local change entry
    pub local_entry: ChangeEntry,
    /// Remote change entry
    pub remote_entry: ChangeEntry,
    /// Local vector clock at time of conflict
    pub local_vector_clock: VectorClock,
    /// Remote vector clock at time of conflict
    pub remote_vector_clock: VectorClock,
    /// When the conflict was detected
    pub detected_at: DateTime<Utc>,
}

/// Represents a change entry (either local or remote)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// The actual data (serialized row)
    pub data: Vec<u8>,
    /// Timestamp of the change
    pub timestamp: DateTime<Utc>,
    /// Node that made the change
    pub node_id: Uuid,
    /// Vector clock at time of change
    pub vector_clock: VectorClock,
    /// Operation type
    pub operation: ChangeOperation,
}

/// Operation type for a change
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChangeOperation {
    Insert,
    Update,
    Delete,
}

/// Conflict resolution report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    /// The conflict that was resolved
    pub conflict: Conflict,
    /// Resolution strategy used
    pub resolution: ConflictResolution,
    /// Winner of the resolution ("local" or "remote")
    pub winner: String,
    /// Explanation of why this resolution was chosen
    pub reason: String,
    /// When the conflict was resolved
    pub resolved_at: DateTime<Utc>,
}

/// Conflict resolution strategies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Use timestamp-based resolution (with node_id tie-breaking)
    LastWriteWins,
    /// Use vector clock causality ordering
    VectorClockCausal,
    /// Require manual intervention
    Manual,
}

/// Conflict detector and resolver
pub struct ConflictDetector {
    /// Resolution strategy to use
    resolution_strategy: ConflictResolution,
    /// Log of all conflicts and their resolutions
    conflict_log: Arc<RwLock<Vec<ConflictReport>>>,
    /// Node ID for tie-breaking
    node_id: Uuid,
}

impl ConflictDetector {
    /// Create a new conflict detector with the specified resolution strategy
    ///
    /// # Arguments
    ///
    /// * `strategy` - The conflict resolution strategy to use
    /// * `node_id` - This node's unique identifier
    ///
    /// # Example
    ///
    /// ```no_run
    /// use heliosdb_lite::sync::conflict::{ConflictDetector, ConflictResolution};
    /// use uuid::Uuid;
    ///
    /// let detector = ConflictDetector::new(
    ///     ConflictResolution::VectorClockCausal,
    ///     Uuid::new_v4()
    /// );
    /// ```
    pub fn new(strategy: ConflictResolution, node_id: Uuid) -> Self {
        Self {
            resolution_strategy: strategy,
            conflict_log: Arc::new(RwLock::new(Vec::new())),
            node_id,
        }
    }

    /// Detect if two change entries conflict with each other
    ///
    /// Two changes conflict if they target the same row and are causally concurrent
    /// according to their vector clocks.
    ///
    /// # Performance
    ///
    /// This operation completes in <1ms for typical vector clock sizes
    ///
    /// # Arguments
    ///
    /// * `table` - Table name
    /// * `row_id` - Row identifier
    /// * `local` - Local change entry
    /// * `remote` - Remote change entry
    ///
    /// # Returns
    ///
    /// `Some(Conflict)` if a conflict is detected, `None` otherwise
    pub fn detect(
        &self,
        table: &str,
        row_id: &RowId,
        local: &ChangeEntry,
        remote: &ChangeEntry,
    ) -> Option<Conflict> {
        // Check if changes are causally concurrent
        if !self.is_concurrent(&local.vector_clock, &remote.vector_clock) {
            return None;
        }

        // Determine conflict type based on operations
        let conflict_type = match (&local.operation, &remote.operation) {
            (ChangeOperation::Update, ChangeOperation::Update) => ConflictType::UpdateUpdate,
            (ChangeOperation::Update, ChangeOperation::Delete) => ConflictType::UpdateDelete,
            (ChangeOperation::Delete, ChangeOperation::Update) => ConflictType::DeleteUpdate,
            (ChangeOperation::Insert, ChangeOperation::Insert) => ConflictType::InsertInsert,
            _ => return None, // No conflict for other combinations
        };

        Some(Conflict {
            id: Uuid::new_v4(),
            conflict_type,
            table: table.to_string(),
            row_id: row_id.clone(),
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        })
    }

    /// Resolve a detected conflict according to the configured strategy
    ///
    /// # Arguments
    ///
    /// * `conflict` - The conflict to resolve
    ///
    /// # Returns
    ///
    /// `Ok(ConflictReport)` with the resolution details, or `Err` if resolution failed
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use heliosdb_lite::sync::conflict::{ConflictDetector, ConflictResolution};
    /// # use uuid::Uuid;
    /// # let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
    /// # let conflict = todo!();
    /// let report = detector.resolve(conflict)?;
    /// println!("Winner: {}", report.winner);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn resolve(&self, conflict: Conflict) -> Result<ConflictReport, ConflictError> {
        let (winner, reason) = match self.resolution_strategy {
            ConflictResolution::LastWriteWins => self.lww_resolve(&conflict),
            ConflictResolution::VectorClockCausal => self.vector_clock_resolve(&conflict),
            ConflictResolution::Manual => {
                return Err(ConflictError::ManualResolutionRequired(conflict.id))
            }
        };

        let report = ConflictReport {
            conflict,
            resolution: self.resolution_strategy.clone(),
            winner,
            reason,
            resolved_at: Utc::now(),
        };

        // Log the resolution
        if let Ok(mut log) = self.conflict_log.write() {
            log.push(report.clone());
        }

        Ok(report)
    }

    /// Apply the resolution to determine which change should be kept
    ///
    /// # Arguments
    ///
    /// * `report` - The conflict report with resolution
    ///
    /// # Returns
    ///
    /// The winning `ChangeEntry` to be applied
    pub fn apply_resolution(&self, report: &ConflictReport) -> Result<ChangeEntry, ConflictError> {
        match report.winner.as_str() {
            "local" => Ok(report.conflict.local_entry.clone()),
            "remote" => Ok(report.conflict.remote_entry.clone()),
            _ => Err(ConflictError::InvalidWinner(report.winner.clone())),
        }
    }

    /// Get all conflicts since a specific time
    ///
    /// # Arguments
    ///
    /// * `since` - Only return conflicts detected after this time
    ///
    /// # Returns
    ///
    /// Vector of conflict reports matching the criteria
    pub fn get_conflicts(&self, since: SystemTime) -> Vec<ConflictReport> {
        let since_datetime = DateTime::<Utc>::from(since);

        match self.conflict_log.read() {
            Ok(log) => log
                .iter()
                .filter(|report| report.conflict.detected_at > since_datetime)
                .cloned()
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Get conflict statistics
    ///
    /// # Returns
    ///
    /// `ConflictStats` with counts and metrics
    pub fn get_stats(&self) -> ConflictStats {
        match self.conflict_log.read() {
            Ok(log) => {
                let total = log.len();
                let local_wins = log.iter().filter(|r| r.winner == "local").count();
                let remote_wins = log.iter().filter(|r| r.winner == "remote").count();

                let mut by_type = std::collections::HashMap::new();
                for report in log.iter() {
                    *by_type.entry(report.conflict.conflict_type.clone()).or_insert(0) += 1;
                }

                ConflictStats {
                    total_conflicts: total,
                    local_wins,
                    remote_wins,
                    by_type,
                }
            }
            Err(_) => ConflictStats::default(),
        }
    }

    /// Clear the conflict log
    ///
    /// Useful for testing or periodic cleanup
    pub fn clear_log(&self) {
        if let Ok(mut log) = self.conflict_log.write() {
            log.clear();
        }
    }

    // Private helper methods

    /// Check if two vector clocks are concurrent (neither happens-before the other)
    fn is_concurrent(&self, local_vc: &VectorClock, remote_vc: &VectorClock) -> bool {
        !local_vc.happens_before(remote_vc) && !remote_vc.happens_before(local_vc)
    }

    /// Last-Write-Wins resolution with deterministic tie-breaking
    ///
    /// Algorithm:
    /// 1. Compare timestamps - later timestamp wins
    /// 2. If timestamps equal, compare node_ids lexicographically
    /// 3. This ensures deterministic resolution across all replicas
    fn lww_resolve(&self, conflict: &Conflict) -> (String, String) {
        let local_ts = conflict.local_entry.timestamp;
        let remote_ts = conflict.remote_entry.timestamp;

        if remote_ts > local_ts {
            (
                "remote".to_string(),
                format!("Remote timestamp ({}) is later than local ({})",
                    remote_ts, local_ts),
            )
        } else if local_ts > remote_ts {
            (
                "local".to_string(),
                format!("Local timestamp ({}) is later than remote ({})",
                    local_ts, remote_ts),
            )
        } else {
            // Timestamps are equal - use node_id for deterministic tie-breaking
            let local_node = conflict.local_entry.node_id;
            let remote_node = conflict.remote_entry.node_id;

            if remote_node > local_node {
                (
                    "remote".to_string(),
                    format!("Timestamp tie - remote node_id ({}) > local node_id ({})",
                        remote_node, local_node),
                )
            } else {
                (
                    "local".to_string(),
                    format!("Timestamp tie - local node_id ({}) >= remote node_id ({})",
                        local_node, remote_node),
                )
            }
        }
    }

    /// Vector clock causal resolution with LWW fallback
    ///
    /// Algorithm:
    /// 1. If remote happens-before local, local wins (local is causally later)
    /// 2. If local happens-before remote, remote wins (remote is causally later)
    /// 3. If concurrent (shouldn't happen if detect() worked), fall back to LWW
    fn vector_clock_resolve(&self, conflict: &Conflict) -> (String, String) {
        let local_vc = &conflict.local_vector_clock;
        let remote_vc = &conflict.remote_vector_clock;

        if remote_vc.happens_before(local_vc) {
            (
                "local".to_string(),
                "Local change is causally later than remote".to_string(),
            )
        } else if local_vc.happens_before(remote_vc) {
            (
                "remote".to_string(),
                "Remote change is causally later than local".to_string(),
            )
        } else {
            // Concurrent - fall back to LWW
            let (winner, _) = self.lww_resolve(conflict);
            (
                winner.clone(),
                format!("Changes are concurrent - falling back to LWW, {} wins", winner),
            )
        }
    }
}

impl Default for ConflictDetector {
    fn default() -> Self {
        Self::new(ConflictResolution::VectorClockCausal, Uuid::new_v4())
    }
}

/// Conflict statistics
#[derive(Debug, Clone, Default)]
pub struct ConflictStats {
    pub total_conflicts: usize,
    pub local_wins: usize,
    pub remote_wins: usize,
    pub by_type: std::collections::HashMap<ConflictType, usize>,
}

/// Conflict detection and resolution errors
#[derive(Debug, thiserror::Error)]
pub enum ConflictError {
    #[error("Manual resolution required for conflict {0}")]
    ManualResolutionRequired(Uuid),

    #[error("Invalid winner specified: {0}")]
    InvalidWinner(String),

    #[error("Failed to log conflict: {0}")]
    LoggingFailed(String),

    #[error("Conflict resolution failed: {0}")]
    ResolutionFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_entry(
        node_id: Uuid,
        timestamp: DateTime<Utc>,
        operation: ChangeOperation,
    ) -> ChangeEntry {
        let mut vc = VectorClock::new();
        vc.increment(node_id);

        ChangeEntry {
            data: vec![1, 2, 3],
            timestamp,
            node_id,
            vector_clock: vc,
            operation,
        }
    }

    #[test]
    fn test_detect_update_update_conflict() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let mut remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        // Make them concurrent
        local.vector_clock.increment(node1);
        remote.vector_clock.increment(node2);

        let conflict = detector.detect("users", &vec![1], &local, &remote);

        assert!(conflict.is_some());
        let conflict = conflict.expect("Expected conflict");
        assert_eq!(conflict.conflict_type, ConflictType::UpdateUpdate);
        assert_eq!(conflict.table, "users");
    }

    #[test]
    fn test_detect_update_delete_conflict() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let mut remote = create_test_entry(node2, Utc::now(), ChangeOperation::Delete);

        local.vector_clock.increment(node1);
        remote.vector_clock.increment(node2);

        let conflict = detector.detect("orders", &vec![42], &local, &remote);

        assert!(conflict.is_some());
        let conflict = conflict.expect("Expected conflict");
        assert_eq!(conflict.conflict_type, ConflictType::UpdateDelete);
    }

    #[test]
    fn test_detect_delete_update_conflict() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut local = create_test_entry(node1, Utc::now(), ChangeOperation::Delete);
        let mut remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        local.vector_clock.increment(node1);
        remote.vector_clock.increment(node2);

        let conflict = detector.detect("products", &vec![99], &local, &remote);

        assert!(conflict.is_some());
        let conflict = conflict.expect("Expected conflict");
        assert_eq!(conflict.conflict_type, ConflictType::DeleteUpdate);
    }

    #[test]
    fn test_detect_insert_insert_conflict() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut local = create_test_entry(node1, Utc::now(), ChangeOperation::Insert);
        let mut remote = create_test_entry(node2, Utc::now(), ChangeOperation::Insert);

        local.vector_clock.increment(node1);
        remote.vector_clock.increment(node2);

        let conflict = detector.detect("items", &vec![7], &local, &remote);

        assert!(conflict.is_some());
        let conflict = conflict.expect("Expected conflict");
        assert_eq!(conflict.conflict_type, ConflictType::InsertInsert);
    }

    #[test]
    fn test_no_conflict_when_not_concurrent() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();

        let mut local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let mut remote = create_test_entry(node1, Utc::now(), ChangeOperation::Update);

        // Make local happen-after remote
        remote.vector_clock.increment(node1);
        local.vector_clock = remote.vector_clock.clone();
        local.vector_clock.increment(node1);

        let conflict = detector.detect("users", &vec![1], &local, &remote);

        assert!(conflict.is_none(), "Should not detect conflict when not concurrent");
    }

    #[test]
    fn test_lww_resolve_later_timestamp() {
        let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let now = Utc::now();
        let later = now + chrono::Duration::seconds(10);

        let local = create_test_entry(node1, now, ChangeOperation::Update);
        let remote = create_test_entry(node2, later, ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = detector.resolve(conflict).expect("Resolution failed");
        assert_eq!(report.winner, "remote", "Remote should win with later timestamp");
    }

    #[test]
    fn test_lww_resolve_earlier_timestamp() {
        let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let now = Utc::now();
        let earlier = now - chrono::Duration::seconds(10);

        let local = create_test_entry(node1, now, ChangeOperation::Update);
        let remote = create_test_entry(node2, earlier, ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = detector.resolve(conflict).expect("Resolution failed");
        assert_eq!(report.winner, "local", "Local should win with later timestamp");
    }

    #[test]
    fn test_lww_resolve_tie_breaking() {
        let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let now = Utc::now();

        let local = create_test_entry(node1, now, ChangeOperation::Update);
        let remote = create_test_entry(node2, now, ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = detector.resolve(conflict).expect("Resolution failed");

        // Winner should be deterministic based on node_id comparison
        if node2 > node1 {
            assert_eq!(report.winner, "remote");
        } else {
            assert_eq!(report.winner, "local");
        }
    }

    #[test]
    fn test_vector_clock_resolve_local_later() {
        let detector = ConflictDetector::new(
            ConflictResolution::VectorClockCausal,
            Uuid::new_v4(),
        );
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let mut local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        // Make local causally after remote
        local.vector_clock.merge(&remote.vector_clock);
        local.vector_clock.increment(node1);

        // For this test, we manually create a conflict even though they're not concurrent
        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = detector.resolve(conflict).expect("Resolution failed");
        assert_eq!(report.winner, "local", "Local should win as it's causally later");
    }

    #[test]
    fn test_vector_clock_resolve_remote_later() {
        let detector = ConflictDetector::new(
            ConflictResolution::VectorClockCausal,
            Uuid::new_v4(),
        );
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let mut remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        // Make remote causally after local
        remote.vector_clock.merge(&local.vector_clock);
        remote.vector_clock.increment(node2);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = detector.resolve(conflict).expect("Resolution failed");
        assert_eq!(report.winner, "remote", "Remote should win as it's causally later");
    }

    #[test]
    fn test_manual_resolution_error() {
        let detector = ConflictDetector::new(ConflictResolution::Manual, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let result = detector.resolve(conflict);
        assert!(result.is_err(), "Manual resolution should return error");
    }

    #[test]
    fn test_apply_resolution_local_winner() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = ConflictReport {
            conflict: conflict.clone(),
            resolution: ConflictResolution::LastWriteWins,
            winner: "local".to_string(),
            reason: "Test".to_string(),
            resolved_at: Utc::now(),
        };

        let result = detector.apply_resolution(&report).expect("Apply failed");
        assert_eq!(result.node_id, local.node_id);
    }

    #[test]
    fn test_apply_resolution_remote_winner() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        let report = ConflictReport {
            conflict: conflict.clone(),
            resolution: ConflictResolution::LastWriteWins,
            winner: "remote".to_string(),
            reason: "Test".to_string(),
            resolved_at: Utc::now(),
        };

        let result = detector.apply_resolution(&report).expect("Apply failed");
        assert_eq!(result.node_id, remote.node_id);
    }

    #[test]
    fn test_get_conflicts_filtering() {
        let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        // Create and resolve a conflict
        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        detector.resolve(conflict).expect("Resolution failed");

        // Get conflicts from before the conflict was created
        let past = SystemTime::now() - std::time::Duration::from_secs(10);
        let conflicts = detector.get_conflicts(past);
        assert_eq!(conflicts.len(), 1);

        // Get conflicts from the future
        let future = SystemTime::now() + std::time::Duration::from_secs(10);
        let conflicts = detector.get_conflicts(future);
        assert_eq!(conflicts.len(), 0);
    }

    #[test]
    fn test_conflict_stats() {
        let detector = ConflictDetector::new(ConflictResolution::LastWriteWins, Uuid::new_v4());
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        // Create multiple conflicts with known outcomes
        for i in 0..5 {
            let now = Utc::now();
            let local = create_test_entry(node1, now, ChangeOperation::Update);
            let remote = create_test_entry(
                node2,
                now + chrono::Duration::seconds(i),
                ChangeOperation::Update,
            );

            let conflict = Conflict {
                id: Uuid::new_v4(),
                conflict_type: ConflictType::UpdateUpdate,
                table: "users".to_string(),
                row_id: vec![i as u8],
                local_entry: local.clone(),
                remote_entry: remote.clone(),
                local_vector_clock: local.vector_clock.clone(),
                remote_vector_clock: remote.vector_clock.clone(),
                detected_at: Utc::now(),
            };

            detector.resolve(conflict).expect("Resolution failed");
        }

        let stats = detector.get_stats();
        assert_eq!(stats.total_conflicts, 5);
        assert!(stats.remote_wins > 0, "Should have some remote wins");
    }

    #[test]
    fn test_deterministic_resolution() {
        // Test that same inputs always produce same output
        let node1 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("Valid UUID");
        let node2 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001")
            .expect("Valid UUID");

        let detector1 = ConflictDetector::new(ConflictResolution::LastWriteWins, node1);
        let detector2 = ConflictDetector::new(ConflictResolution::LastWriteWins, node2);

        let timestamp = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .expect("Valid timestamp")
            .with_timezone(&Utc);

        let local = create_test_entry(node1, timestamp, ChangeOperation::Update);
        let remote = create_test_entry(node2, timestamp, ChangeOperation::Update);

        let conflict1 = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: timestamp,
        };

        let conflict2 = conflict1.clone();

        let report1 = detector1.resolve(conflict1).expect("Resolution failed");
        let report2 = detector2.resolve(conflict2).expect("Resolution failed");

        // Both detectors should resolve to the same winner
        assert_eq!(
            report1.winner, report2.winner,
            "Deterministic resolution failed"
        );
    }

    #[test]
    fn test_clear_log() {
        let detector = ConflictDetector::default();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();

        let local = create_test_entry(node1, Utc::now(), ChangeOperation::Update);
        let remote = create_test_entry(node2, Utc::now(), ChangeOperation::Update);

        let conflict = Conflict {
            id: Uuid::new_v4(),
            conflict_type: ConflictType::UpdateUpdate,
            table: "users".to_string(),
            row_id: vec![1],
            local_entry: local.clone(),
            remote_entry: remote.clone(),
            local_vector_clock: local.vector_clock.clone(),
            remote_vector_clock: remote.vector_clock.clone(),
            detected_at: Utc::now(),
        };

        detector.resolve(conflict).expect("Resolution failed");

        let stats_before = detector.get_stats();
        assert_eq!(stats_before.total_conflicts, 1);

        detector.clear_log();

        let stats_after = detector.get_stats();
        assert_eq!(stats_after.total_conflicts, 0);
    }
}
