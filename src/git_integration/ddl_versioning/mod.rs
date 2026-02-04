//! DDL Versioning Module
//!
//! Implements hybrid DDL versioning:
//! 1. WAL-based DDL History: Automatic capture of all DDL operations
//! 2. Schema Snapshots: User-triggered full schema captures
//!
//! ## Architecture
//!
//! ```text
//! WAL Stream → DDL Filter → helios_ddl_history table
//!                              ↓
//! User Command → CREATE SCHEMA SNAPSHOT → schema_snapshots
//!                              ↓
//! Branch Merge → Choose Strategy:
//!   - DDL REPLAY: Replay source DDL on target
//!   - SCHEMA DIFF: Generate minimal DDL from schema comparison
//! ```

#![allow(unused_variables)]
#![allow(dead_code)]

use crate::storage::{BranchId, SnapshotId, GIT_DDL_HISTORY_PREFIX, GIT_SCHEMA_SNAPSHOT_PREFIX};
use crate::{Error, Result};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use parking_lot::RwLock;

/// DDL operation type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DdlOperation {
    Create,
    Alter,
    Drop,
    Truncate,
    Rename,
    Comment,
}

impl std::fmt::Display for DdlOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create => write!(f, "CREATE"),
            Self::Alter => write!(f, "ALTER"),
            Self::Drop => write!(f, "DROP"),
            Self::Truncate => write!(f, "TRUNCATE"),
            Self::Rename => write!(f, "RENAME"),
            Self::Comment => write!(f, "COMMENT"),
        }
    }
}

/// DDL object type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DdlObjectType {
    Table,
    Index,
    View,
    MaterializedView,
    Sequence,
    Function,
    Procedure,
    Trigger,
    Constraint,
    Schema,
    Extension,
    Type,
}

impl std::fmt::Display for DdlObjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table => write!(f, "TABLE"),
            Self::Index => write!(f, "INDEX"),
            Self::View => write!(f, "VIEW"),
            Self::MaterializedView => write!(f, "MATERIALIZED VIEW"),
            Self::Sequence => write!(f, "SEQUENCE"),
            Self::Function => write!(f, "FUNCTION"),
            Self::Procedure => write!(f, "PROCEDURE"),
            Self::Trigger => write!(f, "TRIGGER"),
            Self::Constraint => write!(f, "CONSTRAINT"),
            Self::Schema => write!(f, "SCHEMA"),
            Self::Extension => write!(f, "EXTENSION"),
            Self::Type => write!(f, "TYPE"),
        }
    }
}

/// DDL history entry - automatically captured from WAL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdlHistoryEntry {
    /// Unique DDL ID (auto-incremented)
    pub ddl_id: u64,

    /// Branch ID where executed
    pub branch_id: BranchId,

    /// WAL LSN (Log Sequence Number)
    pub lsn: u64,

    /// Operation type
    pub operation: DdlOperation,

    /// Object type
    pub object_type: DdlObjectType,

    /// Object name (e.g., "users", "idx_users_email")
    pub object_name: String,

    /// Full DDL statement (validated - executed successfully)
    pub ddl_statement: String,

    /// Execution timestamp
    pub executed_at: u64,

    /// User who executed (from session)
    pub executed_by: Option<String>,

    /// Parent DDL ID (for branch tracking)
    pub parent_ddl_id: Option<u64>,

    /// Associated Git commit (if in sync)
    pub git_commit: Option<String>,

    /// Transaction ID that executed this DDL
    pub transaction_id: Option<u64>,
}

/// Schema snapshot - user-triggered full schema capture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// Snapshot name (e.g., "release-1.0", "before-migration")
    pub name: String,

    /// Unique snapshot ID
    pub snapshot_id: u64,

    /// Branch ID
    pub branch_id: BranchId,

    /// Full schema as ordered DDL statements
    pub schema_ddl: Vec<String>,

    /// Creation timestamp
    pub created_at: u64,

    /// User comment
    pub comment: Option<String>,

    /// Associated Git commit
    pub git_commit: Option<String>,

    /// Last DDL ID at snapshot time (for replay tracking)
    pub last_ddl_id: u64,

    /// MVCC snapshot ID for point-in-time reference
    pub mvcc_snapshot_id: SnapshotId,
}

