//! Git Integration Module
//!
//! Provides deep integration between HeliosDB-Lite database branching and Git workflows.
//!
//! ## Features
//!
//! - **Git-DB Branch Linking**: Link database branches to Git branches
//! - **Commit State Tracking**: Record and restore database state at Git commits
//! - **DDL Versioning**: Automatic WAL-based DDL capture + explicit schema snapshots
//! - **Branch Diffing**: Schema and data comparison between branches
//! - **Git Hooks**: Auto-switch DB branch on checkout, validate schema on commit
//! - **Webhooks**: GitHub/GitLab PR lifecycle automation
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     GitIntegrationManager                        │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  LinkManager      │ CommitTracker    │ DdlVersioning            │
//! │  - link/unlink    │ - record state   │ - WAL capture            │
//! │  - sync           │ - restore        │ - schema snapshots       │
//! ├───────────────────┴──────────────────┴──────────────────────────┤
//! │  DiffEngine       │ HookManager      │ WebhookServer            │
//! │  - schema diff    │ - post-checkout  │ - GitHub                 │
//! │  - data diff      │ - pre-commit     │ - GitLab                 │
//! │  - sampled diff   │ - post-merge     │ - Generic                │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

#![allow(unused_variables)]

pub mod config;
pub mod link_manager;
pub mod commit_tracker;
pub mod ddl_versioning;
pub mod diff;
pub mod hooks;
pub mod webhooks;

use crate::storage::{
    BranchId, BranchManager, SnapshotId,
    GIT_LINK_PREFIX, GIT_COMMIT_PREFIX, GIT_CONFIG_KEY,
};
use crate::{Error, Result};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::RwLock;

pub use config::GitConfig;
pub use link_manager::LinkManager;
pub use commit_tracker::{CommitTracker, CommitState};

/// Git repository configuration stored in the database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRepoConfig {
    /// Path to the Git repository root
    pub repo_path: PathBuf,

    /// Database path or connection string
    pub database_path: String,

    /// Default provider (github, gitlab, generic)
    pub default_provider: String,

    /// Auto-sync enabled globally
    pub auto_sync_enabled: bool,

    /// Webhook secret for signature validation
    pub webhook_secret: Option<String>,

    /// Configuration timestamp
    pub configured_at: u64,

    /// Last sync timestamp
    pub last_sync: Option<u64>,
}

impl Default for GitRepoConfig {
    fn default() -> Self {
        Self {
            repo_path: PathBuf::new(),
            database_path: String::new(),
            default_provider: "generic".to_string(),
            auto_sync_enabled: true,
            webhook_secret: None,
            configured_at: 0,
            last_sync: None,
        }
    }
}

/// DDL history entry - automatically captured from WAL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdlHistoryEntry {
    /// Unique DDL ID
    pub ddl_id: u64,

    /// Branch ID where executed
    pub branch_id: BranchId,

    /// WAL LSN (Log Sequence Number)
    pub lsn: u64,

    /// Operation type (CREATE, ALTER, DROP, etc.)
    pub operation: String,

    /// Object type (TABLE, INDEX, VIEW, etc.)
    pub object_type: String,

    /// Object name
    pub object_name: String,

    /// Full DDL statement (validated - executed successfully)
    pub ddl_statement: String,

    /// Execution timestamp
    pub executed_at: u64,

    /// User who executed
    pub executed_by: Option<String>,

    /// Parent DDL ID (for branch tracking)
    pub parent_ddl_id: Option<u64>,

    /// Associated Git commit (if known)
    pub git_commit: Option<String>,
}

/// Schema snapshot - user-triggered full schema capture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// Snapshot name (e.g., "release-1.0")
    pub name: String,

    /// Snapshot ID
    pub snapshot_id: u64,

    /// Branch ID
    pub branch_id: BranchId,

    /// Full schema as DDL statements (CREATE TABLE, CREATE INDEX, etc.)
    pub schema_ddl: Vec<String>,

    /// Creation timestamp
    pub created_at: u64,

    /// User comment
    pub comment: Option<String>,

    /// Associated Git commit
    pub git_commit: Option<String>,

    /// DDL ID at snapshot time (for replay tracking)
    pub last_ddl_id: u64,
}

/// PR/MR preview branch info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrBranchInfo {
    /// Provider (github, gitlab)
    pub provider: String,

    /// PR/MR number
    pub pr_number: u64,

    /// Database branch ID
    pub db_branch_id: BranchId,

    /// Source Git branch
    pub source_branch: String,

    /// Target Git branch
    pub target_branch: String,

    /// PR title
    pub title: Option<String>,

    /// Creation timestamp
    pub created_at: u64,

    /// Last update timestamp
    pub updated_at: u64,

    /// Current status
    pub status: PrStatus,
}

/// PR status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrStatus {
    /// PR is open, preview branch active
    Open,
    /// PR was merged, DB branch merged and cleaned up
    Merged,
    /// PR was closed without merge, preview branch dropped
    Closed,
}

/// Git Integration Manager - main entry point
pub struct GitIntegrationManager {
    /// RocksDB instance
    db: Arc<DB>,

    /// Branch manager reference
    branch_manager: Arc<BranchManager>,

