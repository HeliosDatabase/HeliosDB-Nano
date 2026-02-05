//! Branch storage implementation
//!
//! Implements copy-on-write branch storage for database branching.
//! Enables instant branch creation with minimal storage overhead.

#![allow(unused_variables)]

use super::{Key, Transaction};
use crate::{Error, Result};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use parking_lot::RwLock;

/// Snapshot identifier (timestamp-based versioning)
pub type SnapshotId = u64;

/// Branch identifier (globally unique)
pub type BranchId = u64;

/// Key prefixes for branch metadata
const BRANCH_META_PREFIX: &[u8] = b"branch:meta:";
const BRANCH_REGISTRY_KEY: &[u8] = b"branch:registry";
const BRANCH_CHILDREN_PREFIX: &[u8] = b"branch:children:";
const BRANCH_DATA_PREFIX: &[u8] = b"bdata:";

/// Key prefixes for Git integration
pub const GIT_CONFIG_KEY: &[u8] = b"git:config";
pub const GIT_LINK_PREFIX: &[u8] = b"git:link:";
pub const GIT_COMMIT_PREFIX: &[u8] = b"git:commit:";
pub const GIT_DDL_HISTORY_PREFIX: &[u8] = b"git:ddl:";
pub const GIT_SCHEMA_SNAPSHOT_PREFIX: &[u8] = b"git:schema_snapshot:";
pub const GIT_PR_PREFIX: &[u8] = b"git:pr:";

/// Branch state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchState {
    /// Branch is active
    Active,

    /// Branch has been merged
    Merged {
        /// Target branch ID
        into_branch: BranchId,
        /// Merge timestamp
        at_timestamp: u64,
    },

    /// Branch has been dropped
    Dropped {
        /// Drop timestamp
        at_timestamp: u64,
    },
}

/// Merge strategy for resolving conflicts
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    /// Automatically resolve conflicts (prefer source)
    Auto,

    /// Fail on any conflict
    Manual,

    /// Always prefer source branch changes
    Theirs,

    /// Always prefer target branch changes
    Ours,
}

impl Default for MergeStrategy {
    fn default() -> Self {
        Self::Auto
    }
}

/// Conflict information for a key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflict {
    /// Key that has conflict
    pub key: String,

    /// Value in merge base (common ancestor)
    pub base_value: Option<Vec<u8>>,

    /// Value in source branch
    pub source_value: Option<Vec<u8>>,

    /// Value in target branch
    pub target_value: Option<Vec<u8>>,

    /// Timestamp in source
    pub source_timestamp: u64,

    /// Timestamp in target
    pub target_timestamp: u64,
}

/// Result of merge operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeResult {
    /// Merge commit timestamp
    pub merge_timestamp: u64,

    /// Number of keys merged
    pub merged_keys: usize,

    /// Conflicts detected
    pub conflicts: Vec<MergeConflict>,

    /// Whether merge was completed
    pub completed: bool,
}

/// Git integration metadata for linking database branches to Git branches
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLinkMetadata {
    /// Git branch name (e.g., "feature/user-auth")
    pub git_branch: String,

    /// Last synced Git commit SHA
    pub last_commit: Option<String>,

    /// Auto-sync enabled (switch DB branch on git checkout)
    pub auto_sync: bool,

    /// Git provider (e.g., "github", "gitlab", "generic")
    pub provider: Option<String>,

    /// Associated PR/MR number (for preview branches)
    pub pr_number: Option<u64>,

    /// Repository path (for local detection)
    pub repo_path: Option<String>,

    /// Link creation timestamp
    pub linked_at: u64,
}

impl Default for GitLinkMetadata {
    fn default() -> Self {
        Self {
            git_branch: String::new(),
            last_commit: None,
            auto_sync: true,
            provider: None,
            pr_number: None,
            repo_path: None,
            linked_at: 0,
        }
    }
}

/// Branch creation/merge options
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchOptions {
    /// Replication factor (for distributed mode)
    pub replication_factor: Option<usize>,

    /// Region hint (for distributed mode)
    pub region: Option<String>,

    /// Custom metadata
    pub metadata: HashMap<String, String>,

    /// Git integration link (links this DB branch to a Git branch)
    pub git_link: Option<GitLinkMetadata>,
}

/// Branch statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchStats {
    /// Number of modified keys in this branch
    pub modified_keys: u64,

    /// Approximate storage size (bytes)
    pub storage_bytes: u64,

    /// Number of commits in this branch
    pub commit_count: u64,

    /// Last activity timestamp
    pub last_modified: u64,
}

/// Branch metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchMetadata {
    /// Branch name (user-visible)
    pub name: String,

    /// Unique branch ID
    pub branch_id: BranchId,

    /// Parent branch ID (None for root)
    pub parent_id: Option<BranchId>,

    /// Creation timestamp
    pub created_at: u64,

    /// Snapshot at branch point
    pub created_from_snapshot: SnapshotId,

    /// Current state
    pub state: BranchState,

    /// Last merge base (for three-way merge)
    pub merge_base: Option<SnapshotId>,

    /// Additional metadata
    pub options: BranchOptions,

    /// Statistics
    pub stats: BranchStats,
}

/// Branch registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRegistry {
    /// Map of branch ID -> branch name
    pub branches: HashMap<BranchId, String>,

    /// Main branch ID
    pub main_branch: BranchId,

    /// Next available branch ID
    pub next_branch_id: u64,
}

impl BranchRegistry {
    /// Create a new registry with the main branch
    pub fn new() -> Self {
        let mut branches = HashMap::new();
        branches.insert(1, "main".to_string());

        Self {
            branches,
            main_branch: 1,
            next_branch_id: 2,
        }
    }

    /// Get next branch ID (auto-increment)
    pub fn next_id(&mut self) -> BranchId {
        let id = self.next_branch_id;
        self.next_branch_id += 1;
        id
    }

    /// Add a branch
    pub fn add_branch(&mut self, branch_id: BranchId, name: String) {
        self.branches.insert(branch_id, name);
    }

    /// Remove a branch
    pub fn remove_branch(&mut self, branch_id: BranchId) {
        self.branches.remove(&branch_id);
    }

    /// Check if branch exists
    pub fn contains(&self, branch_id: BranchId) -> bool {
        self.branches.contains_key(&branch_id)
    }

    /// Get branch name
    pub fn get_name(&self, branch_id: BranchId) -> Option<&str> {
        self.branches.get(&branch_id).map(|s| s.as_str())
    }
}

