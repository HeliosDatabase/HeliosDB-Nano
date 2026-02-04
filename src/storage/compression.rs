//! Compression types (deprecated, kept for backward compatibility)
//!
//! Note: Custom compression codecs (ALP, FSST, etc.) have been removed in favor
//! of RocksDB's built-in LZ4 block compression. These types are kept for backward
//! compatibility with existing catalog entries.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Compression configuration for a table (deprecated)
///
/// Note: Per-column storage modes (DICTIONARY, CONTENT_ADDRESSED, COLUMNAR)
/// have replaced this configuration. Use ALTER TABLE ALTER COLUMN SET STORAGE instead.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompressionConfig {
    /// Whether compression is enabled (always true with RocksDB LZ4)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum data size to compress (deprecated)
    #[serde(default = "default_min_data_size")]
    pub min_data_size: usize,
    /// Minimum compression ratio to keep (deprecated)
    #[serde(default = "default_min_compression_ratio")]
    pub min_compression_ratio: f64,
    /// Per-column overrides (deprecated)
    #[serde(default)]
    pub column_overrides: HashMap<String, bool>,
    /// Compression level (deprecated, RocksDB uses default LZ4)
    #[serde(default)]
    pub compression_level: u8,
}

fn default_true() -> bool { true }
fn default_min_data_size() -> usize { 1024 }
fn default_min_compression_ratio() -> f64 { 1.2 }

/// Compression statistics for a table (deprecated)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompressionStats {
    /// Total original size in bytes
    pub total_original_size: usize,
    /// Total compressed size in bytes
    pub total_compressed_size: usize,
    /// Overall compression ratio
    pub overall_ratio: f64,
    /// Per-column statistics
    #[serde(default)]
    pub column_stats: HashMap<String, ColumnCompressionMetadata>,
}

/// Compression codec (deprecated)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum CompressionCodec {
    #[default]
    None,
    ALP,
    FSST,
    Delta,
    Dictionary,
    RLE,
}

/// Per-column compression metadata (deprecated)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ColumnCompressionMetadata {
    /// Compression codec used (deprecated)
    #[serde(default)]
    pub codec: CompressionCodec,
    /// Number of values
    #[serde(default)]
    pub value_count: usize,
    /// Original size
    pub original_size: usize,
    /// Compressed size
    pub compressed_size: usize,
    /// Compression ratio
    pub compression_ratio: f64,
}

/// Compression manager (deprecated stub)
#[derive(Debug, Clone, Default)]
pub struct CompressionManager;

impl CompressionManager {
    /// Create a new compression manager
    pub fn new() -> Self {
        Self
    }

    /// Rename table resources (no-op since compression is handled by RocksDB)
    pub fn rename_table(&self, _old_name: &str, _new_name: &str) -> crate::Result<()> {
        Ok(())
    }
}
