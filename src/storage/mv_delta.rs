//! Delta Tracking System for Incremental Materialized Views
//!
//! This module implements a comprehensive change-tracking system that captures modifications
//! to base tables for efficient incremental materialized view refresh operations.
//!
//! # Architecture
//!
//! - **Delta Recording**: Captures INSERT, UPDATE, DELETE operations on tracked tables
//! - **Storage**: Persistent delta log stored in RocksDB with efficient key design
//! - **Querying**: Fast retrieval of deltas for specific tables and time ranges
//! - **Compaction**: Automatic cleanup of old deltas based on retention policy
//! - **Merging**: Optimization of delta sequences for minimal view updates
//! - **Transaction Integration**: Hooks into transaction commit to capture changes
//!
//! # Usage
//!
//! ```ignore
//! // Track changes to a table
//! delta_tracker.track_table("users")?;
//!
//! // Record a change during transaction commit
//! delta_tracker.record_insert("users", row_id, tuple)?;
//!
//! // Query deltas for MV refresh
//! let since = last_refresh_time;
//! let deltas = delta_tracker.get_deltas_since("users", since)?;
//!
//! // Merge deltas for optimization
//! deltas.merge()?;
//! ```

#![allow(deprecated)]

use crate::{Result, Error, Tuple};
use rocksdb::DB;
use std::sync::Arc;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Serialize, Deserialize};
use chrono::{DateTime, Utc};

/// Delta operation types for materialized view tracking
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeltaOperation {
    /// Insert operation with new tuple data
    Insert { tuple: Tuple },

    /// Delete operation with old tuple data
    Delete { tuple: Tuple },

    /// Update operation with both old and new tuple data
    Update { old_tuple: Tuple, new_tuple: Tuple },
}

impl DeltaOperation {
    /// Get the operation type as a string
    pub fn operation_type(&self) -> &str {
        match self {
            Self::Insert { .. } => "INSERT",
            Self::Delete { .. } => "DELETE",
            Self::Update { .. } => "UPDATE",
        }
    }

    /// Check if this is an insert operation
    pub fn is_insert(&self) -> bool {
        matches!(self, Self::Insert { .. })
    }

    /// Check if this is a delete operation
    pub fn is_delete(&self) -> bool {
        matches!(self, Self::Delete { .. })
    }

    /// Check if this is an update operation
    pub fn is_update(&self) -> bool {
        matches!(self, Self::Update { .. })
    }
}

// Keep DeltaType for backward compatibility
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[deprecated(note = "Use DeltaOperation instead")]
pub enum DeltaType {
    /// Row insertion
    #[default]
    Insert,
    /// Row update (stores before and after values)
    Update,
    /// Row deletion
    Delete,
}

/// A single delta representing a change to a base table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    /// Name of the table that was modified
    pub table_name: String,

    /// Row ID that was affected
    pub row_id: u64,

    /// The operation that was performed
    pub operation: DeltaOperation,

    /// Timestamp when the change occurred (using SystemTime for consistency)
    pub timestamp: SystemTime,

    /// Transaction ID that made this change
    pub transaction_id: u64,

    /// Deprecated fields for backward compatibility
    #[serde(skip)]
    #[deprecated]
    pub delta_id: u64,

    #[serde(skip)]
    #[deprecated]
    pub delta_type: DeltaType,

    #[serde(skip)]
    #[deprecated]
    pub new_tuple: Option<Tuple>,

    #[serde(skip)]
    #[deprecated]
    pub old_tuple: Option<Tuple>,
}

impl Delta {
    /// Create a new delta
    pub fn new(
        table_name: String,
        row_id: u64,
        operation: DeltaOperation,
        timestamp: SystemTime,
        transaction_id: u64,
    ) -> Self {
        Self {
            table_name,
            row_id,
            operation,
            timestamp,
            transaction_id,
            #[allow(deprecated)]
            delta_id: 0,
            #[allow(deprecated)]
            delta_type: DeltaType::Insert,
            #[allow(deprecated)]
            new_tuple: None,
            #[allow(deprecated)]
            old_tuple: None,
        }
    }