impl Default for BranchRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Branch garbage collection configuration
#[derive(Debug, Clone)]
pub struct BranchGcConfig {
    /// Minimum retention period (seconds) before GC can delete branch data
    pub min_retention_seconds: u64,
    /// Whether to enable automatic GC on branch drop
    pub auto_gc_enabled: bool,
    /// GC mode: Immediate or Deferred
    pub gc_mode: BranchGcMode,
}

impl Default for BranchGcConfig {
    fn default() -> Self {
        Self {
            min_retention_seconds: 300, // 5 minutes
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Deferred,
        }
    }
}

/// Branch garbage collection mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchGcMode {
    /// Immediate deletion (fast but may block)
    Immediate,
    /// Deferred deletion (non-blocking, GC runs later)
    Deferred,
}

/// Branch manager - coordinates all branch operations
pub struct BranchManager {
    /// Storage engine reference
    db: Arc<DB>,

    /// Branch registry (cached)
    registry: Arc<RwLock<BranchRegistry>>,

    /// Branch metadata cache
    metadata_cache: Arc<RwLock<HashMap<BranchId, BranchMetadata>>>,

    /// Current timestamp generator
    timestamp: Arc<RwLock<u64>>,

    /// GC configuration
    gc_config: BranchGcConfig,

    /// Pending GC queue (branch_id -> drop_timestamp)
    pending_gc: Arc<RwLock<HashMap<BranchId, u64>>>,
}

impl BranchManager {
    /// Create new branch manager with default GC configuration
    pub fn new(db: Arc<DB>, timestamp: Arc<RwLock<u64>>) -> Result<Self> {
        Self::with_gc_config(db, timestamp, BranchGcConfig::default())
    }

    /// Create new branch manager with custom GC configuration
    pub fn with_gc_config(
        db: Arc<DB>,
        timestamp: Arc<RwLock<u64>>,
        gc_config: BranchGcConfig,
    ) -> Result<Self> {
        let registry = Self::load_or_create_registry(&db)?;

        // Load pending GC queue from storage
        let pending_gc = Self::load_pending_gc(&db)?;

        Ok(Self {
            db,
            registry: Arc::new(RwLock::new(registry)),
            metadata_cache: Arc::new(RwLock::new(HashMap::new())),
            timestamp,
            gc_config,
            pending_gc: Arc::new(RwLock::new(pending_gc)),
        })
    }

    /// Load registry from storage or create new one
    fn load_or_create_registry(db: &DB) -> Result<BranchRegistry> {
        match db.get(BRANCH_REGISTRY_KEY) {
            Ok(Some(data)) => {
                bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize registry: {}", e)))
            }
            Ok(None) => {
                // Create new registry with main branch
                let registry = BranchRegistry::new();

                // Create main branch metadata
                let main_metadata = BranchMetadata {
                    name: "main".to_string(),
                    branch_id: 1,
                    parent_id: None,
                    created_at: 0,
                    created_from_snapshot: 0,
                    state: BranchState::Active,
                    merge_base: None,
                    options: BranchOptions::default(),
                    stats: BranchStats::default(),
                };

                // Save main branch metadata
                let meta_key = encode_branch_meta_key("main");
                let meta_value = bincode::serialize(&main_metadata)
                    .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
                db.put(&meta_key, &meta_value)
                    .map_err(|e| Error::storage(format!("Failed to save main branch metadata: {}", e)))?;

                // Save registry
                let registry_value = bincode::serialize(&registry)
                    .map_err(|e| Error::storage(format!("Failed to serialize registry: {}", e)))?;
                db.put(BRANCH_REGISTRY_KEY, &registry_value)
                    .map_err(|e| Error::storage(format!("Failed to save registry: {}", e)))?;

                Ok(registry)
            }
            Err(e) => Err(Error::storage(format!("Failed to load registry: {}", e))),
        }
    }

    /// Save registry to storage
    fn save_registry(&self) -> Result<()> {
        let registry = self.registry.read();
        let value = bincode::serialize(&*registry)
            .map_err(|e| Error::storage(format!("Failed to serialize registry: {}", e)))?;
        self.db.put(BRANCH_REGISTRY_KEY, &value)
            .map_err(|e| Error::storage(format!("Failed to save registry: {}", e)))
    }

    /// Load pending GC queue from storage
    fn load_pending_gc(db: &DB) -> Result<HashMap<BranchId, u64>> {
        const PENDING_GC_KEY: &[u8] = b"branch:pending_gc";

        match db.get(PENDING_GC_KEY) {
            Ok(Some(data)) => {
                bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize pending GC queue: {}", e)))
            }
            Ok(None) => Ok(HashMap::new()),
            Err(e) => Err(Error::storage(format!("Failed to load pending GC queue: {}", e))),
        }
    }

    /// Save pending GC queue to storage
    fn save_pending_gc(&self) -> Result<()> {
        const PENDING_GC_KEY: &[u8] = b"branch:pending_gc";

        let pending = self.pending_gc.read();
        let value = bincode::serialize(&*pending)
            .map_err(|e| Error::storage(format!("Failed to serialize pending GC queue: {}", e)))?;
        self.db.put(PENDING_GC_KEY, &value)
            .map_err(|e| Error::storage(format!("Failed to save pending GC queue: {}", e)))
    }

    /// Create a new branch
    pub fn create_branch(
        &self,
        name: &str,
        parent_name: Option<&str>,
        snapshot_id: SnapshotId,
        options: BranchOptions,
    ) -> Result<BranchId> {
        // Validate branch name doesn't already exist
        if self.get_branch_by_name(name).is_ok() {
            return Err(Error::storage(format!("Branch '{}' already exists", name)));
        }

        // Resolve parent branch
        let parent_id = if let Some(parent) = parent_name {
            Some(self.get_branch_by_name(parent)?.branch_id)
        } else {
            // Default to main branch
            Some(self.registry.read().main_branch)
        };

        // Allocate branch ID
        let branch_id = {
            let mut registry = self.registry.write();
            registry.next_id()
        };

        // Get current timestamp
        let current_ts = self.next_timestamp();

        // Create metadata
        let metadata = BranchMetadata {
            name: name.to_string(),
            branch_id,
            parent_id,
            created_at: current_ts,
            created_from_snapshot: snapshot_id,
            state: BranchState::Active,
            merge_base: None,
            options,
            stats: BranchStats::default(),
        };

        // Save metadata
        let meta_key = encode_branch_meta_key(name);
        let meta_value = bincode::serialize(&metadata)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        self.db.put(&meta_key, &meta_value)
            .map_err(|e| Error::storage(format!("Failed to save branch metadata: {}", e)))?;

        // Update registry
        {
            let mut registry = self.registry.write();
            registry.add_branch(branch_id, name.to_string());
        }
        self.save_registry()?;

        // Update parent's children list
        if let Some(parent_id) = parent_id {
            self.add_child_branch(parent_id, branch_id)?;
        }

        // Cache metadata
        self.metadata_cache.write().insert(branch_id, metadata);

        Ok(branch_id)
    }