/// Merge strategy for branch merging
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlMergeStrategy {
    /// Replay source branch's DDL statements on target
    /// Preserves original intent, detects conflicts
    DdlReplay,

    /// Compare schemas and generate minimal DDL
    /// More flexible, may miss intent (rename vs drop+create)
    SchemaDiff,
}

/// DDL conflict during merge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdlConflict {
    /// Object that conflicts
    pub object_type: DdlObjectType,

    /// Object name
    pub object_name: String,

    /// Source branch DDL
    pub source_ddl: String,

    /// Target branch DDL (if exists)
    pub target_ddl: Option<String>,

    /// Conflict type
    pub conflict_type: DdlConflictType,
}

/// Type of DDL conflict
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DdlConflictType {
    /// Same object modified in both branches
    BothModified,

    /// Object created in both branches with different definitions
    BothCreated,

    /// Object dropped in one branch, modified in another
    DroppedVsModified,

    /// Incompatible alterations
    IncompatibleAlter,
}

/// DDL Versioning Manager
pub struct DdlVersioningManager {
    /// RocksDB instance
    db: Arc<DB>,

    /// Current timestamp
    timestamp: Arc<RwLock<u64>>,

    /// Next DDL ID
    next_ddl_id: Arc<RwLock<u64>>,

    /// Next snapshot ID
    next_snapshot_id: Arc<RwLock<u64>>,
}

impl DdlVersioningManager {
    /// Create a new DDL versioning manager
    pub fn new(db: Arc<DB>, timestamp: Arc<RwLock<u64>>) -> Result<Self> {
        // Load next IDs from storage or start at 1
        let next_ddl_id = Self::load_next_id(&db, b"git:ddl:next_id")?;
        let next_snapshot_id = Self::load_next_id(&db, b"git:schema_snapshot:next_id")?;

        Ok(Self {
            db,
            timestamp,
            next_ddl_id: Arc::new(RwLock::new(next_ddl_id)),
            next_snapshot_id: Arc::new(RwLock::new(next_snapshot_id)),
        })
    }

    fn load_next_id(db: &DB, key: &[u8]) -> Result<u64> {
        match db.get(key) {
            Ok(Some(data)) => {
                let id: u64 = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize ID: {}", e)))?;
                Ok(id)
            }
            Ok(None) => Ok(1),
            Err(e) => Err(Error::storage(format!("Failed to load ID: {}", e))),
        }
    }

    fn save_next_id(&self, key: &[u8], id: u64) -> Result<()> {
        let data = bincode::serialize(&id)
            .map_err(|e| Error::storage(format!("Failed to serialize ID: {}", e)))?;
        self.db.put(key, &data)
            .map_err(|e| Error::storage(format!("Failed to save ID: {}", e)))
    }

    /// Record a DDL operation (called by WAL applicator)
    pub fn record_ddl(
        &self,
        branch_id: BranchId,
        lsn: u64,
        operation: DdlOperation,
        object_type: DdlObjectType,
        object_name: &str,
        ddl_statement: &str,
        executed_by: Option<String>,
        transaction_id: Option<u64>,
    ) -> Result<DdlHistoryEntry> {
        let ddl_id = {
            let mut id = self.next_ddl_id.write();
            let current = *id;
            *id += 1;
            current
        };

        self.save_next_id(b"git:ddl:next_id", *self.next_ddl_id.read())?;

        let entry = DdlHistoryEntry {
            ddl_id,
            branch_id,
            lsn,
            operation,
            object_type,
            object_name: object_name.to_string(),
            ddl_statement: ddl_statement.to_string(),
            executed_at: *self.timestamp.read(),
            executed_by,
            parent_ddl_id: None, // Can be set for branch tracking
            git_commit: None,
            transaction_id,
        };

        // Store entry
        let key = Self::encode_ddl_key(branch_id, ddl_id);
        let value = bincode::serialize(&entry)
            .map_err(|e| Error::storage(format!("Failed to serialize DDL entry: {}", e)))?;
        self.db.put(&key, &value)
            .map_err(|e| Error::storage(format!("Failed to save DDL entry: {}", e)))?;

        tracing::debug!(
            "Recorded DDL {}: {} {} {} on branch {}",
            ddl_id, entry.operation, entry.object_type, object_name, branch_id
        );

        Ok(entry)
    }

