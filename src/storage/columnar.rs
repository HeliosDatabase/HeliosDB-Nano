//! Columnar storage module for analytics-optimized column storage
//!
//! Provides column-grouped storage where values from the same column
//! are stored together in batches. This improves:
//! - Compression (similar values compress better together)
//! - Analytics queries (can read single column without loading entire row)
//! - Aggregation performance (sequential scan of homogeneous data)
//!
//! # Key Format
//!
//! ```text
//! col:{table}:{column}:{batch_id}  -> bincode ColumnBatch (up to 1024 values)
//! colz:{table}:{column}:{batch_id} -> bincode BatchStats (zone-map sidecar, R3.1)
//! colzm:{table}:{column}           -> [1] manifest marker: every col: batch of
//!                                     this column has a colz: sidecar entry
//! ```
//!
//! The `colz:` sidecar is written in the same RocksDB `WriteBatch` as the
//! batch itself (or staged in the same transaction write set), so batch and
//! stats can never diverge. Cost: stats are recomputed from the fully
//! materialized batch on every store — O(BATCH_SIZE) per written value, which
//! is marginal next to the existing whole-batch read-modify-write.
//!
//! Downgrade caveat: a binary older than R3.1 writes `col:` batches without
//! maintaining `colz:`/`colzm:`. Running such a binary against a data
//! directory that already has manifest markers, then upgrading again, can
//! leave markers that overstate sidecar completeness (wrong pruning).
//! Binary downgrades against a live data directory are unsupported; if one
//! happened, deleting the `colzm:` markers (or setting `HELIOS_ZONE_MAP_OFF`)
//! restores correctness and the next scans re-backfill.
//!
//! # Example
//!
//! ```sql
//! CREATE TABLE metrics (
//!     id INT PRIMARY KEY,
//!     timestamp INT8 STORAGE COLUMNAR,
//!     value FLOAT8 STORAGE COLUMNAR
//! );
//! ```

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use super::Transaction;
use crate::{Error, Result, Value};

/// R3.1: serializes zone-stats sidecar writes (autocommit stores, transaction
/// commits touching `col:` keys, and lazy backfill) so a backfill can never
/// attach stats computed from a stale batch image to a freshly written batch.
/// Uncontended in steady state; only columnar writers and the one-time legacy
/// backfill ever take it.
static STATS_WRITE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Acquire the zone-stats write lock (see [`STATS_WRITE_LOCK`]).
pub(crate) fn stats_write_lock() -> parking_lot::MutexGuard<'static, ()> {
    STATS_WRITE_LOCK.lock()
}

/// Number of values per columnar batch
/// 1024 provides good balance between compression and random access
pub const BATCH_SIZE: usize = 1024;

/// A batch of column values stored together
///
/// Each batch contains up to BATCH_SIZE values for a single column,
/// stored contiguously for better compression and sequential access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnBatch {
    /// Column name (for verification)
    pub column: String,
    /// Starting row_id for this batch (row_id = start_row_id + index)
    pub start_row_id: u64,
    /// Values in order, indexed by (row_id - start_row_id)
    pub values: Vec<Value>,
}

impl ColumnBatch {
    /// Create a new empty batch
    pub fn new(column: &str, start_row_id: u64) -> Self {
        Self {
            column: column.to_string(),
            start_row_id,
            values: vec![Value::Null; BATCH_SIZE],
        }
    }

    /// Get value at a specific row_id
    pub fn get(&self, row_id: u64) -> Option<&Value> {
        if row_id < self.start_row_id {
            return None;
        }
        let offset = (row_id - self.start_row_id) as usize;
        self.values.get(offset)
    }

    /// Set value at a specific row_id
    pub fn set(&mut self, row_id: u64, value: Value) -> bool {
        if row_id < self.start_row_id {
            return false;
        }
        let offset = (row_id - self.start_row_id) as usize;
        if offset >= BATCH_SIZE {
            return false;
        }

        // Ensure vector is large enough
        while self.values.len() <= offset {
            self.values.push(Value::Null);
        }

        if let Some(slot) = self.values.get_mut(offset) {
            *slot = value;
        }

        true
    }

    /// Count non-null values in batch
    pub fn count_non_null(&self) -> usize {
        self.values.iter().filter(|v| !matches!(v, Value::Null)).count()
    }
}

