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

use super::write_gate::WriteGate;
use crate::crypto::KeyManager;
use crate::{Error, Result, Tuple};
use chrono::{DateTime, Utc};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Permanent, bounded allocator evidence. Each lane is monotone independently;
/// a delayed producer never overwrites another lane's higher durable ID.
pub(crate) const DELTA_ID_LANES: usize = 64;
pub(crate) const DELTA_FLOOR_PREFIX: &[u8] = b"meta:delta:floor:";
pub(crate) const LEGACY_DELTA_ID_KEY: &[u8] = b"meta:delta:last_id";

pub(crate) fn delta_key_id(key: &[u8]) -> Option<u64> {
    let suffix = key.strip_prefix(b"delta:")?;
    let split = suffix.iter().rposition(|byte| *byte == b':')?;
    let digits = suffix.get(split + 1..)?;
    if split == 0 || digits.len() != 20 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

pub(crate) fn delta_floor_key(lane: usize) -> String {
    format!("meta:delta:floor:{lane:02}")
}

pub(crate) fn delta_floor_lane(key: &[u8]) -> Option<usize> {
    let suffix = key.strip_prefix(DELTA_FLOOR_PREFIX)?;
    if suffix.len() != 2 || !suffix.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let lane = usize::from(suffix[0] - b'0') * 10 + usize::from(suffix[1] - b'0');
    (lane < DELTA_ID_LANES).then_some(lane)
}

pub(crate) fn decode_delta_id(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| Error::storage("Invalid delta ID format"))?,
    ))
}

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
            operation: DeltaOperation::Insert { tuple: tuple.clone() },
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
    pub fn update(delta_id: u64, table_name: String, row_id: u64, old_tuple: Tuple, new_tuple: Tuple) -> Self {
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
            operation: DeltaOperation::Delete { tuple: tuple.clone() },
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
        let key = format!("mv_delta:{}:{:020}:{}", self.table_name, timestamp_micros, self.row_id);
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
    /// RocksDB instance. Written only through `gate`.
    db: Arc<DB>,
    /// The write gate of the engine that owns `db`.
    gate: Arc<WriteGate>,
    /// TDE key manager, mirroring `StorageEngine::key_manager`.
    ///
    /// This type writes `delta:` records straight to the `Arc<DB>` it holds, so
    /// `StorageEngine::put` is never on its path and the storage boundary's rule
    /// has to be applied here. A `Delta` carries whole `Tuple`s — including the
    /// contents of a row a `DELETE` has just removed — so `delta:` is one of the
    /// keyspaces `storage::tde::is_row_value_key` seals.
    ///
    /// `None` (the default configuration) makes every use a single `Option`
    /// check with no allocation and no copy.
    key_manager: Option<Arc<KeyManager>>,
}

impl DeltaTracker {
    /// Create a new delta tracker with no key manager.
    ///
    /// Delegates to [`DeltaTracker::new_with_key_manager`]; both engine
    /// constructors use that one, so this is the shape a caller outside the
    /// engine gets. `gate` is the write gate of the engine that owns `db`
    /// (`WriteGate::open()` for a handle the caller owns outright).
    pub fn new(db: Arc<DB>, gate: Arc<WriteGate>) -> Result<Self> {
        Self::new_with_key_manager(db, gate, None)
    }

    /// Create a delta tracker that seals its records under `key_manager`.
    ///
    /// The key is taken at construction rather than through a setter so that a
    /// tracker can never be published to the engine in a state where it would
    /// write `delta:` records in a different form from the one its own readers
    /// expect.
    pub fn new_with_key_manager(
        db: Arc<DB>,
        gate: Arc<WriteGate>,
        key_manager: Option<Arc<KeyManager>>,
    ) -> Result<Self> {
        // Hydration is read-only, including a physical standby. Multiple tracker
        // handles on one gate share the allocator and cannot issue duplicate IDs.
        let last_delta_id = Self::load_last_delta_id(&db)?;
        gate.install_delta_ids(last_delta_id);
        Ok(Self { db, gate, key_manager })
    }

