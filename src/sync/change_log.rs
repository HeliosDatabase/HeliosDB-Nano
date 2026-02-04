//! Change Log System for HeliosDB-Lite v2.3.0 Sync Protocol
//!
//! Comprehensive change capture system that logs all data mutations for replication.
//! Provides efficient storage, querying, and compaction of change entries with
//! production-ready error handling and thread-safe operations.
//!
//! # Features
//!
//! - Captures all DML operations (INSERT, UPDATE, DELETE)
//! - Captures DDL operations (CREATE TABLE, DROP TABLE)
//! - Vector clock integration for conflict detection
//! - Efficient LSN-based indexing
//! - Table-specific change queries
//! - Automatic compaction of old entries
//! - Real-time change notifications (optional)
//! - Thread-safe concurrent access
//!
//! # Storage Layout
//!
//! - `change_log:{lsn}` → bincode-serialized ChangeEntry
//! - `change_index:{table}:{timestamp}` → lsn (for table-specific queries)
//! - `change_meta:current_lsn` → current LSN counter
//! - `change_meta:compaction_watermark` → compaction watermark LSN
//!
//! # Performance Characteristics
//!
//! - Append: O(1) - single RocksDB write
//! - Query by LSN: O(1) - direct lookup
//! - Query by table: O(n) - sequential scan with prefix filtering
//! - Query by time range: O(n) - sequential scan with timestamp filtering
//! - Compaction: O(m) - where m = number of entries to delete
//!
//! # Example Usage
//!
//! ```ignore
//! use heliosdb_lite::sync::change_log::{ChangeLog, ChangeType, ChangeEntry};
//! use std::sync::Arc;
//! use rocksdb::DB;
//!
//! let db = Arc::new(DB::open_default("/path/to/db")?);
//! let mut change_log = ChangeLog::new(db)?;
//!
//! // Append a change
//! let change = ChangeType::Insert {
//!     table: "users".to_string(),
//!     row_id: 42,
//!     data: vec![1, 2, 3],
//! };
//! let lsn = change_log.append(1, change, vector_clock)?;
//!
//! // Query changes since LSN
//! let changes = change_log.query_since_lsn(0, None)?;
//!
//! // Compact old entries
//! change_log.compact(100)?;
//! ```

use super::VectorClock;
use crate::types::Schema;
use crate::{Error, Result};
use rocksdb::{DB, WriteBatch, IteratorMode, ReadOptions};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn, error};

/// Change type enumeration representing all possible mutations
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChangeType {
    /// Insert operation - new row added to table
    Insert {
        /// Table name
        table: String,
        /// Row ID (unique identifier)
        row_id: u64,
        /// Serialized row data
        data: Vec<u8>,
    },
    /// Update operation - existing row modified
    Update {
        /// Table name
        table: String,
        /// Row ID (unique identifier)
        row_id: u64,
        /// Previous row data (for rollback)
        old_data: Vec<u8>,
        /// New row data
        new_data: Vec<u8>,
    },
    /// Delete operation - row removed from table
    Delete {
        /// Table name
        table: String,
        /// Row ID (unique identifier)
        row_id: u64,
        /// Deleted row data (for rollback)
        data: Vec<u8>,
    },
    /// Create table operation - new table schema defined
    CreateTable {
        /// Table name
        table: String,
        /// Serialized schema
        schema: Schema,
    },
    /// Drop table operation - table removed from database
    DropTable {
        /// Table name
        table: String,
    },
}

impl ChangeType {
    /// Get the table name associated with this change
    pub fn table_name(&self) -> &str {
        match self {
            ChangeType::Insert { table, .. } => table,
            ChangeType::Update { table, .. } => table,
            ChangeType::Delete { table, .. } => table,
            ChangeType::CreateTable { table, .. } => table,
            ChangeType::DropTable { table } => table,
        }
    }

