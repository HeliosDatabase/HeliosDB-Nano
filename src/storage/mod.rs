//! Storage layer for HeliosDB Lite
//!
//! Basic RocksDB wrapper with standard MVCC (no proprietary optimizations).

#![allow(deprecated)]

mod branch;
mod catalog;
mod dirty_tracker;
pub mod dump;
mod engine;
mod gin_index;
mod lock_manager;
mod materialized_view;
mod mv_auto_refresh;
mod mv_delta;
mod mv_incremental;
mod mv_scheduler;
mod mv_system_views;
mod mvcc;
mod prefix_decode;
pub mod statistics;
mod stats;
pub mod time_travel;
mod transaction;
mod vector_index;
mod view_catalog;
mod wal;

// Storage-level filtering modules
pub mod bloom_filter;
pub mod predicate_pushdown;
pub mod simd_filter;
pub mod zone_map;

// Lock-free high-performance ingestion subsystem
pub mod lockfree;

// Per-column storage optimization modules
pub mod columnar;
pub mod compression;
pub mod content_addr;
pub mod dictionary;

// Row-level caching
pub mod row_cache;

// ART (Adaptive Radix Tree) Index
pub mod art_index;
pub mod art_manager;
pub mod art_node;

// Self-Maintaining Filter Index (SMFI) modules - Phase 1-4
pub mod columnar_zone_summary;
pub mod filter_consolidation_worker;
pub mod filter_index_delta;
pub mod parallel_filter;
pub mod speculative_filter;

pub use branch::{
    BranchGcConfig, BranchGcMode, BranchId, BranchManager, BranchMetadata, BranchOptions, BranchRegistry, BranchState,
    BranchStats, BranchTransaction, GitLinkMetadata, MergeConflict, MergeResult, MergeStrategy, GIT_COMMIT_PREFIX,
    GIT_CONFIG_KEY, GIT_DDL_HISTORY_PREFIX, GIT_LINK_PREFIX, GIT_PR_PREFIX, GIT_SCHEMA_SNAPSHOT_PREFIX,
};
pub use catalog::Catalog;
pub use dirty_tracker::{Change, ChangeType, DirtyTracker, DirtyTrackerError};
pub use dump::{
    CompressionType as DumpCompressionType, DumpManager, DumpMetadata, DumpMode, DumpOptions, DumpOutputFormat,
    DumpReport, DumpType, RestoreOptions, RestoreReport,
};
pub use engine::{DirectBulkLoadResult, StorageEngine, StorageStats};
pub use gin_index::{GinIndex, GinIndexStats};
pub use lock_manager::{LockGuard, LockManager, LockState, LockType};
pub use materialized_view::{MaterializedViewCatalog, MaterializedViewMetadata};
pub use mv_auto_refresh::{AutoRefreshConfig, AutoRefreshWorker};
pub use mv_delta::{
    Delta as MvDelta, DeltaOperation as MvDeltaOperation, DeltaSet as MvDeltaSet, DeltaTracker as MvDeltaTracker,
    DeltaType as MvDeltaType,
};
pub use mv_incremental::{
    Delta as IncDelta, DeltaOperation, DeltaSet as IncDeltaSet, DeltaTracker as IncDeltaTracker, IncrementalRefresher,
    RefreshCost, RefreshResult, RefreshStrategy,
};
pub use mv_scheduler::{CpuMonitor, MVScheduler, Priority, RefreshTask, SchedulerConfig, SchedulerStats};
pub use mv_system_views::{AutoRefreshStatus, CpuUsageInfo, MvSystemViews};
pub use mvcc::{Snapshot, SnapshotId};
pub use statistics::{ColumnStatistics, StatisticsAnalyzer, StatisticsCache, TableStatistics};
pub use stats::{DatabaseStats, GlobalStatsCollector, ReplicationRole, StatsSnapshot};
pub use time_travel::{GcConfig, Scn, SnapshotManager, SnapshotMetadata, TransactionId};
pub use transaction::Transaction;
pub use vector_index::{
    StoredVectorRecord, StoredVectorSearchResult, VectorIndexManager, VectorIndexMetadata, VectorIndexStats,
    VectorIndexType,
};
pub use view_catalog::{ViewCatalog, ViewMetadata};
pub use wal::{
    CleanupStats, ReplayStats, WalEntry, WalIntegrityReport, WalMetrics, WalOperation, WalSyncMode, WriteAheadLog,
};