    /// Recover all surviving allocator evidence. Old producers could commit their
    /// last_id markers out of order, so retained key IDs are authoritative too.
    /// New floor keys are never removed by delta cleanup, including an empty log.
    fn load_last_delta_id(db: &DB) -> Result<u64> {
        let mut maximum = db
            .get(LEGACY_DELTA_ID_KEY)?
            .as_deref()
            .map(decode_delta_id)
            .transpose()?
            .unwrap_or(0);
        for prefix in [DELTA_FLOOR_PREFIX, b"delta:".as_slice()] {
            let mut options = rocksdb::ReadOptions::default();
            options.set_total_order_seek(true);
            let mut end = prefix.to_vec();
            if let Some(last) = end.last_mut() {
                *last += 1;
            }
            options.set_iterate_upper_bound(end);
            let mut iter = db.raw_iterator_opt(options);
            iter.seek(prefix);
            while iter.valid() {
                let key = iter
                    .key()
                    .ok_or_else(|| Error::storage("Missing delta allocator key"))?;
                if !key.starts_with(prefix) {
                    break;
                }
                let id = if prefix == DELTA_FLOOR_PREFIX {
                    if delta_floor_lane(key).is_none() {
                        return Err(Error::storage("Invalid delta allocator floor key"));
                    }
                    decode_delta_id(
                        iter.value()
                            .ok_or_else(|| Error::storage("Missing delta allocator floor"))?,
                    )?
                } else {
                    delta_key_id(key).ok_or_else(|| Error::storage("Invalid delta record key"))?
                };
                maximum = maximum.max(id);
                iter.next();
            }
            iter.status()?;
        }
        Ok(maximum)
    }

    fn next_delta_id(&self) -> Result<u64> {
        self.gate.next_delta_id()
    }

    pub(crate) fn recover_durable_ids(&self) -> Result<()> {
        self.gate.observe_delta_id(Self::load_last_delta_id(&self.db)?);
        Ok(())
    }

    /// Physical publication only raises volatile state; it never changes source
    /// bytes or creates allocator markers on a standby.
    pub(crate) fn observe_physical_id(&self, id: u64) {
        self.gate.observe_delta_id(id);
    }

    /// Record a delta for a table change.
    ///
    /// Allocates the storage-key id when the caller has not set one. `delta_id`
    /// is `#[deprecated]` *and* `#[serde(skip)]` — it is never persisted in the
    /// value, it only builds the key — and `Delta::new`, the **only** constructor
    /// that accepts a timestamp, hardcodes it to 0. So every delta built that way
    /// keyed `delta:<table>:00000000000000000000` and silently overwrote the
    /// previous one: a caller recording N timestamped deltas for a table read
    /// back exactly one, with no error.
    ///
    /// 0 is a safe "unset" sentinel: `next_delta_id` pre-increments, so allocated
    /// ids start at 1 and 0 is never issued. Callers that do pass an id
    /// (`record_insert`/`record_update`/`record_delete`) keep their existing
    /// numbering untouched.
    pub fn record_delta(&self, delta: Delta) -> Result<()> {
        #[allow(deprecated)]
        let delta_id = if delta.delta_id == 0 {
            self.next_delta_id()?
        } else {
            self.gate.observe_delta_id(delta.delta_id);
            delta.delta_id
        };

        // Store delta in RocksDB
        let key = format!("delta:{}:{:020}", delta.table_name, delta_id);
        let value =
            bincode::serialize(&delta).map_err(|e| Error::storage(format!("Failed to serialize delta: {}", e)))?;

        // THE STORAGE BOUNDARY for this keyspace. A `Delta` is a full row image
        // (see the `delta:` entry in `storage::tde::is_row_value_key`), so it is
        // sealed by the same key-driven rule as `data:` — and `Cow::Borrowed`,
        // i.e. byte-for-byte the call below made before, when encryption is off.
        let stored = super::tde::seal_row_value(self.key_manager.as_deref(), key.as_bytes(), &value)?;

        let mut batch = rocksdb::WriteBatch::default();
        batch.put(key.as_bytes(), stored.as_ref());
        // The gate adds a permanent monotone floor in this same write group.
        self.gate.write(&self.db, batch, None)?;

        Ok(())
    }

    /// Spec R5.4 (F1.2): [`Self::record_insert`] into the caller's write group: the
    /// delta is put into `batch`; the gate adds its allocator floor to that same
    /// group, instead of separate writes after the row.
    pub fn stage_insert(
        &self,
        batch: &mut rocksdb::WriteBatch,
        table_name: &str,
        row_id: u64,
        tuple: Tuple,
    ) -> Result<()> {
        let delta_id = self.next_delta_id()?;
        let delta = Delta::insert(delta_id, table_name.to_string(), row_id, tuple);
        let key = format!("delta:{}:{:020}", table_name, delta_id);
        let value =
            bincode::serialize(&delta).map_err(|e| Error::storage(format!("Failed to serialize delta: {}", e)))?;
        // The same storage boundary as `record_delta`.
        let stored = super::tde::seal_row_value(self.key_manager.as_deref(), key.as_bytes(), &value)?;
        batch.put(key.as_bytes(), stored.as_ref());
        Ok(())
    }

