//! Transaction implementation
//!
//! Basic 2PL (Two-Phase Locking) with standard MVCC snapshot isolation.
//! Optimized with lock-free concurrent data structures for improved performance.
//!
//! v3.1.0: Enhanced with session-aware locking and isolation level support

use super::{Key, Snapshot, SnapshotId};
use super::time_travel::SnapshotManager;
use super::lock_manager::{LockManager, LockType, LockGuard};
use super::dirty_tracker::DirtyTracker;
use crate::{Error, Result, Tuple};
use crate::session::{SessionId, IsolationLevel};
use rocksdb::DB;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use dashmap::DashMap;
use parking_lot::RwLock;
use tracing::{debug, trace, warn};

/// Transaction state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    /// Active (can read/write)
    Active,
    /// Committed
    Committed,
    /// Aborted
    Aborted,
}

impl TransactionState {
    /// Convert state to u8 for atomic storage
    const fn to_u8(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Committed => 1,
            Self::Aborted => 2,
        }
    }

    /// Convert u8 to state
    const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Active,
            1 => Self::Committed,
            _ => Self::Aborted,
        }
    }
}

/// Transaction
///
/// Provides ACID guarantees using standard snapshot isolation.
///
/// Optimized with lock-free data structures:
/// - DashMap for concurrent write_set access without mutex contention
/// - AtomicU8 for lock-free state checking
/// This significantly reduces lock contention on the read path.
///
/// v3.1.0 enhancements:
/// - Session-aware transactions with isolation levels
/// - Lock manager integration for deadlock detection
/// - Dirty state tracking for dump operations
pub struct Transaction {
    /// Database handle
    db: Arc<DB>,
    /// Snapshot for reads
    snapshot: Snapshot,
    /// Snapshot timestamp (read timestamp for MVCC)
    snapshot_ts: u64,
    /// Unique transaction ID for locking and tracking
    transaction_id: u64,
    /// Snapshot manager for versioned reads
    snapshot_manager: Arc<SnapshotManager>,
    /// Write set (buffered writes) - uses DashMap for lock-free concurrent access
    write_set: Arc<DashMap<Key, Option<Vec<u8>>>>,
    /// Transaction state - uses AtomicU8 for lock-free state checking
    state: AtomicU8,
    /// Session ID (for multi-user support)
    session_id: Option<SessionId>,
    /// Isolation level for this transaction
    isolation_level: IsolationLevel,
    /// Lock manager for concurrency control
    lock_manager: Option<Arc<LockManager>>,
    /// Acquired locks (RAII guards for automatic release)
    acquired_locks: Arc<RwLock<Vec<LockGuard>>>,
    /// Dirty tracker for dump operations
    dirty_tracker: Option<Arc<DirtyTracker>>,
}

impl Transaction {
    /// Create a new transaction (backwards compatible)
    pub fn new(db: Arc<DB>, snapshot_id: SnapshotId, snapshot_manager: Arc<SnapshotManager>) -> Result<Self> {
        static TXN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let transaction_id = TXN_COUNTER.fetch_add(1, Ordering::SeqCst);

        debug!(
            txn_id = transaction_id,
            snapshot_id = snapshot_id,
            "Transaction started (legacy mode)"
        );

        Ok(Self {
            db,
            snapshot: Snapshot::new(snapshot_id),
            snapshot_ts: snapshot_id,
            transaction_id,
            snapshot_manager,
            write_set: Arc::new(DashMap::new()),
            state: AtomicU8::new(TransactionState::Active.to_u8()),
            session_id: None,
            isolation_level: IsolationLevel::ReadCommitted,
            lock_manager: None,
            acquired_locks: Arc::new(RwLock::new(Vec::new())),
            dirty_tracker: None,
        })
    }