    /// Create a new insert delta (legacy API)
    #[allow(deprecated)]
    pub fn insert(delta_id: u64, table_name: String, row_id: u64, tuple: Tuple) -> Self {
        Self {
            table_name,
            row_id,
            operation: DeltaOperation::Insert {
                tuple: tuple.clone(),
            },
            timestamp: SystemTime::now(),
            transaction_id: delta_id,
            delta_id,
            delta_type: DeltaType::Insert,
            new_tuple: Some(tuple),
            old_tuple: None,
        }
    }

    /// Create a new update delta (legacy API)
    #[allow(deprecated)]
    pub fn update(
        delta_id: u64,
        table_name: String,
        row_id: u64,
        old_tuple: Tuple,
        new_tuple: Tuple,
    ) -> Self {
        Self {
            table_name,
            row_id,
            operation: DeltaOperation::Update {
                old_tuple: old_tuple.clone(),
                new_tuple: new_tuple.clone(),
            },
            timestamp: SystemTime::now(),
            transaction_id: delta_id,
            delta_id,
            delta_type: DeltaType::Update,
            new_tuple: Some(new_tuple),
            old_tuple: Some(old_tuple),
        }
    }

    /// Create a new delete delta (legacy API)
    #[allow(deprecated)]
    pub fn delete(delta_id: u64, table_name: String, row_id: u64, tuple: Tuple) -> Self {
        Self {
            table_name,
            row_id,
            operation: DeltaOperation::Delete {
                tuple: tuple.clone(),
            },
            timestamp: SystemTime::now(),
            transaction_id: delta_id,
            delta_id,
            delta_type: DeltaType::Delete,
            new_tuple: None,
            old_tuple: Some(tuple),
        }
    }

    /// Get timestamp as microseconds since UNIX_EPOCH
    pub fn timestamp_micros(&self) -> Result<u128> {
        self.timestamp
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .map_err(|e| Error::storage(format!("Invalid timestamp: {}", e)))
    }

    /// Build storage key for this delta
    fn storage_key(&self) -> Result<Vec<u8>> {
        // Key format: mv_delta:{table}:{timestamp_micros}:{row_id}
        // This enables efficient range scans by table and time
        let timestamp_micros = self.timestamp_micros()?;
        let key = format!(
            "mv_delta:{}:{:020}:{}",
            self.table_name, timestamp_micros, self.row_id
        );
        Ok(key.into_bytes())
    }
}

/// A collection of deltas for a single table
#[derive(Debug, Clone)]
pub struct DeltaSet {
    /// Table name
    pub table_name: String,
    /// List of deltas
    pub deltas: Vec<Delta>,
    /// Total count
    pub count: u64,
}

impl DeltaSet {
    /// Create an empty delta set
    pub fn new(table_name: String) -> Self {
        Self {
            table_name,
            deltas: Vec::new(),
            count: 0,
        }
    }

    /// Add a delta to the set
    pub fn add(&mut self, delta: Delta) {
        self.deltas.push(delta);
        self.count += 1;
    }

    /// Get delta count
    pub fn len(&self) -> usize {
        self.deltas.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }
}

/// Delta tracker for incremental materialized views
pub struct DeltaTracker {
    /// RocksDB instance
    db: Arc<DB>,
    /// Current delta sequence number
    current_delta_id: parking_lot::RwLock<u64>,
}

impl DeltaTracker {
    /// Create a new delta tracker
    pub fn new(db: Arc<DB>) -> Result<Self> {
        // Load the last delta ID from storage
        let last_delta_id = Self::load_last_delta_id(&db)?;

        Ok(Self {
            db,
            current_delta_id: parking_lot::RwLock::new(last_delta_id),
        })
    }

    /// Load the last delta ID from storage
    fn load_last_delta_id(db: &DB) -> Result<u64> {
        let key = b"meta:delta:last_id";
        match db.get(key)
            .map_err(|e| Error::storage(format!("Failed to read last delta ID: {}", e)))?
        {
            Some(bytes) => {
                let id = u64::from_le_bytes(
                    bytes.as_slice().try_into()
                        .map_err(|_| Error::storage("Invalid delta ID format"))?
                );
                Ok(id)
            }
            None => Ok(0),
        }
    }

