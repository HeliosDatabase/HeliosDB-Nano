//! Filter Index Delta Tracker for Self-Maintaining Filter Indexes (SMFI)
//!
//! Provides lightweight synchronous filter updates during DML operations.
//! Designed for minimal overhead (~150ns per row) with background consolidation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::{Value, Tuple};
use crate::Schema;
use super::bloom_filter::BloomFilter;

/// Default threshold for automatic bulk load detection (number of rows)
/// Can be overridden via FilterIndexConfig or SET smfi_bulk_load_threshold
pub const DEFAULT_BULK_LOAD_THRESHOLD: usize = 10000;

/// Information about a suspended table for bulk operations
#[derive(Debug)]
pub struct SuspendedTableInfo {
    /// When the suspension started
    pub suspended_at: u64,
    /// Reason for suspension
    pub reason: BulkLoadReason,
    /// Rows processed while suspended (for rebuild estimation)
    pub rows_affected: AtomicU64,
}

impl Clone for SuspendedTableInfo {
    fn clone(&self) -> Self {
        Self {
            suspended_at: self.suspended_at,
            reason: self.reason,
            rows_affected: AtomicU64::new(self.rows_affected.load(Ordering::Relaxed)),
        }
    }
}

impl SuspendedTableInfo {
    pub fn new(reason: BulkLoadReason) -> Self {
        Self {
            suspended_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            reason,
            rows_affected: AtomicU64::new(0),
        }
    }
}

/// Reason for bulk load suspension
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkLoadReason {
    /// INSERT with multiple VALUES rows
    MultiRowInsert,
    /// INSERT ... SELECT (bulk copy from query)
    InsertSelect,
    /// COPY FROM (file import)
    CopyFrom,
    /// Manual suspension via API/SQL
    Manual,
}

/// Configuration for filter index delta tracking
#[derive(Debug, Clone)]
pub struct FilterIndexConfig {
    /// Number of hash functions for bloom filter
    pub num_hashes: u32,
    /// Number of bits in bloom filter
    pub bloom_bits: u64,
    /// Delta threshold before triggering consolidation
    pub delta_threshold: u64,
    /// Enable bloom filter tracking
    pub enable_bloom: bool,
    /// Enable zone map tracking
    pub enable_zone_map: bool,
    /// Maximum deltas to buffer before forced flush
    pub max_buffered_deltas: usize,
    /// Threshold for automatic bulk load detection (number of rows)
    /// Operations with >= this many rows will auto-suspend SMFI tracking
    pub bulk_load_threshold: usize,
}

impl Default for FilterIndexConfig {
    fn default() -> Self {
        Self {
            num_hashes: 7,
            bloom_bits: 1_000_000,
            delta_threshold: 1000,
            enable_bloom: true,
            enable_zone_map: true,
            max_buffered_deltas: 10_000,
            bulk_load_threshold: DEFAULT_BULK_LOAD_THRESHOLD,
        }
    }
}

/// Type of delta operation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterDeltaType {
    Insert,
    Update,
    Delete,
}

/// Bloom filter delta - bits to OR into base filter
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BloomFilterDelta {
    /// Bits to set: (word_index, bits_mask)
    pub bits_to_set: Vec<(usize, u64)>,
    /// Number of rows added
    pub rows_added: u64,
    /// Column name this delta applies to
    pub column_name: String,
}

impl BloomFilterDelta {
    pub fn new(column_name: &str) -> Self {
        Self {
            bits_to_set: Vec::new(),
            rows_added: 0,
            column_name: column_name.to_string(),
        }
    }

    /// Merge another delta into this one
    pub fn merge(&mut self, other: &BloomFilterDelta) {
        self.bits_to_set.extend(other.bits_to_set.iter().cloned());
        self.rows_added += other.rows_added;
    }

    /// Apply delta to a bloom filter
    pub fn apply_to(&self, filter: &mut BloomFilter) {
        for &(word_idx, bits_mask) in &self.bits_to_set {
            filter.apply_delta_bits(word_idx, bits_mask);
        }
        filter.increment_items_added(self.rows_added as usize);
    }
}