    /// Check if this is a DDL operation
    pub fn is_ddl(&self) -> bool {
        matches!(self, ChangeType::CreateTable { .. } | ChangeType::DropTable { .. })
    }

    /// Check if this is a DML operation
    pub fn is_dml(&self) -> bool {
        !self.is_ddl()
    }

    /// Get the affected row ID if applicable
    pub fn row_id(&self) -> Option<u64> {
        match self {
            ChangeType::Insert { row_id, .. } => Some(*row_id),
            ChangeType::Update { row_id, .. } => Some(*row_id),
            ChangeType::Delete { row_id, .. } => Some(*row_id),
            _ => None,
        }
    }
}

/// Change entry with full metadata
///
/// Represents a single change in the change log with all necessary
/// metadata for replication, conflict detection, and recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// Log Sequence Number (LSN) - monotonically increasing unique ID
    pub lsn: u64,
    /// Timestamp in microseconds since UNIX epoch
    pub timestamp: u64,
    /// Transaction ID that produced this change
    pub transaction_id: u64,
    /// Type of change and associated data
    pub change_type: ChangeType,
    /// Vector clock for causality tracking and conflict detection
    pub vector_clock: VectorClock,
}

impl ChangeEntry {
    /// Create a new change entry
    ///
    /// # Arguments
    ///
    /// * `lsn` - Log sequence number (should be unique and monotonic)
    /// * `transaction_id` - ID of the transaction that produced this change
    /// * `change_type` - Type of change and associated data
    /// * `vector_clock` - Vector clock for conflict detection
    ///
    /// # Returns
    ///
    /// A new ChangeEntry with current timestamp
    pub fn new(
        lsn: u64,
        transaction_id: u64,
        change_type: ChangeType,
        vector_clock: VectorClock,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);

        Self {
            lsn,
            timestamp,
            transaction_id,
            change_type,
            vector_clock,
        }
    }

    /// Serialize entry to bytes using bincode
    pub fn serialize(&self) -> Result<Vec<u8>> {
        bincode::serialize(self)
            .map_err(|e| Error::storage(format!("Failed to serialize change entry: {}", e)))
    }

    /// Deserialize entry from bytes using bincode
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data)
            .map_err(|e| Error::storage(format!("Failed to deserialize change entry: {}", e)))
    }

    /// Get table name for this entry
    pub fn table_name(&self) -> &str {
        self.change_type.table_name()
    }
}

/// Query options for filtering change log entries
#[derive(Debug, Clone)]
pub struct QueryOptions {
    /// Start LSN (inclusive)
    pub start_lsn: Option<u64>,
    /// End LSN (inclusive)
    pub end_lsn: Option<u64>,
    /// Filter by table name
    pub table: Option<String>,
    /// Start timestamp (inclusive)
    pub start_timestamp: Option<u64>,
    /// End timestamp (inclusive)
    pub end_timestamp: Option<u64>,
    /// Maximum number of entries to return
    pub limit: Option<usize>,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            start_lsn: None,
            end_lsn: None,
            table: None,
            start_timestamp: None,
            end_timestamp: None,
            limit: None,
        }
    }
}

impl QueryOptions {
    /// Create a new query with default options
    pub fn new() -> Self {
        Self::default()
    }

    /// Set start LSN filter
    pub fn with_start_lsn(mut self, lsn: u64) -> Self {
        self.start_lsn = Some(lsn);
        self
    }

    /// Set end LSN filter
    pub fn with_end_lsn(mut self, lsn: u64) -> Self {
        self.end_lsn = Some(lsn);
        self
    }

    /// Set table filter
    pub fn with_table(mut self, table: String) -> Self {
        self.table = Some(table);
        self
    }

    /// Set timestamp range filter
    pub fn with_timestamp_range(mut self, start: u64, end: u64) -> Self {
        self.start_timestamp = Some(start);
        self.end_timestamp = Some(end);
        self
    }