    /// Record an insert delta
    pub fn record_insert(&self, table_name: &str, row_id: u64, tuple: Tuple) -> Result<()> {
        let delta_id = self.next_delta_id()?;
        let delta = Delta::insert(delta_id, table_name.to_string(), row_id, tuple);
        self.record_delta(delta)
    }

    /// Record an update delta
    pub fn record_update(&self, table_name: &str, row_id: u64, old_tuple: Tuple, new_tuple: Tuple) -> Result<()> {
        let delta_id = self.next_delta_id()?;
        let delta = Delta::update(delta_id, table_name.to_string(), row_id, old_tuple, new_tuple);
        self.record_delta(delta)
    }

    /// Record a delete delta
    pub fn record_delete(&self, table_name: &str, row_id: u64, tuple: Tuple) -> Result<()> {
        let delta_id = self.next_delta_id()?;
        let delta = Delta::delete(delta_id, table_name.to_string(), row_id, tuple);
        self.record_delta(delta)
    }

    /// Generic storage mutations carry their delta in the row's transaction,
    /// including when the write applies but a later durability wait fails.
    pub(crate) fn stage_mutation_in_transaction(
        &self,
        txn: &super::transaction::Transaction,
        table: &str,
        row: u64,
        old: Tuple,
        new: Option<Tuple>,
    ) -> Result<()> {
        let id = self.next_delta_id()?;
        let delta = match new {
            Some(new) => Delta::update(id, table.to_string(), row, old, new),
            None => Delta::delete(id, table.to_string(), row, old),
        };
        let key = format!("delta:{table}:{id:020}");
        let value =
            bincode::serialize(&delta).map_err(|e| Error::storage(format!("Failed to serialize delta: {e}")))?;
        txn.put(key.into_bytes(), value)
    }

    /// Get deltas for tables since a given timestamp
    pub fn get_deltas_since(&self, table_names: &[String], since: DateTime<Utc>) -> Result<HashMap<String, DeltaSet>> {
        let mut result: HashMap<String, DeltaSet> = HashMap::new();

        for table_name in table_names {
            let prefix = format!("delta:{}:", table_name);
            let prefix_bytes = prefix.as_bytes();

            let mut delta_set = DeltaSet::new(table_name.clone());

            // Seek straight to the `delta:{table}:` prefix instead of iterating
            // from the start of the keyspace (which walked every `data:` row key
            // before reaching the deltas). total_order_seek is needed because the
            // DB uses a fixed 5-byte prefix extractor.
            let mut read_opts = rocksdb::ReadOptions::default();
            read_opts.set_total_order_seek(true);
            let iter = self.db.iterator_opt(
                rocksdb::IteratorMode::From(prefix_bytes, rocksdb::Direction::Forward),
                read_opts,
            );
            for item in iter {
                let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

                // Stop at the first key past this prefix.
                if !key.starts_with(prefix_bytes) {
                    break;
                }

                // Decode through the same rule `record_delta` seals with.
                let stored = super::tde::open_ref(self.key_manager.as_deref(), &key, &value)?;
                let delta: Delta = bincode::deserialize(&stored)
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

            let mut read_opts = rocksdb::ReadOptions::default();
            read_opts.set_total_order_seek(true);
            let iter = self.db.iterator_opt(
                rocksdb::IteratorMode::From(prefix_bytes, rocksdb::Direction::Forward),
                read_opts,
            );
            for item in iter {
                let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

                if !key.starts_with(prefix_bytes) {
                    break;
                }

                // Decode through the same rule `record_delta` seals with.
                let stored = super::tde::open_ref(self.key_manager.as_deref(), &key, &value)?;
                let delta: Delta = bincode::deserialize(&stored)
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

        // Seek to the `delta:` prefix rather than scanning the whole keyspace.
        let prefix = b"delta:";
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_slice(), rocksdb::Direction::Forward),
            read_opts,
        );

        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                break;
            }

            // Decode through the same rule `record_delta` seals with.
            let stored = super::tde::open_ref(self.key_manager.as_deref(), &key, &value)?;
            let delta: Delta = bincode::deserialize(&stored)
                .map_err(|e| Error::storage(format!("Failed to deserialize delta: {}", e)))?;

            let before_system_time: SystemTime = before.into();
            if delta.timestamp < before_system_time {
                keys_to_delete.push(key.to_vec());
            }
        }