/// R3.1 per-batch zone-map statistics, stored as a `colz:` sidecar next to
/// every `col:` batch.
///
/// `min`/`max` are only populated for orderable scalar batches (integers,
/// finite floats, text, date, timestamp) whose non-null values all belong to
/// one comparable family; anything else stores `None`, which disables
/// min/max pruning for the batch (always sound). `row_count` counts SLOTS
/// (`BATCH_SIZE`), not live rows: deleted rows are stored as `Value::Null`
/// and row lookups treat absent trailing slots as NULL, so slots beyond
/// `values.len()` are counted into `null_count`. This is also why COUNT
/// cannot be answered from `row_count` (deletion and NULL are conflated).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchStats {
    /// Minimum non-null value, when the batch is orderable.
    pub min: Option<Value>,
    /// Maximum non-null value, when the batch is orderable.
    pub max: Option<Value>,
    /// Number of NULL slots (includes deleted rows and absent trailing slots).
    pub null_count: u32,
    /// Total slots covered by the batch (always `BATCH_SIZE` in practice).
    pub row_count: u32,
}

impl BatchStats {
    /// Compute stats from a fully materialized batch. O(values.len()).
    pub fn from_batch(batch: &ColumnBatch) -> Self {
        let slot_count = batch.values.len().max(BATCH_SIZE);
        let mut null_count = (slot_count - batch.values.len()) as u32;
        let mut min: Option<&Value> = None;
        let mut max: Option<&Value> = None;
        let mut orderable = true;

        for value in &batch.values {
            if matches!(value, Value::Null) {
                null_count += 1;
                continue;
            }
            if !orderable {
                continue;
            }
            match (min, max) {
                (None, _) => {
                    // First non-null value: zone_cmp(v, v) is None exactly for
                    // non-orderable types and non-finite floats.
                    if zone_cmp(value, value).is_some() {
                        min = Some(value);
                        max = Some(value);
                    } else {
                        orderable = false;
                    }
                }
                (Some(current_min), Some(current_max)) => {
                    match zone_cmp(value, current_min) {
                        Some(std::cmp::Ordering::Less) => min = Some(value),
                        Some(_) => {}
                        None => {
                            orderable = false;
                            continue;
                        }
                    }
                    match zone_cmp(value, current_max) {
                        Some(std::cmp::Ordering::Greater) => max = Some(value),
                        Some(_) => {}
                        None => orderable = false,
                    }
                }
                _ => unreachable!("min/max always set together"),
            }
        }

        let (min, max) = if orderable {
            (min.cloned(), max.cloned())
        } else {
            (None, None)
        };
        Self {
            min,
            max,
            null_count,
            row_count: slot_count as u32,
        }
    }
}