// Storage-level filtering exports
pub use bloom_filter::{
    BlockBloomFilter, BloomFilter, BloomFilterConfig, BloomFilterStats, ColumnBloomFilter, TableBloomFilters,
};
pub use predicate_pushdown::{
    analyze_for_pushdown, AnalyzedPredicate, PredicateOp, PredicatePushdownManager, PushdownAnalysis, PushdownConfig,
    PushdownStats,
};
pub use simd_filter::{
    simd_capabilities, CombinedPredicate, FilterOp, FilterPredicate, FilterResult, SimdCapabilities, SimdFilterStats,
    SimdLevel, SimdPredicateFilteringEngine,
};
pub use zone_map::{BlockZoneMap, ColumnZoneMap, RangeOp, TableZoneMap, ValueRange};

// SMFI exports - Self-Maintaining Filter Index
pub use columnar_zone_summary::{
    BlockDecision, BlockZoneSummary, ColumnZoneSummary, Histogram, HistogramBucket, HyperLogLog, McvEntry,
    SummaryMatch, TableZoneSummaries,
};
pub use filter_consolidation_worker::{
    ConsolidationConfig, ConsolidationHistoryEntry, ConsolidationStats, FilterConsolidationWorker,
};
pub use filter_index_delta::{
    BloomFilterDelta,
    // Bulk load suspension support
    BulkLoadGuard,
    BulkLoadReason,
    BulkLoadResult,
    FilterDelta,
    FilterDeltaStats,
    FilterDeltaType,
    FilterIndexConfig,
    FilterIndexDeltaTracker,
    SuspendedTableInfo,
    TableFilterDeltas,
    ZoneMapDelta,
    DEFAULT_BULK_LOAD_THRESHOLD,
};
pub use parallel_filter::{
    AdaptiveParallelFilter, ParallelBlockScanner, ParallelFilterConfig, ParallelFilterEngine, ParallelFilterStats,
};
pub use speculative_filter::{
    FilterStatus, PatternStats, PatternType, QueryPattern, QueryPatternTracker, SpeculativeConfig,
    SpeculativeFilterManager, SpeculativeFilterMeta, SpeculativeFilterStats,
};

// Lock-free ingestion exports
pub use lockfree::{
    // Row ID generation
    BatchRowIdAllocator,
    // High-level API
    BulkInsertResult,
    HierarchicalRowIdGenerator,
    IngestionError,
    IngestionResult,
    // Configuration
    IngestionSafetyLevel,
    IngestionStats,
    LockFreeIngestionConfig,
    LockFreeIngestionEngine,
    // WAL management
    PartitionedWalManager,
    RecoveryResult,
    RowIdGenerator,
    // Write buffer
    TransactionBuffer,
    TransactionHandle,
    WalOp,
    WalPartition,
    WalRecord,
    WalRecovery,
    WriteOp,
};

// Per-column storage optimization exports
pub use columnar::{ColumnBatch, ColumnarStats, ColumnarStore, BATCH_SIZE};
pub use compression::{
    ColumnCompressionMetadata, CompressionCodec, CompressionConfig, CompressionManager, CompressionStats,
};
pub use content_addr::{ContentAddressedStore, CAS_MIN_SIZE};
pub use dictionary::{ColumnDictionary, DictionaryManager, DictionaryStats};
pub use row_cache::{RowCache, RowCacheConfig, RowCacheKey, RowCacheStats};

// ART Index exports
pub use art_index::{AdaptiveRadixTree, ArtIndexError, ArtIndexStats, ArtIndexType, ArtIterator, ArtResult};
pub use art_manager::{ArtIndexManager, ArtManagerStats, ForeignKeyInfo};
pub use art_node::{ArtNode, LeafNode, Node16, Node256, Node4, Node48, NodeHeader, RowId, MAX_PREFIX_LEN};

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
