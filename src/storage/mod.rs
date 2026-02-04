//! Storage layer for HeliosDB Lite
//!
//! Basic RocksDB wrapper with standard MVCC (no proprietary optimizations).

#![allow(deprecated)]

mod engine;
mod mvcc;
mod transaction;
mod wal;
mod catalog;
mod vector_index;
mod gin_index;
mod time_travel;
mod branch;
mod materialized_view;
mod mv_auto_refresh;
mod mv_delta;
mod mv_scheduler;
mod mv_incremental;
mod mv_system_views;
mod stats;
mod dirty_tracker;
mod lock_manager;
pub mod statistics;
pub mod dump;

// Storage-level filtering modules
pub mod bloom_filter;
pub mod zone_map;
pub mod simd_filter;
pub mod predicate_pushdown;

// Lock-free high-performance ingestion subsystem
pub mod lockfree;

// Per-column storage optimization modules
pub mod dictionary;
pub mod content_addr;
pub mod columnar;
pub mod compression;

// Row-level caching
pub mod row_cache;

// ART (Adaptive Radix Tree) Index
pub mod art_node;
pub mod art_index;
pub mod art_manager;

// Self-Maintaining Filter Index (SMFI) modules - Phase 1-4
pub mod filter_index_delta;
pub mod filter_consolidation_worker;
pub mod columnar_zone_summary;
pub mod speculative_filter;
pub mod parallel_filter;

pub use engine::{StorageEngine, StorageStats, DirectBulkLoadResult};
pub use transaction::Transaction;
pub use mvcc::{Snapshot, SnapshotId};
pub use catalog::Catalog;
pub use vector_index::{VectorIndexManager, VectorIndexMetadata, VectorIndexStats, VectorIndexType};
pub use gin_index::{GinIndex, GinIndexStats};
pub use time_travel::{SnapshotManager, SnapshotMetadata, Scn, TransactionId, GcConfig};
pub use branch::{
    BranchId, BranchManager, BranchMetadata, BranchOptions, BranchRegistry,
    BranchState, BranchStats, BranchTransaction, MergeStrategy, MergeConflict, MergeResult,
    BranchGcConfig, BranchGcMode, GitLinkMetadata,
    GIT_CONFIG_KEY, GIT_LINK_PREFIX, GIT_COMMIT_PREFIX,
    GIT_DDL_HISTORY_PREFIX, GIT_SCHEMA_SNAPSHOT_PREFIX, GIT_PR_PREFIX,
};
pub use materialized_view::{MaterializedViewCatalog, MaterializedViewMetadata};
pub use mv_auto_refresh::{AutoRefreshWorker, AutoRefreshConfig};
pub use mv_delta::{
    DeltaTracker as MvDeltaTracker, Delta as MvDelta, DeltaSet as MvDeltaSet,
    DeltaType as MvDeltaType, DeltaOperation as MvDeltaOperation,
};
pub use mv_scheduler::{
    MVScheduler, SchedulerConfig, Priority, RefreshTask, CpuMonitor, SchedulerStats,
};
pub use mv_incremental::{
    IncrementalRefresher, RefreshStrategy, RefreshResult, RefreshCost,
    DeltaTracker as IncDeltaTracker, DeltaOperation, Delta as IncDelta, DeltaSet as IncDeltaSet,
};
pub use mv_system_views::{
    MvSystemViews, AutoRefreshStatus, CpuUsageInfo,
};
pub use wal::{
    WriteAheadLog, WalEntry, WalOperation, WalSyncMode,
    WalIntegrityReport, ReplayStats, CleanupStats, WalMetrics,
};
pub use stats::{DatabaseStats, StatsSnapshot, GlobalStatsCollector, ReplicationRole};
pub use statistics::{TableStatistics, ColumnStatistics, StatisticsAnalyzer, StatisticsCache};
pub use dirty_tracker::{DirtyTracker, Change, ChangeType, DirtyTrackerError};
pub use lock_manager::{LockManager, LockType, LockState, LockGuard};
pub use dump::{
    DumpManager, DumpOptions, DumpMode, DumpType, DumpOutputFormat, RestoreOptions, DumpReport, RestoreReport,
    DumpMetadata, CompressionType as DumpCompressionType,
};