    /// Drop a branch
    pub fn drop_branch(&self, name: &str, if_exists: bool) -> Result<()> {
        // Get branch metadata
        let metadata = match self.get_branch_by_name(name) {
            Ok(meta) => meta,
            Err(_) if if_exists => return Ok(()),
            Err(e) => return Err(e),
        };

        // Prevent dropping main branch
        if metadata.branch_id == self.registry.read().main_branch {
            return Err(Error::storage("Cannot drop main branch"));
        }

        // Check for child branches
        let children = self.get_child_branches(metadata.branch_id)?;
        if !children.is_empty() {
            return Err(Error::storage(format!(
                "Cannot drop branch '{}': has {} child branch(es)",
                name,
                children.len()
            )));
        }

        // Mark as dropped (soft delete)
        let mut updated_meta = metadata.clone();
        updated_meta.state = BranchState::Dropped {
            at_timestamp: self.current_timestamp(),
        };

        // Save updated metadata
        let meta_key = encode_branch_meta_key(name);
        let meta_value = bincode::serialize(&updated_meta)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        self.db.put(&meta_key, &meta_value)
            .map_err(|e| Error::storage(format!("Failed to save updated metadata: {}", e)))?;

        // Remove from registry
        {
            let mut registry = self.registry.write();
            registry.remove_branch(metadata.branch_id);
        }
        self.save_registry()?;

        // Remove from cache
        self.metadata_cache.write().remove(&metadata.branch_id);

        // Schedule GC for branch data
        self.schedule_branch_gc(metadata.branch_id, metadata.name.clone())?;

        Ok(())
    }

    /// Schedule garbage collection for a deleted branch
    ///
    /// Depending on the GC mode:
    /// - Immediate: Deletes branch data right away (may block)
    /// - Deferred: Adds to GC queue for later cleanup (non-blocking)
    fn schedule_branch_gc(&self, branch_id: BranchId, branch_name: String) -> Result<()> {
        if !self.gc_config.auto_gc_enabled {
            tracing::debug!("Branch GC disabled, skipping cleanup for branch {}", branch_name);
            return Ok(());
        }

        let current_ts = self.current_timestamp();

        match self.gc_config.gc_mode {
            BranchGcMode::Immediate => {
                tracing::info!("Starting immediate GC for branch '{}' (ID: {})", branch_name, branch_id);
                self.gc_branch_data(branch_id)?;
                tracing::info!("Completed immediate GC for branch '{}'", branch_name);
                Ok(())
            }
            BranchGcMode::Deferred => {
                tracing::debug!("Scheduling deferred GC for branch '{}' (ID: {})", branch_name, branch_id);

                // Add to pending GC queue
                self.pending_gc.write().insert(branch_id, current_ts);
                self.save_pending_gc()?;

                // In deferred mode, don't run GC immediately
                // GC will be triggered by explicit run_gc() calls or periodic background tasks

                Ok(())
            }
        }
    }

    /// Garbage collect data for a specific branch
    ///
    /// Deletes all data keys associated with the branch using prefix scan.
    /// This is the core GC operation that frees storage.
    fn gc_branch_data(&self, branch_id: BranchId) -> Result<()> {
        // Build key prefix for this branch: data:{branch_id}:
        let mut prefix = Vec::new();
        prefix.extend_from_slice(BRANCH_DATA_PREFIX);
        prefix.extend_from_slice(&branch_id.to_be_bytes());
        prefix.push(b':');

        let mut delete_count = 0;
        let mut keys_to_delete = Vec::new();

        // Collect all keys with this prefix
        let iter = self.db.prefix_iterator(&prefix);
        for item in iter {
            let (key, _value) = item
                .map_err(|e| Error::storage(format!("GC iterator error: {}", e)))?;

            keys_to_delete.push(key.to_vec());
        }

        // Delete collected keys
        for key in keys_to_delete {
            self.db.delete(&key)
                .map_err(|e| Error::storage(format!("Failed to delete branch data: {}", e)))?;
            delete_count += 1;
        }

        tracing::info!(
            "Branch GC deleted {} data keys for branch ID {}",
            delete_count,
            branch_id
        );

        Ok(())
    }

    /// Garbage collect eligible branches from pending queue
    ///
    /// Processes branches that have been in the queue longer than min_retention_seconds.
    /// Returns the number of branches garbage collected.
    pub fn gc_eligible_branches(&self) -> Result<usize> {
        let current_ts = self.current_timestamp();
        let min_retention = self.gc_config.min_retention_seconds;

        let mut gc_count = 0;
        let mut pending = self.pending_gc.write();
        let mut to_remove = Vec::new();

        // Find branches eligible for GC
        for (branch_id, drop_ts) in pending.iter() {
            let age_seconds = current_ts.saturating_sub(*drop_ts);

            if age_seconds >= min_retention {
                tracing::debug!(
                    "Branch ID {} eligible for GC (age: {}s >= {}s)",
                    branch_id,
                    age_seconds,
                    min_retention
                );
                to_remove.push(*branch_id);
            }
        }

        // Process eligible branches
        for branch_id in to_remove {
            match self.gc_branch_data(branch_id) {
                Ok(()) => {
                    pending.remove(&branch_id);
                    gc_count += 1;
                    tracing::info!("Successfully GC'd branch ID {}", branch_id);
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to GC branch ID {}: {}. Will retry later.",
                        branch_id,
                        e
                    );
                    // Keep in queue for retry
                }
            }
        }

        // Persist updated queue
        drop(pending);
        self.save_pending_gc()?;

        if gc_count > 0 {
            tracing::info!("Branch GC completed: {} branches cleaned up", gc_count);
        }