    /// Set result limit
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Check if an entry matches the filter criteria
    fn matches(&self, entry: &ChangeEntry) -> bool {
        // Check LSN range
        if let Some(start) = self.start_lsn {
            if entry.lsn < start {
                return false;
            }
        }
        if let Some(end) = self.end_lsn {
            if entry.lsn > end {
                return false;
            }
        }

        // Check table filter
        if let Some(ref table) = self.table {
            if entry.table_name() != table {
                return false;
            }
        }

        // Check timestamp range
        if let Some(start) = self.start_timestamp {
            if entry.timestamp < start {
                return false;
            }
        }
        if let Some(end) = self.end_timestamp {
            if entry.timestamp > end {
                return false;
            }
        }

        true
    }
}

/// Change log statistics
#[derive(Debug, Clone, Default)]
pub struct ChangeLogStats {
    /// Total number of entries in the log
    pub total_entries: u64,
    /// Current LSN (highest assigned)
    pub current_lsn: u64,
    /// Compaction watermark LSN
    pub compaction_watermark: u64,
    /// Oldest entry LSN
    pub oldest_lsn: Option<u64>,
    /// Oldest entry timestamp
    pub oldest_timestamp: Option<u64>,
    /// Newest entry timestamp
    pub newest_timestamp: Option<u64>,
    /// Estimated size in bytes
    pub estimated_size_bytes: u64,
}

/// Change log implementation
///
/// Provides comprehensive change capture and querying capabilities
/// with production-ready error handling and performance optimization.
pub struct ChangeLog {
    /// RocksDB storage instance
    storage: Arc<DB>,
    /// Current LSN counter (atomic for thread-safe increment)
    current_lsn: Arc<AtomicU64>,
    /// Compaction watermark (entries below this can be deleted)
    compaction_watermark: Arc<AtomicU64>,
}

impl ChangeLog {
    /// Create a new change log instance
    ///
    /// # Arguments
    ///
    /// * `storage` - RocksDB instance for persistent storage
    ///
    /// # Returns
    ///
    /// A new ChangeLog instance with initialized LSN and watermark
    ///
    /// # Errors
    ///
    /// Returns an error if RocksDB initialization fails
    pub fn new(storage: Arc<DB>) -> Result<Self> {
        debug!("Initializing change log");

        // Load current LSN from storage
        let current_lsn = match storage.get(b"change_meta:current_lsn") {
            Ok(Some(bytes)) => {
                let lsn = u64::from_le_bytes(
                    bytes.as_slice().try_into()
                        .map_err(|e| Error::storage(format!("Invalid LSN format: {:?}", e)))?
                );
                debug!("Loaded current LSN: {}", lsn);
                lsn
            }
            Ok(None) => {
                debug!("No existing LSN, starting at 0");
                0
            }
            Err(e) => {
                return Err(Error::storage(format!("Failed to load current LSN: {}", e)));
            }
        };

        // Load compaction watermark from storage
        let compaction_watermark = match storage.get(b"change_meta:compaction_watermark") {
            Ok(Some(bytes)) => {
                let watermark = u64::from_le_bytes(
                    bytes.as_slice().try_into()
                        .map_err(|e| Error::storage(format!("Invalid watermark format: {:?}", e)))?
                );
                debug!("Loaded compaction watermark: {}", watermark);
                watermark
            }
            Ok(None) => {
                debug!("No existing watermark, starting at 0");
                0
            }
            Err(e) => {
                return Err(Error::storage(format!("Failed to load compaction watermark: {}", e)));
            }
        };

        info!("Change log initialized with LSN={}, watermark={}", current_lsn, compaction_watermark);

        Ok(Self {
            storage,
            current_lsn: Arc::new(AtomicU64::new(current_lsn)),
            compaction_watermark: Arc::new(AtomicU64::new(compaction_watermark)),
        })
    }