        // Delete old deltas
        for key in &keys_to_delete {
            self.gate
                .delete(&self.db, key, None)
                .map_err(|e| Error::storage(format!("Failed to delete delta: {}", e)))?;
            purged_count += 1;
        }

        Ok(purged_count)
    }

    /// Get current delta ID
    pub fn current_delta_id(&self) -> u64 {
        self.gate.current_delta_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageEngine;
    use crate::{Column, Config, DataType, Schema, Value};

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
        let tracker = DeltaTracker::new(Arc::clone(&engine.db), Arc::clone(engine.write_gate()))
            .expect("Failed to create tracker");

        let tuple = Tuple::new(vec![Value::Int4(1), Value::String("test".to_string())]);

        tracker
            .record_insert("users", 1, tuple.clone())
            .expect("Failed to record insert");

        assert_eq!(tracker.current_delta_id(), 1);
    }

    #[test]
    fn test_delta_retrieval() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let tracker = DeltaTracker::new(Arc::clone(&engine.db), Arc::clone(engine.write_gate()))
            .expect("Failed to create tracker");

        let tuple = Tuple::new(vec![Value::Int4(1)]);
        tracker
            .record_insert("users", 1, tuple.clone())
            .expect("Failed to record");

        let now = Utc::now();
        let before = now - chrono::Duration::seconds(60);

        let deltas = tracker
            .get_deltas_since(&["users".to_string()], before)
            .expect("Failed to get deltas");

        assert!(deltas.contains_key("users"));
        assert_eq!(deltas.get("users").map(|s| s.len()), Some(1));
    }

    #[test]
    fn test_delta_count() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let tracker = DeltaTracker::new(Arc::clone(&engine.db), Arc::clone(engine.write_gate()))
            .expect("Failed to create tracker");

        let tuple = Tuple::new(vec![Value::Int4(1)]);
        tracker
            .record_insert("users", 1, tuple.clone())
            .expect("Failed to record 1");
        tracker
            .record_insert("users", 2, tuple.clone())
            .expect("Failed to record 2");
        tracker
            .record_insert("products", 1, tuple.clone())
            .expect("Failed to record 3");

        let before = Utc::now() - chrono::Duration::seconds(60);
        let count = tracker
            .count_deltas_since(&["users".to_string()], before)
            .expect("Failed to count");

        assert_eq!(count, 2);
    }

    /// A `Delta` carries the whole row, including the row a DELETE removed, so
    /// `delta:` is a sealed keyspace. Asserted on the STORED bytes, because a
    /// record-then-read-back round trip passes whether or not anything was
    /// sealed.
    #[test]
    fn delta_records_are_sealed_on_disk_and_still_read_back() {
        let config = Config::in_memory();
        let engine = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let km = Arc::new(crate::crypto::KeyManager::generate_random());
        let tracker = DeltaTracker::new_with_key_manager(
            Arc::clone(&engine.db),
            Arc::clone(engine.write_gate()),
            Some(Arc::clone(&km)),
        )
        .expect("Failed to create tracker");

        const SECRET: &str = "QZX-MV-DELTA-DELETED-ROW-5501";
        let tuple = Tuple::new(vec![Value::Int4(1), Value::String(SECRET.to_string())]);
        tracker.record_delete("users", 1, tuple.clone()).expect("record delete");

        // The deleted row's contents must not be on disk in the clear.
        let stored = engine
            .db
            .get(b"delta:users:00000000000000000001")
            .expect("get")
            .expect("the delta record must exist");
        assert!(
            !stored.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
            "*** ROW DATA IN THE CLEAR *** the deleted row's contents were found verbatim in the \
             stored `delta:` record"
        );

        // …and the tracker still reads its own records.
        let deltas = tracker
            .get_deltas_since(&["users".to_string()], Utc::now() - chrono::Duration::seconds(60))
            .expect("read back");
        let set = deltas.get("users").expect("a delta set for the table");
        assert_eq!(set.len(), 1);
        let recorded = set.deltas.first().expect("one delta");
        assert!(
            matches!(&recorded.operation, DeltaOperation::Delete { tuple: t } if t == &tuple),
            "the sealed record must decode back to the row it was written from"
        );
    }
}