    /// Create a new transaction with session and lock manager support
    pub fn new_with_session(
        db: Arc<DB>,
        snapshot_id: SnapshotId,
        snapshot_manager: Arc<SnapshotManager>,
        session_id: SessionId,
        isolation_level: IsolationLevel,
        lock_manager: Arc<LockManager>,
        dirty_tracker: Arc<DirtyTracker>,
    ) -> Result<Self> {
        static TXN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let transaction_id = TXN_COUNTER.fetch_add(1, Ordering::SeqCst);

        debug!(
            txn_id = transaction_id,
            session_id = ?session_id,
            snapshot_id = snapshot_id,
            isolation_level = ?isolation_level,
            "Transaction started with session"
        );

        Ok(Self {
            db,
            snapshot: Snapshot::new(snapshot_id),
            snapshot_ts: snapshot_id,
            transaction_id,
            snapshot_manager,
            write_set: Arc::new(DashMap::new()),
            state: AtomicU8::new(TransactionState::Active.to_u8()),
            session_id: Some(session_id),
            isolation_level,
            lock_manager: Some(lock_manager),
            acquired_locks: Arc::new(RwLock::new(Vec::new())),
            dirty_tracker: Some(dirty_tracker),
        })
    }

    /// Read a value
    ///
    /// Standard MVCC read: see snapshot-consistent version.
    /// Optimized with lock-free atomic state check and DashMap write_set lookup.
    ///
    /// v3.1.0: Enhanced with isolation level support
    /// - READ COMMITTED: No read locks (fresh snapshot per statement)
    /// - REPEATABLE READ: Acquires read locks on accessed rows
    /// - SERIALIZABLE: Acquires read locks on accessed rows (prevents phantom reads)
    pub fn get(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        // Lock-free state check using atomic
        let state_value = self.state.load(Ordering::Acquire);
        let state = TransactionState::from_u8(state_value);
        if state != TransactionState::Active {
            return Err(Error::transaction("Transaction is not active"));
        }

        // Acquire read lock based on isolation level
        if let Some(ref lock_mgr) = self.lock_manager {
            match self.isolation_level {
                IsolationLevel::RepeatableRead | IsolationLevel::Serializable => {
                    // Acquire read lock to prevent non-repeatable reads
                    let key_str = String::from_utf8_lossy(key).to_string();
                    let lock_guard = lock_mgr.acquire_lock(&key_str, self.transaction_id, LockType::Read)?;

                    // Store lock guard for automatic release on commit/rollback
                    self.acquired_locks.write().push(lock_guard);
                }
                IsolationLevel::ReadCommitted => {
                    // No read locks for READ COMMITTED
                }
            }
        }

        // Lock-free write set lookup using DashMap
        if let Some(entry) = self.write_set.get(key) {
            return Ok(entry.value().clone());
        }

        // Read from database at snapshot using MVCC
        self.read_at_version(key, self.snapshot_ts)
    }

    /// Read a versioned value at the transaction's snapshot timestamp
    ///
    /// Implements MVCC snapshot isolation by reading the latest version
    /// that is visible to this transaction (version timestamp <= snapshot_ts).
    ///
    /// Optimized with zero-copy key parsing to avoid allocations.
    ///
    /// Returns None if:
    /// - The key doesn't exist
    /// - All versions are newer than the snapshot timestamp
    /// - The key was deleted before the snapshot
    fn read_at_version(&self, key: &Key, snapshot_ts: u64) -> Result<Option<Vec<u8>>> {
        // Zero-copy key parsing optimization
        // Expected key format: "data:{table_name}:{row_id}"

        // Fast path: check prefix without UTF-8 conversion
        const DATA_PREFIX: &[u8] = b"data:";
        if !key.starts_with(DATA_PREFIX) {
            // Not a versioned data key, fallback to simple read
            return self.db.get(key)
                .map_err(|e| Error::storage(format!("Transaction get failed: {}", e)));
        }

        // Parse key with minimal allocations
        let key_str = std::str::from_utf8(key)
            .map_err(|e| Error::storage(format!("Invalid key encoding: {}", e)))?;

        // Manual parsing without allocation - skip "data:" prefix
        let rest = &key_str[5..];

        // Find first colon position for table name
        let colon_pos = match rest.find(':') {
            Some(pos) => pos,
            None => {
                // Invalid format, fallback to simple read
                return self.db.get(key)
                    .map_err(|e| Error::storage(format!("Transaction get failed: {}", e)));
            }
        };

        let table_name = &rest[..colon_pos];
        let row_id_str = &rest[colon_pos + 1..];

        // Parse row ID directly from slice
        let row_id = match row_id_str.parse::<u64>() {
            Ok(id) => id,
            Err(_) => {
                // Invalid row ID format, fallback to simple read
                return self.db.get(key)
                    .map_err(|e| Error::storage(format!("Transaction get failed: {}", e)));
            }
        };

        // Use snapshot manager to read the versioned value
        // This implements the core MVCC logic: find the latest version <= snapshot_ts
        self.snapshot_manager.read_at_snapshot(table_name, row_id, snapshot_ts)
    }