    /// Append a change entry to the log
    ///
    /// This method is called from transaction commit to capture changes.
    /// It atomically assigns an LSN, creates the entry, and persists it to storage.
    ///
    /// # Arguments
    ///
    /// * `transaction_id` - ID of the transaction producing this change
    /// * `change_type` - Type of change and associated data
    /// * `vector_clock` - Vector clock for conflict detection
    ///
    /// # Returns
    ///
    /// The assigned LSN for this entry
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or storage operations fail
    pub fn append(
        &self,
        transaction_id: u64,
        change_type: ChangeType,
        vector_clock: VectorClock,
    ) -> Result<u64> {
        // Atomically increment LSN
        let lsn = self.current_lsn.fetch_add(1, Ordering::SeqCst);

        // Create change entry
        let entry = ChangeEntry::new(lsn, transaction_id, change_type, vector_clock);

        debug!("Appending change entry: LSN={}, table={}, tx={}",
               lsn, entry.table_name(), transaction_id);

        // Serialize entry
        let entry_bytes = entry.serialize()?;

        // Create atomic batch for all writes
        let mut batch = WriteBatch::default();

        // Store change entry: change_log:{lsn} → entry
        let entry_key = format!("change_log:{:020}", lsn);
        batch.put(entry_key.as_bytes(), &entry_bytes);

        // Store table index: change_index:{table}:{timestamp} → lsn
        let index_key = format!("change_index:{}:{:020}", entry.table_name(), entry.timestamp);
        batch.put(index_key.as_bytes(), &lsn.to_le_bytes());

        // Update current LSN metadata
        batch.put(b"change_meta:current_lsn", &(lsn + 1).to_le_bytes());

        // Atomic write
        self.storage.write(batch)
            .map_err(|e| Error::storage(format!("Failed to write change entry: {}", e)))?;

        debug!("Successfully appended change entry LSN={}", lsn);

        Ok(lsn)
    }

    /// Query changes since a specific LSN
    ///
    /// Returns all change entries with LSN >= start_lsn in ascending order.
    ///
    /// # Arguments
    ///
    /// * `start_lsn` - Starting LSN (inclusive)
    /// * `limit` - Optional maximum number of entries to return
    ///
    /// # Returns
    ///
    /// Vector of change entries ordered by LSN
    ///
    /// # Errors
    ///
    /// Returns an error if storage or deserialization fails
    pub fn query_since_lsn(&self, start_lsn: u64, limit: Option<usize>) -> Result<Vec<ChangeEntry>> {
        debug!("Querying changes since LSN={} with limit={:?}", start_lsn, limit);

        let mut options = QueryOptions::new().with_start_lsn(start_lsn);
        if let Some(limit_val) = limit {
            options = options.with_limit(limit_val);
        }
        self.query(&options)
    }

    /// Query changes within a time range
    ///
    /// Returns all change entries within the specified timestamp range.
    ///
    /// # Arguments
    ///
    /// * `start_timestamp` - Start timestamp in microseconds (inclusive)
    /// * `end_timestamp` - End timestamp in microseconds (inclusive)
    ///
    /// # Returns
    ///
    /// Vector of change entries ordered by LSN
    ///
    /// # Errors
    ///
    /// Returns an error if storage or deserialization fails
    pub fn query_by_timestamp(&self, start_timestamp: u64, end_timestamp: u64) -> Result<Vec<ChangeEntry>> {
        debug!("Querying changes between timestamps {} and {}", start_timestamp, end_timestamp);

        let options = QueryOptions::new().with_timestamp_range(start_timestamp, end_timestamp);
        self.query(&options)
    }

    /// Query changes for a specific table
    ///
    /// Returns all change entries for the specified table.
    ///
    /// # Arguments
    ///
    /// * `table_name` - Name of the table to query
    ///
    /// # Returns
    ///
    /// Vector of change entries for the specified table ordered by LSN
    ///
    /// # Errors
    ///
    /// Returns an error if storage or deserialization fails
    pub fn query_by_table(&self, table_name: &str) -> Result<Vec<ChangeEntry>> {
        debug!("Querying changes for table '{}'", table_name);

        let options = QueryOptions::new().with_table(table_name.to_string());
        self.query(&options)
    }