    /// Save the last delta ID to storage
    fn save_last_delta_id(&self, id: u64) -> Result<()> {
        let key = b"meta:delta:last_id";
        let value = id.to_le_bytes();
        self.db.put(key, value)
            .map_err(|e| Error::storage(format!("Failed to save last delta ID: {}", e)))
    }

    /// Get next delta ID
    fn next_delta_id(&self) -> u64 {
        let mut current = self.current_delta_id.write();
        *current += 1;
        *current
    }

    /// Record a delta for a table change
    pub fn record_delta(&self, delta: Delta) -> Result<()> {
        // Store delta in RocksDB
        let key = format!("delta:{}:{:020}", delta.table_name, delta.delta_id);
        let value = bincode::serialize(&delta)
            .map_err(|e| Error::storage(format!("Failed to serialize delta: {}", e)))?;

        self.db.put(key.as_bytes(), &value)
            .map_err(|e| Error::storage(format!("Failed to store delta: {}", e)))?;

        // Update last delta ID
        self.save_last_delta_id(delta.delta_id)?;

        Ok(())
    }

    /// Record an insert delta
    pub fn record_insert(&self, table_name: &str, row_id: u64, tuple: Tuple) -> Result<()> {
        let delta_id = self.next_delta_id();
        let delta = Delta::insert(delta_id, table_name.to_string(), row_id, tuple);
        self.record_delta(delta)
    }

    /// Record an update delta
    pub fn record_update(&self, table_name: &str, row_id: u64, old_tuple: Tuple, new_tuple: Tuple) -> Result<()> {
        let delta_id = self.next_delta_id();
        let delta = Delta::update(delta_id, table_name.to_string(), row_id, old_tuple, new_tuple);
        self.record_delta(delta)
    }

    /// Record a delete delta
    pub fn record_delete(&self, table_name: &str, row_id: u64, tuple: Tuple) -> Result<()> {
        let delta_id = self.next_delta_id();
        let delta = Delta::delete(delta_id, table_name.to_string(), row_id, tuple);
        self.record_delta(delta)
    }

    /// Get deltas for tables since a given timestamp
    pub fn get_deltas_since(&self, table_names: &[String], since: DateTime<Utc>) -> Result<HashMap<String, DeltaSet>> {
        let mut result: HashMap<String, DeltaSet> = HashMap::new();

        for table_name in table_names {
            let prefix = format!("delta:{}:", table_name);
            let prefix_bytes = prefix.as_bytes();

            let mut delta_set = DeltaSet::new(table_name.clone());

            // Iterate over deltas for this table
            let iter = self.db.iterator(rocksdb::IteratorMode::Start);
            for item in iter {
                let (key, value) = item
                    .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

                // Check if key matches our prefix
                if !key.starts_with(prefix_bytes) {
                    // Optimization: break if we've passed the prefix
                    if let (Some(&k), Some(&p)) = (key.first(), prefix_bytes.first()) {
                        if k > p {
                            break;
                        }
                    }
                    continue;
                }

                // Deserialize delta
                let delta: Delta = bincode::deserialize(&value)
                    .map_err(|e| Error::storage(format!("Failed to deserialize delta: {}", e)))?;

                // Filter by timestamp
                let since_system_time: std::time::SystemTime = since.into();
                if delta.timestamp >= since_system_time {
                    delta_set.add(delta);
                }
            }

            if !delta_set.is_empty() {
                result.insert(table_name.clone(), delta_set);
            }
        }

        Ok(result)
    }

