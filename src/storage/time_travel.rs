//! Time-Travel Query Support
//!
//! Implements AS OF TIMESTAMP/TRANSACTION/SCN queries for point-in-time database access.
//!
//! This module provides:
//! - Snapshot metadata storage and management
//! - Timestamp-to-snapshot mapping
//! - Transaction-ID-to-snapshot mapping
//! - SCN (System Change Number) tracking
//! - Historical snapshot creation and query execution
//! - Snapshot garbage collection
//!
//! ## Performance Characteristics
//!
//! - AS OF queries have <2x overhead vs current time queries
//! - Snapshot metadata is stored in-memory for fast lookups
//! - Historical versions are stored in RocksDB with efficient key encoding
//! - GC runs periodically to clean up old snapshots
//!
//! ## Key Encoding
//!
//! - Version keys: `v:{table}:{row_id}:{timestamp}`
//! - Snapshot metadata: `snapshot:{timestamp}`
//! - Transaction mapping: `txn_map:{txn_id}`
//! - SCN mapping: `scn_map:{scn}`

use crate::{Error, Result};
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::{RwLock, Mutex};
use chrono::{DateTime, NaiveDateTime, Utc};
use lru::LruCache;
use std::num::NonZeroUsize;

/// System Change Number (Oracle-compatible)
pub type Scn = u64;

/// Transaction ID
pub type TransactionId = u64;

/// Snapshot metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Snapshot timestamp (also serves as snapshot ID)
    pub timestamp: u64,
    /// Transaction ID that created this snapshot
    pub transaction_id: TransactionId,
    /// System Change Number
    pub scn: Scn,
    /// Wall-clock time when snapshot was created (RFC3339 format)
    pub wall_clock_time: String,
    /// Number of active transactions at snapshot time
    pub active_transactions: u64,
    /// Whether this snapshot can be garbage collected
    pub gc_eligible: bool,
}

impl SnapshotMetadata {
    /// Create a new snapshot metadata
    pub fn new(timestamp: u64, transaction_id: TransactionId, scn: Scn) -> Self {
        Self {
            timestamp,
            transaction_id,
            scn,
            wall_clock_time: Utc::now().to_rfc3339(),
            active_transactions: 0,
            gc_eligible: true,
        }
    }
}

/// Snapshot cache key: (table_name, row_id, snapshot_ts)
type SnapshotCacheKey = (String, u64, u64);

/// Snapshot Manager
///
/// Manages historical snapshots for time-travel queries.
pub struct SnapshotManager {
    /// Database handle
    db: Arc<DB>,
    /// In-memory snapshot registry for fast lookups
    snapshots: Arc<RwLock<HashMap<u64, SnapshotMetadata>>>,
    /// Transaction ID to timestamp mapping
    txn_to_timestamp: Arc<RwLock<HashMap<TransactionId, u64>>>,
    /// SCN to timestamp mapping
    scn_to_timestamp: Arc<RwLock<HashMap<Scn, u64>>>,
    /// Current SCN counter
    current_scn: Arc<RwLock<Scn>>,
    /// Current transaction ID counter
    current_txn_id: Arc<RwLock<TransactionId>>,
    /// Snapshot read cache for performance
    snapshot_cache: Arc<Mutex<LruCache<SnapshotCacheKey, Option<Vec<u8>>>>>,
    /// Cache configuration
    cache_config: CacheConfig,
    /// GC configuration
    gc_config: GcConfig,
}

/// Snapshot cache configuration
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of cached snapshot entries
    pub max_entries: usize,
    /// Whether to enable snapshot caching
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            enabled: true,
        }
    }
}

/// Garbage collection configuration
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Minimum retention period (seconds)
    pub min_retention_seconds: u64,
    /// Maximum number of snapshots to keep
    pub max_snapshots: usize,
    /// Whether to enable automatic GC
    pub auto_gc_enabled: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            min_retention_seconds: 3600, // 1 hour
            max_snapshots: 1000,
            auto_gc_enabled: true,
        }
    }
}

