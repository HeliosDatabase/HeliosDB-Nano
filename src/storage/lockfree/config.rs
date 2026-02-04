//! Lock-free ingestion configuration
//!
//! Provides configurable safety levels for bulk data ingestion,
//! allowing users to trade durability guarantees for performance.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Get the number of available CPU cores
fn get_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
}

/// Ingestion safety level controlling ACID guarantees vs performance
///
/// # Safety Levels
///
/// ```text
/// Level      | Atomicity | Consistency | Isolation | Durability | Performance
/// -----------|-----------|-------------|-----------|------------|------------
/// Full       | ✓         | ✓           | ✓         | ✓          | 1x (baseline)
/// Batched    | ✓         | ✓           | ✓         | ~✓         | 3-5x
/// Async      | ✓         | ✓           | ✓         | ~          | 5-10x
/// Unsafe     | ✓         | ✓           | ✓         | ✗          | 10-50x
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IngestionSafetyLevel {
    /// Full ACID compliance - maximum safety
    ///
    /// - WAL fsync on every transaction commit
    /// - Row IDs persisted and recovered from WAL
    /// - Full MVCC snapshot isolation
    /// - Zero data loss on crash
    ///
    /// Use for: Financial transactions, critical data
    Full,

    /// Batched durability - controlled data loss window
    ///
    /// - WAL fsync every N commits or T milliseconds (whichever first)
    /// - Row IDs recovered from WAL on crash
    /// - Full isolation maintained
    /// - Up to batch_size transactions may be lost on crash
    ///
    /// Use for: High-throughput with acceptable small loss window
    Batched {
        /// Maximum commits before forced fsync
        batch_size: usize,
        /// Maximum time before forced fsync
        batch_timeout_ms: u64,
    },

    /// Async durability - background persistence
    ///
    /// - WAL written but fsynced asynchronously
    /// - Background thread syncs at intervals
    /// - Full isolation maintained
    /// - Recent transactions may be lost on crash
    ///
    /// Use for: Analytics ingestion, log data
    Async {
        /// Interval between background fsyncs
        sync_interval_ms: u64,
    },

    /// No durability - maximum performance
    ///
    /// - WAL can be completely disabled
    /// - Hierarchical row IDs (no persistence needed)
    /// - Isolation still maintained via MVCC
    /// - ALL data since last checkpoint lost on crash
    ///
    /// Use for: Bulk loading with external recovery source,
    /// temporary/derived data, development/testing
    ///
    /// # Safety
    /// Data can be recovered by re-importing from source.
    /// Enable checkpointing for periodic snapshots.
    Unsafe {
        /// If true, completely disable WAL (fastest)
        /// If false, write WAL but never fsync
        disable_wal: bool,
        /// Checkpoint interval (0 = no checkpoints)
        checkpoint_interval_secs: u64,
    },
}

impl Default for IngestionSafetyLevel {
    fn default() -> Self {
        Self::Full
    }
}

impl IngestionSafetyLevel {
    /// Create a batched safety level with sensible defaults
    pub fn batched() -> Self {
        Self::Batched {
            batch_size: 1000,
            batch_timeout_ms: 100,
        }
    }

    /// Create an async safety level with sensible defaults
    pub fn async_default() -> Self {
        Self::Async {
            sync_interval_ms: 1000,
        }
    }

    /// Create unsafe mode for bulk loading
    pub fn bulk_load() -> Self {
        Self::Unsafe {
            disable_wal: false,
            checkpoint_interval_secs: 60,
        }
    }

    /// Create maximum performance mode (use with caution)
    pub fn maximum_performance() -> Self {
        Self::Unsafe {
            disable_wal: true,
            checkpoint_interval_secs: 300,
        }
    }

    /// Check if WAL should be used
    pub fn use_wal(&self) -> bool {
        match self {
            Self::Unsafe { disable_wal: true, .. } => false,
            _ => true,
        }
    }

