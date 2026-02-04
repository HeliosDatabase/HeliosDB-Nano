//! Git Commit State Tracker
//!
//! Records and restores database state at specific Git commits.
//! Enables `AS OF COMMIT 'sha'` queries and commit-based restoration.

use crate::storage::{BranchId, SnapshotId, GIT_COMMIT_PREFIX};
use crate::{Error, Result};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use parking_lot::RwLock;

/// Commit state - database state at a specific Git commit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitState {
    /// Git commit SHA (full 40-char or abbreviated)
    pub commit_sha: String,

    /// Database branch ID at this commit
    pub db_branch_id: BranchId,

    /// Snapshot ID for time-travel queries
    pub snapshot_id: SnapshotId,

    /// Applied DDL IDs at this commit (for DDL replay)
    pub applied_ddl_ids: Vec<u64>,

    /// Recording timestamp
    pub timestamp: u64,

    /// Parent commit SHA (for chain traversal)
    pub parent_commit: Option<String>,

    /// Commit message (optional, for display)
    pub message: Option<String>,

    /// Author (optional)
    pub author: Option<String>,
}

/// Commit Tracker - records and retrieves database state at Git commits
pub struct CommitTracker {
    /// RocksDB instance
    db: Arc<DB>,

    /// Current timestamp
    timestamp: Arc<RwLock<u64>>,

    /// State cache (commit_sha -> state)
    cache: RwLock<std::collections::HashMap<String, CommitState>>,
}