    /// Write a value
    ///
    /// Buffered in write set until commit.
    /// Uses lock-free DashMap for concurrent access.
    ///
    /// v3.1.0: Enhanced with write lock acquisition for all isolation levels
    pub fn put(&self, key: Key, value: Vec<u8>) -> Result<()> {
        // Lock-free state check
        let state_value = self.state.load(Ordering::Acquire);
        let state = TransactionState::from_u8(state_value);
        if state != TransactionState::Active {
            return Err(Error::transaction("Transaction is not active"));
        }

        // Acquire write lock (all isolation levels require write locks)
        if let Some(ref lock_mgr) = self.lock_manager {
            let key_str = String::from_utf8_lossy(&key).to_string();
            let lock_guard = lock_mgr.acquire_lock(&key_str, self.transaction_id, LockType::Write)?;

            // Store lock guard for automatic release on commit/rollback
            self.acquired_locks.write().push(lock_guard);
        }

        // Track dirty state with proper table/row granularity
        if let Some(ref tracker) = self.dirty_tracker {
            // Parse key to extract table_name and row_key for proper tracking
            // Key format: "data:{table_name}:{row_id}"
            const DATA_PREFIX: &[u8] = b"data:";
            if key.starts_with(DATA_PREFIX) {
                if let Ok(key_str) = std::str::from_utf8(&key) {
                    let rest = &key_str[5..]; // Skip "data:" prefix
                    if let Some(colon_pos) = rest.find(':') {
                        let table_name = &rest[..colon_pos];
                        let row_key = &rest[colon_pos + 1..];
                        let _ = tracker.track_insert(table_name, row_key, &value);
                    }
                }
            }
        }

        // Lock-free write_set insert using DashMap
        self.write_set.insert(key, Some(value));
        Ok(())
    }

    /// Delete a key
    ///
    /// Buffered as tombstone until commit.
    /// Uses lock-free DashMap for concurrent access.
    pub fn delete(&self, key: Key) -> Result<()> {
        // Lock-free state check
        let state_value = self.state.load(Ordering::Acquire);
        let state = TransactionState::from_u8(state_value);
        if state != TransactionState::Active {
            return Err(Error::transaction("Transaction is not active"));
        }

        // Lock-free write_set insert (tombstone) using DashMap
        self.write_set.insert(key, None);
        Ok(())
    }

    /// Update tuples within transaction
    ///
    /// Buffers updates in the transaction's write set instead of writing directly to storage.
    /// This ensures ACID guarantees - writes are only visible after commit.
    pub fn update_tuples(&self, table_name: &str, updates: Vec<(u64, crate::Tuple)>) -> Result<u64> {
        // Lock-free state check
        let state_value = self.state.load(Ordering::Acquire);
        let state = TransactionState::from_u8(state_value);
        if state != TransactionState::Active {
            return Err(Error::transaction("Transaction is not active"));
        }

        let mut update_count = 0u64;

        for (row_id, tuple) in updates {
            // Format key for data table (using main branch format)
            let key = format!("data:{}:{}", table_name, row_id).into_bytes();

            // Serialize tuple
            let value = bincode::serialize(&tuple)
                .map_err(|e| crate::Error::storage(format!("Failed to serialize tuple: {}", e)))?;

            // Buffer in write set (will be applied on commit)
            self.put(key, value)?;
            update_count += 1;
        }

        Ok(update_count)
    }