    /// Query changes with custom options
    ///
    /// Flexible query method supporting multiple filter criteria.
    ///
    /// # Arguments
    ///
    /// * `options` - Query filter options
    ///
    /// # Returns
    ///
    /// Vector of change entries matching the filter criteria
    ///
    /// # Errors
    ///
    /// Returns an error if storage or deserialization fails
    pub fn query(&self, options: &QueryOptions) -> Result<Vec<ChangeEntry>> {
        let mut entries = Vec::new();
        let prefix = b"change_log:";

        // Iterate over all change log entries
        // Use total_order_seek to bypass prefix bloom filter for full table scans
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.iterator_opt(IteratorMode::Start, read_opts);
        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            // Check if this is a change log entry
            if !key.starts_with(prefix) {
                // Skip non-change-log keys
                continue;
            }

            // Stop if we've moved past change log entries
            if !key.is_empty() && key.len() >= prefix.len() && &key[0..prefix.len()] != prefix {
                break;
            }

            // Deserialize entry
            let entry = ChangeEntry::deserialize(&value)?;

            // Apply filters
            if !options.matches(&entry) {
                continue;
            }

            entries.push(entry);

            // Check limit
            if let Some(limit) = options.limit {
                if entries.len() >= limit {
                    break;
                }
            }
        }