impl CommitTracker {
    /// Create a new commit tracker
    pub fn new(db: Arc<DB>, timestamp: Arc<RwLock<u64>>) -> Self {
        Self {
            db,
            timestamp,
            cache: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Encode commit key
    fn encode_key(commit_sha: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(GIT_COMMIT_PREFIX);
        key.extend_from_slice(commit_sha.as_bytes());
        key
    }

    /// Record database state at a Git commit
    pub fn record_state(
        &self,
        commit_sha: &str,
        db_branch_id: BranchId,
        snapshot_id: SnapshotId,
    ) -> Result<CommitState> {
        let current_ts = *self.timestamp.read();

        let state = CommitState {
            commit_sha: commit_sha.to_string(),
            db_branch_id,
            snapshot_id,
            applied_ddl_ids: Vec::new(), // Will be populated by DDL versioning
            timestamp: current_ts,
            parent_commit: None, // Can be set separately
            message: None,
            author: None,
        };

        // Serialize and store
        let key = Self::encode_key(commit_sha);
        let value = bincode::serialize(&state)
            .map_err(|e| Error::storage(format!("Failed to serialize commit state: {}", e)))?;

        self.db.put(&key, &value)
            .map_err(|e| Error::storage(format!("Failed to save commit state: {}", e)))?;

        // Update cache
        self.cache.write().insert(commit_sha.to_string(), state.clone());

        tracing::info!(
            "Recorded state for commit {} -> branch {}, snapshot {}",
            commit_sha, db_branch_id, snapshot_id
        );

        Ok(state)
    }

    /// Record state with full commit info
    pub fn record_state_full(
        &self,
        commit_sha: &str,
        db_branch_id: BranchId,
        snapshot_id: SnapshotId,
        parent_commit: Option<String>,
        message: Option<String>,
        author: Option<String>,
        applied_ddl_ids: Vec<u64>,
    ) -> Result<CommitState> {
        let current_ts = *self.timestamp.read();

        let state = CommitState {
            commit_sha: commit_sha.to_string(),
            db_branch_id,
            snapshot_id,
            applied_ddl_ids,
            timestamp: current_ts,
            parent_commit,
            message,
            author,
        };

        let key = Self::encode_key(commit_sha);
        let value = bincode::serialize(&state)
            .map_err(|e| Error::storage(format!("Failed to serialize commit state: {}", e)))?;

        self.db.put(&key, &value)
            .map_err(|e| Error::storage(format!("Failed to save commit state: {}", e)))?;

        self.cache.write().insert(commit_sha.to_string(), state.clone());

        Ok(state)
    }

    /// Get database state at a Git commit
    pub fn get_state(&self, commit_sha: &str) -> Result<Option<CommitState>> {
        // Try abbreviated match first (at least 7 chars)
        let full_sha = self.resolve_commit_sha(commit_sha)?;

        if let Some(sha) = full_sha {
            // Check cache
            if let Some(state) = self.cache.read().get(&sha).cloned() {
                return Ok(Some(state));
            }

            // Load from storage
            let key = Self::encode_key(&sha);

            match self.db.get(&key) {
                Ok(Some(data)) => {
                    let state: CommitState = bincode::deserialize(&data)
                        .map_err(|e| Error::storage(format!("Failed to deserialize commit state: {}", e)))?;

                    self.cache.write().insert(sha, state.clone());
                    Ok(Some(state))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(Error::storage(format!("Failed to load commit state: {}", e))),
            }
        } else {
            Ok(None)
        }
    }

    /// Resolve abbreviated commit SHA to full SHA
    fn resolve_commit_sha(&self, abbreviated: &str) -> Result<Option<String>> {
        // If already looks like a full SHA (40 chars), use directly
        if abbreviated.len() >= 40 {
            return Ok(Some(abbreviated.to_string()));
        }

        // Search for commits matching prefix
        let prefix = {
            let mut p = Vec::new();
            p.extend_from_slice(GIT_COMMIT_PREFIX);
            p.extend_from_slice(abbreviated.as_bytes());
            p
        };

        let iter = self.db.prefix_iterator(&prefix);
        let mut matches = Vec::new();

        for item in iter {
            let (key, _value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(GIT_COMMIT_PREFIX) {
                break;
            }

            // Extract SHA from key
            let sha_bytes = &key[GIT_COMMIT_PREFIX.len()..];
            if let Ok(sha) = std::str::from_utf8(sha_bytes) {
                if sha.starts_with(abbreviated) {
                    matches.push(sha.to_string());
                }
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.into_iter().next().expect("checked length"))),
            _ => Err(Error::storage(format!(
                "Ambiguous commit SHA '{}': {} matches",
                abbreviated, matches.len()
            ))),
        }
    }

    /// Get snapshot ID for a commit (for AS OF COMMIT queries)
    pub fn get_snapshot_for_commit(&self, commit_sha: &str) -> Result<Option<SnapshotId>> {
        if let Some(state) = self.get_state(commit_sha)? {
            Ok(Some(state.snapshot_id))
        } else {
            Ok(None)
        }
    }

    /// Get branch ID for a commit
    pub fn get_branch_for_commit(&self, commit_sha: &str) -> Result<Option<BranchId>> {
        if let Some(state) = self.get_state(commit_sha)? {
            Ok(Some(state.db_branch_id))
        } else {
            Ok(None)
        }
    }

    /// List recent commits (for display)
    pub fn list_recent(&self, limit: usize) -> Result<Vec<CommitState>> {
        let mut states = Vec::new();

        let iter = self.db.prefix_iterator(GIT_COMMIT_PREFIX);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(GIT_COMMIT_PREFIX) {
                break;
            }

            let state: CommitState = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize: {}", e)))?;

            states.push(state);

            if states.len() >= limit * 2 {
                break; // We'll sort and truncate
            }
        }

        // Sort by timestamp descending
        states.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        states.truncate(limit);

        Ok(states)
    }

    /// Delete commit state (for cleanup)
    pub fn delete_state(&self, commit_sha: &str) -> Result<()> {
        let key = Self::encode_key(commit_sha);

        self.db.delete(&key)
            .map_err(|e| Error::storage(format!("Failed to delete commit state: {}", e)))?;

        self.cache.write().remove(commit_sha);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::storage::StorageEngine;

    #[test]
    fn test_commit_tracker_basic() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

        let tracker = CommitTracker::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        );

        // Record state
        let state = tracker.record_state("abc123def456", 1, 100)
            .expect("Failed to record state");

        assert_eq!(state.commit_sha, "abc123def456");
        assert_eq!(state.db_branch_id, 1);
        assert_eq!(state.snapshot_id, 100);

        // Get state
        let retrieved = tracker.get_state("abc123def456")
            .expect("Failed to get state")
            .expect("State should exist");

        assert_eq!(retrieved.commit_sha, "abc123def456");
        assert_eq!(retrieved.db_branch_id, 1);

        // Get by abbreviated SHA
        let by_abbrev = tracker.get_state("abc123")
            .expect("Failed to get by abbreviated SHA");
        assert!(by_abbrev.is_some());
    }

    #[test]
    fn test_snapshot_for_commit() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

        let tracker = CommitTracker::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        );

        tracker.record_state("commit123", 2, 500).expect("Failed to record");

        let snapshot = tracker.get_snapshot_for_commit("commit123")
            .expect("Failed to get snapshot")
            .expect("Snapshot should exist");

        assert_eq!(snapshot, 500);
    }
}