impl SnapshotManager {
    /// Create a new snapshot manager
    pub fn new(db: Arc<DB>) -> Self {
        let cache_config = CacheConfig::default();
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(RwLock::new(1)),
            current_txn_id: Arc::new(RwLock::new(1)),
            snapshot_cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            cache_config,
            gc_config: GcConfig::default(),
        }
    }

    /// Create a new snapshot manager with custom GC config
    pub fn with_gc_config(db: Arc<DB>, gc_config: GcConfig) -> Self {
        let cache_config = CacheConfig::default();
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(RwLock::new(1)),
            current_txn_id: Arc::new(RwLock::new(1)),
            snapshot_cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            cache_config,
            gc_config,
        }
    }

    /// Create a new snapshot manager with custom cache and GC config
    pub fn with_config(db: Arc<DB>, cache_config: CacheConfig, gc_config: GcConfig) -> Self {
        let cache_size = NonZeroUsize::new(cache_config.max_entries)
            .unwrap_or_else(|| NonZeroUsize::new(1000).unwrap_or(NonZeroUsize::MIN));

        Self {
            db,
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            txn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            scn_to_timestamp: Arc::new(RwLock::new(HashMap::new())),
            current_scn: Arc::new(RwLock::new(1)),
            current_txn_id: Arc::new(RwLock::new(1)),
            snapshot_cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            cache_config,
            gc_config,
        }
    }

    /// Register a new snapshot
    ///
    /// This should be called every time a transaction commits to track
    /// the snapshot state at that point in time.
    pub fn register_snapshot(&self, timestamp: u64) -> Result<SnapshotMetadata> {
        let txn_id = self.next_transaction_id();
        self.register_snapshot_internal(timestamp, txn_id)
    }

    /// Register a new snapshot with a specific transaction/LSN ID
    ///
    /// This allows the caller to specify the transaction ID (e.g., WAL LSN)
    /// which enables AS OF TRANSACTION queries to use the same IDs that
    /// users see in the REPL.
    pub fn register_snapshot_with_lsn(&self, timestamp: u64, lsn: u64) -> Result<SnapshotMetadata> {
        // Update our internal counter to stay ahead of externally provided LSNs
        {
            let mut txn_id = self.current_txn_id.write();
            if lsn >= *txn_id {
                *txn_id = lsn + 1;
            }
        }
        self.register_snapshot_internal(timestamp, lsn)
    }

    /// Internal snapshot registration
    fn register_snapshot_internal(&self, timestamp: u64, txn_id: TransactionId) -> Result<SnapshotMetadata> {
        let scn = self.next_scn();

        let metadata = SnapshotMetadata::new(timestamp, txn_id, scn);

        // Store in-memory
        self.snapshots.write().insert(timestamp, metadata.clone());
        self.txn_to_timestamp.write().insert(txn_id, timestamp);
        self.scn_to_timestamp.write().insert(scn, timestamp);

        // Persist to RocksDB
        self.persist_snapshot_metadata(&metadata)?;

        // Run GC if enabled
        if self.gc_config.auto_gc_enabled {
            if let Err(e) = self.gc_if_needed() {
                eprintln!("Warning: Snapshot GC failed: {}", e);
            }
        }

        Ok(metadata)
    }

    /// Get next transaction ID
    fn next_transaction_id(&self) -> TransactionId {
        let mut txn_id = self.current_txn_id.write();
        let current = *txn_id;
        *txn_id += 1;
        current
    }

    /// Get next SCN
    fn next_scn(&self) -> Scn {
        let mut scn = self.current_scn.write();
        let current = *scn;
        *scn += 1;
        current
    }

    /// Resolve AS OF clause to a timestamp
    ///
    /// Converts TIMESTAMP/TRANSACTION/SCN to a snapshot timestamp.
    /// For VersionsBetween, this returns an error - use scan_versions_between directly.
    pub fn resolve_as_of(&self, as_of: &crate::sql::logical_plan::AsOfClause) -> Result<u64> {
        use crate::sql::logical_plan::AsOfClause;

        match as_of {
            AsOfClause::Now => {
                // Get current timestamp
                Ok(self.get_current_timestamp())
            }
            AsOfClause::Timestamp(ts_str) => {
                self.resolve_timestamp(ts_str)
            }
            AsOfClause::Transaction(txn_id) => {
                self.resolve_transaction(*txn_id)
            }
            AsOfClause::Scn(scn) => {
                self.resolve_scn(*scn)
            }
            AsOfClause::VersionsBetween { .. } => {
                // VersionsBetween cannot be resolved to a single timestamp
                // The executor should handle this variant separately
                Err(Error::query_execution(
                    "VERSIONS BETWEEN cannot be resolved to a single timestamp. Use scan_versions_between instead."
                ))
            }
            AsOfClause::Commit(sha) => {
                // AS OF COMMIT queries are handled by the CommitTracker in git_integration
                // The executor should handle this variant separately
                Err(Error::query_execution(format!(
                    "AS OF COMMIT '{}' should be resolved by the CommitTracker. Use git_integration::CommitTracker::get_snapshot_for_commit() instead.",
                    sha
                )))
            }
        }
    }

    /// Resolve timestamp string to snapshot timestamp
    fn resolve_timestamp(&self, ts_str: &str) -> Result<u64> {
        // Parse timestamp string
        let dt = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S"))
            .map_err(|e| Error::query_execution(format!("Invalid timestamp format: {}", e)))?;

        let target_time = dt.and_utc().timestamp() as u64;

        // Find the closest snapshot <= target time
        let snapshots = self.snapshots.read();
        let mut best_match: Option<u64> = None;
        let mut best_diff = u64::MAX;

        for metadata in snapshots.values() {
            if let Ok(snap_time) = DateTime::parse_from_rfc3339(&metadata.wall_clock_time) {
                let snap_timestamp = snap_time.timestamp() as u64;
                if snap_timestamp <= target_time {
                    let diff = target_time - snap_timestamp;
                    if diff < best_diff {
                        best_diff = diff;
                        best_match = Some(metadata.timestamp);
                    }
                }
            }
        }

        best_match.ok_or_else(|| {
            Error::query_execution(format!(
                "No snapshot found for timestamp '{}'",
                ts_str
            ))
        })
    }

    /// Resolve timestamp for VERSIONS BETWEEN range queries
    ///
    /// Returns internal LSN timestamp for use in version range queries.
    /// For timestamps, finds the nearest snapshot or uses boundary values.
    pub fn resolve_timestamp_for_range(&self, as_of: &crate::sql::logical_plan::AsOfClause, is_start: bool) -> Result<u64> {
        use crate::sql::logical_plan::AsOfClause;

        match as_of {
            AsOfClause::Now => {
                // For NOW, use the maximum timestamp (current)
                Ok(self.get_current_timestamp())
            }
            AsOfClause::Timestamp(ts_str) => {
                // Parse the target timestamp
                let dt = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S")
                    .or_else(|_| NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H:%M:%S"))
                    .map_err(|e| Error::query_execution(format!("Invalid timestamp format: {}", e)))?;

                let target_time = dt.and_utc().timestamp() as u64;

                // Search through snapshots to find matching LSN
                let snapshots = self.snapshots.read();

                if snapshots.is_empty() {
                    // No snapshots - use boundary values for full range
                    return Ok(if is_start { 0 } else { u64::MAX });
                }

                // Find appropriate snapshot based on whether this is start or end
                let mut best_match: Option<u64> = None;

                for metadata in snapshots.values() {
                    if let Ok(snap_time) = DateTime::parse_from_rfc3339(&metadata.wall_clock_time) {
                        let snap_ts_seconds = snap_time.timestamp() as u64;

                        if is_start {
                            // For start: find earliest snapshot >= target
                            if snap_ts_seconds >= target_time {
                                match best_match {
                                    Some(best) if metadata.timestamp < best => {
                                        best_match = Some(metadata.timestamp);
                                    }
                                    None => {
                                        best_match = Some(metadata.timestamp);
                                    }
                                    _ => {}
                                }
                            }
                        } else {
                            // For end: find latest snapshot <= target
                            if snap_ts_seconds <= target_time {
                                match best_match {
                                    Some(best) if metadata.timestamp > best => {
                                        best_match = Some(metadata.timestamp);
                                    }
                                    None => {
                                        best_match = Some(metadata.timestamp);
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }

                // If no matching snapshot found, use boundary values
                Ok(best_match.unwrap_or(if is_start { 0 } else { u64::MAX }))
            }
            AsOfClause::Transaction(txn_id) => {
                self.resolve_transaction(*txn_id)
            }
            AsOfClause::Scn(scn) => {
                self.resolve_scn(*scn)
            }
            AsOfClause::VersionsBetween { .. } => {
                Err(Error::query_execution(
                    "Cannot resolve VersionsBetween to a single timestamp"
                ))
            }
            AsOfClause::Commit(sha) => {
                // AS OF COMMIT queries should be handled by git_integration::CommitTracker
                Err(Error::query_execution(format!(
                    "AS OF COMMIT '{}' should be resolved via git_integration::CommitTracker",
                    sha
                )))
            }
        }
    }

    /// Resolve transaction ID to snapshot timestamp
    fn resolve_transaction(&self, txn_id: TransactionId) -> Result<u64> {
        self.txn_to_timestamp
            .read()
            .get(&txn_id)
            .copied()
            .ok_or_else(|| {
                Error::query_execution(format!(
                    "Transaction {} not found or has been garbage collected",
                    txn_id
                ))
            })
    }

    /// Resolve SCN to snapshot timestamp
    pub fn resolve_scn(&self, scn: Scn) -> Result<u64> {
        self.scn_to_timestamp
            .read()
            .get(&scn)
            .copied()
            .ok_or_else(|| {
                Error::query_execution(format!(
                    "SCN {} not found or has been garbage collected",
                    scn
                ))
            })
    }

    /// Get current timestamp
    fn get_current_timestamp(&self) -> u64 {
        // Get the latest snapshot timestamp
        self.snapshots
            .read()
            .values()
            .map(|m| m.timestamp)
            .max()
            .unwrap_or(1)
    }

    /// Read a versioned value at a specific snapshot (legacy - linear scan)
    ///
    /// This implements the core time-travel query logic with O(N) complexity.
    /// Use read_at_snapshot_indexed() for O(log N) performance.
    #[allow(dead_code)]
    pub fn read_at_snapshot_linear(
        &self,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<Option<Vec<u8>>> {
        // Build key prefix for all versions of this row
        let prefix = format!("v:{}:{}:", table_name, row_id);

        // Iterate through versions in reverse chronological order
        // to find the most recent version <= snapshot_ts
        let mut best_version: Option<(u64, Vec<u8>)> = None;

        let iter = self.db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, value) = item.map_err(|e| {
                Error::storage(format!("Iterator error: {}", e))
            })?;

            // Parse key: v:{table}:{row_id}:{timestamp}
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with(&prefix) {
                    if let Some(ts_str) = key_str.rsplit(':').next() {
                        if let Ok(ts) = ts_str.parse::<u64>() {
                            if ts <= snapshot_ts {
                                // Check if this is better than our current best
                                let should_update = match &best_version {
                                    None => true,
                                    Some((best_ts, _)) => *best_ts < ts,
                                };
                                if should_update {
                                    best_version = Some((ts, value.to_vec()));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Return the best version found
        Ok(best_version.map(|(_, value)| value))
    }

    /// Read a versioned value at a specific snapshot (optimized with reverse index and cache)
    ///
    /// This implements O(log N) time-travel queries using a reverse timestamp index.
    /// The reverse index uses `u64::MAX - timestamp` to enable efficient lookups.
    /// Additionally, uses an LRU cache for frequently accessed snapshots.
    pub fn read_at_snapshot(
        &self,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<Option<Vec<u8>>> {
        // Check cache first if enabled
        if self.cache_config.enabled {
            let cache_key = (table_name.to_string(), row_id, snapshot_ts);
            if let Some(cached_value) = self.snapshot_cache.lock().get(&cache_key) {
                // Cache hit - return cloned value
                return Ok(cached_value.clone());
            }
        }

        // Cache miss - perform database lookup
        let result = self.read_at_snapshot_uncached(table_name, row_id, snapshot_ts)?;

        // Store in cache if enabled
        if self.cache_config.enabled {
            let cache_key = (table_name.to_string(), row_id, snapshot_ts);
            self.snapshot_cache.lock().put(cache_key, result.clone());
        }

        Ok(result)
    }

    /// Read a versioned value without using cache (internal method)
    ///
    /// This is the core implementation that performs the actual database lookup.
    fn read_at_snapshot_uncached(
        &self,
        table_name: &str,
        row_id: u64,
        snapshot_ts: u64,
    ) -> Result<Option<Vec<u8>>> {
        // Use reverse timestamp index for O(log N) lookup
        // Reverse timestamp allows us to find the latest version <= snapshot_ts
        let reverse_ts = u64::MAX - snapshot_ts;

        // Seek to the reverse timestamp index
        // Index format: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
        let seek_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);

        // Since we use reverse timestamps (larger actual_ts -> smaller reverse_ts),
        // we need to seek forward to find versions with actual_ts <= snapshot_ts
        // (which have reverse_ts >= our target reverse_ts)
        let mut iter = self.db.iterator(rocksdb::IteratorMode::From(
            seek_key.as_bytes(),
            rocksdb::Direction::Forward
        ));

        let expected_prefix = format!("v_idx:{}:{}:", table_name, row_id);

        // Check if we found a matching index entry
        if let Some(Ok((key, value))) = iter.next() {
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with(&expected_prefix) {
                    // Decode the actual timestamp from the index value
                    if value.len() >= 8 {
                        let actual_ts = u64::from_be_bytes(
                            value.get(0..8)
                                .ok_or_else(|| Error::storage("Timestamp bytes too short"))?
                                .try_into()
                                .map_err(|e| Error::storage(format!("Invalid timestamp bytes: {}", e)))?
                        );

                        // Verify this version is visible to our snapshot
                        if actual_ts <= snapshot_ts {
                            // Now fetch the actual versioned data
                            return self.get_version_by_exact_timestamp(table_name, row_id, actual_ts);
                        }
                    }
                }
            }
        }

        // No version found - fallback to checking if there are any versions at all
        Ok(None)
    }

    /// Get a specific version by exact timestamp
    ///
    /// Helper method used by the indexed lookup.
    fn get_version_by_exact_timestamp(
        &self,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
    ) -> Result<Option<Vec<u8>>> {
        let key = format!("v:{}:{}:{}", table_name, row_id, timestamp);
        self.db.get(key.as_bytes())
            .map_err(|e| Error::storage(format!("Failed to read version: {}", e)))
            .map(|opt| opt.map(|v| v.to_vec()))
    }

    /// Write a new version of a value
    ///
    /// Called when a transaction commits to create a new historical version.
    /// Also creates a reverse timestamp index entry for efficient lookups.
    /// Invalidates cache entries for this row.
    pub fn write_version(
        &self,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
        value: &[u8],
    ) -> Result<()> {
        // Write the actual versioned data
        let key = format!("v:{}:{}:{}", table_name, row_id, timestamp);
        self.db.put(key.as_bytes(), value)
            .map_err(|e| Error::storage(format!("Failed to write version: {}", e)))?;

        // Create reverse timestamp index entry
        // Index structure: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
        // Reverse timestamp = u64::MAX - timestamp for efficient SeekForPrev
        self.create_reverse_timestamp_index(table_name, row_id, timestamp)?;

        // Invalidate cache entries for this row
        // We need to remove all cached entries for this (table, row_id) combination
        // since a new version may affect reads at different snapshot timestamps
        self.invalidate_cache_for_row(table_name, row_id);

        Ok(())
    }

    /// Invalidate all cache entries for a specific row
    ///
    /// This is called when a new version is written to ensure cache consistency.
    fn invalidate_cache_for_row(&self, table_name: &str, row_id: u64) {
        if !self.cache_config.enabled {
            return;
        }

        // Lock the cache and remove all entries matching (table_name, row_id, *)
        let mut cache = self.snapshot_cache.lock();

        // Collect keys to remove (we can't modify while iterating)
        let keys_to_remove: Vec<SnapshotCacheKey> = cache.iter()
            .filter_map(|(key, _)| {
                if key.0 == table_name && key.1 == row_id {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();

        // Remove the keys
        for key in keys_to_remove {
            cache.pop(&key);
        }
    }

    /// Create reverse timestamp index for O(log N) lookups
    ///
    /// Index structure: v_idx:{table}:{row_id}:{reverse_ts} -> {actual_ts}
    /// Reverse timestamp allows RocksDB to find "latest before X" efficiently.
    fn create_reverse_timestamp_index(
        &self,
        table_name: &str,
        row_id: u64,
        timestamp: u64,
    ) -> Result<()> {
        let reverse_ts = u64::MAX - timestamp;
        let index_key = format!("v_idx:{}:{}:{:020}", table_name, row_id, reverse_ts);

        // Store the actual timestamp as the value (8 bytes, big-endian)
        let timestamp_bytes = timestamp.to_be_bytes();

        self.db.put(index_key.as_bytes(), timestamp_bytes)
            .map_err(|e| Error::storage(format!("Failed to create reverse index: {}", e)))
    }

    /// Persist snapshot metadata to disk
    fn persist_snapshot_metadata(&self, metadata: &SnapshotMetadata) -> Result<()> {
        let key = format!("snapshot:{}", metadata.timestamp);
        let value = bincode::serialize(metadata)
            .map_err(|e| Error::storage(format!("Failed to serialize metadata: {}", e)))?;

        self.db.put(key.as_bytes(), value)
            .map_err(|e| Error::storage(format!("Failed to persist metadata: {}", e)))?;

        // Also persist mappings
        let txn_key = format!("txn_map:{}", metadata.transaction_id);
        let txn_value = bincode::serialize(&metadata.timestamp)
            .map_err(|e| Error::storage(format!("Failed to serialize txn mapping: {}", e)))?;
        self.db.put(txn_key.as_bytes(), txn_value)
            .map_err(|e| Error::storage(format!("Failed to persist txn mapping: {}", e)))?;

        let scn_key = format!("scn_map:{}", metadata.scn);
        let scn_value = bincode::serialize(&metadata.timestamp)
            .map_err(|e| Error::storage(format!("Failed to serialize scn mapping: {}", e)))?;
        self.db.put(scn_key.as_bytes(), scn_value)
            .map_err(|e| Error::storage(format!("Failed to persist scn mapping: {}", e)))?;

        Ok(())
    }

    /// Garbage collect old snapshots
    ///
    /// Removes snapshots that are:
    /// - Older than min_retention_seconds
    /// - Beyond max_snapshots limit
    /// - Marked as gc_eligible
    pub fn gc_old_snapshots(&self) -> Result<usize> {
        let now = Utc::now().timestamp() as u64;
        let min_retention = self.gc_config.min_retention_seconds;

        let mut snapshots = self.snapshots.write();
        let mut to_remove = Vec::new();

        // Find snapshots eligible for GC
        for (ts, metadata) in snapshots.iter() {
            if !metadata.gc_eligible {
                continue;
            }

            // Parse wall clock time
            if let Ok(snap_time) = DateTime::parse_from_rfc3339(&metadata.wall_clock_time) {
                let age = now.saturating_sub(snap_time.timestamp() as u64);
                if age > min_retention {
                    to_remove.push(*ts);
                }
            }
        }

        // If we're still over the limit, remove oldest eligible snapshots
        if snapshots.len() - to_remove.len() > self.gc_config.max_snapshots {
            let mut eligible: Vec<_> = snapshots
                .iter()
                .filter(|(_, m)| m.gc_eligible && !to_remove.contains(&m.timestamp))
                .map(|(ts, m)| (*ts, m.clone()))
                .collect();

            eligible.sort_by_key(|(ts, _)| *ts);

            let excess = (snapshots.len() - to_remove.len()).saturating_sub(self.gc_config.max_snapshots);
            for (ts, _) in eligible.iter().take(excess) {
                to_remove.push(*ts);
            }
        }

        // Remove snapshots
        let count = to_remove.len();
        for ts in &to_remove {
            if let Some(metadata) = snapshots.remove(ts) {
                // Remove from mappings
                self.txn_to_timestamp.write().remove(&metadata.transaction_id);
                self.scn_to_timestamp.write().remove(&metadata.scn);

                // Remove from disk
                let snap_key = format!("snapshot:{}", ts);
                let txn_key = format!("txn_map:{}", metadata.transaction_id);
                let scn_key = format!("scn_map:{}", metadata.scn);

                let _ = self.db.delete(snap_key.as_bytes());
                let _ = self.db.delete(txn_key.as_bytes());
                let _ = self.db.delete(scn_key.as_bytes());

                // Note: We don't delete the versioned data (v:*) here
                // That would require a separate GC pass to avoid breaking
                // any in-flight queries
            }
        }

        Ok(count)
    }

    /// Run GC if needed
    fn gc_if_needed(&self) -> Result<()> {
        let snapshot_count = self.snapshots.read().len();
        if snapshot_count > self.gc_config.max_snapshots {
            self.gc_old_snapshots()?;
        }
        Ok(())
    }

    /// Get snapshot metadata
    pub fn get_snapshot_metadata(&self, timestamp: u64) -> Option<SnapshotMetadata> {
        self.snapshots.read().get(&timestamp).cloned()
    }

    /// Get current SCN
    pub fn current_scn(&self) -> Scn {
        *self.current_scn.read()
    }

    /// Get current transaction ID
    pub fn current_transaction_id(&self) -> TransactionId {
        *self.current_txn_id.read()
    }

    /// Get snapshot count
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.read().len()
    }

    /// List all snapshots
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotMetadata>> {
        let snapshots = self.snapshots.read();
        let mut result: Vec<_> = snapshots.values().cloned().collect();
        result.sort_by_key(|s| s.timestamp);
        Ok(result)
    }

    /// Load existing snapshots from disk (for recovery)
    pub fn recover_snapshots(&self) -> Result<usize> {
        let mut count = 0;
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);

        for item in iter {
            let (key, value) = item.map_err(|e| {
                Error::storage(format!("Iterator error during recovery: {}", e))
            })?;

            if let Ok(key_str) = std::str::from_utf8(&key) {
                if key_str.starts_with("snapshot:") {
                    if let Ok(metadata) = bincode::deserialize::<SnapshotMetadata>(&value) {
                        // Restore in-memory state
                        self.snapshots.write().insert(metadata.timestamp, metadata.clone());
                        self.txn_to_timestamp.write().insert(metadata.transaction_id, metadata.timestamp);
                        self.scn_to_timestamp.write().insert(metadata.scn, metadata.timestamp);

                        // Update counters
                        let mut scn = self.current_scn.write();
                        if metadata.scn >= *scn {
                            *scn = metadata.scn + 1;
                        }

                        let mut txn_id = self.current_txn_id.write();
                        if metadata.transaction_id >= *txn_id {
                            *txn_id = metadata.transaction_id + 1;
                        }

                        count += 1;
                    }
                }
            }
        }

        Ok(count)
    }

    /// Get cache statistics
    ///
    /// Returns (current_size, max_capacity) of the snapshot cache
    pub fn cache_stats(&self) -> (usize, usize) {
        let cache = self.snapshot_cache.lock();
        (cache.len(), cache.cap().get())
    }

    /// Clear the snapshot cache
    ///
    /// Useful for testing or manual cache management
    pub fn clear_cache(&self) {
        self.snapshot_cache.lock().clear();
    }

    /// Calculate approximate size of a snapshot in bytes
    ///
    /// This estimates the storage footprint by counting version keys
    /// that exist at the snapshot's timestamp.
    pub fn calculate_snapshot_size(&self, timestamp: u64) -> Result<u64> {
        let mut total_size: u64 = 0;
        let prefix = format!("v:");

        // Iterate through all version keys
        let iter = self.db.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));

        for item in iter {
            let (key, value) = item.map_err(|e| {
                Error::storage(format!("Iterator error during size calculation: {}", e))
            })?;

            if let Ok(key_str) = std::str::from_utf8(&key) {
                // Version keys: v:{table}:{row_id}:{timestamp}
                if key_str.starts_with("v:") {
                    // Parse timestamp from key
                    if let Some(ts_str) = key_str.rsplit(':').next() {
                        if let Ok(ts) = ts_str.parse::<u64>() {
                            // Count versions <= snapshot timestamp
                            if ts <= timestamp {
                                total_size += key.len() as u64 + value.len() as u64;
                            }
                        }
                    }
                }
            }

            // Stop if we've moved past version keys
            if !key.starts_with(b"v:") {
                break;
            }
        }

        Ok(total_size)
    }

    /// Scan all versions of all rows in a table between two timestamps
    ///
    /// Returns a vector of (row_id, timestamp, value_bytes) for each version
    /// within the specified range [start_ts, end_ts].
    ///
    /// Used for VERSIONS BETWEEN queries.
    pub fn scan_versions_between(
        &self,
        table_name: &str,
        start_ts: u64,
        end_ts: u64,
    ) -> Result<Vec<(u64, u64, Vec<u8>)>> {
        let mut versions = Vec::new();
        let prefix = format!("v:{}:", table_name);

        // Iterate through all version keys for this table
        let iter = self.db.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        ));

        for item in iter {
            let (key, value) = item.map_err(|e| {
                Error::storage(format!("Iterator error during version scan: {}", e))
            })?;

            // Stop if we've moved past this table's version keys
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }

            if let Ok(key_str) = std::str::from_utf8(&key) {
                // Parse key: v:{table}:{row_id}:{timestamp}
                let parts: Vec<&str> = key_str.split(':').collect();
                if let (Some(p2), Some(p3)) = (parts.get(2), parts.get(3)) {
                    if let (Ok(row_id), Ok(ts)) = (
                        p2.parse::<u64>(),
                        p3.parse::<u64>(),
                    ) {
                        // Check if timestamp is within range
                        if ts >= start_ts && ts <= end_ts {
                            versions.push((row_id, ts, value.to_vec()));
                        }
                    }
                }
            }
        }

        // Sort by row_id first, then by timestamp descending (newest first)
        versions.sort_by(|a, b| {
            match a.0.cmp(&b.0) {
                std::cmp::Ordering::Equal => b.1.cmp(&a.1), // Descending timestamp
                other => other,
            }
        });

        Ok(versions)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;
    use tempfile::tempdir;

    fn create_test_db() -> (Arc<DB>, tempfile::TempDir) {
        let temp_dir = tempdir().unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, temp_dir.path()).unwrap();
        (Arc::new(db), temp_dir)
    }

    #[test]
    fn test_snapshot_registration() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        let metadata = manager.register_snapshot(100).unwrap();
        assert_eq!(metadata.timestamp, 100);
        assert_eq!(metadata.transaction_id, 1);
        assert_eq!(metadata.scn, 1);
    }

    #[test]
    fn test_resolve_transaction() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        let metadata = manager.register_snapshot(100).unwrap();
        let txn_id = metadata.transaction_id;

        let resolved = manager.resolve_transaction(txn_id).unwrap();
        assert_eq!(resolved, 100);
    }

    #[test]
    fn test_resolve_scn() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        let metadata = manager.register_snapshot(100).unwrap();
        let scn = metadata.scn;

        let resolved = manager.resolve_scn(scn).unwrap();
        assert_eq!(resolved, 100);
    }

    #[test]
    fn test_version_write_and_read() {
        let (db, _temp) = create_test_db();
        let manager = SnapshotManager::new(db);

        // Write versions at different timestamps
        let value1 = b"value_at_100".to_vec();
        let value2 = b"value_at_200".to_vec();

        manager.write_version("users", 1, 100, &value1).unwrap();
        manager.write_version("users", 1, 200, &value2).unwrap();

        // Read at timestamp 150 should get value1
        let result = manager.read_at_snapshot("users", 1, 150).unwrap();
        assert_eq!(result, Some(value1));

        // Read at timestamp 250 should get value2
        let result = manager.read_at_snapshot("users", 1, 250).unwrap();
        assert_eq!(result, Some(value2));

        // Read at timestamp 50 should get nothing
        let result = manager.read_at_snapshot("users", 1, 50).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_snapshot_gc() {
        let (db, _temp) = create_test_db();
        let gc_config = GcConfig {
            min_retention_seconds: 0, // Allow immediate GC for testing
            max_snapshots: 5,
            auto_gc_enabled: false, // Manual GC for testing
        };
        let manager = SnapshotManager::with_gc_config(db, gc_config);

        // Create 10 snapshots
        for i in 1..=10 {
            manager.register_snapshot(i * 100).unwrap();
        }

        assert_eq!(manager.snapshot_count(), 10);

        // Run GC - should keep only 5 newest
        let removed = manager.gc_old_snapshots().unwrap();
        assert_eq!(removed, 5);
        assert_eq!(manager.snapshot_count(), 5);
    }

    #[test]
    fn test_snapshot_recovery() {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path();

        // Create snapshots and close
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, db_path).unwrap());
            let manager = SnapshotManager::new(db);

            manager.register_snapshot(100).unwrap();
            manager.register_snapshot(200).unwrap();
        }

        // Reopen and recover
        {
            let mut opts = rocksdb::Options::default();
            opts.create_if_missing(true);
            let db = Arc::new(DB::open(&opts, db_path).unwrap());
            let manager = SnapshotManager::new(db);

            let count = manager.recover_snapshots().unwrap();
            assert_eq!(count, 2);
            assert_eq!(manager.snapshot_count(), 2);
        }
    }
}