        debug!("Query returned {} entries", entries.len());
        Ok(entries)
    }

    /// Compact the change log by removing entries before the watermark
    ///
    /// Deletes all change entries with LSN < watermark_lsn to free space.
    /// This operation is useful for managing log size and removing old entries
    /// that are no longer needed for replication.
    ///
    /// # Arguments
    ///
    /// * `watermark_lsn` - LSN below which entries will be deleted
    ///
    /// # Returns
    ///
    /// Number of entries deleted
    ///
    /// # Errors
    ///
    /// Returns an error if storage operations fail
    pub fn compact(&self, watermark_lsn: u64) -> Result<usize> {
        info!("Compacting change log up to LSN={}", watermark_lsn);

        let mut deleted_count = 0;
        let mut batch = WriteBatch::default();

        // Iterate through entries below watermark
        let prefix = b"change_log:";
        // Use total_order_seek to bypass prefix bloom filter for full table scans
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.iterator_opt(IteratorMode::Start, read_opts);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error during compaction: {}", e)))?;

            // Only process change log entries
            if !key.starts_with(prefix) {
                continue;
            }

            // Parse LSN from key
            let key_str = std::str::from_utf8(&key)
                .map_err(|e| Error::storage(format!("Invalid key UTF-8: {}", e)))?;

            if let Some(lsn_str) = key_str.strip_prefix("change_log:") {
                if let Ok(lsn) = lsn_str.parse::<u64>() {
                    if lsn < watermark_lsn {
                        // Deserialize to get table name for index cleanup
                        if let Ok(entry) = ChangeEntry::deserialize(&value) {
                            // Delete main entry
                            batch.delete(&key);

                            // Delete index entry
                            let index_key = format!("change_index:{}:{:020}",
                                                  entry.table_name(), entry.timestamp);
                            batch.delete(index_key.as_bytes());

                            deleted_count += 1;

                            // Batch in chunks to avoid too large batches
                            if deleted_count % 1000 == 0 {
                                self.storage.write(batch)
                                    .map_err(|e| Error::storage(format!("Failed to write compaction batch: {}", e)))?;
                                batch = WriteBatch::default();
                                debug!("Compaction progress: {} entries deleted", deleted_count);
                            }
                        }
                    } else {
                        // Entries are ordered by LSN, so we can break early
                        break;
                    }
                }
            }
        }

        // Write final batch
        if deleted_count % 1000 != 0 {
            self.storage.write(batch)
                .map_err(|e| Error::storage(format!("Failed to write final compaction batch: {}", e)))?;
        }

        // Update compaction watermark
        self.compaction_watermark.store(watermark_lsn, Ordering::SeqCst);
        self.storage.put(b"change_meta:compaction_watermark", &watermark_lsn.to_le_bytes())
            .map_err(|e| Error::storage(format!("Failed to update compaction watermark: {}", e)))?;

        info!("Compaction complete: deleted {} entries", deleted_count);

        Ok(deleted_count)
    }

    /// Get the current LSN (highest assigned)
    ///
    /// # Returns
    ///
    /// Current LSN value
    pub fn get_latest_lsn(&self) -> u64 {
        self.current_lsn.load(Ordering::SeqCst)
    }

    /// Get the current compaction watermark
    ///
    /// # Returns
    ///
    /// Current compaction watermark LSN
    pub fn get_compaction_watermark(&self) -> u64 {
        self.compaction_watermark.load(Ordering::SeqCst)
    }

    /// Get a specific change entry by LSN
    ///
    /// # Arguments
    ///
    /// * `lsn` - LSN of the entry to retrieve
    ///
    /// # Returns
    ///
    /// The change entry if found, None otherwise
    ///
    /// # Errors
    ///
    /// Returns an error if storage or deserialization fails
    pub fn get_entry(&self, lsn: u64) -> Result<Option<ChangeEntry>> {
        let key = format!("change_log:{:020}", lsn);

        match self.storage.get(key.as_bytes()) {
            Ok(Some(bytes)) => {
                let entry = ChangeEntry::deserialize(&bytes)?;
                Ok(Some(entry))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::storage(format!("Failed to get entry: {}", e))),
        }
    }

    /// Get change log statistics
    ///
    /// Computes comprehensive statistics about the change log including
    /// entry counts, LSN ranges, timestamps, and estimated size.
    ///
    /// # Returns
    ///
    /// ChangeLogStats with current statistics
    ///
    /// # Errors
    ///
    /// Returns an error if storage operations fail
    pub fn get_stats(&self) -> Result<ChangeLogStats> {
        let mut stats = ChangeLogStats::default();

        stats.current_lsn = self.current_lsn.load(Ordering::SeqCst);
        stats.compaction_watermark = self.compaction_watermark.load(Ordering::SeqCst);

        let prefix = b"change_log:";
        // Use total_order_seek to bypass prefix bloom filter for full table scans
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.iterator_opt(IteratorMode::Start, read_opts);

        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                continue;
            }

            if let Ok(entry) = ChangeEntry::deserialize(&value) {
                stats.total_entries += 1;
                stats.estimated_size_bytes += value.len() as u64;

                // Track oldest entry
                if stats.oldest_lsn.is_none() || Some(entry.lsn) < stats.oldest_lsn {
                    stats.oldest_lsn = Some(entry.lsn);
                    stats.oldest_timestamp = Some(entry.timestamp);
                }

                // Track newest entry
                if stats.newest_timestamp.is_none() || Some(entry.timestamp) > stats.newest_timestamp {
                    stats.newest_timestamp = Some(entry.timestamp);
                }
            }
        }

        Ok(stats)
    }

    /// Reset the change log (for testing purposes)
    ///
    /// WARNING: This deletes all change log data. Use with caution.
    ///
    /// # Errors
    ///
    /// Returns an error if storage operations fail
    #[cfg(test)]
    pub fn reset(&self) -> Result<()> {
        warn!("Resetting change log - all data will be deleted");

        let mut batch = WriteBatch::default();
        let prefixes = [b"change_log:", b"change_index:", b"change_meta:"];

        for prefix in &prefixes {
            // Use total_order_seek to bypass prefix bloom filter for full table scans
            let mut read_opts = ReadOptions::default();
            read_opts.set_total_order_seek(true);
            let iter = self.storage.iterator_opt(IteratorMode::Start, read_opts);
            for item in iter {
                let (key, _) = item
                    .map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

                if key.starts_with(prefix) {
                    batch.delete(&key);
                }
            }
        }

        self.storage.write(batch)
            .map_err(|e| Error::storage(format!("Failed to reset change log: {}", e)))?;

        self.current_lsn.store(0, Ordering::SeqCst);
        self.compaction_watermark.store(0, Ordering::SeqCst);

        info!("Change log reset complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use rocksdb::{DB, Options};

    fn create_test_db() -> (Arc<DB>, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let mut opts = Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, temp_dir.path()).expect("Failed to open DB");
        (Arc::new(db), temp_dir)
    }

    #[test]
    fn test_change_log_new() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        assert_eq!(change_log.get_latest_lsn(), 0);
        assert_eq!(change_log.get_compaction_watermark(), 0);
    }

    #[test]
    fn test_append_and_query() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        let change = ChangeType::Insert {
            table: "users".to_string(),
            row_id: 42,
            data: vec![1, 2, 3],
        };

        let vector_clock = VectorClock::new();
        let lsn = change_log.append(1, change, vector_clock)
            .expect("Failed to append");

        assert_eq!(lsn, 0);
        assert_eq!(change_log.get_latest_lsn(), 1);

        let entries = change_log.query_since_lsn(0, None)
            .expect("Failed to query");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lsn, 0);
        assert_eq!(entries[0].transaction_id, 1);
    }

    #[test]
    fn test_multiple_appends() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        for i in 0..10 {
            let change = ChangeType::Insert {
                table: format!("table_{}", i % 3),
                row_id: i,
                data: vec![i as u8],
            };

            let vector_clock = VectorClock::new();
            let lsn = change_log.append(i, change, vector_clock)
                .expect("Failed to append");

            assert_eq!(lsn, i);
        }

        assert_eq!(change_log.get_latest_lsn(), 10);

        let entries = change_log.query_since_lsn(0, None)
            .expect("Failed to query");

        assert_eq!(entries.len(), 10);
    }

    #[test]
    fn test_query_by_table() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        for i in 0..10 {
            let change = ChangeType::Insert {
                table: if i % 2 == 0 { "even" } else { "odd" }.to_string(),
                row_id: i,
                data: vec![i as u8],
            };

            let vector_clock = VectorClock::new();
            change_log.append(i, change, vector_clock)
                .expect("Failed to append");
        }

        let even_entries = change_log.query_by_table("even")
            .expect("Failed to query");
        assert_eq!(even_entries.len(), 5);

        let odd_entries = change_log.query_by_table("odd")
            .expect("Failed to query");
        assert_eq!(odd_entries.len(), 5);
    }

    #[test]
    fn test_compaction() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        // Add 100 entries
        for i in 0..100 {
            let change = ChangeType::Insert {
                table: "test".to_string(),
                row_id: i,
                data: vec![i as u8],
            };

            let vector_clock = VectorClock::new();
            change_log.append(i, change, vector_clock)
                .expect("Failed to append");
        }

        // Compact entries below LSN 50
        let deleted = change_log.compact(50)
            .expect("Failed to compact");

        assert_eq!(deleted, 50);
        assert_eq!(change_log.get_compaction_watermark(), 50);

        // Query should only return entries >= 50
        let entries = change_log.query_since_lsn(0, None)
            .expect("Failed to query");

        assert_eq!(entries.len(), 50);
        assert_eq!(entries[0].lsn, 50);
    }

    #[test]
    fn test_query_with_limit() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        for i in 0..100 {
            let change = ChangeType::Insert {
                table: "test".to_string(),
                row_id: i,
                data: vec![i as u8],
            };

            let vector_clock = VectorClock::new();
            change_log.append(i, change, vector_clock)
                .expect("Failed to append");
        }

        let options = QueryOptions::new().with_limit(10);
        let entries = change_log.query(&options)
            .expect("Failed to query");

        assert_eq!(entries.len(), 10);
    }

    #[test]
    fn test_get_entry() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        let change = ChangeType::Update {
            table: "users".to_string(),
            row_id: 42,
            old_data: vec![1, 2, 3],
            new_data: vec![4, 5, 6],
        };

        let vector_clock = VectorClock::new();
        let lsn = change_log.append(1, change, vector_clock)
            .expect("Failed to append");

        let entry = change_log.get_entry(lsn)
            .expect("Failed to get entry")
            .expect("Entry not found");

        assert_eq!(entry.lsn, lsn);
        assert!(matches!(entry.change_type, ChangeType::Update { .. }));
    }

    #[test]
    fn test_get_stats() {
        let (db, _temp_dir) = create_test_db();
        let change_log = ChangeLog::new(db).expect("Failed to create change log");

        for i in 0..50 {
            let change = ChangeType::Insert {
                table: "test".to_string(),
                row_id: i,
                data: vec![i as u8; 100],
            };

            let vector_clock = VectorClock::new();
            change_log.append(i, change, vector_clock)
                .expect("Failed to append");
        }

        let stats = change_log.get_stats()
            .expect("Failed to get stats");

        assert_eq!(stats.total_entries, 50);
        assert_eq!(stats.current_lsn, 50);
        assert_eq!(stats.oldest_lsn, Some(0));
        assert!(stats.estimated_size_bytes > 0);
    }

    #[test]
    fn test_change_type_methods() {
        let insert = ChangeType::Insert {
            table: "users".to_string(),
            row_id: 1,
            data: vec![1, 2, 3],
        };

        assert_eq!(insert.table_name(), "users");
        assert!(insert.is_dml());
        assert!(!insert.is_ddl());
        assert_eq!(insert.row_id(), Some(1));

        let create_table = ChangeType::CreateTable {
            table: "products".to_string(),
            schema: Schema::new(vec![]),
        };

        assert_eq!(create_table.table_name(), "products");
        assert!(create_table.is_ddl());
        assert!(!create_table.is_dml());
        assert_eq!(create_table.row_id(), None);
    }

    #[test]
    fn test_persistence() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let db_path = temp_dir.path();

        // Create and populate change log
        {
            let mut opts = Options::default();
            opts.create_if_missing(true);
            let db = DB::open(&opts, db_path).expect("Failed to open DB");
            let change_log = ChangeLog::new(Arc::new(db)).expect("Failed to create change log");

            for i in 0..10 {
                let change = ChangeType::Insert {
                    table: "test".to_string(),
                    row_id: i,
                    data: vec![i as u8],
                };

                let vector_clock = VectorClock::new();
                change_log.append(i, change, vector_clock)
                    .expect("Failed to append");
            }
        }

        // Reopen and verify
        {
            let opts = Options::default();
            let db = DB::open(&opts, db_path).expect("Failed to open DB");
            let change_log = ChangeLog::new(Arc::new(db)).expect("Failed to create change log");

            assert_eq!(change_log.get_latest_lsn(), 10);

            let entries = change_log.query_since_lsn(0, None)
                .expect("Failed to query");

            assert_eq!(entries.len(), 10);
        }
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let (db, _temp_dir) = create_test_db();
        let change_log = Arc::new(ChangeLog::new(db).expect("Failed to create change log"));

        let mut handles = vec![];

        for thread_id in 0..4 {
            let change_log_clone = Arc::clone(&change_log);
            let handle = thread::spawn(move || {
                for i in 0..25 {
                    let change = ChangeType::Insert {
                        table: format!("thread_{}", thread_id),
                        row_id: i,
                        data: vec![i as u8],
                    };

                    let vector_clock = VectorClock::new();
                    change_log_clone.append(thread_id * 100 + i, change, vector_clock)
                        .expect("Failed to append");
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        assert_eq!(change_log.get_latest_lsn(), 100);

        let entries = change_log.query_since_lsn(0, None)
            .expect("Failed to query");

        assert_eq!(entries.len(), 100);
    }
}