        Ok(gc_count)
    }

    /// Manually trigger garbage collection for all eligible branches
    ///
    /// This can be called periodically or on-demand to clean up dropped branches.
    /// Returns the number of branches garbage collected.
    pub fn run_gc(&self) -> Result<usize> {
        tracing::info!("Starting manual branch GC run");
        self.gc_eligible_branches()
    }

    /// Get number of branches pending garbage collection
    pub fn pending_gc_count(&self) -> usize {
        self.pending_gc.read().len()
    }

    /// Get GC configuration
    pub fn gc_config(&self) -> &BranchGcConfig {
        &self.gc_config
    }

    /// Get branch metadata by name
    pub fn get_branch_by_name(&self, name: &str) -> Result<BranchMetadata> {
        let meta_key = encode_branch_meta_key(name);
        let data = self.db.get(&meta_key)
            .map_err(|e| Error::storage(format!("Failed to read branch metadata: {}", e)))?
            .ok_or_else(|| Error::storage(format!("Branch '{}' not found", name)))?;

        bincode::deserialize(&data)
            .map_err(|e| Error::storage(format!("Failed to deserialize metadata: {}", e)))
    }

    /// Get branch metadata by ID
    pub fn get_branch_by_id(&self, branch_id: BranchId) -> Result<BranchMetadata> {
        // Check cache first
        if let Some(metadata) = self.metadata_cache.read().get(&branch_id) {
            return Ok(metadata.clone());
        }

        // Get branch name from registry
        let name = self.registry.read()
            .get_name(branch_id)
            .ok_or_else(|| Error::storage(format!("Branch ID {} not found", branch_id)))?
            .to_string();

        // Load metadata
        let metadata = self.get_branch_by_name(&name)?;

        // Cache it
        self.metadata_cache.write().insert(branch_id, metadata.clone());

        Ok(metadata)
    }

    /// List all active branches
    pub fn list_branches(&self) -> Result<Vec<BranchMetadata>> {
        let registry = self.registry.read();
        let mut branches = Vec::new();

        for (branch_id, name) in &registry.branches {
            match self.get_branch_by_name(name) {
                Ok(metadata) if metadata.state == BranchState::Active => {
                    branches.push(metadata);
                }
                _ => continue,
            }
        }

        Ok(branches)
    }

    /// Get branch name by ID
    pub fn get_branch_name(&self, branch_id: BranchId) -> Option<String> {
        let registry = self.registry.read();
        registry.branches.get(&branch_id).cloned()
    }

    /// Add a child branch to parent's children list
    fn add_child_branch(&self, parent_id: BranchId, child_id: BranchId) -> Result<()> {
        let key = encode_branch_children_key(parent_id);

        let mut children: Vec<BranchId> = match self.db.get(&key) {
            Ok(Some(data)) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize children: {}", e)))?,
            Ok(None) => Vec::new(),
            Err(e) => return Err(Error::storage(format!("Failed to read children: {}", e))),
        };

        children.push(child_id);

        let value = bincode::serialize(&children)
            .map_err(|e| Error::storage(format!("Failed to serialize children: {}", e)))?;
        self.db.put(&key, &value)
            .map_err(|e| Error::storage(format!("Failed to save children: {}", e)))
    }

    /// Get child branches
    fn get_child_branches(&self, parent_id: BranchId) -> Result<Vec<BranchId>> {
        let key = encode_branch_children_key(parent_id);

        match self.db.get(&key) {
            Ok(Some(data)) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize children: {}", e))),
            Ok(None) => Ok(Vec::new()),
            Err(e) => Err(Error::storage(format!("Failed to read children: {}", e))),
        }
    }

    /// Get current timestamp
    pub fn current_timestamp(&self) -> u64 {
        *self.timestamp.read()
    }

    /// Get next timestamp (increment)
    fn next_timestamp(&self) -> u64 {
        let mut ts = self.timestamp.write();
        *ts += 1;
        *ts
    }

    /// Merge a source branch into a target branch
    ///
    /// Performs a three-way merge using the common ancestor as the merge base.
    /// Supports multiple merge strategies for conflict resolution.
    pub fn merge_branch(
        &self,
        source_name: &str,
        target_name: &str,
        strategy: MergeStrategy,
    ) -> Result<MergeResult> {
        tracing::info!(
            "Starting merge: {} -> {} (strategy: {:?})",
            source_name,
            target_name,
            strategy
        );

        // Get source and target metadata
        let source = self.get_branch_by_name(source_name)?;
        let target = self.get_branch_by_name(target_name)?;

        // Validate branches are active
        if source.state != BranchState::Active {
            return Err(Error::branch_merge(format!(
                "Source branch '{}' is not active",
                source_name
            )));
        }
        if target.state != BranchState::Active {
            return Err(Error::branch_merge(format!(
                "Target branch '{}' is not active",
                target_name
            )));
        }

        // Find common ancestor (merge base)
        let merge_base = self.find_merge_base(source.branch_id, target.branch_id)?;

        tracing::debug!(
            "Merge base found: snapshot_id = {}",
            merge_base
        );

        // Collect all modified keys from both branches
        let source_keys = self.collect_modified_keys(source.branch_id, merge_base)?;
        let target_keys = self.collect_modified_keys(target.branch_id, merge_base)?;

        // Detect conflicts
        let conflicts = self.detect_conflicts(
            &source_keys,
            &target_keys,
            source.branch_id,
            target.branch_id,
            merge_base,
        )?;

        tracing::info!(
            "Merge analysis: {} source keys, {} target keys, {} conflicts",
            source_keys.len(),
            target_keys.len(),
            conflicts.len()
        );

        // Check if we should fail on conflicts
        if strategy == MergeStrategy::Manual && !conflicts.is_empty() {
            return Ok(MergeResult {
                merge_timestamp: self.current_timestamp(),
                merged_keys: 0,
                conflicts,
                completed: false,
            });
        }

        // Perform merge with selected strategy
        let merge_timestamp = self.next_timestamp();
        let merged_keys = self.apply_merge(
            source.branch_id,
            target.branch_id,
            merge_base,
            &source_keys,
            &target_keys,
            &conflicts,
            strategy,
            merge_timestamp,
        )?;

        // Update source branch metadata to mark as merged
        let mut updated_source = source.clone();
        updated_source.state = BranchState::Merged {
            into_branch: target.branch_id,
            at_timestamp: merge_timestamp,
        };

        let meta_key = encode_branch_meta_key(source_name);
        let meta_value = bincode::serialize(&updated_source)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        self.db.put(&meta_key, &meta_value)
            .map_err(|e| Error::storage(format!("Failed to save merged branch metadata: {}", e)))?;

        // Update cache
        self.metadata_cache.write().insert(source.branch_id, updated_source);

        // Update target's merge_base for future merges
        let mut updated_target = target.clone();
        updated_target.merge_base = Some(merge_timestamp);

        let target_meta_key = encode_branch_meta_key(target_name);
        let target_meta_value = bincode::serialize(&updated_target)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;
        self.db.put(&target_meta_key, &target_meta_value)
            .map_err(|e| Error::storage(format!("Failed to save target branch metadata: {}", e)))?;

        // Update cache
        self.metadata_cache.write().insert(target.branch_id, updated_target);

        tracing::info!(
            "Merge completed: {} keys merged, {} conflicts",
            merged_keys,
            conflicts.len()
        );

        Ok(MergeResult {
            merge_timestamp,
            merged_keys,
            conflicts,
            completed: true,
        })
    }

    /// Find common ancestor (merge base) between two branches
    ///
    /// Uses the snapshot IDs to find the most recent common ancestor
    fn find_merge_base(&self, source_id: BranchId, target_id: BranchId) -> Result<SnapshotId> {
        // Build parent chains for both branches
        let source_chain = self.build_parent_chain(source_id)?;
        let target_chain = self.build_parent_chain(target_id)?;

        // Convert chains to sets for O(1) lookup
        let source_snapshots: HashSet<SnapshotId> = source_chain
            .iter()
            .map(|(_, snapshot)| *snapshot)
            .collect();

        // Find first common snapshot in target chain (most recent ancestor)
        for (_, target_snapshot) in &target_chain {
            if source_snapshots.contains(target_snapshot) {
                return Ok(*target_snapshot);
            }
        }

        // Check if target is ancestor of source or vice versa
        let source_meta = self.get_branch_by_id(source_id)?;
        let target_meta = self.get_branch_by_id(target_id)?;

        // If branches diverged from same snapshot, use that
        if source_meta.created_from_snapshot == target_meta.created_from_snapshot {
            return Ok(source_meta.created_from_snapshot);
        }

        // Fallback: use the earlier creation snapshot
        let merge_base = source_meta.created_from_snapshot.min(target_meta.created_from_snapshot);

        tracing::warn!(
            "No common ancestor found, using earlier snapshot: {}",
            merge_base
        );

        Ok(merge_base)
    }

    /// Collect all modified keys in a branch since merge base
    fn collect_modified_keys(
        &self,
        branch_id: BranchId,
        since_snapshot: SnapshotId,
    ) -> Result<HashSet<String>> {
        let mut keys = HashSet::new();

        // Build key prefix for this branch
        let prefix = {
            let mut p = Vec::new();
            p.extend_from_slice(BRANCH_DATA_PREFIX);
            p.extend_from_slice(&branch_id.to_be_bytes());
            p.push(b':');
            p
        };

        // Scan all keys with this branch prefix
        let iter = self.db.prefix_iterator(&prefix);

        for item in iter {
            let (key, _value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            // Decode the key
            if let Some((_branch_id, user_key)) = decode_branch_data_key(&key) {
                // Include all keys from this branch
                // Note: Without timestamp in key, we can't detect modification time
                // This is acceptable since we're scanning all branch keys
                keys.insert(user_key);
            }
        }

        Ok(keys)
    }

    /// Detect conflicts between source and target branches
    fn detect_conflicts(
        &self,
        source_keys: &HashSet<String>,
        target_keys: &HashSet<String>,
        source_id: BranchId,
        target_id: BranchId,
        merge_base: SnapshotId,
    ) -> Result<Vec<MergeConflict>> {
        let mut conflicts = Vec::new();

        // Find keys that exist in both branches (potential conflicts)
        let common_keys: HashSet<_> = source_keys.intersection(target_keys).collect();

        for key in common_keys {
            // Get values from all three versions
            let base_value = self.get_key_at_snapshot(target_id, key, merge_base)?;
            let source_value = self.get_latest_key_value(source_id, key)?;
            let target_value = self.get_latest_key_value(target_id, key)?;

            // Check if there's an actual conflict (both modified differently)
            let source_modified = source_value != base_value;
            let target_modified = target_value != base_value;

            if source_modified && target_modified && source_value != target_value {
                // Get timestamps
                let source_ts = self.get_key_timestamp(source_id, key)?;
                let target_ts = self.get_key_timestamp(target_id, key)?;

                conflicts.push(MergeConflict {
                    key: key.to_string(),
                    base_value: base_value.map(|v| v.0),
                    source_value: source_value.map(|v| v.0),
                    target_value: target_value.map(|v| v.0),
                    source_timestamp: source_ts,
                    target_timestamp: target_ts,
                });
            }
        }

        Ok(conflicts)
    }

    /// Apply merge with given strategy
    fn apply_merge(
        &self,
        source_id: BranchId,
        target_id: BranchId,
        merge_base: SnapshotId,
        source_keys: &HashSet<String>,
        target_keys: &HashSet<String>,
        conflicts: &[MergeConflict],
        strategy: MergeStrategy,
        merge_timestamp: u64,
    ) -> Result<usize> {
        let mut merged_count = 0;

        // Build conflict set for quick lookup
        let conflict_keys: HashSet<String> = conflicts.iter()
            .map(|c| c.key.clone())
            .collect();

        // Merge keys from source branch
        for key in source_keys {
            // Skip if in target (will be handled by conflict resolution or kept as-is)
            if target_keys.contains(key) {
                // Check if it's a conflict
                if conflict_keys.contains(key) {
                    // Resolve conflict based on strategy
                    match strategy {
                        MergeStrategy::Auto | MergeStrategy::Theirs => {
                            // Use source value
                            self.copy_key_to_branch(source_id, target_id, key, merge_timestamp)?;
                            merged_count += 1;
                        }
                        MergeStrategy::Ours => {
                            // Keep target value (do nothing)
                        }
                        MergeStrategy::Manual => {
                            // Should not reach here (handled earlier)
                        }
                    }
                } else {
                    // Not a conflict, check if source changed from base
                    let base_value = self.get_key_at_snapshot(target_id, key, merge_base)?;
                    let source_value = self.get_latest_key_value(source_id, key)?;

                    if source_value != base_value {
                        // Source modified, target didn't -> use source
                        self.copy_key_to_branch(source_id, target_id, key, merge_timestamp)?;
                        merged_count += 1;
                    }
                }
            } else {
                // Key only in source -> copy to target
                self.copy_key_to_branch(source_id, target_id, key, merge_timestamp)?;
                merged_count += 1;
            }
        }

        Ok(merged_count)
    }

    /// Copy a key from source branch to target branch
    fn copy_key_to_branch(
        &self,
        source_id: BranchId,
        target_id: BranchId,
        key: &str,
        timestamp: u64,
    ) -> Result<()> {
        // Get value from source
        let value = self.get_latest_key_value(source_id, key)?;

        // Encode target key
        let target_key = encode_branch_data_key(target_id, key, timestamp);

        if let Some((data, _ts)) = value {
            // Write to target
            self.db.put(&target_key, &data)
                .map_err(|e| Error::storage(format!("Failed to copy key: {}", e)))?;
        } else {
            // Tombstone (deleted key)
            self.db.delete(&target_key)
                .map_err(|e| Error::storage(format!("Failed to delete key: {}", e)))?;
        }

        Ok(())
    }

    /// Get key value at specific snapshot
    fn get_key_at_snapshot(
        &self,
        branch_id: BranchId,
        key: &str,
        snapshot: SnapshotId,
    ) -> Result<Option<(Vec<u8>, u64)>> {
        let branch_key = encode_branch_data_key(branch_id, key, snapshot);

        match self.db.get(&branch_key) {
            Ok(Some(data)) => Ok(Some((data, snapshot))),
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to read key: {}", e))),
        }
    }

    /// Get latest key value from branch
    fn get_latest_key_value(&self, branch_id: BranchId, key: &str) -> Result<Option<(Vec<u8>, u64)>> {
        // Build prefix for this key in branch
        let mut prefix = Vec::new();
        prefix.extend_from_slice(BRANCH_DATA_PREFIX);
        prefix.extend_from_slice(&branch_id.to_be_bytes());
        prefix.push(b':');
        prefix.extend_from_slice(key.as_bytes());
        prefix.push(b':');

        // Get the current value for this key
        // Since we no longer store timestamps in the key, we just return the value directly
        let mut iter = self.db.prefix_iterator(&prefix);
        if let Some(item) = iter.next() {
            let (_k, v) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            // Return the first (and only) match for this key
            return Ok(Some((v.to_vec(), 0))); // Return 0 as default timestamp since it's not stored
        }

        Ok(None)
    }

    /// Get timestamp of latest key version
    fn get_key_timestamp(&self, branch_id: BranchId, key: &str) -> Result<u64> {
        if let Some((_data, ts)) = self.get_latest_key_value(branch_id, key)? {
            Ok(ts)
        } else {
            Ok(0)
        }
    }

    /// Build parent chain for a branch
    pub fn build_parent_chain(&self, branch_id: BranchId) -> Result<Vec<(BranchId, SnapshotId)>> {
        let mut chain = Vec::new();
        let mut current_id = branch_id;

        loop {
            let metadata = self.get_branch_by_id(current_id)?;

            match metadata.parent_id {
                Some(parent_id) => {
                    chain.push((parent_id, metadata.created_from_snapshot));
                    current_id = parent_id;
                }
                None => break, // Reached root
            }
        }

        Ok(chain)
    }
}