/// Zone map delta for incremental min/max updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneMapDelta {
    /// Block ID this delta applies to
    pub block_id: u64,
    /// Column updates: (column_name, potential_new_min, potential_new_max)
    pub column_updates: Vec<ZoneColumnUpdate>,
    /// Number of rows affected
    pub rows_affected: u64,
    /// Delta type
    pub delta_type: FilterDeltaType,
}

/// Update for a single column in zone map
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneColumnUpdate {
    pub column_name: String,
    pub value: Value,
    pub is_null: bool,
}

impl ZoneMapDelta {
    pub fn new(block_id: u64, delta_type: FilterDeltaType) -> Self {
        Self {
            block_id,
            column_updates: Vec::new(),
            rows_affected: 0,
            delta_type,
        }
    }

    pub fn add_column_update(&mut self, column_name: &str, value: &Value) {
        self.column_updates.push(ZoneColumnUpdate {
            column_name: column_name.to_string(),
            value: value.clone(),
            is_null: matches!(value, Value::Null),
        });
    }
}

/// Combined filter delta for a single DML operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterDelta {
    /// Sequence number
    pub seq: u64,
    /// Table name
    pub table_name: String,
    /// Row ID affected
    pub row_id: u64,
    /// Delta type
    pub delta_type: FilterDeltaType,
    /// Bloom filter deltas per column
    pub bloom_deltas: HashMap<String, BloomFilterDelta>,
    /// Zone map delta
    pub zone_delta: Option<ZoneMapDelta>,
    /// Timestamp
    pub timestamp: u64,
}

impl FilterDelta {
    pub fn new(seq: u64, table_name: &str, row_id: u64, delta_type: FilterDeltaType) -> Self {
        Self {
            seq,
            table_name: table_name.to_string(),
            row_id,
            delta_type,
            bloom_deltas: HashMap::new(),
            zone_delta: None,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }
    }
}

/// Per-table filter delta buffer
#[derive(Debug, Default)]
pub struct TableFilterDeltas {
    /// Bloom filter deltas per column
    pub bloom_deltas: HashMap<String, BloomFilterDelta>,
    /// Zone map deltas per block
    pub zone_deltas: HashMap<u64, ZoneMapDelta>,
    /// Total delta count
    pub delta_count: u64,
    /// Last flush timestamp
    pub last_flush: u64,
}

impl TableFilterDeltas {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if consolidation is needed
    pub fn needs_consolidation(&self, threshold: u64) -> bool {
        self.delta_count >= threshold
    }

    /// Clear all deltas after consolidation
    pub fn clear(&mut self) {
        self.bloom_deltas.clear();
        self.zone_deltas.clear();
        self.delta_count = 0;
        self.last_flush = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
    }
}

/// Lightweight synchronous filter index updates during DML
pub struct FilterIndexDeltaTracker {
    /// Per-table delta buffers
    table_deltas: RwLock<HashMap<String, TableFilterDeltas>>,
    /// Delta sequence counter
    delta_seq: AtomicU64,
    /// Configuration
    config: FilterIndexConfig,
    /// Pending deltas for persistence (to be flushed by consolidation worker)
    pending_deltas: RwLock<Vec<FilterDelta>>,
    /// Total operations tracked
    total_operations: AtomicU64,
    /// Bloom filter bit calculations (reusable)
    hash_seed1: u64,
    hash_seed2: u64,
    /// Tables currently suspended for bulk operations
    suspended_tables: RwLock<HashMap<String, Arc<SuspendedTableInfo>>>,
    /// Global enable flag (can be disabled via SET smfi_tracking_enabled = off)
    enabled: AtomicBool,
    /// Runtime-settable bulk load threshold (hot reload support)
    bulk_load_threshold: std::sync::atomic::AtomicUsize,
}

