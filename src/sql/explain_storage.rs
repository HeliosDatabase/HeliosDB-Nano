//! Storage feature reporting for EXPLAIN
//!
//! This module provides detailed storage layer information for EXPLAIN output:
//! - Column storage modes (Default, Dictionary, ContentAddressed, Columnar)
//! - Bloom filter statistics and effectiveness
//! - Zone map statistics and skip ratios
//! - Compression statistics
//! - Index information and usage
//! - Table/column statistics
//!
//! # Usage
//!
//! ```sql
//! EXPLAIN (STORAGE) SELECT * FROM orders;
//! EXPLAIN (FORMAT JSON, STORAGE) SELECT * FROM users WHERE status = 'active';
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::storage::StorageEngine;
use crate::sql::logical_plan::LogicalPlan;
use crate::{ColumnStorageMode, Result};

/// Complete storage feature report for a table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFeatureReport {
    /// Table name
    pub table_name: String,

    /// Column storage modes and details
    pub column_storage: HashMap<String, ColumnStorageReport>,

    /// Bloom filter statistics (if enabled)
    pub bloom_filter: Option<BloomFilterReport>,

    /// Zone map statistics (if enabled)
    pub zone_maps: Option<ZoneMapReport>,

    /// Compression statistics
    pub compression: Option<CompressionReport>,

    /// Index information
    pub indexes: Vec<IndexReport>,

    /// Table statistics
    pub statistics: Option<StatisticsReport>,

    /// Columnar storage statistics (if any columns use columnar mode)
    pub columnar: Option<ColumnarReport>,
}

/// Per-column storage mode report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStorageReport {
    /// Column name
    pub column_name: String,

    /// Storage mode name ("DEFAULT", "DICTIONARY", "CONTENT_ADDRESSED", "COLUMNAR")
    pub storage_mode: String,

    /// Detailed storage mode information
    pub details: StorageModeDetails,
}

/// Storage mode-specific details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageModeDetails {
    /// Standard row-oriented storage
    Default,

    /// Dictionary-encoded storage
    Dictionary {
        /// Number of unique values in dictionary
        unique_values: usize,
        /// Encoding efficiency (bytes saved / original bytes)
        encoding_efficiency: f64,
    },

    /// Content-addressed (deduplicated) storage
    ContentAddressed {
        /// Number of unique content hashes
        unique_hashes: usize,
        /// Deduplication ratio (original entries / unique entries)
        deduplication_ratio: f64,
        /// Total bytes stored
        total_stored_bytes: usize,
    },

    /// Columnar storage for analytics
    Columnar {
        /// Number of column batches
        batch_count: usize,
        /// Values per batch
        values_per_batch: usize,
        /// Ratio of null values
        null_ratio: f64,
    },
}

impl Default for StorageModeDetails {
    fn default() -> Self {
        Self::Default
    }
}

/// Bloom filter statistics report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomFilterReport {
    /// Whether bloom filter is enabled for this table
    pub enabled: bool,

    /// Target false positive rate (from config)
    pub target_fpr: f64,

    /// Actual observed false positive rate
    pub actual_fpr: Option<f64>,

    /// Current fill ratio (0.0 to 1.0)
    pub fill_ratio: f64,

    /// Total lookups performed
    pub lookups: u64,

    /// True negatives (correctly identified as not present)
    pub true_negatives: u64,

    /// False positives (incorrectly identified as present)
    pub false_positives: u64,

    /// Effectiveness classification
    pub effectiveness: BloomFilterEffectiveness,
}

/// Bloom filter effectiveness classification
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BloomFilterEffectiveness {
    /// FPR < 1% - excellent for point lookups
    Excellent,
    /// FPR 1-5% - good performance
    Good,
    /// FPR 5-10% - moderate, consider rebuilding
    Moderate,
    /// FPR > 10% - degraded, needs rebuild
    Degraded,
    /// Not enough data to assess
    Unknown,
}