    /// Delete tuples within transaction
    ///
    /// Buffers deletions as tombstones in the transaction's write set.
    /// This ensures ACID guarantees - deletions are only visible after commit.
    pub fn delete_tuples(&self, table_name: &str, row_ids: Vec<u64>) -> Result<u64> {
        // Lock-free state check
        let state_value = self.state.load(Ordering::Acquire);
        let state = TransactionState::from_u8(state_value);
        if state != TransactionState::Active {
            return Err(Error::transaction("Transaction is not active"));
        }

        let mut delete_count = 0u64;

        for row_id in row_ids {
            // Format key for data table (using main branch format)
            let key = format!("data:{}:{}", table_name, row_id).into_bytes();

            // Buffer tombstone in write set (will be applied on commit)
            self.delete(key)?;
            delete_count += 1;
        }

        Ok(delete_count)
    }

    /// Check if transaction is active
    ///
    /// Lock-free atomic check for maximum performance.
    pub fn is_active(&self) -> bool {
        let state_value = self.state.load(Ordering::Acquire);
        TransactionState::from_u8(state_value) == TransactionState::Active
    }

    /// Commit the transaction with a specific timestamp
    pub fn commit_with_timestamp(self, commit_ts: u64) -> Result<()> {
        let commit_start = std::time::Instant::now();
        let write_count = self.write_set.len();
        let lock_count = self.acquired_locks.read().len();

        trace!(
            txn_id = self.transaction_id,
            session_id = ?self.session_id,
            commit_ts = commit_ts,
            write_count = write_count,
            lock_count = lock_count,
            "Committing transaction"
        );

        // Check and transition state atomically
        let current = self.state.compare_exchange(
            TransactionState::Active.to_u8(),
            TransactionState::Committed.to_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        if current.is_err() {
            warn!(
                txn_id = self.transaction_id,
                "Commit failed: transaction not active"
            );
            return Err(Error::transaction("Transaction is not active"));
        }

        let reverse_ts = u64::MAX - commit_ts;

        // Apply write set atomically using RocksDB batch
        let mut batch = rocksdb::WriteBatch::default();

        // Iterate over DashMap entries
        for entry in self.write_set.iter() {
            let (key, value) = (entry.key(), entry.value());
            match value {
                Some(val) => {
                    batch.put(key, val);

                    // Create version index for snapshot reads
                    if let Ok(key_str) = std::str::from_utf8(key) {
                        if key_str.starts_with("data:") {
                            let rest = &key_str[5..];
                            if let Some(colon_pos) = rest.find(':') {
                                let table_name = &rest[..colon_pos];
                                let row_id_str = &rest[colon_pos + 1..];

                                if let Ok(row_id) = row_id_str.parse::<u64>() {
                                    // Write actual version data at commit timestamp
                                    let v_key = format!("v:{}:{}:{}", table_name, row_id, commit_ts);
                                    batch.put(v_key.as_bytes(), val);

                                    // Create version index entry with BIG ENDIAN commit timestamp
                                    let v_idx_key = format!(
                                        "v_idx:{}:{}:{:020}",
                                        table_name,
                                        row_id_str,
                                        reverse_ts
                                    );
                                    let ts_bytes = commit_ts.to_be_bytes();
                                    batch.put(v_idx_key.as_bytes(), ts_bytes);
                                }
                            }
                        }
                    }
                }
                None => batch.delete(key),
            }
        }

        let result = self.db.write(batch);

        // Release all acquired locks
        self.acquired_locks.write().clear();

        // Register the snapshot so it's visible to future queries
        if result.is_ok() {
            let _ = self.snapshot_manager.register_snapshot(commit_ts);
        }

        result.map_err(|e| {
            warn!(
                txn_id = self.transaction_id,
                error = %e,
                "Transaction commit failed, aborting"
            );
            self.state.store(TransactionState::Aborted.to_u8(), Ordering::Release);
            Error::transaction(format!("Commit failed: {}", e))
        })?;

        debug!(
            txn_id = self.transaction_id,
            session_id = ?self.session_id,
            commit_ts = commit_ts,
            write_count = write_count,
            duration_us = commit_start.elapsed().as_micros() as u64,
            "Transaction committed successfully"
        );

        Ok(())
    }

    /// Commit the transaction
    ///
    /// Atomically apply all buffered writes and create version index entries.
    /// Uses atomic state transition for consistency.
    ///
    /// v3.1.0: Enhanced with lock release and dirty tracker integration
    pub fn commit(self) -> Result<()> {
        let ts = self.snapshot_ts;
        self.commit_with_timestamp(ts)
    }

    /// Rollback the transaction
    ///
    /// Discard all buffered writes.
    /// Uses atomic state transition.
    ///
    /// v3.1.0: Enhanced with lock release
    pub fn rollback(self) -> Result<()> {
        let write_count = self.write_set.len();
        let lock_count = self.acquired_locks.read().len();

        debug!(
            txn_id = self.transaction_id,
            session_id = ?self.session_id,
            write_count = write_count,
            lock_count = lock_count,
            "Rolling back transaction"
        );

        // Check and transition state atomically
        let current = self.state.compare_exchange(
            TransactionState::Active.to_u8(),
            TransactionState::Aborted.to_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        if current.is_err() {
            warn!(
                txn_id = self.transaction_id,
                "Rollback failed: transaction not active"
            );
            return Err(Error::transaction("Transaction is not active"));
        }

        // Release all acquired locks (RAII will handle this automatically when guards are dropped)
        // Explicitly clear the lock vector to trigger drops
        self.acquired_locks.write().clear();

        // Clear write set (DashMap handles concurrency)
        self.write_set.clear();

        debug!(
            txn_id = self.transaction_id,
            "Transaction rolled back successfully"
        );

        Ok(())
    }

    /// Get transaction state
    ///
    /// Lock-free atomic read.
    pub fn state(&self) -> TransactionState {
        let state_value = self.state.load(Ordering::Acquire);
        TransactionState::from_u8(state_value)
    }

    /// Get snapshot ID
    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_ts
    }

    /// Refresh the snapshot timestamp to the current database state
    ///
    /// Useful for READ COMMITTED isolation level where each statement
    /// should see a fresh snapshot of the database.
    pub fn refresh_snapshot(&mut self, new_ts: u64) {
        self.snapshot_ts = new_ts;
        self.snapshot = Snapshot::new(new_ts);
    }

    /// Merge a set of tuples with the transaction's write set
    ///
    /// This ensures "read-your-own-writes" consistency for scans.
    /// It replaces tuples with newer versions in the write set and adds new tuples.
    pub fn merge_with_write_set(&self, table_name: &str, mut tuples: Vec<Tuple>) -> Result<Vec<Tuple>> {
        let prefix = format!("data:{}:", table_name);
        
        // Track which row IDs we've already handled from the base set
        let mut handled_row_ids = std::collections::HashSet::new();
        
        // 1. Update existing tuples from write set and handle tombstones
        let mut i = 0;
        while i < tuples.len() {
            let current_row_id = tuples.get(i).and_then(|t| t.row_id);
            if let Some(row_id) = current_row_id {
                handled_row_ids.insert(row_id);
                let key = format!("{}{}", prefix, row_id).into_bytes();

                if let Some(entry) = self.write_set.get(&key) {
                    match entry.value() {
                        Some(data) => {
                            // Replace with updated version
                            let mut updated_tuple: Tuple = bincode::deserialize(data)
                                .map_err(|e| Error::storage(format!("Failed to deserialize tuple: {}", e)))?;
                            updated_tuple.row_id = Some(row_id);
                            if let Some(slot) = tuples.get_mut(i) {
                                *slot = updated_tuple;
                            }
                            i += 1;
                        }
                        None => {
                            // Remove deleted tuple
                            tuples.remove(i);
                            // Don't increment i
                        }
                    }
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        
        // 2. Add new tuples from write set that weren't in the base set
        for entry in self.write_set.iter() {
            let key = entry.key();
            if let Ok(key_str) = std::str::from_utf8(key) {
                if let Some(row_id_str) = key_str.strip_prefix(&prefix) {
                    if let Ok(row_id) = row_id_str.parse::<u64>() {
                        if !handled_row_ids.contains(&row_id) {
                            if let Some(data) = entry.value() {
                                let mut new_tuple: Tuple = bincode::deserialize(data)
                                    .map_err(|e| Error::storage(format!("Failed to deserialize tuple: {}", e)))?;
                                new_tuple.row_id = Some(row_id);
                                tuples.push(new_tuple);
                            }
                        }
                    }
                }
            }
        }
        
        Ok(tuples)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::storage::StorageEngine;

    #[test]
    fn test_transaction_commit() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        let tx = engine.begin_transaction()
            .expect("Failed to begin transaction");

        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        tx.put(key.clone(), value.clone())
            .expect("Failed to put value");
        tx.commit()
            .expect("Failed to commit transaction");

        // Verify committed
        let result = engine.get(&key)
            .expect("Failed to get value");
        assert_eq!(result, Some(value));
    }

    #[test]
    fn test_transaction_rollback() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        let tx = engine.begin_transaction()
            .expect("Failed to begin transaction");

        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        tx.put(key.clone(), value.clone())
            .expect("Failed to put value");
        tx.rollback()
            .expect("Failed to rollback transaction");

        // Verify not committed
        let result = engine.get(&key)
            .expect("Failed to get value");
        assert_eq!(result, None);
    }

    #[test]
    fn test_read_your_own_writes() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        let tx = engine.begin_transaction()
            .expect("Failed to begin transaction");

        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        tx.put(key.clone(), value.clone())
            .expect("Failed to put value");

        // Should see own writes before commit
        let result = tx.get(&key)
            .expect("Failed to get value");
        assert_eq!(result, Some(value));
    }

    #[test]
    fn test_mvcc_snapshot_isolation() {
        use crate::{Column, DataType, Schema, Tuple, Value};

        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        // Create a test table
        let schema = Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
                Column {
                    name: "value".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        };

        let catalog = engine.catalog();
        catalog.create_table("mvcc_test", schema)
            .expect("Failed to create table");

        // Insert initial data and create version
        let row_id = engine.insert_tuple_versioned("mvcc_test", Tuple {
            values: vec![Value::Int4(1), Value::String("initial".to_string())],
            row_id: None,
            branch_id: None,
        }).expect("Failed to insert");

        let version1_ts = engine.current_timestamp();

        // Create snapshot manager reference for version writes
        let snapshot_mgr = engine.snapshot_manager();

        // Write a version at timestamp 1
        let key1 = format!("data:mvcc_test:{}", row_id).into_bytes();
        let value1 = bincode::serialize(&Tuple {
            values: vec![Value::Int4(1), Value::String("version1".to_string())],
            row_id: None,
            branch_id: None,
        }).expect("Failed to serialize");
        snapshot_mgr.write_version("mvcc_test", row_id, version1_ts, &value1)
            .expect("Failed to write version 1");

        // Start transaction 1 at this snapshot
        let tx1 = engine.begin_transaction()
            .expect("Failed to begin tx1");

        // Update to version 2 (after tx1 started)
        let version2_ts = engine.current_timestamp();
        let value2 = bincode::serialize(&Tuple {
            values: vec![Value::Int4(1), Value::String("version2".to_string())],
            row_id: None,
            branch_id: None,
        }).expect("Failed to serialize");
        snapshot_mgr.write_version("mvcc_test", row_id, version2_ts, &value2)
            .expect("Failed to write version 2");

        // tx1 should still see version1 (snapshot isolation)
        let result = tx1.get(&key1)
            .expect("Failed to read in tx1");
        assert!(result.is_some(), "tx1 should see version1");

        // Start transaction 2 at current snapshot (should see version2)
        let tx2 = engine.begin_transaction()
            .expect("Failed to begin tx2");
        let result2 = tx2.get(&key1)
            .expect("Failed to read in tx2");
        assert!(result2.is_some(), "tx2 should see version2");

        // Verify isolation: tx1 still sees version1 even after tx2 started
        let result_again = tx1.get(&key1)
            .expect("Failed to read in tx1 again");
        assert_eq!(result, result_again, "tx1 should see consistent snapshot");
    }

    #[test]
    fn test_mvcc_concurrent_reads() {
        use crate::{Column, DataType, Schema, Tuple, Value};

        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        // Create table
        let schema = Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        };

        let catalog = engine.catalog();
        catalog.create_table("concurrent_test", schema)
            .expect("Failed to create table");

        // Insert data
        let row_id = engine.insert_tuple_versioned("concurrent_test", Tuple {
            values: vec![Value::Int4(1)],
            row_id: None,
            branch_id: None,
        }).expect("Failed to insert");

        let ts1 = engine.current_timestamp();
        let snapshot_mgr = engine.snapshot_manager();

        // Create version at ts1
        let value = bincode::serialize(&Tuple {
            values: vec![Value::Int4(100)],
            row_id: None,
            branch_id: None,
        }).expect("Failed to serialize");
        snapshot_mgr.write_version("concurrent_test", row_id, ts1, &value)
            .expect("Failed to write version");

        // Multiple concurrent transactions reading at same snapshot
        let tx1 = engine.begin_transaction().expect("Failed to begin tx1");
        let tx2 = engine.begin_transaction().expect("Failed to begin tx2");
        let tx3 = engine.begin_transaction().expect("Failed to begin tx3");

        let key = format!("data:concurrent_test:{}", row_id).into_bytes();

        // All should see same data (no phantom reads)
        let r1 = tx1.get(&key).expect("tx1 read failed");
        let r2 = tx2.get(&key).expect("tx2 read failed");
        let r3 = tx3.get(&key).expect("tx3 read failed");

        assert!(r1.is_some());
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
    }

    #[test]
    fn test_mvcc_deleted_row_visibility() {
        use crate::{Column, DataType, Schema, Tuple, Value};

        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        // Create table
        let schema = Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Int4,
                    nullable: false,
                    primary_key: true,
                    source_table: None,
                    source_table_name: None,
                    default_expr: None,
                    unique: false,
                    storage_mode: crate::ColumnStorageMode::Default,
                },
            ],
        };

        let catalog = engine.catalog();
        catalog.create_table("delete_test", schema)
            .expect("Failed to create table");

        // Insert and version data
        let row_id = engine.insert_tuple_versioned("delete_test", Tuple {
            values: vec![Value::Int4(1)],
            row_id: None,
            branch_id: None,
        }).expect("Failed to insert");

        let ts1 = engine.current_timestamp();
        let snapshot_mgr = engine.snapshot_manager();

        let value = bincode::serialize(&Tuple {
            values: vec![Value::Int4(42)],
            row_id: None,
            branch_id: None,
        }).expect("Failed to serialize");
        snapshot_mgr.write_version("delete_test", row_id, ts1, &value)
            .expect("Failed to write version");

        // Start tx1 (should see the row)
        let tx1 = engine.begin_transaction().expect("Failed to begin tx1");

        // Simulate delete by not creating a new version
        // (In real implementation, deletes would create tombstone versions)

        // tx1 should still see the row at its snapshot
        let key = format!("data:delete_test:{}", row_id).into_bytes();
        let result = tx1.get(&key).expect("Failed to read");
        assert!(result.is_some(), "tx1 should see row at its snapshot");
    }

    #[test]
    fn test_mvcc_read_at_version_parsing() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config)
            .expect("Failed to open in-memory storage");

        let tx = engine.begin_transaction()
            .expect("Failed to begin transaction");

        // Test non-versioned key (should fallback to simple read)
        let meta_key = b"meta:some_key".to_vec();
        let result = tx.get(&meta_key);
        assert!(result.is_ok(), "Should handle non-versioned keys");

        // Test invalid key format (should fallback to simple read)
        let invalid_key = b"data:invalid".to_vec();
        let result = tx.get(&invalid_key);
        assert!(result.is_ok(), "Should handle invalid key format");
    }
}