/// Total-order comparison restricted to the zone-map orderable families:
/// integers (cross-width), finite floats (cross-width), text, timestamp and
/// date. Returns `None` for any other type, for mixed families, and for
/// non-finite floats — callers must treat `None` as "batch not orderable".
fn zone_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    fn as_int(v: &Value) -> Option<i64> {
        match v {
            Value::Int2(x) => Some(*x as i64),
            Value::Int4(x) => Some(*x as i64),
            Value::Int8(x) => Some(*x),
            _ => None,
        }
    }
    fn as_float(v: &Value) -> Option<f64> {
        match v {
            Value::Float4(x) => Some(*x as f64),
            Value::Float8(x) => Some(*x),
            _ => None,
        }
    }
    if let (Some(a), Some(b)) = (as_int(a), as_int(b)) {
        return Some(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (as_float(a), as_float(b)) {
        if a.is_finite() && b.is_finite() {
            return a.partial_cmp(&b);
        }
        return None;
    }
    match (a, b) {
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Columnar storage manager
///
/// Provides methods to store and retrieve column values using
/// batch-based columnar storage.
pub struct ColumnarStore;

impl ColumnarStore {
    /// Build the RocksDB key for a column batch
    ///
    /// Format: `col:{table}:{column}:{batch_id}`
    fn batch_key(table: &str, column: &str, batch_id: u64) -> Vec<u8> {
        format!("col:{}:{}:{}", table, column, batch_id).into_bytes()
    }

    /// Build prefix for scanning all batches of a column
    fn column_prefix(table: &str, column: &str) -> Vec<u8> {
        format!("col:{}:{}:", table, column).into_bytes()
    }

    /// Build the RocksDB key for a batch's zone-stats sidecar (R3.1)
    ///
    /// Format: `colz:{table}:{column}:{batch_id}`
    fn stats_key(table: &str, column: &str, batch_id: u64) -> Vec<u8> {
        format!("colz:{}:{}:{}", table, column, batch_id).into_bytes()
    }

    /// Build prefix for scanning all zone-stats entries of a column
    fn stats_prefix(table: &str, column: &str) -> Vec<u8> {
        format!("colz:{}:{}:", table, column).into_bytes()
    }

    /// Manifest marker key: present iff every `col:` batch of the column has
    /// a `colz:` sidecar entry (lets reads enumerate batches from the tiny
    /// sidecar and skip pruned batches without any batch I/O).
    fn stats_marker_key(table: &str, column: &str) -> Vec<u8> {
        format!("colzm:{}:{}", table, column).into_bytes()
    }

    /// Parse the trailing decimal batch id out of a `col:`/`colz:` key.
    fn parse_batch_id(key: &[u8], prefix_len: usize) -> Option<u64> {
        let digits = key.get(prefix_len..)?;
        if digits.is_empty() {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    }

    /// Calculate batch_id and offset for a row_id
    fn batch_location(row_id: u64) -> (u64, usize) {
        let batch_id = row_id / BATCH_SIZE as u64;
        let offset = (row_id % BATCH_SIZE as u64) as usize;
        (batch_id, offset)
    }

    /// Store a value in columnar format
    ///
    /// # Arguments
    /// * `db` - RocksDB instance
    /// * `table` - Table name
    /// * `column` - Column name
    /// * `row_id` - Row identifier
    /// * `value` - Value to store
    pub fn store(db: &DB, table: &str, column: &str, row_id: u64, value: Value) -> Result<()> {
        let (batch_id, _offset) = Self::batch_location(row_id);
        let key = Self::batch_key(table, column, batch_id);

        // Load or create batch
        let mut batch = match db
            .get(&key)
            .map_err(|e| Error::storage(format!("Columnar load failed: {}", e)))?
        {
            Some(data) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?,
            None => ColumnBatch::new(column, batch_id * BATCH_SIZE as u64),
        };

        // Update value
        if !batch.set(row_id, value) {
            return Err(Error::storage(format!(
                "Invalid row_id {} for batch starting at {}",
                row_id, batch.start_row_id
            )));
        }

        // Save batch + zone-stats sidecar in one atomic WriteBatch (R3.1).
        let data =
            bincode::serialize(&batch).map_err(|e| Error::storage(format!("Columnar serialize failed: {}", e)))?;
        let stats = BatchStats::from_batch(&batch);
        let stats_data = bincode::serialize(&stats)
            .map_err(|e| Error::storage(format!("Columnar stats serialize failed: {}", e)))?;

        let mut write_batch = rocksdb::WriteBatch::default();
        write_batch.put(&key, &data);
        write_batch.put(Self::stats_key(table, column, batch_id), &stats_data);

        let _guard = stats_write_lock();
        db.write(write_batch)
            .map_err(|e| Error::storage(format!("Columnar store failed: {}", e)))?;

        Ok(())
    }

    /// Stage a value in columnar format inside a transaction write set.
    ///
    /// This mirrors `store`, but reads any previously staged batch through the
    /// transaction first and buffers the updated batch with `txn.put()`. That
    /// keeps explicit transaction INSERTs rollback-safe and prevents multiple
    /// rows in the same columnar batch from overwriting each other before commit.
    pub fn store_in_transaction(
        db: &DB,
        txn: &Transaction,
        table: &str,
        column: &str,
        row_id: u64,
        value: Value,
    ) -> Result<()> {
        let (batch_id, _offset) = Self::batch_location(row_id);
        let key = Self::batch_key(table, column, batch_id);

        let mut batch = match txn.get(&key)? {
            Some(data) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?,
            None => match db
                .get(&key)
                .map_err(|e| Error::storage(format!("Columnar load failed: {}", e)))?
            {
                Some(data) => bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?,
                None => ColumnBatch::new(column, batch_id * BATCH_SIZE as u64),
            },
        };

        if !batch.set(row_id, value) {
            return Err(Error::storage(format!(
                "Invalid row_id {} for batch starting at {}",
                row_id, batch.start_row_id
            )));
        }

        let data =
            bincode::serialize(&batch).map_err(|e| Error::storage(format!("Columnar serialize failed: {}", e)))?;
        // Stage the zone-stats sidecar alongside the batch: the commit applies
        // the whole write set in one RocksDB WriteBatch (and takes the
        // zone-stats write lock when columnar keys are staged), so batch and
        // stats stay consistent across commit/rollback.
        let stats = BatchStats::from_batch(&batch);
        let stats_data = bincode::serialize(&stats)
            .map_err(|e| Error::storage(format!("Columnar stats serialize failed: {}", e)))?;
        txn.put(Self::stats_key(table, column, batch_id), stats_data)?;
        txn.put(key, data)
    }

    /// Retrieve a value from columnar storage
    ///
    /// # Arguments
    /// * `db` - RocksDB instance
    /// * `table` - Table name
    /// * `column` - Column name
    /// * `row_id` - Row identifier
    ///
    /// # Returns
    /// The value if found, None if batch doesn't exist
    pub fn get(db: &DB, table: &str, column: &str, row_id: u64) -> Result<Option<Value>> {
        let (batch_id, _offset) = Self::batch_location(row_id);
        let key = Self::batch_key(table, column, batch_id);

        match db
            .get(&key)
            .map_err(|e| Error::storage(format!("Columnar load failed: {}", e)))?
        {
            Some(data) => {
                let batch: ColumnBatch = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;
                Ok(batch.get(row_id).cloned())
            }
            None => Ok(None),
        }
    }

    /// Scan an entire column (efficient for aggregations)
    ///
    /// Returns all non-null values with their row_ids.
    ///
    /// # Arguments
    /// * `db` - RocksDB instance
    /// * `table` - Table name
    /// * `column` - Column name
    ///
    /// # Returns
    /// Vector of (row_id, value) pairs for all non-null values
    pub fn scan_column(db: &DB, table: &str, column: &str) -> Result<Vec<(u64, Value)>> {
        let prefix = Self::column_prefix(table, column);
        let mut results = Vec::new();

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

            // Stop if we've passed the prefix
            if !key.starts_with(&prefix) {
                break;
            }

            let batch: ColumnBatch = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;

            // Collect non-null values
            for (i, val) in batch.values.iter().enumerate() {
                if !matches!(val, Value::Null) {
                    results.push((batch.start_row_id + i as u64, val.clone()));
                }
            }
        }

        results.sort_by_key(|(row_id, _)| *row_id);
        Ok(results)
    }

    /// Scan all physical batches for a column.
    ///
    /// This is the lower-overhead primitive for executor-side columnar scans:
    /// callers can use the batch id and row offset directly instead of expanding
    /// every cell into a row-id map first.
    pub fn scan_column_batches(db: &DB, table: &str, column: &str) -> Result<Vec<(u64, ColumnBatch)>> {
        let prefix = Self::column_prefix(table, column);
        let mut batches = Vec::new();

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

            if !key.starts_with(&prefix) {
                break;
            }

            let batch: ColumnBatch = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;
            let batch_id = batch.start_row_id / BATCH_SIZE as u64;
            batches.push((batch_id, batch));
        }

        batches.sort_by_key(|(batch_id, _)| *batch_id);
        Ok(batches)
    }

    /// R3.1: load the zone-stats sidecar for a column: batch_id -> BatchStats.
    ///
    /// The sidecar is tiny (one small record per 1024 rows), so this is cheap
    /// even for large columns. Missing entries simply mean "no stats yet" —
    /// callers must treat such batches as unprunable.
    pub fn load_stats_map(db: &DB, table: &str, column: &str) -> Result<HashMap<u64, BatchStats>> {
        let prefix = Self::stats_prefix(table, column);
        let mut map = HashMap::new();

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Columnar stats iterator error: {}", e)))?;
            if !key.starts_with(&prefix) {
                break;
            }
            let Some(batch_id) = Self::parse_batch_id(&key, prefix.len()) else {
                continue;
            };
            let stats: BatchStats = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Columnar stats deserialize failed: {}", e)))?;
            map.insert(batch_id, stats);
        }

        Ok(map)
    }

    /// True when the `colz:` sidecar is a complete manifest of the column's
    /// batches (marker written after a clean backfill pass; maintained by
    /// every subsequent store, which writes batch + stats atomically).
    fn stats_manifest_complete(db: &DB, table: &str, column: &str) -> bool {
        db.get(Self::stats_marker_key(table, column)).ok().flatten().is_some()
    }

    /// R3.1: scan a column's batches while skipping `pruned` batch ids.
    ///
    /// Two regimes:
    /// - Manifest complete (`colzm:` marker present): batch ids are enumerated
    ///   from the tiny `colz:` sidecar and only surviving batches are fetched
    ///   via `multi_get` — pruned batches cost no batch I/O and no decode.
    /// - Manifest incomplete (legacy data): falls back to a full `col:` prefix
    ///   iteration. Pruned batches skip the bincode decode; batches missing
    ///   stats are backfilled (best-effort, non-fatal) and the manifest marker
    ///   is written once a pass completes cleanly, so the next scan takes the
    ///   manifest path.
    pub fn scan_column_batches_pruned(
        db: &DB,
        table: &str,
        column: &str,
        pruned: &HashSet<u64>,
    ) -> Result<Vec<(u64, ColumnBatch)>> {
        let manifest_complete = Self::stats_manifest_complete(db, table, column);
        if manifest_complete && pruned.is_empty() {
            // Nothing to prune: a plain prefix iteration is the cheapest full read.
            return Self::scan_column_batches(db, table, column);
        }

        if manifest_complete {
            let stats_prefix = Self::stats_prefix(table, column);
            let mut ids: Vec<u64> = Vec::new();
            let iter = db.prefix_iterator(&stats_prefix);
            for item in iter {
                let (key, _) = item.map_err(|e| Error::storage(format!("Columnar stats iterator error: {}", e)))?;
                if !key.starts_with(&stats_prefix) {
                    break;
                }
                if let Some(batch_id) = Self::parse_batch_id(&key, stats_prefix.len()) {
                    if !pruned.contains(&batch_id) {
                        ids.push(batch_id);
                    }
                }
            }
            ids.sort_unstable();

            let keys: Vec<Vec<u8>> = ids.iter().map(|id| Self::batch_key(table, column, *id)).collect();
            let mut batches = Vec::with_capacity(ids.len());
            for (batch_id, result) in ids.iter().zip(db.multi_get(&keys)) {
                let data = result.map_err(|e| Error::storage(format!("Columnar load failed: {}", e)))?;
                // A manifest entry without a batch should not happen (writes
                // are atomic); treat it like an absent batch.
                if let Some(data) = data {
                    let batch: ColumnBatch = bincode::deserialize(&data)
                        .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;
                    batches.push((*batch_id, batch));
                }
            }
            return Ok(batches);
        }

        // Legacy path: stats may be missing for some (or all) batches.
        let stats = Self::load_stats_map(db, table, column)?;
        let prefix = Self::column_prefix(table, column);
        let mut batches = Vec::new();
        let mut pass_clean = true;

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;
            if !key.starts_with(&prefix) {
                break;
            }
            let Some(batch_id) = Self::parse_batch_id(&key, prefix.len()) else {
                pass_clean = false;
                continue;
            };

            if stats.contains_key(&batch_id) {
                if pruned.contains(&batch_id) {
                    continue; // skip decode entirely
                }
            } else if let Err(e) = Self::backfill_batch_stats(db, table, column, batch_id) {
                // Best-effort: stats stay missing (no pruning for this batch),
                // marker is withheld so a later scan retries.
                tracing::warn!(
                    table = table,
                    column = column,
                    batch_id = batch_id,
                    error = %e,
                    "Zone-stats backfill failed"
                );
                pass_clean = false;
            }

            if pruned.contains(&batch_id) {
                continue;
            }
            let batch: ColumnBatch = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;
            batches.push((batch_id, batch));
        }

        if pass_clean {
            // Every batch now has a sidecar entry: record manifest completeness
            // (best-effort; reads work either way).
            let _guard = stats_write_lock();
            if let Err(e) = db.put(Self::stats_marker_key(table, column), [1u8]) {
                tracing::warn!(table = table, column = column, error = %e, "Zone-stats marker write failed");
            }
        }

        batches.sort_by_key(|(batch_id, _)| *batch_id);
        Ok(batches)
    }

    /// Backfill the zone-stats sidecar for one batch from its current
    /// committed content. Takes the stats write lock and re-reads the batch
    /// under it so the stats can never be computed from a stale image while a
    /// concurrent writer updates the batch.
    fn backfill_batch_stats(db: &DB, table: &str, column: &str, batch_id: u64) -> Result<()> {
        let stats_key = Self::stats_key(table, column, batch_id);
        let _guard = stats_write_lock();
        if db
            .get(&stats_key)
            .map_err(|e| Error::storage(format!("Columnar stats load failed: {}", e)))?
            .is_some()
        {
            return Ok(()); // another scan/writer beat us to it
        }
        let Some(data) = db
            .get(Self::batch_key(table, column, batch_id))
            .map_err(|e| Error::storage(format!("Columnar load failed: {}", e)))?
        else {
            return Ok(()); // batch deleted meanwhile
        };
        let batch: ColumnBatch = bincode::deserialize(&data)
            .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;
        let stats_data = bincode::serialize(&BatchStats::from_batch(&batch))
            .map_err(|e| Error::storage(format!("Columnar stats serialize failed: {}", e)))?;
        db.put(&stats_key, &stats_data)
            .map_err(|e| Error::storage(format!("Columnar stats store failed: {}", e)))
    }

    /// Delete columnar data for a specific row
    ///
    /// Sets the value to Null in the batch (doesn't delete the batch).
    pub fn delete(db: &DB, table: &str, column: &str, row_id: u64) -> Result<()> {
        Self::store(db, table, column, row_id, Value::Null)
    }

    /// Delete all columnar data for a table.column
    pub fn drop_column(db: &DB, table: &str, column: &str) -> Result<usize> {
        let prefix = Self::column_prefix(table, column);
        let mut deleted = 0;

        let _guard = stats_write_lock();
        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

            if !key.starts_with(&prefix) {
                break;
            }

            db.delete(&key)
                .map_err(|e| Error::storage(format!("Columnar delete failed: {}", e)))?;
            deleted += 1;
        }

        // Purge the zone-stats sidecar and manifest marker (R3.1) so a
        // recreated column never sees stale stats.
        let stats_prefix = Self::stats_prefix(table, column);
        let iter = db.prefix_iterator(&stats_prefix);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Columnar stats iterator error: {}", e)))?;
            if !key.starts_with(&stats_prefix) {
                break;
            }
            db.delete(&key)
                .map_err(|e| Error::storage(format!("Columnar stats delete failed: {}", e)))?;
        }
        db.delete(Self::stats_marker_key(table, column))
            .map_err(|e| Error::storage(format!("Columnar stats marker delete failed: {}", e)))?;

        Ok(deleted)
    }

    /// Get statistics for a columnar column
    pub fn stats(db: &DB, table: &str, column: &str) -> Result<ColumnarStats> {
        let prefix = Self::column_prefix(table, column);
        let mut total_batches = 0;
        let mut total_values = 0;
        let mut non_null_values = 0;

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

            if !key.starts_with(&prefix) {
                break;
            }

            let batch: ColumnBatch = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Columnar deserialize failed: {}", e)))?;

            total_batches += 1;
            total_values += batch.values.len();
            non_null_values += batch.count_non_null();
        }

        Ok(ColumnarStats {
            batch_count: total_batches,
            total_slots: total_values,
            non_null_values,
            batch_size: BATCH_SIZE,
        })
    }
}