/// Branch-aware transaction
pub struct BranchTransaction {
    /// Underlying transaction
    tx: Transaction,

    /// Branch ID
    branch_id: BranchId,

    /// Branch metadata (cached)
    branch_meta: BranchMetadata,

    /// Parent chain (cached for reads)
    parent_chain: Vec<(BranchId, SnapshotId)>,

    /// Database reference for parent lookups
    db: Arc<DB>,
}

impl BranchTransaction {
    /// Create a new branch transaction
    pub fn new(
        db: Arc<DB>,
        branch_id: BranchId,
        branch_meta: BranchMetadata,
        parent_chain: Vec<(BranchId, SnapshotId)>,
        snapshot_id: SnapshotId,
        snapshot_manager: Arc<super::time_travel::SnapshotManager>,
    ) -> Result<Self> {
        let tx = Transaction::new(Arc::clone(&db), snapshot_id, snapshot_manager)?;

        Ok(Self {
            tx,
            branch_id,
            branch_meta,
            parent_chain,
            db,
        })
    }

    /// Read from branch with parent fallback
    pub fn get(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        // Convert user key to string for branch key encoding
        let user_key = String::from_utf8_lossy(key);

        // Try current branch first
        let branch_key = encode_branch_data_key(
            self.branch_id,
            &user_key,
            self.tx.snapshot_id(),
        );

        // Check if exists in current branch
        if let Some(value) = self.tx.get(&branch_key)? {
            return Ok(Some(value));
        }

        // Walk parent chain
        for (parent_id, _parent_snapshot) in &self.parent_chain {
            // For the main branch (parent_id == 1), use the original key directly
            // For other branches, use the branch-aware key format
            let parent_key = if *parent_id == 1 {
                // Main branch data: use the original key as-is
                key.to_vec()
            } else {
                // Other branch data: bdata:<parent_id>:<user_key>
                encode_branch_data_key(*parent_id, &user_key, 0) // timestamp unused
            };

            if let Some(value) = self.db.get(&parent_key)
                .map_err(|e| Error::storage(format!("Parent read failed: {}", e)))? {
                return Ok(Some(value));
            }
        }

        Ok(None)
    }