// Storage-level filtering exports
pub use bloom_filter::{
    BloomFilter, BloomFilterConfig, BloomFilterStats,
    ColumnBloomFilter, BlockBloomFilter, TableBloomFilters,
};
pub use zone_map::{
    ValueRange, ColumnZoneMap, BlockZoneMap, TableZoneMap, RangeOp,
};
pub use simd_filter::{
    FilterPredicate, FilterOp, CombinedPredicate, SimdPredicateFilteringEngine,
    FilterResult, SimdFilterStats, SimdCapabilities, SimdLevel, simd_capabilities,
};
pub use predicate_pushdown::{
    PredicatePushdownManager, PushdownConfig, AnalyzedPredicate,
    PushdownStats, PredicateOp, PushdownAnalysis, analyze_for_pushdown,
};

// SMFI exports - Self-Maintaining Filter Index
pub use filter_index_delta::{
    FilterIndexDeltaTracker, FilterIndexConfig, FilterDelta, FilterDeltaType,
    BloomFilterDelta, ZoneMapDelta, TableFilterDeltas, FilterDeltaStats,
    // Bulk load suspension support
    BulkLoadGuard, BulkLoadReason, SuspendedTableInfo, BulkLoadResult,
    DEFAULT_BULK_LOAD_THRESHOLD,
};
pub use filter_consolidation_worker::{
    FilterConsolidationWorker, ConsolidationConfig, ConsolidationStats,
    ConsolidationHistoryEntry,
};
pub use columnar_zone_summary::{
    HyperLogLog, ColumnZoneSummary, BlockZoneSummary, TableZoneSummaries,
    BlockDecision, SummaryMatch, McvEntry, Histogram, HistogramBucket,
};
pub use speculative_filter::{
    SpeculativeFilterManager, SpeculativeConfig, QueryPattern, PatternType,
    PatternStats, SpeculativeFilterMeta, FilterStatus, QueryPatternTracker,
    SpeculativeFilterStats,
};
pub use parallel_filter::{
    ParallelFilterEngine, ParallelFilterConfig, ParallelFilterStats,
    ParallelBlockScanner, AdaptiveParallelFilter,
};

// Lock-free ingestion exports
pub use lockfree::{
    // Configuration
    IngestionSafetyLevel, LockFreeIngestionConfig,
    // Row ID generation
    BatchRowIdAllocator, HierarchicalRowIdGenerator, RowIdGenerator,
    // Write buffer
    TransactionBuffer, WriteOp,
    // WAL management
    PartitionedWalManager, WalOp, WalPartition, WalRecord, WalRecovery,
    // High-level API
    BulkInsertResult, IngestionError, IngestionResult, IngestionStats,
    LockFreeIngestionEngine, RecoveryResult, TransactionHandle,
};

// Per-column storage optimization exports
pub use dictionary::{DictionaryManager, ColumnDictionary, DictionaryStats};
pub use content_addr::{ContentAddressedStore, CAS_MIN_SIZE};
pub use columnar::{ColumnarStore, ColumnBatch, ColumnarStats, BATCH_SIZE};
pub use compression::{CompressionConfig, CompressionStats, CompressionManager, ColumnCompressionMetadata, CompressionCodec};
pub use row_cache::{RowCache, RowCacheConfig, RowCacheStats, RowCacheKey};

// ART Index exports
pub use art_node::{ArtNode, LeafNode, Node4, Node16, Node48, Node256, NodeHeader, RowId, MAX_PREFIX_LEN};
pub use art_index::{AdaptiveRadixTree, ArtIndexType, ArtIndexError, ArtIndexStats, ArtResult, ArtIterator};
pub use art_manager::{ArtIndexManager, ArtManagerStats, ForeignKeyInfo};

use crate::Value;

/// Key type
pub type Key = Vec<u8>;

/// Versioned value with timestamp
#[derive(Debug, Clone)]
pub struct VersionedValue {
    /// Value
    pub value: Option<Value>,
    /// Timestamp
    pub timestamp: u64,
    /// Deleted flag
    pub deleted: bool,
}

impl VersionedValue {
    /// Create a new versioned value
    pub fn new(value: Value, timestamp: u64) -> Self {
        Self {
            value: Some(value),
            timestamp,
            deleted: false,
        }
    }

    /// Create a tombstone (deleted)
    pub fn tombstone(timestamp: u64) -> Self {
        Self {
            value: None,
            timestamp,
            deleted: true,
        }
    }

    /// Check if deleted
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }
}
