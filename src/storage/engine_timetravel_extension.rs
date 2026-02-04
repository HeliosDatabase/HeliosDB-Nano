//! Time-travel extensions for StorageEngine
//!
//! This module provides extension methods for time-travel query support.

use super::{StorageEngine, SnapshotManager};
use crate::{Error, Result, Tuple};
use rocksdb::{IteratorMode, ReadOptions};

impl StorageEngine {
    /// Get a reference to the snapshot manager
    pub fn snapshot_manager(&self) -> &SnapshotManager {
        &self.snapshot_manager
    }

    /// Scan table at a specific snapshot (for time-travel queries)
    ///
    /// Returns tuples as they existed at the given snapshot timestamp.
    pub fn scan_table_at_snapshot(&self, table_name: &str, snapshot_ts: u64) -> Result<Vec<Tuple>> {
        let prefix = format!("data:{}:", table_name);
        let prefix_bytes = prefix.as_bytes();

        let mut tuples = Vec::new();
        let mut seen_rows = std::collections::HashSet::new();

        // First, scan current data to get all row IDs
        // Use total_order_seek to bypass prefix bloom filter for full table scans
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.db.iterator_opt(IteratorMode::Start, read_opts);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if key.starts_with(prefix_bytes) {
                // Parse row ID from key: data:{table}:{row_id}
                if let Ok(key_str) = std::str::from_utf8(&key) {
                    if let Some(row_id_str) = key_str.strip_prefix(&prefix) {
                        if let Ok(row_id) = row_id_str.parse::<u64>() {
                            seen_rows.insert(row_id);
                        }
                    }
                }
            } else if !key.is_empty() && key[0] > prefix_bytes[0] {
                break;
            }
        }

        // For each row, read the version at the snapshot
        for row_id in seen_rows {
            if let Some(value) = self.snapshot_manager.read_at_snapshot(table_name, row_id, snapshot_ts)? {
                let tuple: Tuple = bincode::deserialize(&value)
                    .map_err(|e| Error::storage(format!("Failed to deserialize tuple: {}", e)))?;
                tuples.push(tuple);
            }
        }

        Ok(tuples)
    }

    /// Insert tuple with versioning (for time-travel support)
    ///
    /// This creates a new version of the tuple at the current timestamp,
    /// enabling AS OF TIMESTAMP/TRANSACTION/SCN queries.
    ///
    /// This method is automatically called by insert_tuple() when
    /// time_travel_enabled is true (default). It can also be called directly
    /// to force versioning even when automatic versioning is disabled.
    ///
    /// Implementation details:
    /// - Writes current version to main data key (for fast non-time-travel queries)
    /// - Writes versioned copy to snapshot storage (for time-travel queries)
    /// - Registers snapshot metadata (timestamp, transaction ID, SCN)
    /// - Triggers automatic garbage collection if needed
    pub fn insert_tuple_versioned(&self, table_name: &str, tuple: Tuple) -> Result<u64> {
        use super::Catalog;

        let catalog = Catalog::new(self);
        let row_id = catalog.next_row_id(table_name)?;

        // Get table schema for compression
        let schema = catalog.get_table_schema(table_name)?;

        // Calculate original tuple size for metrics
        let original_size = bincode::serialize(&tuple)
            .map_err(|e| Error::storage(format!("Failed to calculate tuple size: {}", e)))?
            .len();

        // Check if compression is disabled for this table
        let compression_enabled = catalog.get_compression_config(table_name)?
            .map(|config| config.enabled)
            .unwrap_or(true); // Default to enabled if no config

        // Compress tuple using per-column compression (or skip if disabled)
        let compressed = if compression_enabled {
            super::compression::compress_tuple(
                &tuple,
                &schema,
                table_name,
                &self.compression_manager,
            )?
        } else {
            // Compression disabled - store uncompressed with None codec
            super::compression::CompressedTuple {
                values: tuple.values.iter().map(|v| {
                    let mut result = vec![super::compression::CompressionCodec::None.to_u8()];
                    let value_bytes = bincode::serialize(v).unwrap_or_default();
                    result.extend_from_slice(&value_bytes);
                    result
                }).collect(),
            }
        };

        // Serialize compressed tuple
        let value = bincode::serialize(&compressed)
            .map_err(|e| Error::storage(format!("Failed to serialize compressed tuple: {}", e)))?;

        // Track compression metrics per column
        for (i, column) in schema.columns.iter().enumerate() {
            if i < tuple.values.len() && i < compressed.values.len() {
                let orig_col_size = bincode::serialize(&tuple.values[i])
                    .map(|v| v.len())
                    .unwrap_or(0);
                let compressed_col_size = compressed.values[i].len();

                // Extract codec from compressed value (first byte)
                let codec = if !compressed.values[i].is_empty() {
                    super::compression::CompressionCodec::from_u8(compressed.values[i][0])
                        .unwrap_or(super::compression::CompressionCodec::None)
                } else {
                    super::compression::CompressionCodec::None
                };

                self.compression_manager.update_stats(
                    table_name,
                    &column.name,
                    orig_col_size,
                    compressed_col_size,
                    codec,
                );
            }
        }

        // Get current timestamp (for MVCC)
        let timestamp = self.next_timestamp();

        eprintln!("DEBUG insert_tuple_versioned: table={}, row_id={}, timestamp={}", table_name, row_id, timestamp);

        // Write current version (for fast non-time-travel queries)
        // This ensures zero-performance overhead for normal queries
        let current_key = format!("data:{}:{}", table_name, row_id).into_bytes();
        self.put(&current_key, &value)?;

        // Write versioned copy (for time-travel queries)
        // This enables AS OF queries with O(1) snapshot lookup
        self.snapshot_manager.write_version(table_name, row_id, timestamp, &value)?;

        // Register snapshot with tri-modal resolution support
        // (Timestamp, Transaction ID, SCN)
        let _ = self.snapshot_manager.register_snapshot(timestamp);

        Ok(row_id)
    }
}