    /// Write to branch (copy-on-write)
    pub fn put(&mut self, key: Key, value: Vec<u8>) -> Result<()> {
        let user_key = String::from_utf8_lossy(&key);
        let branch_key = encode_branch_data_key(
            self.branch_id,
            &user_key,
            self.tx.snapshot_id(),
        );

        self.tx.put(branch_key, value)
    }

    /// Delete from branch
    pub fn delete(&mut self, key: Key) -> Result<()> {
        let user_key = String::from_utf8_lossy(&key);
        let branch_key = encode_branch_data_key(
            self.branch_id,
            &user_key,
            self.tx.snapshot_id(),
        );

        self.tx.delete(branch_key)
    }

    /// Commit transaction
    pub fn commit(self) -> Result<()> {
        self.tx.commit()
    }

    /// Rollback transaction
    pub fn rollback(self) -> Result<()> {
        self.tx.rollback()
    }

    /// Get snapshot ID
    pub fn snapshot_id(&self) -> SnapshotId {
        self.tx.snapshot_id()
    }
}

/// Encode a branch metadata key
fn encode_branch_meta_key(branch_name: &str) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(BRANCH_META_PREFIX);
    key.extend_from_slice(branch_name.as_bytes());
    key
}

/// Encode a branch children key
fn encode_branch_children_key(parent_id: BranchId) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(BRANCH_CHILDREN_PREFIX);
    key.extend_from_slice(&parent_id.to_be_bytes());
    key
}

/// Encode a branch data key
///
/// Format: bdata:<branch_id>:<user_key>
/// Note: timestamp parameter is kept for backward compatibility but not used in the key
/// The key must NOT include the timestamp because each transaction has its own snapshot ID,
/// which would make reads from subsequent transactions unable to find previous writes
pub fn encode_branch_data_key(
    branch_id: BranchId,
    user_key: &str,
    _timestamp: u64,
) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(BRANCH_DATA_PREFIX);
    key.extend_from_slice(&branch_id.to_be_bytes());
    key.push(b':');
    key.extend_from_slice(user_key.as_bytes());
    key
}