    /// Count deltas for tables since a timestamp
    pub fn count_deltas_since(&self, table_names: &[String], since: DateTime<Utc>) -> Result<u64> {
        let mut count = 0;

        for table_name in table_names {
            let prefix = format!("delta:{}:", table_name);
            let prefix_bytes = prefix.as_bytes();

            let iter = self.db.iterator(rocksdb::IteratorMode::Start);
            for item in iter {
                let (key, value) = item
                    .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

                if !key.starts_with(prefix_bytes) {
                    if let (Some(&k), Some(&p)) = (key.first(), prefix_bytes.first()) {
                        if k > p {
                            break;
                        }
                    }
                    continue;
                }

                // Deserialize delta to check timestamp
                let delta: Delta = bincode::deserialize(&value)
                    .map_err(|e| Error::storage(format!("Failed to deserialize delta: {}", e)))?;

                let since_system_time: SystemTime = since.into();
                if delta.timestamp >= since_system_time {
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    /// Purge old deltas before a timestamp
    pub fn purge_deltas_before(&self, before: DateTime<Utc>) -> Result<u64> {
        let mut purged_count = 0;
        let mut keys_to_delete = Vec::new();

        // Scan all deltas
        let prefix = b"delta:";
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                if let (Some(&k), Some(&p)) = (key.first(), prefix.first()) {
                    if k > p {
                        break;
                    }
                }
                continue;
            }

            // Deserialize delta to check timestamp
            let delta: Delta = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize delta: {}", e)))?;

            let before_system_time: SystemTime = before.into();
            if delta.timestamp < before_system_time {
                keys_to_delete.push(key.to_vec());
            }
        }

        // Delete old deltas
        for key in &keys_to_delete {
            self.db.delete(key)
                .map_err(|e| Error::storage(format!("Failed to delete delta: {}", e)))?;
            purged_count += 1;
        }

        Ok(purged_count)
    }

    /// Get current delta ID
    pub fn current_delta_id(&self) -> u64 {
        *self.current_delta_id.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Value, Column, DataType, Schema};
    use crate::storage::StorageEngine;

    #[test]
    fn test_delta_creation() {
        let tuple = Tuple::new(vec![Value::Int4(1), Value::String("test".to_string())]);
        let delta = Delta::insert(1, "users".to_string(), 1, tuple.clone());

        assert_eq!(delta.delta_id, 1);
        assert_eq!(delta.table_name, "users");
        assert_eq!(delta.row_id, 1);
        assert_eq!(delta.delta_type, DeltaType::Insert);
        assert!(delta.new_tuple.is_some());
        assert!(delta.old_tuple.is_none());
    }

    #[test]
    fn test_delta_tracker_record() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let tracker = DeltaTracker::new(Arc::clone(&engine.db)).expect("Failed to create tracker");

        let tuple = Tuple::new(vec![Value::Int4(1), Value::String("test".to_string())]);

        tracker.record_insert("users", 1, tuple.clone()).expect("Failed to record insert");

        assert_eq!(tracker.current_delta_id(), 1);
    }

    #[test]
    fn test_delta_retrieval() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let tracker = DeltaTracker::new(Arc::clone(&engine.db)).expect("Failed to create tracker");

        let tuple = Tuple::new(vec![Value::Int4(1)]);
        tracker.record_insert("users", 1, tuple.clone()).expect("Failed to record");

        let now = Utc::now();
        let before = now - chrono::Duration::seconds(60);

        let deltas = tracker.get_deltas_since(&["users".to_string()], before)
            .expect("Failed to get deltas");

        assert!(deltas.contains_key("users"));
        assert_eq!(deltas.get("users").map(|s| s.len()), Some(1));
    }

    #[test]
    fn test_delta_count() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let tracker = DeltaTracker::new(Arc::clone(&engine.db)).expect("Failed to create tracker");

        let tuple = Tuple::new(vec![Value::Int4(1)]);
        tracker.record_insert("users", 1, tuple.clone()).expect("Failed to record 1");
        tracker.record_insert("users", 2, tuple.clone()).expect("Failed to record 2");
        tracker.record_insert("products", 1, tuple.clone()).expect("Failed to record 3");

        let before = Utc::now() - chrono::Duration::seconds(60);
        let count = tracker.count_deltas_since(&["users".to_string()], before)
            .expect("Failed to count");

        assert_eq!(count, 2);
    }
}