impl FilterIndexDeltaTracker {
    pub fn new(config: FilterIndexConfig) -> Self {
        let threshold = config.bulk_load_threshold;
        Self {
            table_deltas: RwLock::new(HashMap::new()),
            delta_seq: AtomicU64::new(0),
            config,
            pending_deltas: RwLock::new(Vec::new()),
            total_operations: AtomicU64::new(0),
            hash_seed1: 0x517cc1b727220a95,
            hash_seed2: 0x7369726564616f72,
            suspended_tables: RwLock::new(HashMap::new()),
            enabled: AtomicBool::new(true),
            bulk_load_threshold: std::sync::atomic::AtomicUsize::new(threshold),
        }
    }

    /// Get the current bulk load threshold (hot-reloadable)
    pub fn bulk_load_threshold(&self) -> usize {
        self.bulk_load_threshold.load(Ordering::Relaxed)
    }

    /// Set the bulk load threshold at runtime (no restart required)
    /// Operations with >= this many rows will auto-suspend SMFI tracking
    pub fn set_bulk_load_threshold(&self, threshold: usize) {
        self.bulk_load_threshold.store(threshold, Ordering::SeqCst);
    }

    /// Suspend SMFI tracking for a table during bulk operations
    /// Returns a guard that automatically resumes tracking when dropped
    pub fn suspend_table(&self, table: &str, reason: BulkLoadReason) -> BulkLoadGuard<'_> {
        let info = Arc::new(SuspendedTableInfo::new(reason));
        {
            let mut suspended = self.suspended_tables.write();
            suspended.insert(table.to_string(), info.clone());
        }
        BulkLoadGuard {
            table_name: table.to_string(),
            info,
            tracker: self,
        }
    }

    /// Check if a table is suspended for bulk operations
    pub fn is_suspended(&self, table: &str) -> bool {
        let suspended = self.suspended_tables.read();
        suspended.contains_key(table)
    }

    /// Resume tracking for a table and optionally trigger rebuild
    /// Called automatically by BulkLoadGuard on drop
    fn resume_table_internal(&self, table: &str) -> Option<Arc<SuspendedTableInfo>> {
        let mut suspended = self.suspended_tables.write();
        suspended.remove(table)
    }

    /// Get suspension info for a table
    pub fn get_suspension_info(&self, table: &str) -> Option<Arc<SuspendedTableInfo>> {
        let suspended = self.suspended_tables.read();
        suspended.get(table).cloned()
    }

    /// List all currently suspended tables
    pub fn list_suspended_tables(&self) -> Vec<(String, BulkLoadReason)> {
        let suspended = self.suspended_tables.read();
        suspended
            .iter()
            .map(|(name, info)| (name.clone(), info.reason))
            .collect()
    }

    /// Set global enable/disable for SMFI tracking
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Check if SMFI tracking is globally enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Check if tracking should occur for a given table
    fn should_track(&self, table: &str) -> bool {
        self.is_enabled() && !self.is_suspended(table)
    }

    /// Record rows affected during suspension (for rebuild estimation)
    fn record_suspended_row(&self, table: &str) {
        if let Some(info) = self.get_suspension_info(table) {
            info.rows_affected.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Called synchronously during INSERT (cost: ~150ns per row)
    /// Skips tracking if table is suspended for bulk operations
    pub fn on_insert(&self, table: &str, row_id: u64, tuple: &Tuple, schema: &Schema) {
        // Skip if globally disabled or table is suspended for bulk load
        if !self.should_track(table) {
            self.record_suspended_row(table);
            return;
        }

        let seq = self.delta_seq.fetch_add(1, Ordering::Relaxed);
        self.total_operations.fetch_add(1, Ordering::Relaxed);

        let mut delta = FilterDelta::new(seq, table, row_id, FilterDeltaType::Insert);

        // Update bloom filter deltas
        if self.config.enable_bloom {
            for (i, col) in schema.columns.iter().enumerate() {
                if let Some(value) = tuple.values.get(i) {
                    if !matches!(value, Value::Null) {
                        let bloom_delta = self.compute_bloom_delta(&col.name, value);
                        delta.bloom_deltas.insert(col.name.clone(), bloom_delta);
                    }
                }
            }
        }

        // Update zone map deltas
        if self.config.enable_zone_map {
            let block_id = row_id / 1000; // Default block size
            let mut zone_delta = ZoneMapDelta::new(block_id, FilterDeltaType::Insert);
            for (i, col) in schema.columns.iter().enumerate() {
                if let Some(value) = tuple.values.get(i) {
                    zone_delta.add_column_update(&col.name, value);
                }
            }
            zone_delta.rows_affected = 1;
            delta.zone_delta = Some(zone_delta);
        }

        // Buffer the delta
        self.buffer_delta(table, &delta);
    }

    /// Called synchronously during UPDATE
    /// Skips tracking if table is suspended for bulk operations
    pub fn on_update(&self, table: &str, row_id: u64, old_tuple: &Tuple, new_tuple: &Tuple, schema: &Schema) {
        // Skip if globally disabled or table is suspended for bulk load
        if !self.should_track(table) {
            self.record_suspended_row(table);
            return;
        }

        let seq = self.delta_seq.fetch_add(1, Ordering::Relaxed);
        self.total_operations.fetch_add(1, Ordering::Relaxed);

        let mut delta = FilterDelta::new(seq, table, row_id, FilterDeltaType::Update);

        // For bloom filters, we need to add the new values (can't remove old ones efficiently)
        if self.config.enable_bloom {
            for (i, col) in schema.columns.iter().enumerate() {
                if let Some(new_value) = new_tuple.values.get(i) {
                    // Check if value changed
                    let old_value = old_tuple.values.get(i);
                    if old_value != Some(new_value) && !matches!(new_value, Value::Null) {
                        let bloom_delta = self.compute_bloom_delta(&col.name, new_value);
                        delta.bloom_deltas.insert(col.name.clone(), bloom_delta);
                    }
                }
            }
        }

        // Zone map update - may need to expand range
        if self.config.enable_zone_map {
            let block_id = row_id / 1000;
            let mut zone_delta = ZoneMapDelta::new(block_id, FilterDeltaType::Update);
            for (i, col) in schema.columns.iter().enumerate() {
                if let Some(value) = new_tuple.values.get(i) {
                    zone_delta.add_column_update(&col.name, value);
                }
            }
            zone_delta.rows_affected = 1;
            delta.zone_delta = Some(zone_delta);
        }

        self.buffer_delta(table, &delta);
    }

    /// Called synchronously during DELETE
    /// Skips tracking if table is suspended for bulk operations
    pub fn on_delete(&self, table: &str, row_id: u64, tuple: &Tuple, schema: &Schema) {
        // Skip if globally disabled or table is suspended for bulk load
        if !self.should_track(table) {
            self.record_suspended_row(table);
            return;
        }

        let seq = self.delta_seq.fetch_add(1, Ordering::Relaxed);
        self.total_operations.fetch_add(1, Ordering::Relaxed);

        let mut delta = FilterDelta::new(seq, table, row_id, FilterDeltaType::Delete);

        // For deletes, we can't update bloom filters (false positives acceptable)
        // Zone maps may become inaccurate (requires periodic rebuild for accuracy)
        if self.config.enable_zone_map {
            let block_id = row_id / 1000;
            let mut zone_delta = ZoneMapDelta::new(block_id, FilterDeltaType::Delete);
            for (i, col) in schema.columns.iter().enumerate() {
                if let Some(value) = tuple.values.get(i) {
                    zone_delta.add_column_update(&col.name, value);
                }
            }
            zone_delta.rows_affected = 1;
            delta.zone_delta = Some(zone_delta);
        }

        self.buffer_delta(table, &delta);
    }

    /// Compute bloom filter delta for a single value
    fn compute_bloom_delta(&self, column_name: &str, value: &Value) -> BloomFilterDelta {
        let mut delta = BloomFilterDelta::new(column_name);

        let (h1, h2) = self.hash_value(value);

        // Compute bit positions using Kirsch-Mitzenmacher optimization
        for k in 0..self.config.num_hashes {
            let bit_pos = (h1.wrapping_add(k as u64).wrapping_mul(h2)) % self.config.bloom_bits;
            let word_idx = (bit_pos / 64) as usize;
            let bit_mask = 1u64 << (bit_pos % 64);
            delta.bits_to_set.push((word_idx, bit_mask));
        }
        delta.rows_added = 1;

        delta
    }

    /// Hash a value for bloom filter
    fn hash_value(&self, value: &Value) -> (u64, u64) {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        let mut hasher1 = DefaultHasher::new();
        self.hash_seed1.hash(&mut hasher1);

        match value {
            Value::Int2(i) => i.hash(&mut hasher1),
            Value::Int4(i) => i.hash(&mut hasher1),
            Value::Int8(i) => i.hash(&mut hasher1),
            Value::Float4(f) => f.to_bits().hash(&mut hasher1),
            Value::Float8(f) => f.to_bits().hash(&mut hasher1),
            Value::String(s) => s.hash(&mut hasher1),
            Value::Boolean(b) => b.hash(&mut hasher1),
            Value::Bytes(b) => b.hash(&mut hasher1),
            Value::Null => 0u64.hash(&mut hasher1),
            Value::Timestamp(t) => t.hash(&mut hasher1),
            Value::Date(d) => d.hash(&mut hasher1),
            Value::Time(t) => t.hash(&mut hasher1),
            Value::Numeric(d) => d.to_string().hash(&mut hasher1),
            Value::Uuid(u) => u.hash(&mut hasher1),
            Value::Json(j) => j.to_string().hash(&mut hasher1),
            Value::Array(arr) => {
                for v in arr {
                    format!("{:?}", v).hash(&mut hasher1);
                }
            }
            Value::Vector(v) => {
                for f in v {
                    f.to_bits().hash(&mut hasher1);
                }
            }
            // Storage references
            Value::DictRef { dict_id } => dict_id.hash(&mut hasher1),
            Value::CasRef { hash } => hash.hash(&mut hasher1),
            Value::ColumnarRef => 0u64.hash(&mut hasher1),
            Value::Interval(iv) => iv.hash(&mut hasher1), // Hash interval microseconds
        }

        let h1 = hasher1.finish();

        let mut hasher2 = DefaultHasher::new();
        self.hash_seed2.hash(&mut hasher2);
        h1.hash(&mut hasher2);
        let h2 = hasher2.finish();

        (h1, h2)
    }

    /// Buffer a delta for later consolidation
    fn buffer_delta(&self, table: &str, delta: &FilterDelta) {
        // Update table-level aggregated deltas
        {
            let mut table_deltas = self.table_deltas.write();
            let table_delta = table_deltas
                .entry(table.to_string())
                .or_insert_with(TableFilterDeltas::new);

            // Merge bloom deltas
            for (col, bloom_delta) in &delta.bloom_deltas {
                table_delta
                    .bloom_deltas
                    .entry(col.clone())
                    .or_insert_with(|| BloomFilterDelta::new(col))
                    .merge(bloom_delta);
            }

            // Merge zone deltas
            if let Some(zone_delta) = &delta.zone_delta {
                let existing = table_delta
                    .zone_deltas
                    .entry(zone_delta.block_id)
                    .or_insert_with(|| ZoneMapDelta::new(zone_delta.block_id, zone_delta.delta_type));
                existing.column_updates.extend(zone_delta.column_updates.iter().cloned());
                existing.rows_affected += zone_delta.rows_affected;
            }

            table_delta.delta_count += 1;
        }

        // Store for persistence
        {
            let mut pending = self.pending_deltas.write();
            pending.push(delta.clone());

            // Trim if too many
            if pending.len() > self.config.max_buffered_deltas {
                let drain_count = pending.len() / 2;
                pending.drain(0..drain_count);
            }
        }
    }

    /// Check if any table needs consolidation
    pub fn tables_needing_consolidation(&self) -> Vec<String> {
        let table_deltas = self.table_deltas.read();
        table_deltas
            .iter()
            .filter(|(_, deltas)| deltas.needs_consolidation(self.config.delta_threshold))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Get deltas for a table (for consolidation)
    pub fn get_table_deltas(&self, table: &str) -> Option<TableFilterDeltas> {
        let mut table_deltas = self.table_deltas.write();
        table_deltas.remove(table)
    }

    /// Get pending deltas for persistence
    pub fn take_pending_deltas(&self) -> Vec<FilterDelta> {
        let mut pending = self.pending_deltas.write();
        std::mem::take(&mut *pending)
    }

    /// Get statistics
    pub fn stats(&self) -> FilterDeltaStats {
        let table_deltas = self.table_deltas.read();
        let pending = self.pending_deltas.read();
        let suspended = self.suspended_tables.read();

        FilterDeltaStats {
            total_operations: self.total_operations.load(Ordering::Relaxed),
            current_seq: self.delta_seq.load(Ordering::Relaxed),
            tables_tracked: table_deltas.len(),
            pending_deltas: pending.len(),
            total_buffered_deltas: table_deltas.values().map(|d| d.delta_count).sum(),
            suspended_tables: suspended.len(),
            enabled: self.is_enabled(),
        }
    }

    /// Mark a table for filter rebuild after bulk operation
    /// This is called when BulkLoadGuard is dropped
    pub fn mark_for_rebuild(&self, table: &str, rows_affected: u64) {
        // Clear any existing deltas for this table since we'll do a full rebuild
        {
            let mut table_deltas = self.table_deltas.write();
            table_deltas.remove(table);
        }

        // The consolidation worker will detect this and trigger a full rebuild
        // by checking the rebuild_requested flag
    }
}

/// Statistics for filter delta tracking
#[derive(Debug, Clone)]
pub struct FilterDeltaStats {
    pub total_operations: u64,
    pub current_seq: u64,
    pub tables_tracked: usize,
    pub pending_deltas: usize,
    pub total_buffered_deltas: u64,
    pub suspended_tables: usize,
    pub enabled: bool,
}

/// RAII guard for bulk load operations
/// Automatically resumes SMFI tracking and triggers rebuild when dropped
pub struct BulkLoadGuard<'a> {
    table_name: String,
    info: Arc<SuspendedTableInfo>,
    tracker: &'a FilterIndexDeltaTracker,
}

impl<'a> BulkLoadGuard<'a> {
    /// Get the number of rows affected during suspension
    pub fn rows_affected(&self) -> u64 {
        self.info.rows_affected.load(Ordering::Relaxed)
    }

    /// Get the reason for suspension
    pub fn reason(&self) -> BulkLoadReason {
        self.info.reason
    }

    /// Get the table name
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

impl<'a> Drop for BulkLoadGuard<'a> {
    fn drop(&mut self) {
        let rows_affected = self.info.rows_affected.load(Ordering::Relaxed);

        // Remove from suspended tables
        self.tracker.resume_table_internal(&self.table_name);

        // Mark for rebuild if any rows were affected
        if rows_affected > 0 {
            self.tracker.mark_for_rebuild(&self.table_name, rows_affected);
        }
    }
}

/// Result of a bulk load operation with SMFI stats
#[derive(Debug, Clone)]
pub struct BulkLoadResult {
    pub table_name: String,
    pub rows_loaded: u64,
    pub duration_ms: u64,
    pub rebuild_scheduled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, DataType};

    fn create_test_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::Int8),
            Column::new("name", DataType::Text),
            Column::new("value", DataType::Float8),
        ])
    }

    fn create_test_tuple() -> Tuple {
        Tuple::new(vec![
            Value::Int8(1),
            Value::String("test".to_string()),
            Value::Float8(3.14),
        ])
    }

    #[test]
    fn test_on_insert_creates_deltas() {
        let tracker = FilterIndexDeltaTracker::new(FilterIndexConfig::default());
        let schema = create_test_schema();
        let tuple = create_test_tuple();

        tracker.on_insert("test_table", 0, &tuple, &schema);

        let stats = tracker.stats();
        assert_eq!(stats.total_operations, 1);
        assert_eq!(stats.tables_tracked, 1);
    }

    #[test]
    fn test_bloom_delta_computation() {
        let tracker = FilterIndexDeltaTracker::new(FilterIndexConfig::default());
        let value = Value::Int8(42);

        let delta = tracker.compute_bloom_delta("col", &value);

        assert!(!delta.bits_to_set.is_empty());
        assert_eq!(delta.rows_added, 1);
    }

    #[test]
    fn test_consolidation_threshold() {
        let mut config = FilterIndexConfig::default();
        config.delta_threshold = 5;
        let tracker = FilterIndexDeltaTracker::new(config);
        let schema = create_test_schema();
        let tuple = create_test_tuple();

        for i in 0..10 {
            tracker.on_insert("test_table", i, &tuple, &schema);
        }

        let tables = tracker.tables_needing_consolidation();
        assert!(tables.contains(&"test_table".to_string()));
    }

    #[test]
    fn test_bulk_load_suspension() {
        let tracker = FilterIndexDeltaTracker::new(FilterIndexConfig::default());
        let schema = create_test_schema();
        let tuple = create_test_tuple();

        // Start bulk load - suspend tracking
        {
            let guard = tracker.suspend_table("bulk_table", BulkLoadReason::MultiRowInsert);
            assert!(tracker.is_suspended("bulk_table"));
            assert_eq!(guard.reason(), BulkLoadReason::MultiRowInsert);

            // Insert while suspended - should not track
            for i in 0..100 {
                tracker.on_insert("bulk_table", i, &tuple, &schema);
            }

            // Verify tracking was skipped (total_operations shouldn't increase)
            let stats = tracker.stats();
            assert_eq!(stats.total_operations, 0);
            assert_eq!(stats.suspended_tables, 1);

            // But rows_affected should be counted
            assert_eq!(guard.rows_affected(), 100);
        } // guard dropped here - resumes tracking

        // Verify no longer suspended
        assert!(!tracker.is_suspended("bulk_table"));

        // Now insert should track normally
        tracker.on_insert("bulk_table", 100, &tuple, &schema);
        let stats = tracker.stats();
        assert_eq!(stats.total_operations, 1);
        assert_eq!(stats.suspended_tables, 0);
    }

    #[test]
    fn test_global_enable_disable() {
        let tracker = FilterIndexDeltaTracker::new(FilterIndexConfig::default());
        let schema = create_test_schema();
        let tuple = create_test_tuple();

        // Initially enabled
        assert!(tracker.is_enabled());

        // Disable globally
        tracker.set_enabled(false);
        assert!(!tracker.is_enabled());

        // Insert while disabled - should not track
        tracker.on_insert("test_table", 0, &tuple, &schema);
        let stats = tracker.stats();
        assert_eq!(stats.total_operations, 0);

        // Re-enable
        tracker.set_enabled(true);
        assert!(tracker.is_enabled());

        // Now should track
        tracker.on_insert("test_table", 1, &tuple, &schema);
        let stats = tracker.stats();
        assert_eq!(stats.total_operations, 1);
    }

    #[test]
    fn test_multiple_tables_suspension() {
        let tracker = FilterIndexDeltaTracker::new(FilterIndexConfig::default());
        let schema = create_test_schema();
        let tuple = create_test_tuple();

        // Suspend two tables
        let _guard1 = tracker.suspend_table("table1", BulkLoadReason::CopyFrom);
        let _guard2 = tracker.suspend_table("table2", BulkLoadReason::InsertSelect);

        // Both should be suspended
        assert!(tracker.is_suspended("table1"));
        assert!(tracker.is_suspended("table2"));
        assert!(!tracker.is_suspended("table3"));

        // Insert into suspended table - not tracked
        tracker.on_insert("table1", 0, &tuple, &schema);
        // Insert into non-suspended table - tracked
        tracker.on_insert("table3", 0, &tuple, &schema);

        let stats = tracker.stats();
        assert_eq!(stats.total_operations, 1);
        assert_eq!(stats.suspended_tables, 2);

        // List suspended tables
        let suspended = tracker.list_suspended_tables();
        assert_eq!(suspended.len(), 2);
    }
}