/// Decode a branch data key
///
/// Returns (branch_id, user_key)
/// Note: timestamp is no longer part of the key
pub fn decode_branch_data_key(key: &[u8]) -> Option<(BranchId, String)> {
    if !key.starts_with(BRANCH_DATA_PREFIX) {
        return None;
    }

    let remaining = key.get(BRANCH_DATA_PREFIX.len()..)?;

    // Parse branch ID (8 bytes)
    if remaining.len() < 8 {
        return None;
    }
    let branch_id = u64::from_be_bytes(remaining.get(..8)?.try_into().ok()?);

    // Find separator
    if remaining.get(8)? != &b':' {
        return None;
    }

    let remaining = remaining.get(9..)?;

    // Remaining is the user key
    let user_key = String::from_utf8(remaining.to_vec()).ok()?;

    Some((branch_id, user_key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::storage::StorageEngine;

    #[test]
    fn test_branch_key_encoding() {
        let key = encode_branch_data_key(1, "users:123", 100);
        let (branch_id, user_key) = decode_branch_data_key(&key).unwrap();

        assert_eq!(branch_id, 1);
        assert_eq!(user_key, "users:123");
    }

    #[test]
    fn test_create_branch_manager() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        );

        assert!(manager.is_ok());
    }

    #[test]
    fn test_create_branch() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        let branch_id = manager.create_branch(
            "dev",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        assert!(branch_id > 1); // main is 1

        // Verify metadata
        let metadata = manager.get_branch_by_name("dev").unwrap();
        assert_eq!(metadata.name, "dev");
        assert_eq!(metadata.branch_id, branch_id);
        assert_eq!(metadata.parent_id, Some(1));
        assert_eq!(metadata.state, BranchState::Active);
    }

    #[test]
    fn test_drop_branch() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create and drop branch
        manager.create_branch(
            "temp",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        manager.drop_branch("temp", false).unwrap();

        // Verify branch is dropped
        let result = manager.get_branch_by_name("temp");
        assert!(result.is_ok()); // Metadata still exists
        let metadata = result.unwrap();
        assert!(matches!(metadata.state, BranchState::Dropped { .. }));
    }

    #[test]
    fn test_cannot_drop_main() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        let result = manager.drop_branch("main", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_drop_with_children() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create parent and child
        manager.create_branch(
            "parent",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        manager.create_branch(
            "child",
            Some("parent"),
            200,
            BranchOptions::default(),
        ).unwrap();

        // Try to drop parent
        let result = manager.drop_branch("parent", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_list_branches() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create branches
        manager.create_branch("dev", Some("main"), 100, BranchOptions::default()).unwrap();
        manager.create_branch("staging", Some("main"), 200, BranchOptions::default()).unwrap();

        let branches = manager.list_branches().unwrap();
        assert_eq!(branches.len(), 3); // main, dev, staging

        let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"dev"));
        assert!(names.contains(&"staging"));
    }

    #[test]
    fn test_parent_chain() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create hierarchy: main -> dev -> feature
        let dev_id = manager.create_branch(
            "dev",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        let feature_id = manager.create_branch(
            "feature",
            Some("dev"),
            200,
            BranchOptions::default(),
        ).unwrap();

        // Build parent chain for feature
        let chain = manager.build_parent_chain(feature_id).unwrap();

        assert_eq!(chain.len(), 2); // dev, main
        assert_eq!(chain[0].0, dev_id);
        assert_eq!(chain[1].0, 1); // main
    }

    #[test]
    fn test_find_merge_base() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create two branches from main
        let dev_id = manager.create_branch(
            "dev",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        let staging_id = manager.create_branch(
            "staging",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Find merge base (should be snapshot 100)
        let merge_base = manager.find_merge_base(dev_id, staging_id).unwrap();
        assert_eq!(merge_base, 100);
    }

    #[test]
    fn test_merge_branch_auto_strategy() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create dev branch from main
        manager.create_branch(
            "dev",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Simulate adding keys to dev branch (would normally come from transactions)
        // For testing, we'll just test the merge infrastructure

        // Perform merge
        let result = manager.merge_branch(
            "dev",
            "main",
            MergeStrategy::Auto,
        ).unwrap();

        assert!(result.completed);
        assert_eq!(result.conflicts.len(), 0);
    }

    #[test]
    fn test_merge_branch_manual_strategy_no_conflicts() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create staging branch from main
        manager.create_branch(
            "staging",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Merge with manual strategy (no conflicts)
        let result = manager.merge_branch(
            "staging",
            "main",
            MergeStrategy::Manual,
        ).unwrap();

        assert!(result.completed);
        assert_eq!(result.conflicts.len(), 0);
    }

    #[test]
    fn test_merge_updates_branch_state() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create and merge feature branch
        manager.create_branch(
            "feature",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        manager.merge_branch(
            "feature",
            "main",
            MergeStrategy::Auto,
        ).unwrap();

        // Verify feature branch is marked as merged
        let feature_meta = manager.get_branch_by_name("feature").unwrap();
        match feature_meta.state {
            BranchState::Merged { into_branch, .. } => {
                assert_eq!(into_branch, 1); // main branch ID
            }
            _ => panic!("Expected branch to be in Merged state"),
        }
    }

    #[test]
    fn test_cannot_merge_inactive_branch() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create and drop a branch
        manager.create_branch(
            "temp",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        manager.drop_branch("temp", false).unwrap();

        // Try to merge dropped branch
        let result = manager.merge_branch(
            "temp",
            "main",
            MergeStrategy::Auto,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_merge_theirs_strategy() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create hotfix branch
        manager.create_branch(
            "hotfix",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Merge with Theirs strategy (prefer source)
        let result = manager.merge_branch(
            "hotfix",
            "main",
            MergeStrategy::Theirs,
        ).unwrap();

        assert!(result.completed);
    }

    #[test]
    fn test_merge_ours_strategy() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let manager = BranchManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).unwrap();

        // Create experimental branch
        manager.create_branch(
            "experimental",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Merge with Ours strategy (prefer target)
        let result = manager.merge_branch(
            "experimental",
            "main",
            MergeStrategy::Ours,
        ).unwrap();

        assert!(result.completed);
    }

    // --- Garbage Collection Tests ---

    #[test]
    fn test_gc_config_default() {
        let config = BranchGcConfig::default();
        assert_eq!(config.min_retention_seconds, 300);
        assert!(config.auto_gc_enabled);
        assert_eq!(config.gc_mode, BranchGcMode::Deferred);
    }

    #[test]
    fn test_branch_gc_immediate_mode() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create manager with immediate GC
        let gc_config = BranchGcConfig {
            min_retention_seconds: 0,
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Immediate,
        };

        let manager = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        // Create and drop a branch
        let branch_id = manager.create_branch(
            "test_gc",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Insert some data for the branch
        let key = encode_branch_data_key(branch_id, "test_key", 100);
        manager.db.put(&key, b"test_value").unwrap();

        // Verify data exists
        assert!(manager.db.get(&key).unwrap().is_some());

        // Drop branch (should trigger immediate GC)
        manager.drop_branch("test_gc", false).unwrap();

        // Verify data was deleted
        assert!(manager.db.get(&key).unwrap().is_none());

        // Verify branch is not in pending GC queue
        assert_eq!(manager.pending_gc_count(), 0);
    }

    #[test]
    fn test_branch_gc_deferred_mode() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create manager with deferred GC (short retention for testing)
        let gc_config = BranchGcConfig {
            min_retention_seconds: 2,
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Deferred,
        };

        let manager = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        // Create and drop a branch
        let branch_id = manager.create_branch(
            "test_deferred",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Insert some data for the branch
        let key = encode_branch_data_key(branch_id, "test_key", 100);
        manager.db.put(&key, b"test_value").unwrap();

        // Drop branch (should add to pending GC)
        manager.drop_branch("test_deferred", false).unwrap();

        // Verify data still exists (not GC'd yet)
        assert!(manager.db.get(&key).unwrap().is_some());

        // Verify branch is in pending GC queue
        assert_eq!(manager.pending_gc_count(), 1);

        // Advance timestamp to make branch eligible for GC
        {
            let mut ts = manager.timestamp.write();
            *ts += 3; // Advance past retention period
        }

        // Run GC manually
        let gc_count = manager.run_gc().unwrap();
        assert_eq!(gc_count, 1);

        // Verify data was deleted
        assert!(manager.db.get(&key).unwrap().is_none());

        // Verify branch is removed from pending GC queue
        assert_eq!(manager.pending_gc_count(), 0);
    }

    #[test]
    fn test_gc_disabled() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create manager with GC disabled
        let gc_config = BranchGcConfig {
            min_retention_seconds: 0,
            auto_gc_enabled: false,
            gc_mode: BranchGcMode::Immediate,
        };

        let manager = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        // Create and drop a branch
        let branch_id = manager.create_branch(
            "test_no_gc",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Insert some data for the branch
        let key = encode_branch_data_key(branch_id, "test_key", 100);
        manager.db.put(&key, b"test_value").unwrap();

        // Drop branch (GC disabled, data should remain)
        manager.drop_branch("test_no_gc", false).unwrap();

        // Verify data still exists
        assert!(manager.db.get(&key).unwrap().is_some());

        // Verify no pending GC
        assert_eq!(manager.pending_gc_count(), 0);
    }

    #[test]
    fn test_gc_multiple_branches() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create manager with deferred GC
        let gc_config = BranchGcConfig {
            min_retention_seconds: 1,
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Deferred,
        };

        let manager = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        // Create and drop multiple branches
        for i in 0..5 {
            let branch_name = format!("branch_{}", i);
            let branch_id = manager.create_branch(
                &branch_name,
                Some("main"),
                100 + i as u64,
                BranchOptions::default(),
            ).unwrap();

            // Add data
            let key = encode_branch_data_key(branch_id, "key", 100 + i as u64);
            manager.db.put(&key, b"value").unwrap();

            // Drop branch
            manager.drop_branch(&branch_name, false).unwrap();
        }

        // Verify all 5 branches are pending GC
        assert_eq!(manager.pending_gc_count(), 5);

        // Advance timestamp
        {
            let mut ts = manager.timestamp.write();
            *ts += 2;
        }

        // Run GC
        let gc_count = manager.run_gc().unwrap();
        assert_eq!(gc_count, 5);

        // Verify queue is empty
        assert_eq!(manager.pending_gc_count(), 0);
    }

    #[test]
    fn test_gc_retention_period() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        // Create manager with longer retention
        let gc_config = BranchGcConfig {
            min_retention_seconds: 10,
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Deferred,
        };

        let manager = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        // Create and drop branch
        let branch_id = manager.create_branch(
            "test_retention",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        let key = encode_branch_data_key(branch_id, "key", 100);
        manager.db.put(&key, b"value").unwrap();

        manager.drop_branch("test_retention", false).unwrap();

        // Advance timestamp but not enough
        {
            let mut ts = manager.timestamp.write();
            *ts += 5; // Only 5 seconds, need 10
        }

        // Run GC - should not delete yet
        let gc_count = manager.run_gc().unwrap();
        assert_eq!(gc_count, 0);

        // Data should still exist
        assert!(manager.db.get(&key).unwrap().is_some());

        // Advance past retention period
        {
            let mut ts = manager.timestamp.write();
            *ts += 6; // Total 11 seconds now
        }

        // Run GC again - should delete now
        let gc_count = manager.run_gc().unwrap();
        assert_eq!(gc_count, 1);

        // Data should be gone
        assert!(manager.db.get(&key).unwrap().is_none());
    }

    #[test]
    fn test_gc_persistence() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let gc_config = BranchGcConfig {
            min_retention_seconds: 100,
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Deferred,
        };

        // Create manager and drop a branch
        {
            let manager = BranchManager::with_gc_config(
                Arc::clone(&engine.db),
                Arc::clone(&engine.timestamp),
                gc_config.clone(),
            ).unwrap();

            manager.create_branch(
                "test_persist",
                Some("main"),
                100,
                BranchOptions::default(),
            ).unwrap();

            manager.drop_branch("test_persist", false).unwrap();

            assert_eq!(manager.pending_gc_count(), 1);
        }

        // Create new manager (simulates restart)
        let manager2 = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        // Verify pending GC was persisted and restored
        assert_eq!(manager2.pending_gc_count(), 1);
    }

    #[test]
    fn test_gc_with_multiple_keys() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).unwrap();

        let gc_config = BranchGcConfig {
            min_retention_seconds: 0,
            auto_gc_enabled: true,
            gc_mode: BranchGcMode::Immediate,
        };

        let manager = BranchManager::with_gc_config(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
            gc_config,
        ).unwrap();

        let branch_id = manager.create_branch(
            "multi_key",
            Some("main"),
            100,
            BranchOptions::default(),
        ).unwrap();

        // Insert multiple keys for the branch
        for i in 0..100 {
            let key = encode_branch_data_key(branch_id, &format!("key_{}", i), 100);
            manager.db.put(&key, b"value").unwrap();
        }

        // Verify all keys exist
        for i in 0..100 {
            let key = encode_branch_data_key(branch_id, &format!("key_{}", i), 100);
            assert!(manager.db.get(&key).unwrap().is_some());
        }

        // Drop branch (immediate GC)
        manager.drop_branch("multi_key", false).unwrap();

        // Verify all keys were deleted
        for i in 0..100 {
            let key = encode_branch_data_key(branch_id, &format!("key_{}", i), 100);
            assert!(manager.db.get(&key).unwrap().is_none());
        }
    }
}