    fn encode_ddl_key(branch_id: BranchId, ddl_id: u64) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(GIT_DDL_HISTORY_PREFIX);
        key.extend_from_slice(&branch_id.to_be_bytes());
        key.push(b':');
        key.extend_from_slice(&ddl_id.to_be_bytes());
        key
    }

    /// Get DDL history for a branch
    pub fn get_ddl_history(&self, branch_id: BranchId, limit: Option<usize>) -> Result<Vec<DdlHistoryEntry>> {
        let mut entries = Vec::new();

        let prefix = {
            let mut p = Vec::new();
            p.extend_from_slice(GIT_DDL_HISTORY_PREFIX);
            p.extend_from_slice(&branch_id.to_be_bytes());
            p.push(b':');
            p
        };

        let iter = self.db.prefix_iterator(&prefix);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(&prefix) {
                break;
            }

            let entry: DdlHistoryEntry = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize DDL entry: {}", e)))?;

            entries.push(entry);

            if let Some(l) = limit {
                if entries.len() >= l {
                    break;
                }
            }
        }

        Ok(entries)
    }

    /// Create a schema snapshot
    pub fn create_schema_snapshot(
        &self,
        branch_id: BranchId,
        name: &str,
        schema_ddl: Vec<String>,
        mvcc_snapshot_id: SnapshotId,
        comment: Option<String>,
        git_commit: Option<String>,
    ) -> Result<SchemaSnapshot> {
        let snapshot_id = {
            let mut id = self.next_snapshot_id.write();
            let current = *id;
            *id += 1;
            current
        };

        self.save_next_id(b"git:schema_snapshot:next_id", *self.next_snapshot_id.read())?;

        // Get last DDL ID for this branch
        let last_ddl_id = *self.next_ddl_id.read() - 1;

        let snapshot = SchemaSnapshot {
            name: name.to_string(),
            snapshot_id,
            branch_id,
            schema_ddl,
            created_at: *self.timestamp.read(),
            comment,
            git_commit,
            last_ddl_id,
            mvcc_snapshot_id,
        };

        // Store snapshot
        let key = Self::encode_snapshot_key(branch_id, &snapshot.name);
        let value = bincode::serialize(&snapshot)
            .map_err(|e| Error::storage(format!("Failed to serialize snapshot: {}", e)))?;
        self.db.put(&key, &value)
            .map_err(|e| Error::storage(format!("Failed to save snapshot: {}", e)))?;

        tracing::info!(
            "Created schema snapshot '{}' for branch {} with {} DDL statements",
            name, branch_id, snapshot.schema_ddl.len()
        );

        Ok(snapshot)
    }

    fn encode_snapshot_key(branch_id: BranchId, name: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(GIT_SCHEMA_SNAPSHOT_PREFIX);
        key.extend_from_slice(&branch_id.to_be_bytes());
        key.push(b':');
        key.extend_from_slice(name.as_bytes());
        key
    }

    /// Get a schema snapshot
    pub fn get_schema_snapshot(&self, branch_id: BranchId, name: &str) -> Result<Option<SchemaSnapshot>> {
        let key = Self::encode_snapshot_key(branch_id, name);

        match self.db.get(&key) {
            Ok(Some(data)) => {
                let snapshot: SchemaSnapshot = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize snapshot: {}", e)))?;
                Ok(Some(snapshot))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to load snapshot: {}", e))),
        }
    }

    /// List all snapshots for a branch
    pub fn list_snapshots(&self, branch_id: BranchId) -> Result<Vec<SchemaSnapshot>> {
        let mut snapshots = Vec::new();

        let prefix = {
            let mut p = Vec::new();
            p.extend_from_slice(GIT_SCHEMA_SNAPSHOT_PREFIX);
            p.extend_from_slice(&branch_id.to_be_bytes());
            p.push(b':');
            p
        };

        let iter = self.db.prefix_iterator(&prefix);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(&prefix) {
                break;
            }

            let snapshot: SchemaSnapshot = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize snapshot: {}", e)))?;

            snapshots.push(snapshot);
        }

        Ok(snapshots)
    }

    /// Get DDL history since a specific DDL ID (for replay)
    pub fn get_ddl_since(&self, branch_id: BranchId, since_ddl_id: u64) -> Result<Vec<DdlHistoryEntry>> {
        let entries = self.get_ddl_history(branch_id, None)?;

        Ok(entries.into_iter()
            .filter(|e| e.ddl_id > since_ddl_id)
            .collect())
    }

    /// Detect DDL conflicts between branches
    pub fn detect_conflicts(
        &self,
        source_branch: BranchId,
        target_branch: BranchId,
        since_ddl_id: u64,
    ) -> Result<Vec<DdlConflict>> {
        let source_ddl = self.get_ddl_since(source_branch, since_ddl_id)?;
        let target_ddl = self.get_ddl_since(target_branch, since_ddl_id)?;

        let mut conflicts = Vec::new();

        // Build object -> DDL map for target
        let mut target_objects: std::collections::HashMap<String, &DdlHistoryEntry> =
            std::collections::HashMap::new();

        for entry in &target_ddl {
            let key = format!("{}.{}", entry.object_type, entry.object_name);
            target_objects.insert(key, entry);
        }

        // Check source DDL against target
        for source_entry in &source_ddl {
            let key = format!("{}.{}", source_entry.object_type, source_entry.object_name);

            if let Some(target_entry) = target_objects.get(&key) {
                // Same object modified in both branches
                let conflict_type = match (&source_entry.operation, &target_entry.operation) {
                    (DdlOperation::Create, DdlOperation::Create) => DdlConflictType::BothCreated,
                    (DdlOperation::Drop, _) | (_, DdlOperation::Drop) => DdlConflictType::DroppedVsModified,
                    _ => DdlConflictType::BothModified,
                };

                conflicts.push(DdlConflict {
                    object_type: source_entry.object_type.clone(),
                    object_name: source_entry.object_name.clone(),
                    source_ddl: source_entry.ddl_statement.clone(),
                    target_ddl: Some(target_entry.ddl_statement.clone()),
                    conflict_type,
                });
            }
        }

        Ok(conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::storage::StorageEngine;

    #[test]
    fn test_ddl_versioning_basic() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

        let manager = DdlVersioningManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).expect("Failed to create manager");

        // Record DDL
        let entry = manager.record_ddl(
            1, // branch_id
            100, // lsn
            DdlOperation::Create,
            DdlObjectType::Table,
            "users",
            "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
            Some("admin".to_string()),
            Some(1),
        ).expect("Failed to record DDL");

        assert_eq!(entry.ddl_id, 1);
        assert_eq!(entry.object_name, "users");

        // Get history
        let history = manager.get_ddl_history(1, None).expect("Failed to get history");
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn test_schema_snapshot() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open engine");

        let manager = DdlVersioningManager::new(
            Arc::clone(&engine.db),
            Arc::clone(&engine.timestamp),
        ).expect("Failed to create manager");

        // Create snapshot
        let snapshot = manager.create_schema_snapshot(
            1,
            "release-1.0",
            vec![
                "CREATE TABLE users (id INT PRIMARY KEY)".to_string(),
                "CREATE INDEX idx_users ON users(id)".to_string(),
            ],
            100,
            Some("Initial release".to_string()),
            None,
        ).expect("Failed to create snapshot");

        assert_eq!(snapshot.name, "release-1.0");
        assert_eq!(snapshot.schema_ddl.len(), 2);

        // Get snapshot
        let retrieved = manager.get_schema_snapshot(1, "release-1.0")
            .expect("Failed to get snapshot")
            .expect("Snapshot should exist");

        assert_eq!(retrieved.name, "release-1.0");
    }
}