/// Statistics for columnar storage of a column
#[derive(Debug, Clone)]
pub struct ColumnarStats {
    /// Number of batches stored
    pub batch_count: usize,
    /// Total slots across all batches
    pub total_slots: usize,
    /// Number of non-null values
    pub non_null_values: usize,
    /// Values per batch
    pub batch_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, DB) {
        let dir = TempDir::new().unwrap();
        let db = DB::open_default(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_columnar_store_get() {
        let (_dir, db) = test_db();

        // Store values
        ColumnarStore::store(&db, "metrics", "value", 0, Value::Float8(1.5)).unwrap();
        ColumnarStore::store(&db, "metrics", "value", 1, Value::Float8(2.5)).unwrap();
        ColumnarStore::store(&db, "metrics", "value", 2, Value::Float8(3.5)).unwrap();

        // Retrieve values
        assert_eq!(
            ColumnarStore::get(&db, "metrics", "value", 0).unwrap(),
            Some(Value::Float8(1.5))
        );
        assert_eq!(
            ColumnarStore::get(&db, "metrics", "value", 1).unwrap(),
            Some(Value::Float8(2.5))
        );
        assert_eq!(
            ColumnarStore::get(&db, "metrics", "value", 2).unwrap(),
            Some(Value::Float8(3.5))
        );

        // Non-existent row in existing batch
        assert_eq!(
            ColumnarStore::get(&db, "metrics", "value", 100).unwrap(),
            Some(Value::Null)
        );
    }

    #[test]
    fn test_columnar_scan() {
        let (_dir, db) = test_db();

        // Store sparse values
        ColumnarStore::store(&db, "test", "col", 0, Value::Int4(100)).unwrap();
        ColumnarStore::store(&db, "test", "col", 5, Value::Int4(500)).unwrap();
        ColumnarStore::store(&db, "test", "col", 10, Value::Int4(1000)).unwrap();

        // Scan should return only non-null values
        let results = ColumnarStore::scan_column(&db, "test", "col").unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], (0, Value::Int4(100)));
        assert_eq!(results[1], (5, Value::Int4(500)));
        assert_eq!(results[2], (10, Value::Int4(1000)));
    }

    #[test]
    fn test_columnar_multiple_batches() {
        let (_dir, db) = test_db();

        // Store values across multiple batches
        ColumnarStore::store(&db, "test", "col", 0, Value::Int4(1)).unwrap();
        ColumnarStore::store(&db, "test", "col", 1023, Value::Int4(2)).unwrap(); // Last in batch 0
        ColumnarStore::store(&db, "test", "col", 1024, Value::Int4(3)).unwrap(); // First in batch 1
        ColumnarStore::store(&db, "test", "col", 2048, Value::Int4(4)).unwrap(); // First in batch 2

        // Verify all values
        assert_eq!(ColumnarStore::get(&db, "test", "col", 0).unwrap(), Some(Value::Int4(1)));
        assert_eq!(
            ColumnarStore::get(&db, "test", "col", 1023).unwrap(),
            Some(Value::Int4(2))
        );
        assert_eq!(
            ColumnarStore::get(&db, "test", "col", 1024).unwrap(),
            Some(Value::Int4(3))
        );
        assert_eq!(
            ColumnarStore::get(&db, "test", "col", 2048).unwrap(),
            Some(Value::Int4(4))
        );

        // Stats should show 3 batches
        let stats = ColumnarStore::stats(&db, "test", "col").unwrap();
        assert_eq!(stats.batch_count, 3);
        assert_eq!(stats.non_null_values, 4);
    }

    #[test]
    fn test_columnar_scan_orders_decimal_batch_keys_by_row_id() {
        let (_dir, db) = test_db();

        ColumnarStore::store(&db, "test", "col", 10 * BATCH_SIZE as u64, Value::Int4(10)).unwrap();
        ColumnarStore::store(&db, "test", "col", 2 * BATCH_SIZE as u64, Value::Int4(2)).unwrap();

        let results = ColumnarStore::scan_column(&db, "test", "col").unwrap();
        assert_eq!(
            results,
            vec![
                (2 * BATCH_SIZE as u64, Value::Int4(2)),
                (10 * BATCH_SIZE as u64, Value::Int4(10)),
            ]
        );
    }

    #[test]
    fn test_columnar_delete() {
        let (_dir, db) = test_db();

        // Store and then delete
        ColumnarStore::store(&db, "test", "col", 5, Value::Int4(100)).unwrap();
        ColumnarStore::delete(&db, "test", "col", 5).unwrap();

        // Should be Null now
        assert_eq!(ColumnarStore::get(&db, "test", "col", 5).unwrap(), Some(Value::Null));
    }

    #[test]
    fn test_batch_location() {
        assert_eq!(ColumnarStore::batch_location(0), (0, 0));
        assert_eq!(ColumnarStore::batch_location(1023), (0, 1023));
        assert_eq!(ColumnarStore::batch_location(1024), (1, 0));
        assert_eq!(ColumnarStore::batch_location(2047), (1, 1023));
        assert_eq!(ColumnarStore::batch_location(2048), (2, 0));
    }

    #[test]
    fn test_batch_stats_int_min_max_and_nulls() {
        let mut batch = ColumnBatch::new("v", 0);
        batch.set(0, Value::Int4(7));
        batch.set(1, Value::Int4(-3));
        batch.set(2, Value::Int8(42));

        let stats = BatchStats::from_batch(&batch);
        assert_eq!(stats.min, Some(Value::Int4(-3)));
        assert_eq!(stats.max, Some(Value::Int8(42)));
        assert_eq!(stats.row_count, BATCH_SIZE as u32);
        assert_eq!(stats.null_count, BATCH_SIZE as u32 - 3);
    }

    #[test]
    fn test_batch_stats_unorderable_types_store_none() {
        // Boolean is intentionally outside the orderable set.
        let mut batch = ColumnBatch::new("v", 0);
        batch.set(0, Value::Boolean(true));
        batch.set(1, Value::Boolean(false));
        let stats = BatchStats::from_batch(&batch);
        assert_eq!(stats.min, None);
        assert_eq!(stats.max, None);
        assert_eq!(stats.null_count, BATCH_SIZE as u32 - 2);

        // Mixed families (int + text) must also disable min/max.
        let mut batch = ColumnBatch::new("v", 0);
        batch.set(0, Value::Int4(1));
        batch.set(1, Value::String("x".into()));
        let stats = BatchStats::from_batch(&batch);
        assert_eq!(stats.min, None);
        assert_eq!(stats.max, None);

        // Non-finite floats poison ordering.
        let mut batch = ColumnBatch::new("v", 0);
        batch.set(0, Value::Float8(1.0));
        batch.set(1, Value::Float8(f64::NAN));
        let stats = BatchStats::from_batch(&batch);
        assert_eq!(stats.min, None);
        assert_eq!(stats.max, None);
    }

    #[test]
    fn test_batch_stats_all_null() {
        let batch = ColumnBatch::new("v", 0);
        let stats = BatchStats::from_batch(&batch);
        assert_eq!(stats.min, None);
        assert_eq!(stats.max, None);
        assert_eq!(stats.null_count, BATCH_SIZE as u32);
        assert_eq!(stats.row_count, BATCH_SIZE as u32);
    }

    #[test]
    fn test_store_writes_stats_sidecar_atomically() {
        let (_dir, db) = test_db();
        ColumnarStore::store(&db, "t", "v", 5, Value::Int4(100)).unwrap();

        let stats_map = ColumnarStore::load_stats_map(&db, "t", "v").unwrap();
        assert_eq!(stats_map.len(), 1);
        let stats = stats_map.get(&0).unwrap();
        assert_eq!(stats.min, Some(Value::Int4(100)));
        assert_eq!(stats.max, Some(Value::Int4(100)));

        // Delete (stores Null) must refresh the sidecar in the same write.
        ColumnarStore::delete(&db, "t", "v", 5).unwrap();
        let stats_map = ColumnarStore::load_stats_map(&db, "t", "v").unwrap();
        let stats = stats_map.get(&0).unwrap();
        assert_eq!(stats.min, None);
        assert_eq!(stats.null_count, BATCH_SIZE as u32);
    }

    #[test]
    fn test_lazy_backfill_writes_stats_and_marker() {
        let (_dir, db) = test_db();

        // Simulate legacy data: raw col: batches with no colz: sidecar.
        for batch_id in 0..3u64 {
            let mut batch = ColumnBatch::new("v", batch_id * BATCH_SIZE as u64);
            for offset in 0..BATCH_SIZE {
                batch.set(
                    batch_id * BATCH_SIZE as u64 + offset as u64,
                    Value::Int8((batch_id * BATCH_SIZE as u64 + offset as u64) as i64),
                );
            }
            let data = bincode::serialize(&batch).unwrap();
            db.put(ColumnarStore::batch_key("t", "v", batch_id), data).unwrap();
        }
        assert!(ColumnarStore::load_stats_map(&db, "t", "v").unwrap().is_empty());
        assert!(!ColumnarStore::stats_manifest_complete(&db, "t", "v"));

        // First pruned scan: backfills all stats and sets the marker.
        let empty = HashSet::new();
        let batches = ColumnarStore::scan_column_batches_pruned(&db, "t", "v", &empty).unwrap();
        assert_eq!(batches.len(), 3);
        let stats_map = ColumnarStore::load_stats_map(&db, "t", "v").unwrap();
        assert_eq!(stats_map.len(), 3);
        assert!(ColumnarStore::stats_manifest_complete(&db, "t", "v"));
        assert_eq!(stats_map.get(&1).unwrap().min, Some(Value::Int8(1024)));
        assert_eq!(stats_map.get(&1).unwrap().max, Some(Value::Int8(2047)));

        // Second scan takes the manifest path; pruning skips batch fetches.
        let pruned: HashSet<u64> = [0u64, 2u64].into_iter().collect();
        let batches = ColumnarStore::scan_column_batches_pruned(&db, "t", "v", &pruned).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches.first().map(|(id, _)| *id), Some(1));

        // Equivalence with the eager scan when nothing is pruned.
        let pruned_none = HashSet::new();
        let lazy = ColumnarStore::scan_column_batches_pruned(&db, "t", "v", &pruned_none).unwrap();
        let eager = ColumnarStore::scan_column_batches(&db, "t", "v").unwrap();
        assert_eq!(lazy.len(), eager.len());
        for ((lid, lbatch), (eid, ebatch)) in lazy.iter().zip(eager.iter()) {
            assert_eq!(lid, eid);
            assert_eq!(lbatch.values, ebatch.values);
        }
    }

    #[test]
    fn test_drop_column_purges_stats_and_marker() {
        let (_dir, db) = test_db();
        ColumnarStore::store(&db, "t", "v", 0, Value::Int4(1)).unwrap();
        let empty = HashSet::new();
        ColumnarStore::scan_column_batches_pruned(&db, "t", "v", &empty).unwrap();
        assert!(ColumnarStore::stats_manifest_complete(&db, "t", "v"));

        ColumnarStore::drop_column(&db, "t", "v").unwrap();
        assert!(ColumnarStore::load_stats_map(&db, "t", "v").unwrap().is_empty());
        assert!(!ColumnarStore::stats_manifest_complete(&db, "t", "v"));
    }
}