    /// Check if fsync should happen on every commit
    pub fn sync_on_commit(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Get batch parameters if batched mode
    pub fn batch_params(&self) -> Option<(usize, Duration)> {
        match self {
            Self::Batched { batch_size, batch_timeout_ms } => {
                Some((*batch_size, Duration::from_millis(*batch_timeout_ms)))
            }
            _ => None,
        }
    }

    /// Get async sync interval if async mode
    pub fn async_sync_interval(&self) -> Option<Duration> {
        match self {
            Self::Async { sync_interval_ms } => {
                Some(Duration::from_millis(*sync_interval_ms))
            }
            _ => None,
        }
    }

    /// Get checkpoint interval for unsafe mode
    pub fn checkpoint_interval(&self) -> Option<Duration> {
        match self {
            Self::Unsafe { checkpoint_interval_secs, .. } if *checkpoint_interval_secs > 0 => {
                Some(Duration::from_secs(*checkpoint_interval_secs))
            }
            _ => None,
        }
    }

    /// Human-readable description of guarantees
    pub fn description(&self) -> &'static str {
        match self {
            Self::Full => "Full ACID - zero data loss, fsync every commit",
            Self::Batched { .. } => "Batched ACID - up to N transactions may be lost",
            Self::Async { .. } => "Async durability - recent transactions may be lost",
            Self::Unsafe { disable_wal: true, .. } => "UNSAFE - all data since checkpoint may be lost",
            Self::Unsafe { disable_wal: false, .. } => "UNSAFE - WAL enabled but no fsync",
        }
    }
}

/// Configuration for the lock-free ingestion engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFreeIngestionConfig {
    /// Safety level for durability guarantees
    pub safety_level: IngestionSafetyLevel,

    /// Number of write partitions (should match CPU cores)
    /// More partitions = less contention, but more memory
    pub partition_count: usize,

    /// Size of per-thread write buffer before flush
    pub write_buffer_size: usize,

    /// Enable hierarchical row IDs (no global counter)
    pub hierarchical_row_ids: bool,

    /// Maximum pending writes before backpressure
    pub max_pending_writes: usize,

    /// Enable parallel compression during ingestion
    pub parallel_compression: bool,

    /// Number of compression worker threads
    pub compression_workers: usize,

    /// Pre-allocate row ID batches of this size
    pub row_id_batch_size: u64,
}

impl Default for LockFreeIngestionConfig {
    fn default() -> Self {
        let cpu_count = get_cpu_count();
        Self {
            safety_level: IngestionSafetyLevel::Full,
            partition_count: cpu_count,
            write_buffer_size: 64 * 1024, // 64KB per buffer
            hierarchical_row_ids: false,
            max_pending_writes: 100_000,
            parallel_compression: true,
            compression_workers: cpu_count.min(4),
            row_id_batch_size: 10_000,
        }
    }
}

impl LockFreeIngestionConfig {
    /// Configuration optimized for bulk loading
    pub fn for_bulk_load() -> Self {
        let cpu_count = get_cpu_count();
        Self {
            safety_level: IngestionSafetyLevel::bulk_load(),
            partition_count: cpu_count,
            write_buffer_size: 1024 * 1024, // 1MB per buffer
            hierarchical_row_ids: true,
            max_pending_writes: 1_000_000,
            parallel_compression: true,
            compression_workers: cpu_count,
            row_id_batch_size: 100_000,
        }
    }

    /// Configuration for maximum performance (unsafe)
    pub fn for_maximum_performance() -> Self {
        let cpu_count = get_cpu_count();
        Self {
            safety_level: IngestionSafetyLevel::maximum_performance(),
            partition_count: cpu_count * 2,
            write_buffer_size: 4 * 1024 * 1024, // 4MB per buffer
            hierarchical_row_ids: true,
            max_pending_writes: 10_000_000,
            parallel_compression: true,
            compression_workers: cpu_count,
            row_id_batch_size: 1_000_000,
        }
    }

    /// Configuration for OLTP workloads (full ACID)
    pub fn for_oltp() -> Self {
        Self {
            safety_level: IngestionSafetyLevel::Full,
            partition_count: 4,
            write_buffer_size: 16 * 1024,
            hierarchical_row_ids: false,
            max_pending_writes: 10_000,
            parallel_compression: false,
            compression_workers: 1,
            row_id_batch_size: 1_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safety_level_defaults() {
        assert!(IngestionSafetyLevel::default().sync_on_commit());
        assert!(IngestionSafetyLevel::default().use_wal());
    }

    #[test]
    fn test_bulk_load_config() {
        let config = LockFreeIngestionConfig::for_bulk_load();
        assert!(config.hierarchical_row_ids);
        assert!(!config.safety_level.sync_on_commit());
    }

    #[test]
    fn test_unsafe_no_wal() {
        let level = IngestionSafetyLevel::maximum_performance();
        assert!(!level.use_wal());
    }
}