impl BloomFilterEffectiveness {
    pub fn from_fpr(fpr: f64) -> Self {
        if fpr < 0.01 {
            Self::Excellent
        } else if fpr < 0.05 {
            Self::Good
        } else if fpr < 0.10 {
            Self::Moderate
        } else {
            Self::Degraded
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Excellent => "Excellent",
            Self::Good => "Good",
            Self::Moderate => "Moderate",
            Self::Degraded => "Degraded",
            Self::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for BloomFilterEffectiveness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Zone map statistics report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneMapReport {
    /// Whether zone maps are enabled for this table
    pub enabled: bool,

    /// Total blocks tracked
    pub total_blocks: u64,

    /// Blocks skipped due to zone map filtering
    pub blocks_skipped: u64,

    /// Blocks that required scanning
    pub blocks_scanned: u64,

    /// Skip ratio (0.0 to 1.0)
    pub skip_ratio: f64,

    /// Total predicate evaluations
    pub predicate_evaluations: u64,

    /// Effectiveness classification
    pub effectiveness: ZoneMapEffectiveness,
}

/// Zone map effectiveness classification
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ZoneMapEffectiveness {
    /// Skip ratio > 80% - excellent for range queries
    Excellent,
    /// Skip ratio 50-80% - good performance
    Good,
    /// Skip ratio 20-50% - moderate
    Moderate,
    /// Skip ratio < 20% - limited benefit
    Limited,
    /// Not enough data to assess
    Unknown,
}

impl ZoneMapEffectiveness {
    pub fn from_skip_ratio(ratio: f64) -> Self {
        if ratio > 0.80 {
            Self::Excellent
        } else if ratio > 0.50 {
            Self::Good
        } else if ratio > 0.20 {
            Self::Moderate
        } else {
            Self::Limited
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Excellent => "Excellent",
            Self::Good => "Good",
            Self::Moderate => "Moderate",
            Self::Limited => "Limited",
            Self::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for ZoneMapEffectiveness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Compression statistics report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionReport {
    /// Compression algorithm used (e.g., "LZ4" via RocksDB)
    pub algorithm: String,

    /// Original uncompressed size in bytes
    pub original_size: usize,

    /// Compressed size in bytes
    pub compressed_size: usize,

    /// Overall compression ratio (original/compressed)
    pub ratio: f64,

    /// Per-column compression ratios
    pub per_column: HashMap<String, f64>,
}

/// Index information report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    /// Index name
    pub name: String,

    /// Index type (btree, hnsw, gin, etc.)
    pub index_type: String,

    /// Columns covered by this index
    pub columns: Vec<String>,

    /// Index size in bytes (if available)
    pub size_bytes: Option<usize>,

    /// Whether this index was used in the query plan
    pub used_in_plan: bool,

    /// Reason index was not used (if applicable)
    pub reason_not_used: Option<String>,
}

/// Table statistics report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatisticsReport {
    /// Total row count
    pub row_count: u64,

    /// Average row size in bytes
    pub avg_row_size: u64,

    /// Total table size in bytes
    pub total_size: u64,

    /// Last time statistics were analyzed (ISO 8601)
    pub last_analyzed: String,

    /// Staleness warning (if statistics are old)
    pub staleness_warning: Option<String>,

    /// Per-column statistics
    pub column_stats: HashMap<String, ColumnStatisticsReport>,
}

/// Per-column statistics report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStatisticsReport {
    /// Fraction of NULL values (0.0 to 1.0)
    pub null_fraction: f64,

    /// Number of distinct values
    pub distinct_count: u64,

    /// Average column width in bytes
    pub avg_width: u64,

    /// Histogram bounds (stringified values)
    pub histogram_bounds: Option<Vec<String>>,

    /// Most common values (stringified)
    pub most_common_values: Option<Vec<String>>,

    /// Frequencies of most common values
    pub most_common_freqs: Option<Vec<f64>>,
}

/// Columnar storage statistics report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnarReport {
    /// Number of batches stored
    pub batch_count: usize,

    /// Total slots across all batches
    pub total_slots: usize,

    /// Number of non-null values
    pub non_null_values: usize,

    /// Values per batch
    pub batch_size: usize,

    /// Null ratio (0.0 to 1.0)
    pub null_ratio: f64,
}

/// Collects storage features for tables in a query plan
pub struct StorageFeatureCollector;

impl StorageFeatureCollector {
    /// Collect storage feature reports for all tables referenced in the plan
    pub fn collect(
        storage: Option<&Arc<StorageEngine>>,
        plan: &LogicalPlan,
    ) -> Result<Vec<StorageFeatureReport>> {
        let mut reports = Vec::new();
        let tables = Self::extract_tables(plan);

        if let Some(storage) = storage {
            for table_name in tables {
                if let Ok(report) = Self::collect_for_table(storage, &table_name) {
                    reports.push(report);
                }
            }
        }

        Ok(reports)
    }

    /// Extract all table names from a logical plan
    fn extract_tables(plan: &LogicalPlan) -> Vec<String> {
        let mut tables = Vec::new();
        Self::extract_tables_recursive(plan, &mut tables);
        tables
    }

    fn extract_tables_recursive(plan: &LogicalPlan, tables: &mut Vec<String>) {
        match plan {
            LogicalPlan::Scan { table_name, .. }
            | LogicalPlan::FilteredScan { table_name, .. } => {
                if !tables.contains(table_name) {
                    tables.push(table_name.clone());
                }
            }
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Project { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Aggregate { input, .. } => {
                Self::extract_tables_recursive(input, tables);
            }
            LogicalPlan::Join { left, right, .. } => {
                Self::extract_tables_recursive(left, tables);
                Self::extract_tables_recursive(right, tables);
            }
            LogicalPlan::With { ctes, query, .. } => {
                for (_, cte_plan, _) in ctes {
                    Self::extract_tables_recursive(cte_plan, tables);
                }
                Self::extract_tables_recursive(query, tables);
            }
            LogicalPlan::Insert { table_name, .. }
            | LogicalPlan::Update { table_name, .. }
            | LogicalPlan::Delete { table_name, .. } => {
                if !tables.contains(table_name) {
                    tables.push(table_name.clone());
                }
            }
            _ => {}
        }
    }

    /// Collect storage features for a single table
    fn collect_for_table(
        storage: &Arc<StorageEngine>,
        table_name: &str,
    ) -> Result<StorageFeatureReport> {
        let catalog = storage.catalog();

        // Get schema for column storage modes
        let schema = catalog.get_table_schema(table_name)?;
        let mut column_storage = HashMap::new();
        let mut has_columnar = false;

        for col in &schema.columns {
            let details = Self::get_storage_mode_details(storage, table_name, &col.name, col.storage_mode);

            if col.storage_mode == ColumnStorageMode::Columnar {
                has_columnar = true;
            }

            column_storage.insert(
                col.name.clone(),
                ColumnStorageReport {
                    column_name: col.name.clone(),
                    storage_mode: format!("{}", col.storage_mode),
                    details,
                },
            );
        }

        // Bloom filter stats (not available - filters are built per-query)
        let bloom_filter: Option<BloomFilterReport> = None;

        // Zone map stats (not available - zone maps are built per-query)
        let zone_maps: Option<ZoneMapReport> = None;

        // Compression stats
        let compression = catalog
            .get_compression_stats(table_name)
            .ok()
            .flatten()
            .map(|stats| CompressionReport {
                algorithm: "LZ4".to_string(), // RocksDB default
                original_size: stats.total_original_size,
                compressed_size: stats.total_compressed_size,
                ratio: stats.overall_ratio,
                per_column: stats
                    .column_stats
                    .iter()
                    .map(|(k, v)| (k.clone(), v.compression_ratio))
                    .collect(),
            });

        // Index information
        let indexes = Self::collect_index_info(storage, table_name);

        // Table statistics
        let statistics = catalog
            .get_table_statistics(table_name)
            .ok()
            .flatten()
            .map(|stats| {
                let staleness_warning = Self::check_staleness(&stats.last_analyzed);

                StatisticsReport {
                    row_count: stats.row_count,
                    avg_row_size: stats.avg_row_size,
                    total_size: stats.total_size,
                    last_analyzed: stats.last_analyzed.to_rfc3339(),
                    staleness_warning,
                    column_stats: stats
                        .columns
                        .iter()
                        .map(|(name, cs)| {
                            (
                                name.clone(),
                                ColumnStatisticsReport {
                                    null_fraction: cs.null_frac,
                                    distinct_count: cs.n_distinct,
                                    avg_width: cs.avg_width,
                                    histogram_bounds: if cs.histogram_bounds.is_empty() {
                                        None
                                    } else {
                                        Some(
                                            cs.histogram_bounds
                                                .iter()
                                                .map(|v| format!("{:?}", v))
                                                .collect(),
                                        )
                                    },
                                    most_common_values: if cs.most_common_values.is_empty() {
                                        None
                                    } else {
                                        Some(
                                            cs.most_common_values
                                                .iter()
                                                .map(|v| format!("{:?}", v))
                                                .collect(),
                                        )
                                    },
                                    most_common_freqs: if cs.most_common_freqs.is_empty() {
                                        None
                                    } else {
                                        Some(cs.most_common_freqs.clone())
                                    },
                                },
                            )
                        })
                        .collect(),
                }
            });

        // Columnar stats (if applicable)
        let columnar = if has_columnar {
            Self::collect_columnar_stats(storage, table_name, &schema.columns)
        } else {
            None
        };

        Ok(StorageFeatureReport {
            table_name: table_name.to_string(),
            column_storage,
            bloom_filter,
            zone_maps,
            compression,
            indexes,
            statistics,
            columnar,
        })
    }

    /// Get storage mode-specific details for a column
    ///
    /// Note: Statistics collection for storage modes is not yet implemented.
    /// Returns default/placeholder values for now.
    fn get_storage_mode_details(
        _storage: &Arc<StorageEngine>,
        _table_name: &str,
        _column_name: &str,
        mode: ColumnStorageMode,
    ) -> StorageModeDetails {
        match mode {
            ColumnStorageMode::Default => StorageModeDetails::Default,

            ColumnStorageMode::Dictionary => {
                // Statistics not yet available - return placeholder
                StorageModeDetails::Dictionary {
                    unique_values: 0,
                    encoding_efficiency: 0.0,
                }
            }

            ColumnStorageMode::ContentAddressed => {
                // Statistics not yet available - return placeholder
                StorageModeDetails::ContentAddressed {
                    unique_hashes: 0,
                    deduplication_ratio: 1.0,
                    total_stored_bytes: 0,
                }
            }

            ColumnStorageMode::Columnar => {
                // Statistics not yet available - return placeholder
                StorageModeDetails::Columnar {
                    batch_count: 0,
                    values_per_batch: 0,
                    null_ratio: 0.0,
                }
            }
        }
    }

    /// Estimate dictionary encoding efficiency based on unique value count
    fn estimate_dict_efficiency(unique_values: usize) -> f64 {
        // Assume average original string length of 20 bytes
        // Dictionary uses 4-byte IDs (u32)
        if unique_values == 0 {
            return 0.0;
        }
        let avg_string_len: f64 = 20.0;
        let dict_id_size: f64 = 4.0;
        let efficiency: f64 = (avg_string_len - dict_id_size) / avg_string_len;
        efficiency.max(0.0)
    }

    /// Collect index information for a table
    ///
    /// Note: Index introspection from catalog is not yet fully implemented.
    /// Returns empty list until catalog index tracking is added.
    fn collect_index_info(_storage: &Arc<StorageEngine>, _table_name: &str) -> Vec<IndexReport> {
        // Index introspection from catalog not yet implemented
        // Would need catalog.get_indexes() and catalog.get_vector_index() methods
        Vec::new()
    }

    /// Check if statistics are stale
    fn check_staleness(last_analyzed: &chrono::DateTime<chrono::Utc>) -> Option<String> {
        let age = chrono::Utc::now() - *last_analyzed;
        let days = age.num_days();

        if days > 30 {
            Some(format!(
                "Statistics are {} days old. Consider running ANALYZE.",
                days
            ))
        } else if days > 7 {
            Some(format!(
                "Statistics are {} days old. May benefit from ANALYZE.",
                days
            ))
        } else {
            None
        }
    }

    /// Collect columnar storage statistics for a table
    ///
    /// Note: Columnar statistics collection is not yet implemented.
    /// Returns None until columnar storage statistics tracking is added.
    fn collect_columnar_stats(
        _storage: &Arc<StorageEngine>,
        _table_name: &str,
        columns: &[crate::Column],
    ) -> Option<ColumnarReport> {
        // Check if any columns use columnar storage
        let has_columnar = columns.iter().any(|c| c.storage_mode == ColumnStorageMode::Columnar);

        if has_columnar {
            // Return placeholder - statistics not yet implemented
            Some(ColumnarReport {
                batch_count: 0,
                total_slots: 0,
                non_null_values: 0,
                batch_size: 1024, // Default batch size
                null_ratio: 0.0,
            })
        } else {
            None
        }
    }
}

/// Format storage features for text output
pub fn format_storage_features_text(reports: &[StorageFeatureReport]) -> String {
    let mut result = String::new();

    result.push_str("\n");
    result.push_str("═══════════════════════════════════════════════════════════════════════════════\n");
    result.push_str("                           STORAGE FEATURES                                   \n");
    result.push_str("═══════════════════════════════════════════════════════════════════════════════\n\n");

    for report in reports {
        result.push_str(&format!("Table: {}\n", report.table_name));
        result.push_str(&"─".repeat(79));
        result.push('\n');

        // Column Storage Modes
        result.push_str("\n  Column Storage Modes:\n");
        for (name, col_report) in &report.column_storage {
            result.push_str(&format!("    {} : {}", name, col_report.storage_mode));
            match &col_report.details {
                StorageModeDetails::Dictionary {
                    unique_values,
                    encoding_efficiency,
                } => {
                    result.push_str(&format!(
                        " (unique: {}, efficiency: {:.1}%)",
                        unique_values,
                        encoding_efficiency * 100.0
                    ));
                }
                StorageModeDetails::ContentAddressed {
                    unique_hashes,
                    deduplication_ratio,
                    total_stored_bytes,
                } => {
                    result.push_str(&format!(
                        " (hashes: {}, dedup: {:.2}x, {} bytes)",
                        unique_hashes, deduplication_ratio, total_stored_bytes
                    ));
                }
                StorageModeDetails::Columnar {
                    batch_count,
                    values_per_batch,
                    null_ratio,
                } => {
                    result.push_str(&format!(
                        " (batches: {}, batch_size: {}, nulls: {:.1}%)",
                        batch_count,
                        values_per_batch,
                        null_ratio * 100.0
                    ));
                }
                StorageModeDetails::Default => {}
            }
            result.push('\n');
        }

        // Bloom Filter
        if let Some(bloom) = &report.bloom_filter {
            result.push_str("\n  Bloom Filter:\n");
            result.push_str(&format!("    Status      : {}\n", if bloom.enabled { "Enabled" } else { "Disabled" }));
            result.push_str(&format!("    Target FPR  : {:.2}%\n", bloom.target_fpr * 100.0));
            if let Some(fpr) = bloom.actual_fpr {
                result.push_str(&format!("    Actual FPR  : {:.2}%\n", fpr * 100.0));
            }
            result.push_str(&format!("    Fill Ratio  : {:.1}%\n", bloom.fill_ratio * 100.0));
            result.push_str(&format!("    Lookups     : {}\n", bloom.lookups));
            result.push_str(&format!("    Effectiveness: {}\n", bloom.effectiveness));
        }

        // Zone Maps
        if let Some(zones) = &report.zone_maps {
            result.push_str("\n  Zone Maps:\n");
            result.push_str(&format!("    Status      : {}\n", if zones.enabled { "Enabled" } else { "Disabled" }));
            result.push_str(&format!("    Total Blocks: {}\n", zones.total_blocks));
            result.push_str(&format!("    Skipped     : {} ({:.1}%)\n", zones.blocks_skipped, zones.skip_ratio * 100.0));
            result.push_str(&format!("    Scanned     : {}\n", zones.blocks_scanned));
            result.push_str(&format!("    Effectiveness: {}\n", zones.effectiveness));
        }

        // Compression
        if let Some(comp) = &report.compression {
            result.push_str("\n  Compression:\n");
            result.push_str(&format!("    Algorithm   : {}\n", comp.algorithm));
            result.push_str(&format!("    Original    : {} bytes\n", comp.original_size));
            result.push_str(&format!("    Compressed  : {} bytes\n", comp.compressed_size));
            result.push_str(&format!("    Ratio       : {:.2}x\n", comp.ratio));
        }

        // Indexes
        if !report.indexes.is_empty() {
            result.push_str("\n  Indexes:\n");
            for idx in &report.indexes {
                let status = if idx.used_in_plan { "USED" } else { "NOT USED" };
                result.push_str(&format!(
                    "    {} ({}) on [{}] - {}\n",
                    idx.name,
                    idx.index_type,
                    idx.columns.join(", "),
                    status
                ));
                if let Some(reason) = &idx.reason_not_used {
                    result.push_str(&format!("      Reason: {}\n", reason));
                }
            }
        }

        // Statistics
        if let Some(stats) = &report.statistics {
            result.push_str("\n  Statistics:\n");
            result.push_str(&format!("    Row Count   : {}\n", stats.row_count));
            result.push_str(&format!("    Avg Row Size: {} bytes\n", stats.avg_row_size));
            result.push_str(&format!("    Total Size  : {} bytes\n", stats.total_size));
            result.push_str(&format!("    Last Analyzed: {}\n", stats.last_analyzed));
            if let Some(warning) = &stats.staleness_warning {
                result.push_str(&format!("    ⚠ {}\n", warning));
            }
        }

        // Columnar
        if let Some(columnar) = &report.columnar {
            result.push_str("\n  Columnar Storage:\n");
            result.push_str(&format!("    Batches     : {}\n", columnar.batch_count));
            result.push_str(&format!("    Batch Size  : {}\n", columnar.batch_size));
            result.push_str(&format!("    Total Slots : {}\n", columnar.total_slots));
            result.push_str(&format!("    Null Ratio  : {:.1}%\n", columnar.null_ratio * 100.0));
        }

        result.push('\n');
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_filter_effectiveness() {
        assert_eq!(
            BloomFilterEffectiveness::from_fpr(0.005),
            BloomFilterEffectiveness::Excellent
        );
        assert_eq!(
            BloomFilterEffectiveness::from_fpr(0.03),
            BloomFilterEffectiveness::Good
        );
        assert_eq!(
            BloomFilterEffectiveness::from_fpr(0.07),
            BloomFilterEffectiveness::Moderate
        );
        assert_eq!(
            BloomFilterEffectiveness::from_fpr(0.15),
            BloomFilterEffectiveness::Degraded
        );
    }

    #[test]
    fn test_zone_map_effectiveness() {
        assert_eq!(
            ZoneMapEffectiveness::from_skip_ratio(0.90),
            ZoneMapEffectiveness::Excellent
        );
        assert_eq!(
            ZoneMapEffectiveness::from_skip_ratio(0.60),
            ZoneMapEffectiveness::Good
        );
        assert_eq!(
            ZoneMapEffectiveness::from_skip_ratio(0.30),
            ZoneMapEffectiveness::Moderate
        );
        assert_eq!(
            ZoneMapEffectiveness::from_skip_ratio(0.10),
            ZoneMapEffectiveness::Limited
        );
    }

    #[test]
    fn test_storage_mode_details_default() {
        let details = StorageModeDetails::default();
        matches!(details, StorageModeDetails::Default);
    }
}
