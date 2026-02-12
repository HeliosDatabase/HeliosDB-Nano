//! PostgreSQL-compatible System Views (pg_catalog)
//!
//! Comprehensive implementation of PostgreSQL system catalog views for HeliosDB-Lite.
//! Ensures all v2.0 and v2.1 features are introspectable via SQL queries.
//!
//! Implemented views:
//! - Core catalog: pg_tables, pg_views, pg_indexes, pg_columns, pg_attribute
//! - Database info: pg_database, pg_namespace, pg_class, pg_type
//! - Session/Activity: pg_stat_activity, pg_stat_database, pg_settings
//! - v2.0 Features: pg_branches, pg_materialized_views, pg_snapshots
//! - v2.1 Features: pg_stat_ssl, pg_authid, pg_stat_optimizer, pg_compression_stats

#![allow(unused_variables)]

use crate::{Result, Error, Schema, Column, DataType, Value, Tuple};
use crate::storage::StorageEngine;
use crate::storage::GlobalStatsCollector;
use crate::sql::SessionRegistry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};
use lru::LruCache;
use std::time::{Duration, Instant};
use std::num::NonZeroUsize;

/// System view definition
#[derive(Debug, Clone)]
pub struct SystemView {
    pub name: String,
    pub schema: Schema,
    pub description: String,
    pub category: ViewCategory,
}

/// System view category
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewCategory {
    Core,
    Session,
    Feature,
    Statistics,
}

/// Cached query result with TTL
#[derive(Debug, Clone)]
struct CachedResult {
    /// Cached tuples
    tuples: Vec<Tuple>,
    /// Timestamp when cached
    cached_at: Instant,
    /// Time-to-live duration
    ttl: Duration,
}

impl CachedResult {
    /// Create a new cached result
    fn new(tuples: Vec<Tuple>, ttl: Duration) -> Self {
        Self {
            tuples,
            cached_at: Instant::now(),
            ttl,
        }
    }

    /// Check if the cached result is still valid
    fn is_valid(&self) -> bool {
        self.cached_at.elapsed() < self.ttl
    }
}

/// Cache key for system view queries
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    /// View name
    view_name: String,
    /// Optional query hash for parameterized queries (future use)
    query_hash: u64,
}

impl CacheKey {
    /// Create a cache key for a simple view query
    fn new(view_name: &str) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        view_name.hash(&mut hasher);

        Self {
            view_name: view_name.to_string(),
            query_hash: hasher.finish(),
        }
    }
}

/// Central registry for all PostgreSQL-compatible system views
pub struct SystemViewRegistry {
    views: HashMap<String, SystemView>,
    session_registry: Option<Arc<SessionRegistry>>,
    /// Global statistics collector for query history and transaction stats
    stats_collector: Option<Arc<GlobalStatsCollector>>,
    /// LRU cache for system view results (view_name -> cached result)
    /// Key: (view_name, query_hash), Value: cached tuples with TTL
    cache: Arc<Mutex<LruCache<CacheKey, CachedResult>>>,
    /// Default TTL for system view cache (5 seconds as per spec)
    default_ttl: Duration,
}

impl SystemViewRegistry {
    /// Create a new system view registry
    pub fn new() -> Self {
        // SAFETY: 500 is a compile-time constant and is guaranteed to be non-zero
        // Performance optimization: Increased from 100 to 500 entries for better cache hit rates
        // in larger deployments with many concurrent queries to system views
        #[allow(clippy::expect_used)]
        let cache_size = NonZeroUsize::new(500).expect("500 is non-zero");
        let mut registry = Self {
            views: HashMap::new(),
            session_registry: None,
            stats_collector: None,
            cache: Arc::new(Mutex::new(LruCache::new(cache_size))),
            default_ttl: Duration::from_secs(5),
        };

        registry.register_all_views();
        registry
    }

    /// Create with session registry for activity tracking
    pub fn with_session_registry(session_registry: Arc<SessionRegistry>) -> Self {
        let mut registry = Self::new();
        registry.session_registry = Some(session_registry);
        registry
    }

    /// Create with statistics collector for query history and transaction stats
    pub fn with_stats_collector(stats_collector: Arc<GlobalStatsCollector>) -> Self {
        let mut registry = Self::new();
        registry.stats_collector = Some(stats_collector);
        registry
    }

    /// Set the statistics collector
    pub fn set_stats_collector(&mut self, stats_collector: Arc<GlobalStatsCollector>) {
        self.stats_collector = Some(stats_collector);
    }

    /// Create with custom cache configuration
    ///
    /// # Errors
    ///
    /// Returns an error if cache_size is zero.
    pub fn with_cache_config(cache_size: usize, ttl_seconds: u64) -> Result<Self> {
        let cache_size_nz = NonZeroUsize::new(cache_size)
            .ok_or_else(|| Error::config("Cache size must be non-zero"))?;
        let mut registry = Self {
            views: HashMap::new(),
            session_registry: None,
            stats_collector: None,
            cache: Arc::new(Mutex::new(LruCache::new(cache_size_nz))),
            default_ttl: Duration::from_secs(ttl_seconds),
        };

        registry.register_all_views();
        Ok(registry)
    }

    /// Register all system views
    fn register_all_views(&mut self) {
        self.register_core_catalog_views();
        self.register_session_activity_views();
        self.register_v2_feature_views();
        self.register_v2_1_feature_views();
        self.register_v2_3_monitoring_views();
        self.register_ha_views();
    }

