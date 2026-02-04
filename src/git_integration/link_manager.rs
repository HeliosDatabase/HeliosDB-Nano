//! Git-DB Branch Link Manager
//!
//! Manages the mapping between Git branches and database branches.

use crate::storage::{BranchId, GIT_LINK_PREFIX};
use crate::{Error, Result};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use parking_lot::RwLock;

/// Git branch link record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBranchLink {
    /// Git branch name
    pub git_branch: String,

    /// Database branch ID
    pub db_branch_id: BranchId,

    /// Auto-sync enabled
    pub auto_sync: bool,

    /// Link creation timestamp
    pub created_at: u64,

    /// Last sync timestamp
    pub last_synced_at: Option<u64>,

    /// Last synced commit SHA
    pub last_commit: Option<String>,
}

/// Link Manager - handles Git-DB branch linking
pub struct LinkManager {
    /// RocksDB instance
    db: Arc<DB>,

    /// Current timestamp
    timestamp: Arc<RwLock<u64>>,

    /// Link cache (git_branch -> link)
    cache: RwLock<std::collections::HashMap<String, GitBranchLink>>,
}

impl LinkManager {
    /// Create a new link manager
    pub fn new(db: Arc<DB>, timestamp: Arc<RwLock<u64>>) -> Self {
        Self {
            db,
            timestamp,
            cache: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Encode link key
    fn encode_key(git_branch: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(GIT_LINK_PREFIX);
        key.extend_from_slice(git_branch.as_bytes());
        key
    }

    /// Link a Git branch to a database branch
    pub fn link(&self, git_branch: &str, db_branch_id: BranchId, auto_sync: bool) -> Result<()> {
        let current_ts = *self.timestamp.read();

        let link = GitBranchLink {
            git_branch: git_branch.to_string(),
            db_branch_id,
            auto_sync,
            created_at: current_ts,
            last_synced_at: Some(current_ts),
            last_commit: None,
        };

        // Serialize and store
        let key = Self::encode_key(git_branch);
        let value = bincode::serialize(&link)
            .map_err(|e| Error::storage(format!("Failed to serialize Git link: {}", e)))?;

        self.db.put(&key, &value)
            .map_err(|e| Error::storage(format!("Failed to save Git link: {}", e)))?;

        // Update cache
        self.cache.write().insert(git_branch.to_string(), link);

        tracing::info!("Linked Git branch '{}' to DB branch ID {}", git_branch, db_branch_id);

        Ok(())
    }

    /// Unlink a Git branch
    pub fn unlink(&self, git_branch: &str) -> Result<()> {
        let key = Self::encode_key(git_branch);

        self.db.delete(&key)
            .map_err(|e| Error::storage(format!("Failed to delete Git link: {}", e)))?;

        // Remove from cache
        self.cache.write().remove(git_branch);

        tracing::info!("Unlinked Git branch '{}'", git_branch);

        Ok(())
    }

    /// Get linked database branch for a Git branch
    pub fn get_linked_branch(&self, git_branch: &str) -> Result<Option<BranchId>> {
        // Check cache first
        if let Some(link) = self.cache.read().get(git_branch) {
            return Ok(Some(link.db_branch_id));
        }

        // Load from storage
        let key = Self::encode_key(git_branch);

        match self.db.get(&key) {
            Ok(Some(data)) => {
                let link: GitBranchLink = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize Git link: {}", e)))?;

                let db_branch_id = link.db_branch_id;

                // Update cache
                self.cache.write().insert(git_branch.to_string(), link);

                Ok(Some(db_branch_id))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to load Git link: {}", e))),
        }
    }

    /// Get full link record
    pub fn get_link(&self, git_branch: &str) -> Result<Option<GitBranchLink>> {
        // Check cache first
        if let Some(link) = self.cache.read().get(git_branch).cloned() {
            return Ok(Some(link));
        }

        // Load from storage
        let key = Self::encode_key(git_branch);

        match self.db.get(&key) {
            Ok(Some(data)) => {
                let link: GitBranchLink = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize Git link: {}", e)))?;

                // Update cache
                self.cache.write().insert(git_branch.to_string(), link.clone());

                Ok(Some(link))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to load Git link: {}", e))),
        }
    }

    /// Update last synced commit
    pub fn update_last_commit(&self, git_branch: &str, commit_sha: &str) -> Result<()> {
        if let Some(mut link) = self.get_link(git_branch)? {
            link.last_commit = Some(commit_sha.to_string());
            link.last_synced_at = Some(*self.timestamp.read());

            let key = Self::encode_key(git_branch);
            let value = bincode::serialize(&link)
                .map_err(|e| Error::storage(format!("Failed to serialize Git link: {}", e)))?;

            self.db.put(&key, &value)
                .map_err(|e| Error::storage(format!("Failed to update Git link: {}", e)))?;

            self.cache.write().insert(git_branch.to_string(), link);
        }

        Ok(())
    }

    /// List all links
    pub fn list_all(&self) -> Result<Vec<(String, BranchId)>> {
        let mut links = Vec::new();

        let iter = self.db.prefix_iterator(GIT_LINK_PREFIX);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            // Check if still in prefix
            if !key.starts_with(GIT_LINK_PREFIX) {
                break;
            }

            let link: GitBranchLink = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize link: {}", e)))?;

            links.push((link.git_branch.clone(), link.db_branch_id));

            // Update cache
            self.cache.write().insert(link.git_branch.clone(), link);
        }

        Ok(links)
    }

    /// Find database branch by PR number
    pub fn find_by_pr(&self, provider: &str, pr_number: u64) -> Result<Option<BranchId>> {
        // Scan all links looking for PR match
        for (_, link) in self.cache.read().iter() {
            // Check if git branch name contains PR reference
            let pr_pattern = format!("pr-{}", pr_number);
            if link.git_branch.contains(&pr_pattern) {
                return Ok(Some(link.db_branch_id));
            }
        }

        // Also check storage
        let iter = self.db.prefix_iterator(GIT_LINK_PREFIX);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(GIT_LINK_PREFIX) {
                break;
            }

            let link: GitBranchLink = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize link: {}", e)))?;

            let pr_pattern = format!("pr-{}", pr_number);
            if link.git_branch.contains(&pr_pattern) {
                return Ok(Some(link.db_branch_id));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::storage::StorageEngine;

    #[test]
    fn test_link_manager_basic() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

        let link_manager = LinkManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        );

        // Link a branch
        link_manager.link("feature/test", 2, true).expect("Failed to link");

        // Get linked branch
        let linked = link_manager.get_linked_branch("feature/test")
            .expect("Failed to get linked branch");
        assert_eq!(linked, Some(2));

        // Unlink
        link_manager.unlink("feature/test").expect("Failed to unlink");

        let linked = link_manager.get_linked_branch("feature/test")
            .expect("Failed to get linked branch");
        assert_eq!(linked, None);
    }
}