    /// Git repository configuration (cached)
    config: Arc<RwLock<Option<GitRepoConfig>>>,

    /// Link manager for Git-DB branch linking
    link_manager: LinkManager,

    /// Commit tracker for state recording/restoration
    commit_tracker: CommitTracker,

    /// Current timestamp
    timestamp: Arc<RwLock<u64>>,
}

impl GitIntegrationManager {
    /// Create a new Git integration manager
    pub fn new(
        db: Arc<DB>,
        branch_manager: Arc<BranchManager>,
        timestamp: Arc<RwLock<u64>>,
    ) -> Result<Self> {
        // Load existing config if any
        let config = Self::load_config(&db)?;

        let link_manager = LinkManager::new(Arc::clone(&db), Arc::clone(&timestamp));
        let commit_tracker = CommitTracker::new(Arc::clone(&db), Arc::clone(&timestamp));

        Ok(Self {
            db,
            branch_manager,
            config: Arc::new(RwLock::new(config)),
            link_manager,
            commit_tracker,
            timestamp,
        })
    }

    /// Load Git config from storage
    fn load_config(db: &DB) -> Result<Option<GitRepoConfig>> {
        match db.get(GIT_CONFIG_KEY) {
            Ok(Some(data)) => {
                let config: GitRepoConfig = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize Git config: {}", e)))?;
                Ok(Some(config))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to load Git config: {}", e))),
        }
    }

    /// Save Git config to storage
    fn save_config(&self, config: &GitRepoConfig) -> Result<()> {
        let data = bincode::serialize(config)
            .map_err(|e| Error::storage(format!("Failed to serialize Git config: {}", e)))?;
        self.db.put(GIT_CONFIG_KEY, &data)
            .map_err(|e| Error::storage(format!("Failed to save Git config: {}", e)))
    }

    /// Initialize Git integration for a repository
    pub fn init(&self, repo_path: PathBuf, database_path: String) -> Result<()> {
        let current_ts = *self.timestamp.read();

        let config = GitRepoConfig {
            repo_path,
            database_path,
            default_provider: "generic".to_string(),
            auto_sync_enabled: true,
            webhook_secret: None,
            configured_at: current_ts,
            last_sync: None,
        };

        self.save_config(&config)?;
        *self.config.write() = Some(config);

        Ok(())
    }

    /// Check if Git integration is initialized
    pub fn is_initialized(&self) -> bool {
        self.config.read().is_some()
    }

    /// Get current configuration
    pub fn get_config(&self) -> Option<GitRepoConfig> {
        self.config.read().clone()
    }

    /// Link a Git branch to a database branch
    pub fn link_branch(
        &self,
        git_branch: &str,
        db_branch_name: &str,
        auto_sync: bool,
    ) -> Result<()> {
        // Get database branch metadata
        let db_branch = self.branch_manager.get_branch_by_name(db_branch_name)?;

        self.link_manager.link(git_branch, db_branch.branch_id, auto_sync)
    }

    /// Unlink a Git branch from its database branch
    pub fn unlink_branch(&self, git_branch: &str) -> Result<()> {
        self.link_manager.unlink(git_branch)
    }

    /// Get linked database branch for a Git branch
    pub fn get_linked_branch(&self, git_branch: &str) -> Result<Option<BranchId>> {
        self.link_manager.get_linked_branch(git_branch)
    }

    /// Record database state at a Git commit
    pub fn record_commit_state(
        &self,
        commit_sha: &str,
        db_branch_id: BranchId,
        snapshot_id: SnapshotId,
    ) -> Result<CommitState> {
        self.commit_tracker.record_state(commit_sha, db_branch_id, snapshot_id)
    }

    /// Get database state at a Git commit
    pub fn get_commit_state(&self, commit_sha: &str) -> Result<Option<CommitState>> {
        self.commit_tracker.get_state(commit_sha)
    }

    /// Sync database state with current Git branch
    pub fn sync(&self, git_branch: &str) -> Result<Option<BranchId>> {
        // Get linked database branch
        if let Some(db_branch_id) = self.link_manager.get_linked_branch(git_branch)? {
            // Update last sync timestamp
            if let Some(mut config) = self.config.write().take() {
                config.last_sync = Some(*self.timestamp.read());
                self.save_config(&config)?;
                *self.config.write() = Some(config);
            }

            Ok(Some(db_branch_id))
        } else {
            Ok(None)
        }
    }

    /// List all Git-DB branch links
    pub fn list_links(&self) -> Result<Vec<(String, BranchId)>> {
        self.link_manager.list_all()
    }

    /// Get link status
    pub fn get_status(&self) -> Result<GitIntegrationStatus> {
        let config = self.config.read().clone();
        let links = self.link_manager.list_all()?;

        Ok(GitIntegrationStatus {
            initialized: config.is_some(),
            config,
            linked_branches: links.len(),
            links,
        })
    }
}

/// Git integration status
#[derive(Debug, Clone)]
pub struct GitIntegrationStatus {
    /// Whether Git integration is initialized
    pub initialized: bool,

    /// Current configuration
    pub config: Option<GitRepoConfig>,

    /// Number of linked branches
    pub linked_branches: usize,

    /// All links (git_branch, db_branch_id)
    pub links: Vec<(String, BranchId)>,
}