    /// Register core PostgreSQL catalog views
    fn register_core_catalog_views(&mut self) {
        // pg_tables - All user tables
        self.register(SystemView {
            name: "pg_tables".to_string(),
            category: ViewCategory::Core,
            description: "Lists all user tables in the database".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("schemaname", DataType::Text),
                    Column::new("tablename", DataType::Text),
                    Column::new("tableowner", DataType::Text),
                    Column::new("tablespace", DataType::Text),
                    Column::new("hasindexes", DataType::Boolean),
                    Column::new("hasrules", DataType::Boolean),
                    Column::new("hastriggers", DataType::Boolean),
                    Column::new("rowsecurity", DataType::Boolean),
                ],
            },
        });

        // pg_views - All views
        self.register(SystemView {
            name: "pg_views".to_string(),
            category: ViewCategory::Core,
            description: "Lists all views in the database".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("schemaname", DataType::Text),
                    Column::new("viewname", DataType::Text),
                    Column::new("viewowner", DataType::Text),
                    Column::new("definition", DataType::Text),
                ],
            },
        });

        // pg_indexes - All indexes
        self.register(SystemView {
            name: "pg_indexes".to_string(),
            category: ViewCategory::Core,
            description: "Lists all indexes in the database".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("schemaname", DataType::Text),
                    Column::new("tablename", DataType::Text),
                    Column::new("indexname", DataType::Text),
                    Column::new("tablespace", DataType::Text),
                    Column::new("indexdef", DataType::Text),
                ],
            },
        });

        // pg_attribute / pg_columns - All columns
        self.register(SystemView {
            name: "pg_attribute".to_string(),
            category: ViewCategory::Core,
            description: "Lists all table columns with detailed attributes".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("attrelid", DataType::Int4),
                    Column::new("attname", DataType::Text),
                    Column::new("atttypid", DataType::Int4),
                    Column::new("attnum", DataType::Int2),
                    Column::new("attlen", DataType::Int2),
                    Column::new("attnotnull", DataType::Boolean),
                    Column::new("atthasdef", DataType::Boolean),
                ],
            },
        });

        // pg_database - Database information
        self.register(SystemView {
            name: "pg_database".to_string(),
            category: ViewCategory::Core,
            description: "Lists database information".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("datname", DataType::Text),
                    Column::new("datdba", DataType::Int4),
                    Column::new("encoding", DataType::Int4),
                    Column::new("datcollate", DataType::Text),
                    Column::new("datctype", DataType::Text),
                    Column::new("datistemplate", DataType::Boolean),
                    Column::new("datallowconn", DataType::Boolean),
                ],
            },
        });

        // pg_namespace - Schema information
        self.register(SystemView {
            name: "pg_namespace".to_string(),
            category: ViewCategory::Core,
            description: "Lists database schemas/namespaces".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("nspname", DataType::Text),
                    Column::new("nspowner", DataType::Int4),
                ],
            },
        });

        // pg_class - Tables, indexes, views, etc.
        self.register(SystemView {
            name: "pg_class".to_string(),
            category: ViewCategory::Core,
            description: "Lists all relations (tables, indexes, views)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("relname", DataType::Text),
                    Column::new("relnamespace", DataType::Int4),
                    Column::new("relkind", DataType::Char(1)),
                    Column::new("relowner", DataType::Int4),
                    Column::new("relam", DataType::Int4),
                    Column::new("relpages", DataType::Int4),
                    Column::new("reltuples", DataType::Float4),
                ],
            },
        });

        // pg_type - Data types
        self.register(SystemView {
            name: "pg_type".to_string(),
            category: ViewCategory::Core,
            description: "Lists all data types".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("typname", DataType::Text),
                    Column::new("typnamespace", DataType::Int4),
                    Column::new("typowner", DataType::Int4),
                    Column::new("typlen", DataType::Int2),
                    Column::new("typtype", DataType::Char(1)),
                    Column::new("typcategory", DataType::Char(1)),
                ],
            },
        });
    }

    /// Register session and activity views
    fn register_session_activity_views(&mut self) {
        // pg_stat_activity - Active queries and sessions
        self.register(SystemView {
            name: "pg_stat_activity".to_string(),
            category: ViewCategory::Session,
            description: "Shows information about current database sessions".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("datid", DataType::Int4),
                    Column::new("datname", DataType::Text),
                    Column::new("pid", DataType::Int4),
                    Column::new("usesysid", DataType::Int4),
                    Column::new("usename", DataType::Text),
                    Column::new("application_name", DataType::Text),
                    Column::new("client_addr", DataType::Text),
                    Column::new("client_port", DataType::Int4),
                    Column::new("backend_start", DataType::Timestamptz),
                    Column::new("state_change", DataType::Timestamptz),
                    Column::new("state", DataType::Text),
                    Column::new("query", DataType::Text),
                ],
            },
        });

        // pg_stat_database - Database statistics
        self.register(SystemView {
            name: "pg_stat_database".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows database-wide statistics".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("datid", DataType::Int4),
                    Column::new("datname", DataType::Text),
                    Column::new("numbackends", DataType::Int4),
                    Column::new("xact_commit", DataType::Int8),
                    Column::new("xact_rollback", DataType::Int8),
                    Column::new("blks_read", DataType::Int8),
                    Column::new("blks_hit", DataType::Int8),
                    Column::new("tup_returned", DataType::Int8),
                    Column::new("tup_fetched", DataType::Int8),
                    Column::new("tup_inserted", DataType::Int8),
                    Column::new("tup_updated", DataType::Int8),
                    Column::new("tup_deleted", DataType::Int8),
                ],
            },
        });

        // pg_settings - Configuration settings
        self.register(SystemView {
            name: "pg_settings".to_string(),
            category: ViewCategory::Core,
            description: "Shows current database configuration settings".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("name", DataType::Text),
                    Column::new("setting", DataType::Text),
                    Column::new("unit", DataType::Text),
                    Column::new("category", DataType::Text),
                    Column::new("short_desc", DataType::Text),
                    Column::new("context", DataType::Text),
                    Column::new("vartype", DataType::Text),
                    Column::new("source", DataType::Text),
                    Column::new("min_val", DataType::Text),
                    Column::new("max_val", DataType::Text),
                ],
            },
        });
    }

    /// Register v2.0 feature views
    fn register_v2_feature_views(&mut self) {
        // pg_branches - Database branches (custom)
        self.register(SystemView {
            name: "pg_branches".to_string(),
            category: ViewCategory::Feature,
            description: "Lists all database branches (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("branch_id", DataType::Int8),
                    Column::new("branch_name", DataType::Text),
                    Column::new("parent_id", DataType::Int8),
                    Column::new("parent_name", DataType::Text),
                    Column::new("created_at", DataType::Timestamptz),
                    Column::new("fork_point_lsn", DataType::Int8),
                    Column::new("state", DataType::Text),
                    Column::new("size_bytes", DataType::Int8),
                    Column::new("num_commits", DataType::Int8),
                ],
            },
        });

        // pg_materialized_views - Materialized view status
        self.register(SystemView {
            name: "pg_matviews".to_string(),
            category: ViewCategory::Feature,
            description: "Lists all materialized views with status".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("schemaname", DataType::Text),
                    Column::new("matviewname", DataType::Text),
                    Column::new("matviewowner", DataType::Text),
                    Column::new("definition", DataType::Text),
                    Column::new("ispopulated", DataType::Boolean),
                    Column::new("created_at", DataType::Timestamptz),
                    Column::new("last_refresh", DataType::Timestamptz),
                    Column::new("row_count", DataType::Int8),
                    Column::new("refresh_strategy", DataType::Text),
                    Column::new("base_tables", DataType::Text),
                ],
            },
        });

        // pg_snapshots - Time-travel snapshots (custom)
        self.register(SystemView {
            name: "pg_snapshots".to_string(),
            category: ViewCategory::Feature,
            description: "Lists all time-travel snapshots (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("snapshot_id", DataType::Int8),
                    Column::new("created_at", DataType::Timestamptz),
                    Column::new("scn", DataType::Int8),
                    Column::new("transaction_id", DataType::Int8),
                    Column::new("description", DataType::Text),
                    Column::new("size_bytes", DataType::Int8),
                    Column::new("is_automatic", DataType::Boolean),
                ],
            },
        });

        // pg_transaction_map - Transaction ID to snapshot mapping
        self.register(SystemView {
            name: "pg_transaction_map".to_string(),
            category: ViewCategory::Feature,
            description: "Maps transaction IDs to snapshot timestamps (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("transaction_id", DataType::Int8),
                    Column::new("snapshot_timestamp", DataType::Int8),
                    Column::new("scn", DataType::Int8),
                    Column::new("created_at", DataType::Timestamptz),
                ],
            },
        });

        // pg_scn_map - System Change Number to snapshot mapping
        self.register(SystemView {
            name: "pg_scn_map".to_string(),
            category: ViewCategory::Feature,
            description: "Maps SCN values to snapshot timestamps (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("scn", DataType::Int8),
                    Column::new("snapshot_timestamp", DataType::Int8),
                    Column::new("transaction_id", DataType::Int8),
                    Column::new("created_at", DataType::Timestamptz),
                ],
            },
        });

        // heliosdb_art_indexes - Adaptive Radix Tree (ART) indexes
        self.register(SystemView {
            name: "heliosdb_art_indexes".to_string(),
            category: ViewCategory::Feature,
            description: "Lists all ART indexes for PK/FK/UNIQUE constraints (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("index_name", DataType::Text),
                    Column::new("table_name", DataType::Text),
                    Column::new("index_type", DataType::Text),
                    Column::new("columns", DataType::Text),
                    Column::new("key_count", DataType::Int8),
                    Column::new("memory_bytes", DataType::Int8),
                    Column::new("constraint_checks", DataType::Int8),
                    Column::new("insertions", DataType::Int8),
                    Column::new("deletions", DataType::Int8),
                ],
            },
        });

        // heliosdb_simd_capabilities - CPU SIMD feature detection
        self.register(SystemView {
            name: "heliosdb_simd_capabilities".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows detected CPU SIMD capabilities and filter statistics (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("feature", DataType::Text),
                    Column::new("available", DataType::Boolean),
                    Column::new("description", DataType::Text),
                ],
            },
        });
    }

    /// Register v2.1 feature views
    fn register_v2_1_feature_views(&mut self) {
        // pg_stat_ssl - SSL/TLS connection statistics
        self.register(SystemView {
            name: "pg_stat_ssl".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows SSL/TLS connection information".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("pid", DataType::Int4),
                    Column::new("ssl", DataType::Boolean),
                    Column::new("version", DataType::Text),
                    Column::new("cipher", DataType::Text),
                    Column::new("bits", DataType::Int4),
                    Column::new("client_dn", DataType::Text),
                    Column::new("client_serial", DataType::Text),
                    Column::new("issuer_dn", DataType::Text),
                ],
            },
        });

        // pg_authid - User authentication info
        self.register(SystemView {
            name: "pg_authid".to_string(),
            category: ViewCategory::Core,
            description: "Lists authentication identities (users and roles)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("rolname", DataType::Text),
                    Column::new("rolsuper", DataType::Boolean),
                    Column::new("rolinherit", DataType::Boolean),
                    Column::new("rolcreaterole", DataType::Boolean),
                    Column::new("rolcreatedb", DataType::Boolean),
                    Column::new("rolcanlogin", DataType::Boolean),
                    Column::new("rolconnlimit", DataType::Int4),
                    Column::new("rolvaliduntil", DataType::Timestamptz),
                ],
            },
        });

        // pg_stat_optimizer - Query optimizer statistics (custom)
        self.register(SystemView {
            name: "pg_stat_optimizer".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows query optimizer statistics (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("query_hash", DataType::Text),
                    Column::new("plan_type", DataType::Text),
                    Column::new("execution_count", DataType::Int8),
                    Column::new("total_time_ms", DataType::Float8),
                    Column::new("avg_time_ms", DataType::Float8),
                    Column::new("min_time_ms", DataType::Float8),
                    Column::new("max_time_ms", DataType::Float8),
                    Column::new("rows_estimate", DataType::Int8),
                    Column::new("rows_actual", DataType::Int8),
                    Column::new("last_execution", DataType::Timestamptz),
                ],
            },
        });

        // pg_compression_stats - Compression statistics (custom)
        self.register(SystemView {
            name: "pg_compression_stats".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows compression statistics per table (HeliosDB extension)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("schemaname", DataType::Text),
                    Column::new("tablename", DataType::Text),
                    Column::new("compression_type", DataType::Text),
                    Column::new("uncompressed_bytes", DataType::Int8),
                    Column::new("compressed_bytes", DataType::Int8),
                    Column::new("compression_ratio", DataType::Float8),
                    Column::new("num_chunks", DataType::Int8),
                    Column::new("avg_chunk_size", DataType::Int8),
                    Column::new("last_updated", DataType::Timestamptz),
                ],
            },
        });
    }

    /// Register v2.3.0 monitoring and sync views
    fn register_v2_3_monitoring_views(&mut self) {
        // pg_stat_replication - Replication status (if sync enabled)
        self.register(SystemView {
            name: "pg_stat_replication".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows replication status and synchronization state".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("client_id", DataType::Text),
                    Column::new("application_name", DataType::Text),
                    Column::new("state", DataType::Text), // 'streaming', 'catchup', 'offline'
                    Column::new("sent_lsn", DataType::Int8),
                    Column::new("write_lsn", DataType::Int8),
                    Column::new("flush_lsn", DataType::Int8),
                    Column::new("replay_lsn", DataType::Int8),
                    Column::new("sync_priority", DataType::Int4),
                    Column::new("sync_state", DataType::Text), // 'sync', 'async'
                    Column::new("connected_at", DataType::Timestamptz),
                    Column::new("last_sync_time", DataType::Timestamptz),
                ],
            },
        });

        // pg_stat_progress_vacuum - Maintenance progress tracking
        self.register(SystemView {
            name: "pg_stat_progress_vacuum".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows progress of ongoing maintenance operations".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("pid", DataType::Int4),
                    Column::new("datid", DataType::Int4),
                    Column::new("datname", DataType::Text),
                    Column::new("relid", DataType::Int4),
                    Column::new("relname", DataType::Text),
                    Column::new("phase", DataType::Text), // 'scanning', 'vacuuming', 'cleaning'
                    Column::new("heap_blks_total", DataType::Int8),
                    Column::new("heap_blks_scanned", DataType::Int8),
                    Column::new("heap_blks_vacuumed", DataType::Int8),
                    Column::new("index_vacuum_count", DataType::Int8),
                    Column::new("current_free_pages", DataType::Int8),
                ],
            },
        });

        // helios_sync_status - HeliosDB-specific sync metrics
        self.register(SystemView {
            name: "helios_sync_status".to_string(),
            category: ViewCategory::Feature,
            description: "Shows HeliosDB sync and replication metrics (v2.3.0)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("node_id", DataType::Text),
                    Column::new("node_name", DataType::Text),
                    Column::new("is_primary", DataType::Boolean),
                    Column::new("connected_replicas", DataType::Int4),
                    Column::new("total_changes_sent", DataType::Int8),
                    Column::new("total_changes_received", DataType::Int8),
                    Column::new("last_sync_time", DataType::Timestamptz),
                    Column::new("avg_sync_latency_ms", DataType::Float8),
                    Column::new("max_sync_latency_ms", DataType::Float8),
                    Column::new("sync_errors", DataType::Int8),
                    Column::new("replication_lag_bytes", DataType::Int8),
                ],
            },
        });

        // helios_query_history - Query execution history
        self.register(SystemView {
            name: "helios_query_history".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows recent query execution history and metrics (v2.3.0)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("query_id", DataType::Int8),
                    Column::new("query_hash", DataType::Text),
                    Column::new("query_text", DataType::Text),
                    Column::new("start_time", DataType::Timestamptz),
                    Column::new("end_time", DataType::Timestamptz),
                    Column::new("duration_ms", DataType::Float8),
                    Column::new("rows_returned", DataType::Int8),
                    Column::new("rows_examined", DataType::Int8),
                    Column::new("cpu_time_ms", DataType::Float8),
                    Column::new("io_time_ms", DataType::Float8),
                    Column::new("status", DataType::Text), // 'success', 'error', 'timeout'
                    Column::new("error_message", DataType::Text),
                    Column::new("user_name", DataType::Text),
                ],
            },
        });

        // helios_table_memory_stats - Per-table memory usage
        self.register(SystemView {
            name: "helios_table_memory_stats".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows per-table memory usage and cache statistics (v2.3.0)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("schemaname", DataType::Text),
                    Column::new("tablename", DataType::Text),
                    Column::new("heap_size_bytes", DataType::Int8),
                    Column::new("cache_size_bytes", DataType::Int8),
                    Column::new("index_size_bytes", DataType::Int8),
                    Column::new("total_size_bytes", DataType::Int8),
                    Column::new("cache_hit_ratio", DataType::Float8),
                    Column::new("cache_accesses", DataType::Int8),
                    Column::new("cache_hits", DataType::Int8),
                    Column::new("last_analyzed", DataType::Timestamptz),
                ],
            },
        });

        // helios_transaction_stats - Transaction statistics
        self.register(SystemView {
            name: "helios_transaction_stats".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows transaction statistics and ACID metrics (v2.3.0)".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("transaction_id", DataType::Int8),
                    Column::new("start_time", DataType::Timestamptz),
                    Column::new("end_time", DataType::Timestamptz),
                    Column::new("duration_ms", DataType::Float8),
                    Column::new("isolation_level", DataType::Text), // 'READ_UNCOMMITTED', 'READ_COMMITTED', etc.
                    Column::new("operations_count", DataType::Int8),
                    Column::new("rows_read", DataType::Int8),
                    Column::new("rows_written", DataType::Int8),
                    Column::new("status", DataType::Text), // 'committed', 'aborted', 'active'
                    Column::new("is_distributed", DataType::Boolean),
                ],
            },
        });

        // heliosdb_compression_stats - Compression statistics by algorithm
        self.register(SystemView {
            name: "heliosdb_compression_stats".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows compression statistics grouped by algorithm".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("algorithm", DataType::Text),
                    Column::new("uses", DataType::Int8),
                    Column::new("avg_ratio", DataType::Float8),
                    Column::new("avg_compress_us", DataType::Float8),
                    Column::new("avg_decompress_us", DataType::Float8),
                    Column::new("total_bytes_in", DataType::Int8),
                    Column::new("total_bytes_out", DataType::Int8),
                ],
            },
        });

        // heliosdb_pattern_stats - Pattern detection statistics
        self.register(SystemView {
            name: "heliosdb_pattern_stats".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows pattern detection statistics for compression".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("pattern", DataType::Text),
                    Column::new("detections", DataType::Int8),
                    Column::new("best_algorithm", DataType::Text),
                    Column::new("avg_ratio", DataType::Float8),
                ],
            },
        });

        // heliosdb_compression_events - Recent compression events
        self.register(SystemView {
            name: "heliosdb_compression_events".to_string(),
            category: ViewCategory::Statistics,
            description: "Shows recent compression events".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("timestamp", DataType::Timestamptz),
                    Column::new("table_name", DataType::Text),
                    Column::new("column_name", DataType::Text),
                    Column::new("algorithm", DataType::Text),
                    Column::new("ratio", DataType::Float8),
                    Column::new("input_bytes", DataType::Int8),
                    Column::new("output_bytes", DataType::Int8),
                    Column::new("duration_us", DataType::Int8),
                ],
            },
        });

        // heliosdb_config - Configuration settings
        self.register(SystemView {
            name: "heliosdb_config".to_string(),
            category: ViewCategory::Core,
            description: "Shows HeliosDB configuration settings".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("key", DataType::Text),
                    Column::new("value", DataType::Text),
                    Column::new("description", DataType::Text),
                ],
            },
        });
    }

    /// Register HA (High Availability) Tier 1 views
    fn register_ha_views(&mut self) {
        // helios_topology - Cluster topology with health information
        self.register(SystemView {
            name: "helios_topology".to_string(),
            category: ViewCategory::Feature,
            description: "Shows cluster topology with node status and health information".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("node_id", DataType::Text),
                    Column::new("alias", DataType::Text),
                    Column::new("role", DataType::Text),
                    Column::new("client_addr", DataType::Text),
                    Column::new("replication_addr", DataType::Text),
                    Column::new("healthy", DataType::Boolean),
                    Column::new("health_msg", DataType::Text),
                    Column::new("last_seen_secs", DataType::Int8),
                    Column::new("lsn", DataType::Int8),
                    Column::new("lag_ms", DataType::Int8),
                    Column::new("priority", DataType::Int4),
                    Column::new("weight", DataType::Int4),
                ],
            },
        });

        // helios_node_aliases - Node alias mappings
        self.register(SystemView {
            name: "helios_node_aliases".to_string(),
            category: ViewCategory::Feature,
            description: "Shows node alias to UUID mappings".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("alias", DataType::Text),
                    Column::new("node_id", DataType::Text),
                    Column::new("role", DataType::Text),
                    Column::new("healthy", DataType::Boolean),
                ],
            },
        });

        // helios_ha_status - HA cluster status summary
        self.register(SystemView {
            name: "helios_ha_status".to_string(),
            category: ViewCategory::Feature,
            description: "Shows HA cluster status summary".to_string(),
            schema: Schema {
                columns: vec![
                    Column::new("cluster_state", DataType::Text),
                    Column::new("primary_node", DataType::Text),
                    Column::new("primary_alias", DataType::Text),
                    Column::new("standby_count", DataType::Int4),
                    Column::new("healthy_standbys", DataType::Int4),
                    Column::new("current_lsn", DataType::Int8),
                    Column::new("oldest_standby_lsn", DataType::Int8),
                    Column::new("max_lag_ms", DataType::Int8),
                    Column::new("sync_mode", DataType::Text),
                ],
            },
        });
    }

    /// Register a system view
    fn register(&mut self, view: SystemView) {
        self.views.insert(view.name.clone(), view);
    }

    /// Check if a view name is a system view
    pub fn is_system_view(&self, name: &str) -> bool {
        self.views.contains_key(name)
    }

    /// Get system view schema
    pub fn get_schema(&self, name: &str) -> Option<&Schema> {
        self.views.get(name).map(|v| &v.schema)
    }

    /// Get system view definition
    pub fn get_view(&self, name: &str) -> Option<&SystemView> {
        self.views.get(name)
    }

    /// List all system views
    pub fn list_views(&self) -> Vec<&str> {
        self.views.keys().map(|s| s.as_str()).collect()
    }

    /// List views by category
    pub fn list_views_by_category(&self, category: ViewCategory) -> Vec<&str> {
        self.views
            .values()
            .filter(|v| v.category == category)
            .map(|v| v.name.as_str())
            .collect()
    }

    /// Invalidate cache for a specific view
    pub fn invalidate_view(&self, view_name: &str) -> Result<()> {
        let cache_key = CacheKey::new(view_name);
        let mut cache_guard = self.cache.lock().map_err(|e| {
            Error::query_execution(format!("Cache lock error: {}", e))
        })?;

        cache_guard.pop(&cache_key);
        tracing::debug!("Invalidated system view cache for '{}'", view_name);
        Ok(())
    }

    /// Invalidate entire cache (e.g., after DDL operations)
    pub fn invalidate_all(&self) -> Result<()> {
        let mut cache_guard = self.cache.lock().map_err(|e| {
            Error::query_execution(format!("Cache lock error: {}", e))
        })?;

        cache_guard.clear();
        tracing::info!("Invalidated entire system view cache");
        Ok(())
    }

    /// Invalidate cache for views in a specific category
    pub fn invalidate_category(&self, category: ViewCategory) -> Result<()> {
        let view_names: Vec<String> = self.views
            .values()
            .filter(|v| v.category == category)
            .map(|v| v.name.clone())
            .collect();

        for view_name in view_names {
            self.invalidate_view(&view_name)?;
        }

        tracing::debug!("Invalidated system view cache for category {:?}", category);
        Ok(())
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> Result<(usize, usize)> {
        let cache_guard = self.cache.lock().map_err(|e| {
            Error::query_execution(format!("Cache lock error: {}", e))
        })?;

        Ok((cache_guard.len(), cache_guard.cap().get()))
    }

    /// Execute a system view query with caching
    pub fn execute(&self, view_name: &str, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        if !self.is_system_view(view_name) {
            return Err(Error::query_execution(format!(
                "Unknown system view: {}",
                view_name
            )));
        }

        // Create cache key
        let cache_key = CacheKey::new(view_name);

        // Try to get from cache first
        {
            let cache_guard = self.cache.lock().map_err(|e| {
                Error::query_execution(format!("Cache lock error: {}", e))
            })?;

            if let Some(cached) = cache_guard.peek(&cache_key) {
                if cached.is_valid() {
                    tracing::debug!(
                        "System view cache HIT for '{}' (age: {:?}, ttl: {:?})",
                        view_name,
                        cached.cached_at.elapsed(),
                        cached.ttl
                    );
                    return Ok(cached.tuples.clone());
                } else {
                    tracing::debug!(
                        "System view cache EXPIRED for '{}' (age: {:?} > ttl: {:?})",
                        view_name,
                        cached.cached_at.elapsed(),
                        cached.ttl
                    );
                }
            } else {
                tracing::debug!("System view cache MISS for '{}'", view_name);
            }
        }

        // Cache miss or expired - execute the query
        let tuples = self.execute_uncached(view_name, storage)?;

        // Store in cache
        {
            let mut cache_guard = self.cache.lock().map_err(|e| {
                Error::query_execution(format!("Cache lock error: {}", e))
            })?;

            let cached_result = CachedResult::new(tuples.clone(), self.default_ttl);
            cache_guard.put(cache_key, cached_result);

            tracing::debug!(
                "System view cached '{}' ({} tuples, ttl: {:?})",
                view_name,
                tuples.len(),
                self.default_ttl
            );
        }

        Ok(tuples)
    }

    /// Execute a system view query without caching (internal use)
    fn execute_uncached(&self, view_name: &str, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        match view_name {
            // Core catalog
            "pg_tables" => self.execute_pg_tables(storage),
            "pg_views" => self.execute_pg_views(storage),
            "pg_indexes" => self.execute_pg_indexes(storage),
            "pg_attribute" => self.execute_pg_attribute(storage),
            "pg_database" => self.execute_pg_database(storage),
            "pg_namespace" => self.execute_pg_namespace(storage),
            "pg_class" => self.execute_pg_class(storage),
            "pg_type" => self.execute_pg_type(storage),

            // Session/Activity
            "pg_stat_activity" => self.execute_pg_stat_activity(storage),
            "pg_stat_database" => self.execute_pg_stat_database(storage),
            "pg_settings" => self.execute_pg_settings(storage),

            // v2.0 Features
            "pg_branches" => self.execute_pg_branches(storage),
            "pg_matviews" => self.execute_pg_matviews(storage),
            "pg_snapshots" => self.execute_pg_snapshots(storage),
            "pg_transaction_map" => self.execute_pg_transaction_map(storage),
            "pg_scn_map" => self.execute_pg_scn_map(storage),

            // v2.1 Features
            "pg_stat_ssl" => self.execute_pg_stat_ssl(storage),
            "pg_authid" => self.execute_pg_authid(storage),
            "pg_stat_optimizer" => self.execute_pg_stat_optimizer(storage),
            "pg_compression_stats" => self.execute_pg_compression_stats(storage),

            // v2.3.0 Monitoring and Sync Views
            "pg_stat_replication" => self.execute_pg_stat_replication(storage),
            "pg_stat_progress_vacuum" => self.execute_pg_stat_progress_vacuum(storage),
            "helios_sync_status" => self.execute_helios_sync_status(storage),
            "helios_query_history" => self.execute_helios_query_history(storage),
            "helios_table_memory_stats" => self.execute_helios_table_memory_stats(storage),
            "helios_transaction_stats" => self.execute_helios_transaction_stats(storage),

            // Compression Monitoring Views (from docs)
            "heliosdb_compression_stats" => self.execute_heliosdb_compression_stats(storage),
            "heliosdb_pattern_stats" => self.execute_heliosdb_pattern_stats(storage),
            "heliosdb_compression_events" => self.execute_heliosdb_compression_events(storage),
            "heliosdb_config" => self.execute_heliosdb_config(storage),

            // HA (High Availability) Tier 1 Views
            "helios_topology" => self.execute_helios_topology(storage),
            "helios_node_aliases" => self.execute_helios_node_aliases(storage),
            "helios_ha_status" => self.execute_helios_ha_status(storage),

            // ART Index View
            "heliosdb_art_indexes" => self.execute_heliosdb_art_indexes(storage),

            // SIMD Capabilities View
            "heliosdb_simd_capabilities" => self.execute_heliosdb_simd_capabilities(storage),

            _ => Ok(vec![]),
        }
    }

    // === Core Catalog View Executors ===

    fn execute_pg_tables(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for table_name in tables {
            // Skip system tables and materialized views
            if table_name.starts_with("helios_") || table_name.starts_with("mv_") {
                continue;
            }

            let tuple = Tuple::new(vec![
                Value::String("public".to_string()),
                Value::String(table_name.clone()),
                Value::String("heliosdb".to_string()),
                Value::Null, // tablespace
                Value::Boolean(false), // hasindexes (simplified)
                Value::Boolean(false), // hasrules
                Value::Boolean(false), // hastriggers
                Value::Boolean(false), // rowsecurity
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_views(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Currently no regular views, only materialized views
        Ok(vec![])
    }

    fn execute_pg_indexes(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let vector_indexes = storage.vector_indexes();
        let all_metadata = vector_indexes.list_all_metadata();
        let mut results = Vec::new();

        for metadata in all_metadata {
            let tuple = Tuple::new(vec![
                Value::String("public".to_string()),
                Value::String(metadata.table_name.clone()),
                Value::String(metadata.name.clone()),
                Value::Null, // tablespace
                Value::String(format!(
                    "CREATE INDEX {} ON {} USING hnsw ({})",
                    metadata.name, metadata.table_name, metadata.column_name
                )),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_attribute(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for (table_idx, table_name) in tables.iter().enumerate() {
            if table_name.starts_with("helios_") {
                continue;
            }

            let schema = catalog.get_table_schema(table_name)?;
            for (col_idx, column) in schema.columns.iter().enumerate() {
                let tuple = Tuple::new(vec![
                    Value::Int4(table_idx as i32), // attrelid
                    Value::String(column.name.clone()), // attname
                    Value::Int4(Self::data_type_to_oid(&column.data_type)), // atttypid
                    Value::Int2(col_idx as i16), // attnum
                    Value::Int2(-1), // attlen (variable)
                    Value::Boolean(!column.nullable), // attnotnull
                    Value::Boolean(false), // atthasdef
                ]);
                results.push(tuple);
            }
        }

        Ok(results)
    }

    fn execute_pg_database(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Return single database entry
        let tuple = Tuple::new(vec![
            Value::String("heliosdb".to_string()),
            Value::Int4(1), // datdba
            Value::Int4(6), // UTF8
            Value::String("en_US.UTF-8".to_string()),
            Value::String("en_US.UTF-8".to_string()),
            Value::Boolean(false), // datistemplate
            Value::Boolean(true), // datallowconn
        ]);

        Ok(vec![tuple])
    }

    fn execute_pg_namespace(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Return public schema
        let tuple = Tuple::new(vec![
            Value::String("public".to_string()),
            Value::Int4(1), // nspowner
        ]);

        Ok(vec![tuple])
    }

    fn execute_pg_class(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for (idx, table_name) in tables.iter().enumerate() {
            // Determine relation kind
            let relkind = if table_name.starts_with("mv_") {
                'm' // materialized view
            } else {
                'r' // regular table
            };

            let tuple = Tuple::new(vec![
                Value::String(table_name.clone()),
                Value::Int4(1), // relnamespace (public)
                Value::String(relkind.to_string()),
                Value::Int4(1), // relowner
                Value::Int4(0), // relam
                Value::Int4(0), // relpages
                Value::Float4(0.0), // reltuples
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_type(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Return built-in types
        let types = vec![
            ("bool", 16, 'b', 'B'),
            ("int2", 21, 'b', 'N'),
            ("int4", 23, 'b', 'N'),
            ("int8", 20, 'b', 'N'),
            ("float4", 700, 'b', 'N'),
            ("float8", 701, 'b', 'N'),
            ("text", 25, 'b', 'S'),
            ("varchar", 1043, 'b', 'S'),
            ("timestamp", 1114, 'b', 'D'),
            ("timestamptz", 1184, 'b', 'D'),
            ("bytea", 17, 'b', 'U'),
            ("json", 114, 'b', 'U'),
            ("jsonb", 3802, 'b', 'U'),
        ];

        let mut results = Vec::new();
        for (name, oid, typtype, typcategory) in types {
            let tuple = Tuple::new(vec![
                Value::String(name.to_string()),
                Value::Int4(1), // typnamespace
                Value::Int4(1), // typowner
                Value::Int2(-1), // typlen
                Value::String(typtype.to_string()),
                Value::String(typcategory.to_string()),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    // === Session/Activity View Executors ===

    fn execute_pg_stat_activity(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let sessions = match &self.session_registry {
            Some(registry) => registry.get_all_sessions()?,
            None => return Ok(vec![]),
        };

        let mut results = Vec::new();
        for session in sessions {
            let tuple = Tuple::new(vec![
                Value::Int4(1), // datid
                Value::String("heliosdb".to_string()),
                Value::Int4(session.session_id as i32), // pid
                Value::Int4(1), // usesysid
                Value::String(session.username.clone()),
                Value::String(session.protocol.as_str().to_string()),
                Value::String(session.client_address.clone()),
                Value::Int4(session.client_port),
                Value::Timestamp(DateTime::from_timestamp(session.connect_time, 0).unwrap_or_else(Utc::now)),
                Value::Timestamp(DateTime::from_timestamp(session.last_activity, 0).unwrap_or_else(Utc::now)),
                Value::String(session.state.as_str().to_string()),
                session.current_query.map(Value::String).unwrap_or(Value::Null),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_stat_database(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Get session count
        let num_backends = match &self.session_registry {
            Some(registry) => registry.get_all_sessions()?.len() as i32,
            None => 0,
        };

        // Get database statistics
        let stats = storage.stats();
        let snapshot = stats.snapshot();

        // Return single database stats row
        let tuple = Tuple::new(vec![
            Value::Int4(1), // datid
            Value::String("heliosdb".to_string()),
            Value::Int4(num_backends),
            Value::Int8(snapshot.xact_commit as i64),
            Value::Int8(snapshot.xact_rollback as i64),
            Value::Int8(snapshot.blks_read as i64),
            Value::Int8(snapshot.blks_hit as i64),
            Value::Int8(snapshot.tup_returned as i64),
            Value::Int8(snapshot.tup_fetched as i64),
            Value::Int8(snapshot.tup_inserted as i64),
            Value::Int8(snapshot.tup_updated as i64),
            Value::Int8(snapshot.tup_deleted as i64),
        ]);

        Ok(vec![tuple])
    }

    fn execute_pg_settings(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let config = storage.config();
        let mut results = Vec::new();

        // WAL enabled
        results.push(Self::setting_tuple(
            "wal_enabled",
            &config.storage.wal_enabled.to_string(),
            "",
            "Write-Ahead Logging",
            "Enables write-ahead logging for durability",
            "postmaster",
            "bool",
        ));

        // Time travel enabled
        results.push(Self::setting_tuple(
            "time_travel_enabled",
            &config.storage.time_travel_enabled.to_string(),
            "",
            "Time Travel",
            "Enables automatic time-travel versioning",
            "postmaster",
            "bool",
        ));

        // Query timeout
        results.push(Self::setting_tuple(
            "query_timeout_ms",
            &config.storage.query_timeout_ms.map_or("unlimited".to_string(), |v| v.to_string()),
            "ms",
            "Query Execution",
            "Maximum query execution time in milliseconds",
            "user",
            "integer",
        ));

        // Cache size
        results.push(Self::setting_tuple(
            "cache_size",
            &(config.storage.cache_size / (1024 * 1024)).to_string(),
            "MB",
            "Memory",
            "Memory cache size",
            "postmaster",
            "integer",
        ));

        Ok(results)
    }

    // === v2.0 Feature View Executors ===

    fn execute_pg_branches(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let branches = storage.list_branches()?;
        let mut results = Vec::new();

        for branch in branches {
            // Lookup parent branch name if parent_id exists
            let parent_name = if let Some(_parent_id) = branch.parent_id {
                // For now, we don't have a direct method to get parent branch name by ID
                // The parent information is already in branch.parent_id
                Value::Null
            } else {
                Value::Null
            };

            let tuple = Tuple::new(vec![
                Value::Int8(branch.branch_id as i64),
                Value::String(branch.name.clone()),
                branch.parent_id.map(|id| Value::Int8(id as i64)).unwrap_or(Value::Null),
                parent_name,
                Value::Timestamp(DateTime::from_timestamp(branch.created_at as i64, 0).unwrap_or_else(Utc::now)),
                Value::Int8(branch.created_from_snapshot as i64),
                Value::String(format!("{:?}", branch.state)),
                Value::Int8(branch.stats.storage_bytes as i64),
                Value::Int8(branch.stats.commit_count as i64),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_matviews(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let mv_catalog = storage.mv_catalog();
        let view_names = mv_catalog.list_views()?;
        let mut results = Vec::new();

        for view_name in view_names {
            let metadata = mv_catalog.get_view(&view_name)?;

            let tuple = Tuple::new(vec![
                Value::String("public".to_string()),
                Value::String(metadata.view_name.clone()),
                Value::String("heliosdb".to_string()),
                Value::String(metadata.query_text.clone()),
                Value::Boolean(metadata.last_refresh.is_some()),
                Value::Timestamp(metadata.created_at),
                metadata.last_refresh.map(Value::Timestamp).unwrap_or(Value::Null),
                metadata.row_count.map(|c| Value::Int8(c as i64)).unwrap_or(Value::Null),
                Value::String(metadata.refresh_strategy.clone()),
                Value::String(format!("{:?}", metadata.base_tables)),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_snapshots(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let snapshot_manager = storage.snapshot_manager();
        let snapshots = snapshot_manager.list_snapshots()?;
        let mut results = Vec::new();

        for snapshot in snapshots {
            // Calculate snapshot size (may be slow for large snapshots)
            let size_bytes = snapshot_manager
                .calculate_snapshot_size(snapshot.timestamp)
                .unwrap_or(0);

            let tuple = Tuple::new(vec![
                Value::Int8(snapshot.timestamp as i64),
                Value::Timestamp(DateTime::from_timestamp(snapshot.timestamp as i64, 0).unwrap_or_else(Utc::now)),
                Value::Int8(snapshot.scn as i64),
                Value::Int8(snapshot.transaction_id as i64),
                Value::String(snapshot.wall_clock_time.clone()),
                Value::Int8(size_bytes as i64),
                Value::Boolean(!snapshot.gc_eligible), // is_automatic if not gc_eligible
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_transaction_map(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let snapshot_manager = storage.snapshot_manager();
        let snapshots = snapshot_manager.list_snapshots()?;
        let mut results = Vec::new();

        for snapshot in snapshots {
            let created_at = DateTime::parse_from_rfc3339(&snapshot.wall_clock_time)
                .ok()
                .and_then(|dt| DateTime::from_timestamp(dt.timestamp(), 0))
                .unwrap_or_else(Utc::now);

            let tuple = Tuple::new(vec![
                Value::Int8(snapshot.transaction_id as i64),
                Value::Int8(snapshot.timestamp as i64),
                Value::Int8(snapshot.scn as i64),
                Value::Timestamp(created_at),
            ]);
            results.push(tuple);
        }

        // Sort by transaction_id for consistent ordering
        results.sort_by(|a, b| {
            let tx_a = match a.values.first() {
                Some(Value::Int8(v)) => *v,
                _ => 0,
            };
            let tx_b = match b.values.first() {
                Some(Value::Int8(v)) => *v,
                _ => 0,
            };
            tx_a.cmp(&tx_b)
        });

        Ok(results)
    }

    fn execute_pg_scn_map(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let snapshot_manager = storage.snapshot_manager();
        let snapshots = snapshot_manager.list_snapshots()?;
        let mut results = Vec::new();

        for snapshot in snapshots {
            let created_at = DateTime::parse_from_rfc3339(&snapshot.wall_clock_time)
                .ok()
                .and_then(|dt| DateTime::from_timestamp(dt.timestamp(), 0))
                .unwrap_or_else(Utc::now);

            let tuple = Tuple::new(vec![
                Value::Int8(snapshot.scn as i64),
                Value::Int8(snapshot.timestamp as i64),
                Value::Int8(snapshot.transaction_id as i64),
                Value::Timestamp(created_at),
            ]);
            results.push(tuple);
        }

        // Sort by SCN for consistent ordering
        results.sort_by(|a, b| {
            let scn_a = match a.values.first() {
                Some(Value::Int8(v)) => *v,
                _ => 0,
            };
            let scn_b = match b.values.first() {
                Some(Value::Int8(v)) => *v,
                _ => 0,
            };
            scn_a.cmp(&scn_b)
        });

        Ok(results)
    }

    // === v2.1 Feature View Executors ===

    fn execute_pg_stat_ssl(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Get SSL info from sessions
        let sessions = match &self.session_registry {
            Some(registry) => registry.get_all_sessions()?,
            None => return Ok(vec![]),
        };

        let mut results = Vec::new();
        for session in sessions {
            // Simplified: assume no SSL for now
            let tuple = Tuple::new(vec![
                Value::Int4(session.session_id as i32),
                Value::Boolean(false), // ssl
                Value::Null, // version
                Value::Null, // cipher
                Value::Null, // bits
                Value::Null, // client_dn
                Value::Null, // client_serial
                Value::Null, // issuer_dn
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_pg_authid(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Return default user
        let tuple = Tuple::new(vec![
            Value::String("heliosdb".to_string()),
            Value::Boolean(true), // rolsuper
            Value::Boolean(true), // rolinherit
            Value::Boolean(true), // rolcreaterole
            Value::Boolean(true), // rolcreatedb
            Value::Boolean(true), // rolcanlogin
            Value::Int4(-1), // rolconnlimit (unlimited)
            Value::Null, // rolvaliduntil
        ]);

        Ok(vec![tuple])
    }

    fn execute_pg_stat_optimizer(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Return table statistics for optimizer
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for table_name in tables {
            if let Some(stats) = catalog.get_table_statistics(&table_name)? {
                // Create a tuple for the table with aggregate statistics
                let tuple = Tuple::new(vec![
                    Value::String(format!("table:{}", table_name)), // query_hash
                    Value::String("SeqScan".to_string()), // plan_type
                    Value::Int8(stats.row_count as i64), // execution_count (using row_count as proxy)
                    Value::Float8(0.0), // total_time_ms (not tracked yet)
                    Value::Float8(0.0), // avg_time_ms
                    Value::Float8(0.0), // min_time_ms
                    Value::Float8(0.0), // max_time_ms
                    Value::Int8(stats.row_count as i64), // rows_estimate
                    Value::Int8(stats.row_count as i64), // rows_actual
                    Value::Timestamp(stats.last_analyzed), // last_execution
                ]);
                results.push(tuple);

                // Add per-column statistics entries
                for (col_name, col_stats) in &stats.columns {
                    let col_tuple = Tuple::new(vec![
                        Value::String(format!("{}:{}", table_name, col_name)), // query_hash
                        Value::String("ColumnScan".to_string()), // plan_type
                        Value::Int8(stats.row_count as i64), // execution_count
                        Value::Float8(0.0), // total_time_ms
                        Value::Float8(0.0), // avg_time_ms
                        Value::Float8(0.0), // min_time_ms
                        Value::Float8(0.0), // max_time_ms
                        Value::Int8(col_stats.n_distinct as i64), // rows_estimate (distinct values)
                        Value::Int8(col_stats.n_distinct as i64), // rows_actual
                        Value::Timestamp(stats.last_analyzed), // last_execution
                    ]);
                    results.push(col_tuple);
                }
            }
        }

        Ok(results)
    }

    fn execute_pg_compression_stats(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for table_name in tables {
            if let Some(stats) = catalog.get_compression_stats(&table_name)? {
                // Determine dominant compression type from column stats
                let compression_type = if !stats.column_stats.is_empty() {
                    "mixed".to_string() // Multiple columns may have different types
                } else {
                    "none".to_string()
                };

                let num_columns = stats.column_stats.len();
                let avg_chunk_size = if num_columns > 0 {
                    stats.total_compressed_size / num_columns
                } else {
                    0
                };

                let tuple = Tuple::new(vec![
                    Value::String("public".to_string()),
                    Value::String(table_name.clone()),
                    Value::String(compression_type),
                    Value::Int8(stats.total_original_size as i64),
                    Value::Int8(stats.total_compressed_size as i64),
                    Value::Float8(stats.overall_ratio),
                    Value::Int8(num_columns as i64),
                    Value::Int8(avg_chunk_size as i64),
                    Value::Timestamp(Utc::now()), // last_updated
                ]);
                results.push(tuple);
            }
        }

        Ok(results)
    }

    // === v2.3.0 Monitoring View Executors ===

    fn execute_pg_stat_replication(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns replication status information
        // If replication is not enabled, returns empty result set

        let mut results = Vec::new();

        if let Some(ref stats) = self.stats_collector {
            let replicas = stats.replication.get_replicas();

            for replica in replicas {
                let tuple = Tuple::new(vec![
                    // pid (backend process ID - simulated)
                    Value::Int4(std::process::id() as i32),
                    // usesysid (user OID)
                    Value::Int4(10), // postgres user
                    // usename
                    Value::String("replicator".to_string()),
                    // application_name
                    replica.application_name.clone().map(Value::String).unwrap_or(Value::Null),
                    // client_addr
                    Value::String(replica.host.clone()),
                    // client_hostname
                    Value::Null,
                    // client_port
                    Value::Int4(replica.port as i32),
                    // backend_start
                    Value::Timestamp(replica.last_msg_time),
                    // backend_xmin
                    Value::Null,
                    // state
                    Value::String(replica.state.to_string()),
                    // sent_lsn
                    Value::String(replica.current_lsn.clone()),
                    // write_lsn
                    Value::String(replica.replay_lsn.clone()),
                    // flush_lsn
                    Value::String(replica.replay_lsn.clone()),
                    // replay_lsn
                    Value::String(replica.replay_lsn.clone()),
                    // write_lag (interval)
                    Value::Null,
                    // flush_lag (interval)
                    Value::Null,
                    // replay_lag (interval)
                    Value::Null,
                    // sync_priority
                    Value::Int4(0),
                    // sync_state
                    Value::String("async".to_string()),
                ]);
                results.push(tuple);
            }
        }

        Ok(results)
    }

    fn execute_pg_stat_progress_vacuum(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns progress of ongoing vacuum/maintenance operations
        // Returns empty result if no vacuum is in progress

        // Placeholder: would connect to maintenance scheduler to track progress
        // In a full implementation, this would query the maintenance task scheduler
        // and return progress information for each active maintenance operation

        Ok(vec![])
    }

    fn execute_helios_sync_status(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns HeliosDB sync and replication status
        // Provides metrics about data synchronization between nodes
        let mut results = Vec::new();

        if let Some(ref stats) = self.stats_collector {
            let summary = stats.replication.get_summary();
            let replicas = stats.replication.get_replicas();

            // Node summary tuple
            let tuple = Tuple::new(vec![
                Value::String("local".to_string()), // node_id
                Value::String("default".to_string()), // node_name
                Value::Boolean(summary.role == crate::storage::ReplicationRole::Primary), // is_primary
                Value::Int4(summary.replica_count as i32), // connected_replicas
                Value::Int8(summary.bytes_sent as i64), // total_changes_sent
                Value::Int8(summary.bytes_received as i64), // total_changes_received
                Value::Timestamp(Utc::now()), // last_sync_time
                Value::Float8(0.0), // avg_sync_latency_ms
                Value::Float8(summary.max_replica_lag_ms as f64), // max_sync_latency_ms
                Value::Int8(0), // sync_errors
                Value::Int8(replicas.iter().map(|r| r.bytes_lag).max().unwrap_or(0) as i64), // replication_lag_bytes
            ]);
            results.push(tuple);

            // Per-replica status
            for replica in replicas {
                let tuple = Tuple::new(vec![
                    Value::String(replica.replica_id.clone()), // node_id
                    Value::String(format!("replica-{}", replica.replica_id)), // node_name
                    Value::Boolean(false), // is_primary
                    Value::Int4(0), // connected_replicas
                    Value::Int8(0), // total_changes_sent
                    Value::Int8(replica.bytes_lag as i64), // total_changes_received (bytes behind)
                    Value::Timestamp(replica.last_msg_time), // last_sync_time
                    Value::Float8(replica.time_lag_ms as f64), // avg_sync_latency_ms
                    Value::Float8(replica.time_lag_ms as f64), // max_sync_latency_ms
                    Value::Int8(0), // sync_errors
                    Value::Int8(replica.bytes_lag as i64), // replication_lag_bytes
                ]);
                results.push(tuple);
            }
        } else {
            // Default standalone node tuple
            let tuple = Tuple::new(vec![
                Value::String("local".to_string()), // node_id
                Value::String("default".to_string()), // node_name
                Value::Boolean(true), // is_primary
                Value::Int4(0), // connected_replicas
                Value::Int8(0), // total_changes_sent
                Value::Int8(0), // total_changes_received
                Value::Timestamp(Utc::now()),
                Value::Float8(0.0), // avg_sync_latency_ms
                Value::Float8(0.0), // max_sync_latency_ms
                Value::Int8(0), // sync_errors
                Value::Int8(0), // replication_lag_bytes
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_helios_query_history(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns recent query execution history
        // Provides performance metrics for queries

        let mut results = Vec::new();

        if let Some(ref stats) = self.stats_collector {
            // Get recent query history (last 1000 queries)
            let history = stats.query_history.get_recent(1000);

            for entry in history {
                let tuple = Tuple::new(vec![
                    // query_id
                    Value::Int8(entry.query_id as i64),
                    // query_hash
                    Value::Int8(entry.query_hash as i64),
                    // query_text
                    Value::String(entry.query_text.clone()),
                    // query_type
                    Value::String(entry.query_type.clone()),
                    // start_time
                    Value::Timestamp(entry.start_time),
                    // end_time
                    entry.end_time.map(Value::Timestamp).unwrap_or(Value::Null),
                    // duration_ms
                    entry.duration_ms.map(|d| Value::Int8(d as i64)).unwrap_or(Value::Null),
                    // rows_returned
                    Value::Int8(entry.rows_returned as i64),
                    // rows_examined
                    Value::Int8(entry.rows_examined as i64),
                    // status
                    Value::String(entry.status.to_string()),
                    // error_message
                    entry.error_message.clone().map(Value::String).unwrap_or(Value::Null),
                    // user_name
                    Value::String(entry.user_name.clone()),
                    // database_name
                    Value::String(entry.database_name.clone()),
                    // client_addr
                    entry.client_addr.clone().map(Value::String).unwrap_or(Value::Null),
                    // application_name
                    entry.application_name.clone().map(Value::String).unwrap_or(Value::Null),
                    // is_prepared
                    Value::Boolean(entry.is_prepared),
                    // plan_time_ms
                    entry.plan_time_ms.map(Value::Float8).unwrap_or(Value::Null),
                    // exec_time_ms
                    entry.exec_time_ms.map(Value::Float8).unwrap_or(Value::Null),
                    // shared_blks_hit
                    Value::Int8(entry.shared_blks_hit as i64),
                    // shared_blks_read
                    Value::Int8(entry.shared_blks_read as i64),
                    // shared_blks_written
                    Value::Int8(entry.shared_blks_written as i64),
                    // temp_blks_read
                    Value::Int8(entry.temp_blks_read as i64),
                    // temp_blks_written
                    Value::Int8(entry.temp_blks_written as i64),
                ]);
                results.push(tuple);
            }

            // Also include currently running queries
            let running = stats.query_history.get_running();
            for entry in running {
                let tuple = Tuple::new(vec![
                    Value::Int8(entry.query_id as i64),
                    Value::Int8(entry.query_hash as i64),
                    Value::String(entry.query_text.clone()),
                    Value::String(entry.query_type.clone()),
                    Value::Timestamp(entry.start_time),
                    Value::Null, // end_time not set for running queries
                    Value::Null, // duration_ms not set for running queries
                    Value::Int8(0), // rows_returned
                    Value::Int8(0), // rows_examined
                    Value::String("running".to_string()),
                    Value::Null, // error_message
                    Value::String(entry.user_name.clone()),
                    Value::String(entry.database_name.clone()),
                    entry.client_addr.clone().map(Value::String).unwrap_or(Value::Null),
                    entry.application_name.clone().map(Value::String).unwrap_or(Value::Null),
                    Value::Boolean(entry.is_prepared),
                    Value::Null, // plan_time_ms
                    Value::Null, // exec_time_ms
                    Value::Int8(0), // shared_blks_hit
                    Value::Int8(0), // shared_blks_read
                    Value::Int8(0), // shared_blks_written
                    Value::Int8(0), // temp_blks_read
                    Value::Int8(0), // temp_blks_written
                ]);
                results.push(tuple);
            }
        }

        Ok(results)
    }

    fn execute_helios_table_memory_stats(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns per-table memory usage and cache statistics
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for table_name in tables {
            // Skip internal tables
            if table_name.starts_with("helios_") || table_name.starts_with("mv_") {
                continue;
            }

            // Get table statistics if available
            let heap_size = if let Ok(Some(stats)) = catalog.get_table_statistics(&table_name) {
                (stats.row_count as i64) * 100 // Rough estimate: 100 bytes per row
            } else {
                0
            };

            let tuple = Tuple::new(vec![
                Value::String("public".to_string()), // schemaname
                Value::String(table_name.clone()), // tablename
                Value::Int8(heap_size), // heap_size_bytes
                Value::Int8(0), // cache_size_bytes (not tracked)
                Value::Int8(0), // index_size_bytes
                Value::Int8(heap_size), // total_size_bytes
                Value::Float8(0.0), // cache_hit_ratio
                Value::Int8(0), // cache_accesses
                Value::Int8(0), // cache_hits
                Value::Timestamp(Utc::now()),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    fn execute_helios_transaction_stats(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns transaction statistics
        // Provides metrics about ACID compliance and transaction processing

        let mut results = Vec::new();

        if let Some(ref stats) = self.stats_collector {
            // Get transaction tracker statistics
            let tx_stats = stats.transactions.get_stats();
            let db_stats = stats.database_stats.snapshot();
            let repl_summary = stats.replication.get_summary();

            // Summary row with overall transaction statistics
            let tuple = Tuple::new(vec![
                // database_name
                Value::String("heliosdb".to_string()),
                // xact_commit - total committed transactions
                Value::Int8(tx_stats.total_committed as i64),
                // xact_rollback - total rolled back transactions
                Value::Int8(tx_stats.total_rolled_back as i64),
                // xact_active - currently active transactions
                Value::Int8(tx_stats.active_transactions as i64),
                // xact_total - total transactions started
                Value::Int8(tx_stats.total_started as i64),
                // deadlocks - total deadlocks detected
                Value::Int8(tx_stats.total_deadlocks as i64),
                // blks_read - blocks read from disk
                Value::Int8(db_stats.blks_read as i64),
                // blks_hit - blocks found in cache
                Value::Int8(db_stats.blks_hit as i64),
                // tup_returned - tuples returned by queries
                Value::Int8(db_stats.tup_returned as i64),
                // tup_fetched - tuples fetched
                Value::Int8(db_stats.tup_fetched as i64),
                // tup_inserted - tuples inserted
                Value::Int8(db_stats.tup_inserted as i64),
                // tup_updated - tuples updated
                Value::Int8(db_stats.tup_updated as i64),
                // tup_deleted - tuples deleted
                Value::Int8(db_stats.tup_deleted as i64),
                // conflicts - replication conflicts (0 if not replicating)
                Value::Int8(0),
                // temp_files - temporary files created
                Value::Int8(0),
                // temp_bytes - temporary bytes written
                Value::Int8(0),
                // blk_read_time - time spent reading blocks (ms)
                Value::Float8(0.0),
                // blk_write_time - time spent writing blocks (ms)
                Value::Float8(0.0),
                // stats_reset - when stats were last reset
                Value::Timestamp(stats.stats_reset_time),
                // replication_role
                Value::String(repl_summary.role.to_string()),
                // replication_state
                repl_summary.state.map(|s| Value::String(s.to_string())).unwrap_or(Value::Null),
            ]);
            results.push(tuple);

            // Also include active transaction details
            let active_transactions = stats.transactions.get_active();
            for tx in active_transactions {
                let tuple = Tuple::new(vec![
                    // database_name
                    Value::String(tx.database_name.clone()),
                    // xact_id
                    Value::Int8(tx.xact_id as i64),
                    // user_name
                    Value::String(tx.user_name.clone()),
                    // state
                    Value::String(tx.state.to_string()),
                    // start_time
                    Value::Timestamp(tx.start_time),
                    // duration_ms
                    Value::Int8(tx.duration_ms()),
                    // statement_count
                    Value::Int8(tx.statement_count as i64),
                    // current_query
                    tx.current_query.clone().map(Value::String).unwrap_or(Value::Null),
                    // backend_pid
                    Value::Int4(tx.backend_pid as i32),
                    // wait_event_type
                    tx.wait_event_type.clone().map(Value::String).unwrap_or(Value::Null),
                    // wait_event
                    tx.wait_event.clone().map(Value::String).unwrap_or(Value::Null),
                    // client_addr
                    tx.client_addr.clone().map(Value::String).unwrap_or(Value::Null),
                    // application_name
                    tx.application_name.clone().map(Value::String).unwrap_or(Value::Null),
                    // is_prepared
                    Value::Boolean(tx.is_prepared),
                    // Additional fields to match schema - pad with nulls
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]);
                results.push(tuple);
            }
        }

        Ok(results)
    }

    // === Compression Monitoring View Executors ===

    fn execute_heliosdb_compression_stats(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Aggregate compression statistics by algorithm
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        // Aggregate stats by codec
        let mut alp_stats = (0i64, 0i64, 0i64, 0.0f64);  // (uses, bytes_in, bytes_out, total_ratio)
        let mut fsst_stats = (0i64, 0i64, 0i64, 0.0f64);
        let mut none_stats = (0i64, 0i64, 0i64, 0.0f64);

        for table_name in &tables {
            if let Some(stats) = catalog.get_compression_stats(table_name)? {
                for (col_name, col_stats) in &stats.column_stats {
                    let codec_name = format!("{:?}", col_stats.codec);
                    match codec_name.as_str() {
                        "ALP" => {
                            alp_stats.0 += col_stats.value_count as i64;
                            alp_stats.1 += col_stats.original_size as i64;
                            alp_stats.2 += col_stats.compressed_size as i64;
                            alp_stats.3 += col_stats.compression_ratio;
                        }
                        "FSST" => {
                            fsst_stats.0 += col_stats.value_count as i64;
                            fsst_stats.1 += col_stats.original_size as i64;
                            fsst_stats.2 += col_stats.compressed_size as i64;
                            fsst_stats.3 += col_stats.compression_ratio;
                        }
                        _ => {
                            none_stats.0 += col_stats.value_count as i64;
                            none_stats.1 += col_stats.original_size as i64;
                            none_stats.2 += col_stats.compressed_size as i64;
                        }
                    }
                }
            }
        }

        // Add ALP row if used
        if alp_stats.0 > 0 {
            results.push(Tuple::new(vec![
                Value::String("ALP".to_string()),
                Value::Int8(alp_stats.0),
                Value::Float8(if alp_stats.2 > 0 { alp_stats.1 as f64 / alp_stats.2 as f64 } else { 1.0 }),
                Value::Float8(0.0), // avg_compress_us (not tracked yet)
                Value::Float8(0.0), // avg_decompress_us
                Value::Int8(alp_stats.1),
                Value::Int8(alp_stats.2),
            ]));
        }

        // Add FSST row if used
        if fsst_stats.0 > 0 {
            results.push(Tuple::new(vec![
                Value::String("FSST".to_string()),
                Value::Int8(fsst_stats.0),
                Value::Float8(if fsst_stats.2 > 0 { fsst_stats.1 as f64 / fsst_stats.2 as f64 } else { 1.0 }),
                Value::Float8(0.0),
                Value::Float8(0.0),
                Value::Int8(fsst_stats.1),
                Value::Int8(fsst_stats.2),
            ]));
        }

        // Add None row if exists
        if none_stats.0 > 0 {
            results.push(Tuple::new(vec![
                Value::String("None".to_string()),
                Value::Int8(none_stats.0),
                Value::Float8(1.0),
                Value::Float8(0.0),
                Value::Float8(0.0),
                Value::Int8(none_stats.1),
                Value::Int8(none_stats.2),
            ]));
        }

        Ok(results)
    }

    fn execute_heliosdb_pattern_stats(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Pattern detection statistics
        // Currently returns predefined patterns based on data type affinity
        let results = vec![
            Tuple::new(vec![
                Value::String("FloatingPointData".to_string()),
                Value::Int8(0), // Will be populated when pattern detection is tracked
                Value::String("ALP".to_string()),
                Value::Float8(3.8),
            ]),
            Tuple::new(vec![
                Value::String("StringData".to_string()),
                Value::Int8(0),
                Value::String("FSST".to_string()),
                Value::Float8(6.2),
            ]),
        ];
        Ok(results)
    }

    fn execute_heliosdb_compression_events(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Recent compression events - shows per-table/column compression info
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for table_name in &tables {
            if table_name.starts_with("helios_") || table_name.starts_with("mv_") {
                continue;
            }

            if let Some(stats) = catalog.get_compression_stats(table_name)? {
                for (col_name, col_stats) in &stats.column_stats {
                    let tuple = Tuple::new(vec![
                        Value::Timestamp(Utc::now()), // timestamp
                        Value::String(table_name.clone()),
                        Value::String(col_name.clone()),
                        Value::String(format!("{:?}", col_stats.codec)),
                        Value::Float8(col_stats.compression_ratio),
                        Value::Int8(col_stats.original_size as i64),
                        Value::Int8(col_stats.compressed_size as i64),
                        Value::Int8(0), // duration_us (not tracked)
                    ]);
                    results.push(tuple);
                }
            }
        }

        Ok(results)
    }

    fn execute_heliosdb_config(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Returns compression-related configuration settings
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        // Global compression settings
        results.push(Tuple::new(vec![
            Value::String("compression.enabled".to_string()),
            Value::String("true".to_string()),
            Value::String("Enable automatic compression".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("compression.algorithm".to_string()),
            Value::String("auto".to_string()),
            Value::String("Default compression algorithm (auto selects best)".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("compression.auto.min_rows".to_string()),
            Value::String("1000".to_string()),
            Value::String("Minimum rows before compression is applied".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("compression.auto.min_data_size".to_string()),
            Value::String("1024".to_string()),
            Value::String("Minimum data size in bytes for compression".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("compression.level".to_string()),
            Value::String("6".to_string()),
            Value::String("Compression level (1-9, higher = better ratio)".to_string()),
        ]));

        // Add per-table compression configs
        for table_name in &tables {
            if table_name.starts_with("helios_") || table_name.starts_with("mv_") {
                continue;
            }

            if let Some(config) = catalog.get_compression_config(table_name)? {
                results.push(Tuple::new(vec![
                    Value::String(format!("compression.table.{}.enabled", table_name)),
                    Value::String(config.enabled.to_string()),
                    Value::String(format!("Compression enabled for table {}", table_name)),
                ]));

                results.push(Tuple::new(vec![
                    Value::String(format!("compression.table.{}.level", table_name)),
                    Value::String(config.compression_level.to_string()),
                    Value::String(format!("Compression level for table {}", table_name)),
                ]));
            }
        }

        Ok(results)
    }

    // === Helper Functions ===

    fn setting_tuple(name: &str, value: &str, unit: &str, category: &str, desc: &str, context: &str, vartype: &str) -> Tuple {
        Tuple::new(vec![
            Value::String(name.to_string()),
            Value::String(value.to_string()),
            Value::String(unit.to_string()),
            Value::String(category.to_string()),
            Value::String(desc.to_string()),
            Value::String(context.to_string()),
            Value::String(vartype.to_string()),
            Value::String("configuration file".to_string()),
            Value::Null, // min_val
            Value::Null, // max_val
        ])
    }

    // === HA (High Availability) View Executors ===

    /// Execute helios_topology - shows cluster topology with health info
    fn execute_helios_topology(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        #[cfg(feature = "ha-tier1")]
        {
            use crate::replication::ha_state::{ha_state, HARole};
            use crate::replication::topology_manager;

            let ha_registry = ha_state();
            let topology = topology_manager();
            let mut results = Vec::new();

            // Helper to get alias for a node
            let get_alias = |node_id: uuid::Uuid| -> Value {
                topology.get_node(node_id)
                    .and_then(|n| n.alias.clone())
                    .map(Value::String)
                    .unwrap_or(Value::Null)
            };

            // Helper to get node info from topology
            let get_topology_info = |node_id: uuid::Uuid| -> (u32, u32, Option<String>) {
                topology.get_node(node_id)
                    .map(|n| (n.priority, n.weight, n.health_message.clone()))
                    .unwrap_or((100, 100, None))
            };

            // Add local node info
            if let Some(config) = ha_registry.get_config() {
                let role_str = match ha_registry.get_role() {
                    HARole::Primary => "Primary",
                    HARole::Standby => "Standby",
                    HARole::Standalone => "Standalone",
                    HARole::Observer => "Observer",
                };

                let alias = get_alias(config.node_id);
                let (priority, weight, health_msg) = get_topology_info(config.node_id);

                results.push(Tuple::new(vec![
                    Value::String(config.node_id.to_string()),
                    alias,
                    Value::String(role_str.to_string()),
                    Value::String(config.listen_addr.clone()),
                    Value::String(format!("{}:{}", config.listen_addr, config.replication_port)),
                    Value::Boolean(true), // Local node is healthy from its perspective
                    Value::String(health_msg.unwrap_or_else(|| "OK".to_string())),
                    Value::Int8(0), // last seen
                    Value::Int8(ha_registry.get_lsn() as i64),
                    Value::Int8(0), // No lag for self
                    Value::Int4(priority as i32),
                    Value::Int4(weight as i32),
                ]));
            }

            // Add standby info
            for standby in ha_registry.get_standbys() {
                let alias = get_alias(standby.node_id);
                let (priority, weight, health_msg) = get_topology_info(standby.node_id);

                results.push(Tuple::new(vec![
                    Value::String(standby.node_id.to_string()),
                    alias,
                    Value::String("Standby".to_string()),
                    Value::String(standby.address.clone()),
                    Value::String(standby.address.clone()),
                    Value::Boolean(true), // Connected standbys are healthy
                    Value::String(health_msg.unwrap_or_else(|| "Connected".to_string())),
                    Value::Int8(0),
                    Value::Int8(standby.apply_lsn as i64),
                    Value::Int8(standby.lag_ms as i64),
                    Value::Int4(priority as i32),
                    Value::Int4(weight as i32),
                ]));
            }

            Ok(results)
        }

        #[cfg(not(feature = "ha-tier1"))]
        {
            // Return empty when HA is not enabled
            Ok(vec![])
        }
    }

    /// Execute helios_node_aliases - shows alias to UUID mappings
    fn execute_helios_node_aliases(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        #[cfg(feature = "ha-tier1")]
        {
            use crate::replication::ha_state::{ha_state, HARole};
            use crate::replication::topology_manager;

            let topology = topology_manager();
            let ha_registry = ha_state();
            let mut results = Vec::new();

            // Get all aliases
            let aliases = topology.get_all_aliases();

            for (alias, node_id) in aliases {
                // Determine role from HA registry
                let role = if let Some(config) = ha_registry.get_config() {
                    if config.node_id == node_id {
                        match ha_registry.get_role() {
                            HARole::Primary => "Primary",
                            HARole::Standby => "Standby",
                            HARole::Standalone => "Standalone",
                            HARole::Observer => "Observer",
                        }
                    } else if ha_registry.get_standbys().iter().any(|s| s.node_id == node_id) {
                        "Standby"
                    } else {
                        "Unknown"
                    }
                } else {
                    "Unknown"
                };

                let healthy = topology.get_node(node_id)
                    .map(|n| n.is_healthy)
                    .unwrap_or(false);

                results.push(Tuple::new(vec![
                    Value::String(alias),
                    Value::String(node_id.to_string()),
                    Value::String(role.to_string()),
                    Value::Boolean(healthy),
                ]));
            }

            Ok(results)
        }

        #[cfg(not(feature = "ha-tier1"))]
        {
            Ok(vec![])
        }
    }

    /// Execute helios_ha_status - shows HA cluster summary
    fn execute_helios_ha_status(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        #[cfg(feature = "ha-tier1")]
        {
            use crate::replication::ha_state::{ha_state, HARole, SyncMode};
            use crate::replication::topology_manager;

            let ha_registry = ha_state();
            let topology = topology_manager();

            let standbys = ha_registry.get_standbys();
            let standby_count = standbys.len() as i32;
            let healthy_standbys = standbys.iter().filter(|_| true).count() as i32; // All connected are healthy

            let current_lsn = ha_registry.get_lsn();
            let oldest_lsn = standbys.iter().map(|s| s.apply_lsn).min().unwrap_or(current_lsn);
            let max_lag = standbys.iter().map(|s| s.lag_ms).max().unwrap_or(0);

            let cluster_state = match ha_registry.get_role() {
                HARole::Primary => if standby_count > 0 { "healthy" } else { "standalone" },
                HARole::Standby => "standby",
                HARole::Standalone => "standalone",
                HARole::Observer => "observer",
            };

            let (primary_node, primary_alias) = if let Some(config) = ha_registry.get_config() {
                if ha_registry.get_role() == HARole::Primary {
                    let alias = topology.get_node(config.node_id)
                        .and_then(|n| n.alias.clone());
                    (Value::String(config.node_id.to_string()), alias.map(Value::String).unwrap_or(Value::Null))
                } else {
                    (Value::Null, Value::Null)
                }
            } else {
                (Value::Null, Value::Null)
            };

            let sync_mode = ha_registry.get_config()
                .map(|c| match c.sync_mode {
                    SyncMode::Async => "async",
                    SyncMode::Sync => "sync",
                    SyncMode::SemiSync { .. } => "semi-sync",
                })
                .unwrap_or("unknown");

            let tuple = Tuple::new(vec![
                Value::String(cluster_state.to_string()),
                primary_node,
                primary_alias,
                Value::Int4(standby_count),
                Value::Int4(healthy_standbys),
                Value::Int8(current_lsn as i64),
                Value::Int8(oldest_lsn as i64),
                Value::Int8(max_lag as i64),
                Value::String(sync_mode.to_string()),
            ]);

            Ok(vec![tuple])
        }

        #[cfg(not(feature = "ha-tier1"))]
        {
            // Return a single row indicating HA is not enabled
            let tuple = Tuple::new(vec![
                Value::String("disabled".to_string()),
                Value::Null,
                Value::Null,
                Value::Int4(0),
                Value::Int4(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::String("none".to_string()),
            ]);
            Ok(vec![tuple])
        }
    }

    /// Execute heliosdb_art_indexes - shows ART index information
    fn execute_heliosdb_art_indexes(&self, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        use crate::storage::ArtIndexType;

        let art_manager = storage.art_indexes();
        let indexes = art_manager.list_indexes();
        let mut results = Vec::new();

        for (index_name, table_name, index_type, columns) in indexes {
            let type_str = match index_type {
                ArtIndexType::PrimaryKey => "PRIMARY KEY",
                ArtIndexType::ForeignKey => "FOREIGN KEY",
                ArtIndexType::Unique => "UNIQUE",
                ArtIndexType::Manual => "MANUAL",
            };

            // Get individual index stats if available
            let (key_count, memory_bytes, constraint_checks, insertions, deletions) =
                if let Some(stats) = art_manager.index_stats(&index_name) {
                    (
                        stats.key_count as i64,
                        stats.memory_bytes as i64,
                        stats.lookup_count as i64,
                        stats.insert_count as i64,
                        stats.delete_count as i64,
                    )
                } else {
                    (0, 0, 0, 0, 0)
                };

            let columns_str = columns.join(", ");

            results.push(Tuple::new(vec![
                Value::String(index_name),
                Value::String(table_name),
                Value::String(type_str.to_string()),
                Value::String(columns_str),
                Value::Int8(key_count),
                Value::Int8(memory_bytes),
                Value::Int8(constraint_checks),
                Value::Int8(insertions),
                Value::Int8(deletions),
            ]));
        }

        Ok(results)
    }

    /// Execute heliosdb_simd_capabilities - shows CPU SIMD features
    fn execute_heliosdb_simd_capabilities(&self, _storage: &StorageEngine) -> Result<Vec<Tuple>> {
        use crate::storage::simd_capabilities;

        let caps = simd_capabilities();
        let mut results = Vec::new();

        // AVX-512
        results.push(Tuple::new(vec![
            Value::String("AVX-512".to_string()),
            Value::Boolean(caps.avx512f),
            Value::String("512-bit SIMD (16 floats/i32s per operation)".to_string()),
        ]));

        // AVX2
        results.push(Tuple::new(vec![
            Value::String("AVX2".to_string()),
            Value::Boolean(caps.avx2),
            Value::String("256-bit SIMD (8 floats/i32s per operation)".to_string()),
        ]));

        // SSE4.1
        results.push(Tuple::new(vec![
            Value::String("SSE4.1".to_string()),
            Value::Boolean(caps.sse41),
            Value::String("128-bit SIMD (4 floats/i32s per operation)".to_string()),
        ]));

        // Best available level
        let best = caps.best_level();
        let best_desc = match best {
            crate::storage::SimdLevel::Avx512 => "AVX-512 (fastest)",
            crate::storage::SimdLevel::Avx2 => "AVX2 (fast)",
            crate::storage::SimdLevel::Sse41 => "SSE4.1 (moderate)",
            crate::storage::SimdLevel::Scalar => "Scalar (no SIMD)",
        };
        results.push(Tuple::new(vec![
            Value::String("Best Level".to_string()),
            Value::Boolean(true),
            Value::String(best_desc.to_string()),
        ]));

        Ok(results)
    }

    fn data_type_to_oid(data_type: &DataType) -> i32 {
        match data_type {
            DataType::Boolean => 16,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Int8 => 20,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Text => 25,
            DataType::Varchar(_) => 1043,
            DataType::Timestamp => 1114,
            DataType::Timestamptz => 1184,
            DataType::Bytea => 17,
            DataType::Json => 114,
            DataType::Jsonb => 3802,
            _ => 0,
        }
    }
}

impl Default for SystemViewRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn test_registry_creation() {
        let registry = SystemViewRegistry::new();

        // Core views
        assert!(registry.is_system_view("pg_tables"));
        assert!(registry.is_system_view("pg_views"));
        assert!(registry.is_system_view("pg_indexes"));
        assert!(registry.is_system_view("pg_attribute"));
        assert!(registry.is_system_view("pg_database"));
        assert!(registry.is_system_view("pg_namespace"));
        assert!(registry.is_system_view("pg_class"));
        assert!(registry.is_system_view("pg_type"));

        // Session views
        assert!(registry.is_system_view("pg_stat_activity"));
        assert!(registry.is_system_view("pg_stat_database"));
        assert!(registry.is_system_view("pg_settings"));

        // Feature views
        assert!(registry.is_system_view("pg_branches"));
        assert!(registry.is_system_view("pg_matviews"));
        assert!(registry.is_system_view("pg_snapshots"));
        assert!(registry.is_system_view("pg_stat_ssl"));
        assert!(registry.is_system_view("pg_authid"));
        assert!(registry.is_system_view("pg_stat_optimizer"));
        assert!(registry.is_system_view("pg_compression_stats"));

        assert!(!registry.is_system_view("nonexistent"));
    }

    #[test]
    fn test_get_schema() {
        let registry = SystemViewRegistry::new();

        let schema = registry.get_schema("pg_tables").unwrap();
        assert_eq!(schema.columns.len(), 8);
        assert_eq!(schema.columns[0].name, "schemaname");
        assert_eq!(schema.columns[1].name, "tablename");
    }

    #[test]
    fn test_list_views_by_category() {
        let registry = SystemViewRegistry::new();

        let core_views = registry.list_views_by_category(ViewCategory::Core);
        assert!(core_views.len() >= 6);

        let session_views = registry.list_views_by_category(ViewCategory::Session);
        assert!(session_views.len() >= 1);

        let feature_views = registry.list_views_by_category(ViewCategory::Feature);
        assert!(feature_views.len() >= 3);
    }

    #[test]
    fn test_execute_pg_tables() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        // Create test tables
        use crate::Schema;
        let schema = Schema::new(vec![
            crate::Column::new("id", DataType::Int4),
        ]);
        storage.catalog().create_table("test_table", schema).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_tables", &storage).unwrap();

        assert!(results.len() >= 1);
        assert_eq!(results[0].values.len(), 8);
    }

    #[test]
    fn test_execute_pg_database() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_database", &storage).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].values.len(), 7);
    }

    #[test]
    fn test_execute_pg_settings() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_settings", &storage).unwrap();

        assert!(results.len() >= 4);
    }

    #[test]
    fn test_v2_3_views_registration() {
        let registry = SystemViewRegistry::new();

        // Check v2.3.0 views are registered
        assert!(registry.is_system_view("pg_stat_replication"));
        assert!(registry.is_system_view("pg_stat_progress_vacuum"));
        assert!(registry.is_system_view("helios_sync_status"));
        assert!(registry.is_system_view("helios_query_history"));
        assert!(registry.is_system_view("helios_table_memory_stats"));
        assert!(registry.is_system_view("helios_transaction_stats"));

        // HA Tier 1 views
        assert!(registry.is_system_view("helios_topology"));
        assert!(registry.is_system_view("helios_node_aliases"));
        assert!(registry.is_system_view("helios_ha_status"));
    }

    #[test]
    fn test_execute_pg_stat_replication() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_stat_replication", &storage).unwrap();

        // Should return empty when replication is disabled
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_execute_pg_stat_progress_vacuum() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_stat_progress_vacuum", &storage).unwrap();

        // Should return empty when no vacuum is in progress
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_execute_helios_sync_status() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("helios_sync_status", &storage).unwrap();

        // Should return exactly 1 row with local node information
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].values.len(), 11);
    }

    #[test]
    fn test_execute_helios_query_history() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("helios_query_history", &storage).unwrap();

        // Should return empty when no queries have been executed
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_execute_helios_table_memory_stats() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        // Create a test table
        let schema = crate::Schema::new(vec![
            crate::Column::new("id", DataType::Int4),
            crate::Column::new("name", DataType::Text),
        ]);
        storage.catalog().create_table("test_table", schema).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("helios_table_memory_stats", &storage).unwrap();

        // Should return stats for the created table
        assert!(results.len() >= 1);
        assert_eq!(results[0].values.len(), 10);
    }

    #[test]
    fn test_execute_helios_transaction_stats() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let registry = SystemViewRegistry::new();
        let results = registry.execute("helios_transaction_stats", &storage).unwrap();

        // Should return empty when no transactions have been tracked
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_v2_3_views_schemas() {
        let registry = SystemViewRegistry::new();

        // Check pg_stat_replication schema
        let schema = registry.get_schema("pg_stat_replication").unwrap();
        assert_eq!(schema.columns.len(), 11);

        // Check pg_stat_progress_vacuum schema
        let schema = registry.get_schema("pg_stat_progress_vacuum").unwrap();
        assert_eq!(schema.columns.len(), 11);

        // Check helios_sync_status schema
        let schema = registry.get_schema("helios_sync_status").unwrap();
        assert_eq!(schema.columns.len(), 11);

        // Check helios_query_history schema
        let schema = registry.get_schema("helios_query_history").unwrap();
        assert_eq!(schema.columns.len(), 13);

        // Check helios_table_memory_stats schema
        let schema = registry.get_schema("helios_table_memory_stats").unwrap();
        assert_eq!(schema.columns.len(), 10);

        // Check helios_transaction_stats schema
        let schema = registry.get_schema("helios_transaction_stats").unwrap();
        assert_eq!(schema.columns.len(), 10);
    }

    #[test]
    fn test_v2_3_views_list() {
        let registry = SystemViewRegistry::new();
        let all_views = registry.list_views();

        // Check that all v2.3.0 views are in the list
        assert!(all_views.contains(&"pg_stat_replication"));
        assert!(all_views.contains(&"pg_stat_progress_vacuum"));
        assert!(all_views.contains(&"helios_sync_status"));
        assert!(all_views.contains(&"helios_query_history"));
        assert!(all_views.contains(&"helios_table_memory_stats"));
        assert!(all_views.contains(&"helios_transaction_stats"));

        // Check that previous versions' views are still present
        assert!(all_views.contains(&"pg_tables"));
        assert!(all_views.contains(&"pg_stat_activity"));
        assert!(all_views.contains(&"pg_compression_stats"));
    }
}
