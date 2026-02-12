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
//! col:{table}:{column}:{batch_id} -> bincode ColumnBatch (up to 1024 values)
//! ```
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

use serde::{Deserialize, Serialize};
use rocksdb::DB;

use crate::{Error, Result, Value};

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
    pub fn store(
        db: &DB,
        table: &str,
        column: &str,
        row_id: u64,
        value: Value,
    ) -> Result<()> {
        let (batch_id, _offset) = Self::batch_location(row_id);
        let key = Self::batch_key(table, column, batch_id);

        // Load or create batch
        let mut batch = match db.get(&key)
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

        // Save batch
        let data = bincode::serialize(&batch)
            .map_err(|e| Error::storage(format!("Columnar serialize failed: {}", e)))?;
        db.put(&key, &data)
            .map_err(|e| Error::storage(format!("Columnar store failed: {}", e)))?;

        Ok(())
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
    pub fn get(
        db: &DB,
        table: &str,
        column: &str,
        row_id: u64,
    ) -> Result<Option<Value>> {
        let (batch_id, _offset) = Self::batch_location(row_id);
        let key = Self::batch_key(table, column, batch_id);

        match db.get(&key)
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
    pub fn scan_column(
        db: &DB,
        table: &str,
        column: &str,
    ) -> Result<Vec<(u64, Value)>> {
        let prefix = Self::column_prefix(table, column);
        let mut results = Vec::new();

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

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

        Ok(results)
    }

    /// Delete columnar data for a specific row
    ///
    /// Sets the value to Null in the batch (doesn't delete the batch).
    pub fn delete(
        db: &DB,
        table: &str,
        column: &str,
        row_id: u64,
    ) -> Result<()> {
        Self::store(db, table, column, row_id, Value::Null)
    }

    /// Delete all columnar data for a table.column
    pub fn drop_column(
        db: &DB,
        table: &str,
        column: &str,
    ) -> Result<usize> {
        let prefix = Self::column_prefix(table, column);
        let mut deleted = 0;

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, _) = item
                .map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

            if !key.starts_with(&prefix) {
                break;
            }

            db.delete(&key)
                .map_err(|e| Error::storage(format!("Columnar delete failed: {}", e)))?;
            deleted += 1;
        }

        Ok(deleted)
    }

    /// Get statistics for a columnar column
    pub fn stats(
        db: &DB,
        table: &str,
        column: &str,
    ) -> Result<ColumnarStats> {
        let prefix = Self::column_prefix(table, column);
        let mut total_batches = 0;
        let mut total_values = 0;
        let mut non_null_values = 0;

        let iter = db.prefix_iterator(&prefix);
        for item in iter {
            let (key, value) = item
                .map_err(|e| Error::storage(format!("Columnar iterator error: {}", e)))?;

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
        assert_eq!(
            ColumnarStore::get(&db, "test", "col", 0).unwrap(),
            Some(Value::Int4(1))
        );
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
    fn test_columnar_delete() {
        let (_dir, db) = test_db();

        // Store and then delete
        ColumnarStore::store(&db, "test", "col", 5, Value::Int4(100)).unwrap();
        ColumnarStore::delete(&db, "test", "col", 5).unwrap();

        // Should be Null now
        assert_eq!(
            ColumnarStore::get(&db, "test", "col", 5).unwrap(),
            Some(Value::Null)
        );
    }

    #[test]
    fn test_batch_location() {
        assert_eq!(ColumnarStore::batch_location(0), (0, 0));
        assert_eq!(ColumnarStore::batch_location(1023), (0, 1023));
        assert_eq!(ColumnarStore::batch_location(1024), (1, 0));
        assert_eq!(ColumnarStore::batch_location(2047), (1, 1023));
        assert_eq!(ColumnarStore::batch_location(2048), (2, 0));
    }
}
