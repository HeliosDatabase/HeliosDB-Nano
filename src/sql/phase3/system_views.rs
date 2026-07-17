//! System Views Registry
//!
//! Provides schemas and execution for Phase 3 system views:
//! - pg_database_branches() - List all database branches
//! - pg_mv_staleness() - Materialized view staleness info
//! - pg_mv_cpu_usage() - MV refresh CPU usage
//! - pg_vector_index_stats() - Vector index statistics
//! - pg_current_scn() - Current System Change Number
//! - pg_compare_branches() - Compare two branches

use crate::storage::{art_index::ArtIndexType, StorageEngine};
use crate::{Column, ColumnStorageMode, DataType, Error, Result, Schema, Tuple, Value};
use std::collections::{HashMap, HashSet};

/// System view registry
pub struct SystemViewRegistry {
    views: HashMap<String, SystemViewSchema>,
}

/// System view schema definition
pub struct SystemViewSchema {
    pub name: String,
    pub schema: Schema,
    pub description: String,
}

impl SystemViewRegistry {
    /// Create a new system view registry with Phase 3 views
    pub fn new() -> Self {
        let mut registry = Self { views: HashMap::new() };

        registry.register_phase3_views();
        registry
    }

    /// Borrow the process-global registry, built exactly once.
    ///
    /// The Phase-3 system-view set is static — `register_phase3_views` consumes
    /// no runtime/storage state, it only allocates string/enum literals — so a
    /// fresh `new()` produces a byte-identical 48-view, hundreds-of-`Column`
    /// HashMap every time. Table resolution (`Planner::table_factor_to_plan`
    /// and the executor scan twins) rebuilt it per query just to answer one
    /// `is_system_view()` membership test, which the profile showed hotter than
    /// the actual index lookup. Sharing one immutable `&'static` removes that
    /// per-query rebuild at no behavioral cost (only `&self` accessors are used
    /// after construction).
    pub fn shared() -> &'static Self {
        static SHARED: std::sync::LazyLock<SystemViewRegistry> =
            std::sync::LazyLock::new(SystemViewRegistry::new);
        &SHARED
    }

    /// Register all Phase 3 system views
    fn register_phase3_views(&mut self) {
        // pg_database_branches()
        self.register_view(SystemViewSchema {
            name: "pg_database_branches".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "branch_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "branch_id".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "parent_id".to_string(),
                        data_type: DataType::Int8,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "created_at".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "fork_point_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "size_mb".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "status".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Lists all database branches with metadata".to_string(),
        });

        // pg_mv_staleness()
        self.register_view(SystemViewSchema {
            name: "pg_mv_staleness".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "view_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "base_tables".to_string(),
                        data_type: DataType::Text, // JSON array
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "last_update".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "pending_changes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "staleness_sec".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "status".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows staleness info for all materialized views".to_string(),
        });

        // pg_vector_index_stats()
        self.register_view(SystemViewSchema {
            name: "pg_vector_index_stats".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "index_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "num_vectors".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "dimensions".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "quantization".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "memory_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "recall_at_10".to_string(),
                        data_type: DataType::Float8,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Vector index statistics including PQ compression ratios".to_string(),
        });

        // pg_compare_branches()
        self.register_view(SystemViewSchema {
            name: "pg_compare_branches".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "key".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "source_value".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "target_value".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "difference_type".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "source_timestamp".to_string(),
                        data_type: DataType::Int8,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "target_timestamp".to_string(),
                        data_type: DataType::Int8,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Compare differences between two branches".to_string(),
        });

        // pg_branch_stats()
        self.register_view(SystemViewSchema {
            name: "pg_branch_stats".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "branch_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "modified_keys".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "storage_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "commit_count".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "last_modified".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "compression_ratio".to_string(),
                        data_type: DataType::Float8,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Statistics for database branches".to_string(),
        });

        // pg_class - Tables, indexes, sequences, views
        self.register_view(SystemViewSchema {
            name: "pg_class".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "oid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "relname".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "relnamespace".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "reltype".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "relkind".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "relfilenode".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    // KanttBan v3.31.0 release-gate: drizzle-kit's
                    // tables-list query (bin.cjs:19810) reads
                    // `c.relrowsecurity AS rls_enabled`. Nano doesn't
                    // expose pg_catalog-level RLS — RLS lives in the
                    // TenantManager — so report `false` for every
                    // row. Pre-v3.31.0 the catalog short-circuit
                    // hid this; now that pg_class flows through the
                    // planner, the column needs to actually exist.
                    sv_col("relrowsecurity", DataType::Boolean),
                    // Round-3 PARTITION BY Stage-0: partition-bound text for a
                    // partition child. The pgrust corpus reads
                    // `SELECT relname, relpartbound FROM pg_class WHERE …`. At
                    // Stage 0 the flatten keeps no typed bound, so every row
                    // reports NULL (nullable text, exactly what PostgreSQL
                    // returns for a non-partition relation); Stage 1 fills real
                    // `pg_get_expr`-style bounds for partition children.
                    sv_col("relpartbound", DataType::Text),
                ],
            },
            description: "Catalog of tables, indexes, sequences, and views".to_string(),
        });

        // pg_attribute - Column metadata
        self.register_view(SystemViewSchema {
            name: "pg_attribute".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "attrelid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "attname".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "atttypid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "attlen".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "attnotnull".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "attnum".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "atttypmod".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    // KanttBan #23 phase 2.4: drizzle-kit reads these
                    // four per-column attributes. attisdropped is the
                    // most critical — drizzle filters on
                    // `NOT a.attisdropped` and would error out
                    // without it. attndims tracks array nesting (0
                    // for scalars). attidentity / attgenerated are
                    // CHAR(1) PG codes — '' (empty) when neither, 'd'
                    // for IDENTITY BY DEFAULT, 'a' for ALWAYS, 's' /
                    // 'v' for STORED / VIRTUAL generated columns.
                    sv_col("attisdropped", DataType::Boolean),
                    sv_col("attndims", DataType::Int4),
                    sv_col("attidentity", DataType::Text),
                    sv_col("attgenerated", DataType::Text),
                    sv_col("atthasdef", DataType::Boolean),
                    sv_col("attcollation", DataType::Int4),
                ],
            },
            description: "Catalog of table columns and their attributes".to_string(),
        });

        // pg_type - Data type definitions
        self.register_view(SystemViewSchema {
            name: "pg_type".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "oid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "typname".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "typlen".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "typbyval".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "typcategory".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "typnotnull".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    // KanttBan #23 phase 2.6: drizzle's
                    // getColumnsInfoQuery LEFT-JOINs pg_type and then
                    // pg_namespace on `enum_t.typnamespace`. Add the
                    // column (every built-in lives in pg_catalog OID
                    // 11) so the join resolves.
                    sv_col("typnamespace", DataType::Int4),
                    sv_col("typtype", DataType::Text),
                    sv_col("typowner", DataType::Int4),
                    sv_col("typrelid", DataType::Int4),
                    sv_col("typbasetype", DataType::Int4),
                ],
            },
            description: "Catalog of data types".to_string(),
        });

        // sqlite_master - SQLite-shaped catalog for sqlite3-driven Python apps.
        // Only the columns sqlite3 callers actually inspect.
        self.register_view(SystemViewSchema {
            name: "sqlite_master".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "type".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "tbl_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "rootpage".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "sql".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "SQLite-compatible catalog (drop-in for sqlite3 apps)".to_string(),
        });

        // pg_namespace - Schemas
        self.register_view(SystemViewSchema {
            name: "pg_namespace".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "oid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "nspname".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "nspowner".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Catalog of schemas (namespaces)".to_string(),
        });

        // pg_index - Indexes
        self.register_view(SystemViewSchema {
            name: "pg_index".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "indexrelid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "indrelid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "indisprimary".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "indisunique".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "indisexclusion".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "indkey".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    sv_col("indnatts", DataType::Int2),
                    sv_col("indnkeyatts", DataType::Int2),
                    sv_col("indisclustered", DataType::Boolean),
                    sv_col("indisvalid", DataType::Boolean),
                    sv_col("indisready", DataType::Boolean),
                    sv_col("indisreplident", DataType::Boolean),
                    sv_col("indexprs", DataType::Text),
                    sv_col("indpred", DataType::Text),
                ],
            },
            description: "Catalog of indexes".to_string(),
        });

        // pg_constraint - Constraints
        self.register_view(SystemViewSchema {
            name: "pg_constraint".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "oid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "conname".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "connamespace".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "contype".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "conrelid".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "confrelid".to_string(),
                        data_type: DataType::Int4,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    sv_col("conindid", DataType::Int4),
                    sv_col("conkey", DataType::Text),
                    sv_col("confkey", DataType::Text),
                    sv_col("confupdtype", DataType::Text),
                    sv_col("confdeltype", DataType::Text),
                    sv_col("confmatchtype", DataType::Text),
                    sv_col("condeferrable", DataType::Boolean),
                    sv_col("condeferred", DataType::Boolean),
                    sv_col("convalidated", DataType::Boolean),
                ],
            },
            description: "Catalog of constraints (primary key, foreign key, unique, check)".to_string(),
        });

        // information_schema.columns - Standard SQL
        self.register_view(SystemViewSchema {
            name: "information_schema.columns".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "table_schema".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "table_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "column_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "ordinal_position".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "column_default".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "is_nullable".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "data_type".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    // KanttBan #23 (v3.31.1 phase 2): identity /
                    // generated-column fields that drizzle-kit's
                    // getColumnsInfoQuery reads.
                    sv_col("udt_name", DataType::Text),
                    sv_col("is_generated", DataType::Text), // 'NEVER' or 'ALWAYS'
                    sv_col("generation_expression", DataType::Text),
                    sv_col("is_identity", DataType::Text),         // 'YES' or 'NO'
                    sv_col("identity_generation", DataType::Text), // 'ALWAYS' or 'BY DEFAULT' or NULL
                    sv_col("identity_start", DataType::Text),
                    sv_col("identity_increment", DataType::Text),
                    sv_col("identity_maximum", DataType::Text),
                    sv_col("identity_minimum", DataType::Text),
                    sv_col("identity_cycle", DataType::Text), // 'YES' or 'NO'
                ],
            },
            description: "Information schema view of all table columns (ANSI SQL standard)".to_string(),
        });

        // heliosdb_compression_stats - Compression statistics by algorithm
        self.register_view(SystemViewSchema {
            name: "heliosdb_compression_stats".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "algorithm".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "uses".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "avg_ratio".to_string(),
                        data_type: DataType::Float8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "avg_compress_us".to_string(),
                        data_type: DataType::Float8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "avg_decompress_us".to_string(),
                        data_type: DataType::Float8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "total_bytes_in".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "total_bytes_out".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows compression statistics grouped by algorithm".to_string(),
        });

        // heliosdb_pattern_stats - Pattern detection statistics
        self.register_view(SystemViewSchema {
            name: "heliosdb_pattern_stats".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "pattern".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "detections".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "best_algorithm".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "avg_ratio".to_string(),
                        data_type: DataType::Float8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows pattern detection statistics".to_string(),
        });

        // heliosdb_compression_events - Recent compression events
        self.register_view(SystemViewSchema {
            name: "heliosdb_compression_events".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "timestamp".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "table_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "column_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "algorithm".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "ratio".to_string(),
                        data_type: DataType::Float8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "input_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "output_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "duration_us".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows recent compression events per table/column".to_string(),
        });

        // heliosdb_config - Configuration settings
        self.register_view(SystemViewSchema {
            name: "heliosdb_config".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "key".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "value".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "description".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows compression-related configuration settings".to_string(),
        });

        // ========== HA Replication System Views ==========

        // pg_replication_status - Current node's HA configuration and role
        self.register_view(SystemViewSchema {
            name: "pg_replication_status".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "node_id".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "role".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "sync_mode".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "is_read_only".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "current_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "standby_count".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "primary_host".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "listen_addr".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "replication_port".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "started_at".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows current node's HA replication status and configuration".to_string(),
        });

        // pg_replication_standbys - Connected standbys (on primary)
        self.register_view(SystemViewSchema {
            name: "pg_replication_standbys".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "node_id".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "address".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "state".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "sync_mode".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "current_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "flush_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "apply_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "lag_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "lag_ms".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "connected_at".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "last_heartbeat".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows connected standby nodes (run on primary)".to_string(),
        });

        // pg_replication_primary - Primary connection status (on standby)
        self.register_view(SystemViewSchema {
            name: "pg_replication_primary".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "primary_node_id".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "primary_address".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "connection_state".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "primary_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "local_lsn".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "lag_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "lag_ms".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "fencing_token".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "connected_at".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "last_heartbeat".to_string(),
                        data_type: DataType::Timestamp,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows primary connection status (run on standby)".to_string(),
        });

        // pg_replication_metrics - Replication performance metrics
        self.register_view(SystemViewSchema {
            name: "pg_replication_metrics".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "metric_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "metric_value".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "description".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows replication performance metrics".to_string(),
        });

        // heliosdb_art_indexes - ART index information
        self.register_view(SystemViewSchema {
            name: "heliosdb_art_indexes".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "index_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "table_name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "columns".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "index_type".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "key_count".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "memory_bytes".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "node_count".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "lookup_count".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows ART (Adaptive Radix Tree) index information".to_string(),
        });

        // heliosdb_simd_capabilities - SIMD CPU feature detection
        self.register_view(SystemViewSchema {
            name: "heliosdb_simd_capabilities".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "feature".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "available".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "vector_width".to_string(),
                        data_type: DataType::Int4,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "description".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows CPU SIMD capabilities for query acceleration".to_string(),
        });

        // heliosdb_row_cache_stats - Row cache statistics
        self.register_view(SystemViewSchema {
            name: "heliosdb_row_cache_stats".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "metric".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "value".to_string(),
                        data_type: DataType::Int8,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "description".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Shows row cache statistics and hit rates".to_string(),
        });

        // heliosdb_lock_census - Read-hot-path lock-contention counters (W3.1).
        // Empty unless built with the `lock-census` feature AND enabled via
        // `[performance] lock_census`. One row per instrumented lock site.
        self.register_view(SystemViewSchema {
            name: "heliosdb_lock_census".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("lock_site", DataType::Text),
                    sv_col("acquisitions", DataType::Int8),
                    sv_col("contended", DataType::Int8),
                    sv_col("contended_wait_nanos", DataType::Int8),
                ],
            },
            description: "Read-hot-path lock-contention census (W3.1 plateau attribution)".to_string(),
        });

        // heliosdb_write_volume - Per-statement-class write-volume census (W3.2).
        // Zero unless `[performance] write_volume_stats` is enabled. One row per
        // statement class: data:/version/index-key byte totals + a row count.
        self.register_view(SystemViewSchema {
            name: "heliosdb_write_volume".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("stmt_class", DataType::Text),
                    sv_col("data_bytes", DataType::Int8),
                    sv_col("version_bytes", DataType::Int8),
                    sv_col("index_key_bytes", DataType::Int8),
                    sv_col("rows", DataType::Int8),
                ],
            },
            description: "Per-statement-class write-volume census (W3.2 version-format quantification)".to_string(),
        });

        // heliosdb_copy_phase_stats - COPY fast-path phase-timing census (W3.4).
        // Zero unless `[performance] copy_phase_stats` is enabled. One row per
        // funnel phase: cumulative wall nanos, a call count, and a row count.
        self.register_view(SystemViewSchema {
            name: "heliosdb_copy_phase_stats".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("phase", DataType::Text),
                    sv_col("total_nanos", DataType::Int8),
                    sv_col("calls", DataType::Int8),
                    sv_col("rows", DataType::Int8),
                ],
            },
            description: "COPY fast-path phase-timing census (W3.4 ART-maintenance attribution)".to_string(),
        });

        // pg_tables — make the basic catalog query work over SQL.
        // The legacy SystemViewRegistry in sql/system_views.rs has
        // a richer implementation we delegate to at execute time.
        self.register_view(SystemViewSchema {
            name: "pg_tables".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "schemaname".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "tablename".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "tableowner".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "tablespace".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "hasindexes".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "hasrules".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "hastriggers".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "rowsecurity".to_string(),
                        data_type: DataType::Boolean,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Lists all user tables (schemaname split honours #188 namespacing)".to_string(),
        });

        // hdb_code_languages — code-graph track grammar inventory.
        // Always registered so the view is discoverable; the
        // executor returns an empty result when the code-graph
        // feature isn't compiled in.
        // ---- KanttBan #22 (v3.31.0) — pg_user for catalog JOINs ----
        // drizzle-kit / Postgres ORMs JOIN pg_namespace ⨝ pg_user on
        // `u.usesysid = n.nspowner` to attribute schemas to owners.
        // Pre-v3.31.0 the substring-routed short-circuit in
        // protocol/postgres/catalog.rs picked the first matching branch
        // (pg_roles) and discarded the pg_namespace side of the JOIN,
        // returning bogus rows. Now JOINs flow through the regular
        // operator pipeline; pg_user just needs to exist as a scannable
        // source with the standard shape.
        self.register_view(SystemViewSchema {
            name: "pg_user".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("usename", DataType::Text),
                    sv_col("usesysid", DataType::Int4),
                    sv_col("usecreatedb", DataType::Boolean),
                    sv_col("usesuper", DataType::Boolean),
                    sv_col("userepl", DataType::Boolean),
                    sv_col("usebypassrls", DataType::Boolean),
                    sv_col("passwd", DataType::Text),
                    sv_col("valuntil", DataType::Text),
                    sv_col("useconfig", DataType::Text),
                ],
            },
            description: "Built-in PG-compat view over pg_authid (read-only stub)".to_string(),
        });

        // pg_roles — PG's authoritative role list (different shape from
        // pg_user, includes rolinherit / rolreplication / rolconnlimit /
        // rolvaliduntil etc.). drizzle-kit queries this directly during
        // introspection. Same two hard-coded roles.
        self.register_view(SystemViewSchema {
            name: "pg_roles".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("rolname", DataType::Text),
                    sv_col("rolsuper", DataType::Boolean),
                    sv_col("rolinherit", DataType::Boolean),
                    sv_col("rolcreaterole", DataType::Boolean),
                    sv_col("rolcreatedb", DataType::Boolean),
                    sv_col("rolcanlogin", DataType::Boolean),
                    sv_col("rolreplication", DataType::Boolean),
                    sv_col("rolconnlimit", DataType::Int4),
                    sv_col("rolpassword", DataType::Text),
                    sv_col("rolvaliduntil", DataType::Text),
                    sv_col("rolbypassrls", DataType::Boolean),
                ],
            },
            description: "Built-in PG-compat view over pg_authid (read-only stub)".to_string(),
        });

        // information_schema.table_constraints — drizzle reads this for
        // PK/UNIQUE/FK constraint info. 5-col PG-standard shape.
        self.register_view(SystemViewSchema {
            name: "information_schema.table_constraints".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("constraint_catalog", DataType::Text),
                    sv_col("constraint_schema", DataType::Text),
                    sv_col("constraint_name", DataType::Text),
                    sv_col("table_name", DataType::Text),
                    sv_col("constraint_type", DataType::Text),
                ],
            },
            description: "Standard SQL info_schema.table_constraints (PK/UNIQUE/FK)".to_string(),
        });

        // information_schema.key_column_usage — column-to-constraint
        // mapping. drizzle joins this with table_constraints to map
        // FK columns to their referenced table/column.
        self.register_view(SystemViewSchema {
            name: "information_schema.key_column_usage".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("constraint_catalog", DataType::Text),
                    sv_col("constraint_schema", DataType::Text),
                    sv_col("constraint_name", DataType::Text),
                    sv_col("table_name", DataType::Text),
                    sv_col("column_name", DataType::Text),
                    sv_col("ordinal_position", DataType::Int4),
                ],
            },
            description: "Standard SQL info_schema.key_column_usage".to_string(),
        });

        // information_schema.constraint_column_usage — columns used
        // BY a constraint (the target side for FKs; same as KCU for
        // PK/UNIQUE). drizzle reads this for FK target resolution.
        self.register_view(SystemViewSchema {
            name: "information_schema.constraint_column_usage".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("table_catalog", DataType::Text),
                    sv_col("table_schema", DataType::Text),
                    sv_col("table_name", DataType::Text),
                    sv_col("column_name", DataType::Text),
                    sv_col("constraint_catalog", DataType::Text),
                    sv_col("constraint_schema", DataType::Text),
                    sv_col("constraint_name", DataType::Text),
                ],
            },
            description: "Standard SQL info_schema.constraint_column_usage".to_string(),
        });

        // information_schema.referential_constraints — FK referential
        // actions (ON UPDATE / ON DELETE) per constraint.
        self.register_view(SystemViewSchema {
            name: "information_schema.referential_constraints".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("constraint_catalog", DataType::Text),
                    sv_col("constraint_schema", DataType::Text),
                    sv_col("constraint_name", DataType::Text),
                    sv_col("unique_constraint_catalog", DataType::Text),
                    sv_col("unique_constraint_schema", DataType::Text),
                    sv_col("unique_constraint_name", DataType::Text),
                    sv_col("match_option", DataType::Text),
                    sv_col("update_rule", DataType::Text),
                    sv_col("delete_rule", DataType::Text),
                ],
            },
            description: "Standard SQL info_schema.referential_constraints (FK actions)".to_string(),
        });

        // information_schema.tables — drizzle-kit / Prisma / Knex /
        // postgres-js all introspect through this. Same 4-column shape
        // as the catalog handler's query_information_schema_tables.
        self.register_view(SystemViewSchema {
            name: "information_schema.tables".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("table_catalog", DataType::Text),
                    sv_col("table_schema", DataType::Text),
                    sv_col("table_name", DataType::Text),
                    sv_col("table_type", DataType::Text),
                ],
            },
            description: "SQL-standard table catalogue, sourced from storage::catalog::list_tables".to_string(),
        });

        // ---- Empty-stub catalogue/view tables (KanttBan #22 v3.31.0 slice 5)
        // Nano doesn't implement these features (sequences as objects,
        // logical replication, RLS policies, extended stats, mat-view
        // catalogue, inheritance, server functions/procedures). Every
        // entry registers the standard PG-shape so introspection tools
        // see the expected column names through the planner pipeline.
        // execute returns vec![] — empty rowset.

        self.register_view(SystemViewSchema {
            name: "pg_sequences".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("schemaname", DataType::Text),
                    sv_col("sequencename", DataType::Text),
                    sv_col("sequenceowner", DataType::Text),
                    sv_col("data_type", DataType::Text),
                    sv_col("start_value", DataType::Int8),
                    sv_col("min_value", DataType::Int8),
                    sv_col("max_value", DataType::Int8),
                    sv_col("increment_by", DataType::Int8),
                    sv_col("cycle", DataType::Boolean),
                    sv_col("cache_size", DataType::Int8),
                    sv_col("last_value", DataType::Int8),
                ],
            },
            description: "PG-compat sequences view (real CREATE SEQUENCE defs + SERIAL/IDENTITY synthetics)".to_string(),
        });

        // information_schema.sequences — SQL-standard 12-col shape. Lists REAL
        // CREATE SEQUENCE definitions only (PG excludes SERIAL/IDENTITY-owned
        // sequences from this view). a2h / ORMs / pg_dump discover sequences
        // through this surface; the prior count=0 break is fixed.
        self.register_view(SystemViewSchema {
            name: "information_schema.sequences".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("sequence_catalog", DataType::Text),
                    sv_col("sequence_schema", DataType::Text),
                    sv_col("sequence_name", DataType::Text),
                    sv_col("data_type", DataType::Text),
                    sv_col("numeric_precision", DataType::Int4),
                    sv_col("numeric_precision_radix", DataType::Int4),
                    sv_col("numeric_scale", DataType::Int4),
                    sv_col("start_value", DataType::Text),
                    sv_col("minimum_value", DataType::Text),
                    sv_col("maximum_value", DataType::Text),
                    sv_col("increment", DataType::Text),
                    sv_col("cycle_option", DataType::Text),
                ],
            },
            description: "SQL-standard sequences catalogue, sourced from storage::catalog::list_sequences".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_proc".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("proname", DataType::Text),
                    sv_col("pronamespace", DataType::Int4),
                    sv_col("proowner", DataType::Int4),
                    sv_col("prolang", DataType::Int4),
                    sv_col("prorettype", DataType::Int4),
                    sv_col("proargtypes", DataType::Text),
                    sv_col("prosrc", DataType::Text),
                ],
            },
            description: "PG-compat procedures catalogue (empty stub)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_description".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("objoid", DataType::Int4),
                    sv_col("classoid", DataType::Int4),
                    sv_col("objsubid", DataType::Int4),
                    sv_col("description", DataType::Text),
                ],
            },
            description: "PG-compat object descriptions (empty — Nano doesn't store COMMENT ON)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_policies".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("schemaname", DataType::Text),
                    sv_col("tablename", DataType::Text),
                    sv_col("policyname", DataType::Text),
                    sv_col("permissive", DataType::Text),
                    sv_col("roles", DataType::Text),
                    sv_col("cmd", DataType::Text),
                    sv_col("qual", DataType::Text),
                    sv_col("with_check", DataType::Text),
                ],
            },
            description: "PG-compat RLS policies view (empty — Nano RLS via TenantManager)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_policy".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("polname", DataType::Text),
                    sv_col("polrelid", DataType::Int4),
                    sv_col("polcmd", DataType::Char(1)),
                    sv_col("polpermissive", DataType::Boolean),
                    sv_col("polroles", DataType::Text),
                    sv_col("polqual", DataType::Text),
                    sv_col("polwithcheck", DataType::Text),
                ],
            },
            description: "PG-compat RLS policy catalogue (empty stub)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_matviews".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("schemaname", DataType::Text),
                    sv_col("matviewname", DataType::Text),
                    sv_col("matviewowner", DataType::Text),
                    sv_col("tablespace", DataType::Text),
                    sv_col("hasindexes", DataType::Boolean),
                    sv_col("ispopulated", DataType::Boolean),
                    sv_col("definition", DataType::Text),
                ],
            },
            description: "PG-compat matview view (empty — use pg_mv_staleness instead)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_inherits".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("inhrelid", DataType::Int4),
                    sv_col("inhparent", DataType::Int4),
                    sv_col("inhseqno", DataType::Int4),
                ],
            },
            description: "PG-compat inheritance catalogue (empty — Nano has no inheritance)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_publication".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("pubname", DataType::Text),
                    sv_col("pubowner", DataType::Int4),
                    sv_col("puballtables", DataType::Boolean),
                    sv_col("pubinsert", DataType::Boolean),
                    sv_col("pubupdate", DataType::Boolean),
                    sv_col("pubdelete", DataType::Boolean),
                    sv_col("pubtruncate", DataType::Boolean),
                ],
            },
            description: "PG-compat logical replication publications (empty stub)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_statistic_ext".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("stxrelid", DataType::Int4),
                    sv_col("stxnamespace", DataType::Int4),
                    sv_col("stxname", DataType::Text),
                    sv_col("stxkeys", DataType::Text),
                    sv_col("stxkind", DataType::Text),
                    sv_col("stxstattarget", DataType::Int4),
                ],
            },
            description: "PG-compat extended stats catalogue (empty stub)".to_string(),
        });

        // pg_attrdef — column-default catalogue. KanttBan #23
        // (v3.31.1 phase 1): drizzle-kit's getColumnsInfoQuery joins
        // here in an EXISTS subquery to detect SERIAL columns. Empty
        // stub means the EXISTS is false → drizzle's SERIAL-detection
        // CASE falls through to format_type, which is what we want.
        // Phase 2 populates from real column defaults to make the
        // SERIAL/IDENTITY detection accurate.
        self.register_view(SystemViewSchema {
            name: "pg_attrdef".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("adrelid", DataType::Int4),
                    sv_col("adnum", DataType::Int2),
                    sv_col("adbin", DataType::Text),
                    sv_col("adsrc", DataType::Text),
                ],
            },
            description: "PG-compat column-default catalogue (empty stub; phase-2 will populate)".to_string(),
        });

        // pg_database — \l, ORM connection introspection. Minimal
        // implementation returns only the implicit 'heliosdb' row;
        // tenant enumeration (the v3.25 CREATE DATABASE wrap) needs
        // EmbeddedDatabase access which the registry execute()
        // signature doesn't expose today — flag for v3.31.x follow-up.
        self.register_view(SystemViewSchema {
            name: "pg_database".to_string(),
            schema: Schema {
                columns: vec![
                    sv_col("oid", DataType::Int4),
                    sv_col("datname", DataType::Text),
                    sv_col("datdba", DataType::Int4),
                    sv_col("encoding", DataType::Int4),
                    sv_col("datcollate", DataType::Text),
                    sv_col("datctype", DataType::Text),
                    sv_col("datistemplate", DataType::Boolean),
                    sv_col("datallowconn", DataType::Boolean),
                    sv_col("datconnlimit", DataType::Int4),
                    sv_col("dattablespace", DataType::Int4),
                ],
            },
            description: "PG-compat database list (registry-stub; tenant enumeration deferred)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "hdb_code_languages".to_string(),
            schema: Schema {
                columns: vec![
                    Column {
                        name: "name".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                    Column {
                        name: "source".to_string(),
                        data_type: DataType::Text,
                        nullable: false,
                        primary_key: false,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: ColumnStorageMode::Default,
                    },
                ],
            },
            description: "Lists every tree-sitter grammar the indexer can parse".to_string(),
        });
    }

    /// Register a system view
    fn register_view(&mut self, view: SystemViewSchema) {
        self.views.insert(view.name.clone(), view);
    }
}

/// KanttBan #23 (v3.31.1 phase 2): map a Nano DataType to the
/// canonical PG udt_name string. Matches `format_pg_type_oid` in
/// `src/sql/evaluator.rs` (different surface, same OID-to-name
/// table) — kept in sync by convention.
fn format_pg_type_name(dt: &DataType) -> String {
    match dt {
        DataType::Boolean => "bool".into(),
        DataType::Int2 => "int2".into(),
        DataType::Int4 => "int4".into(),
        DataType::Int8 => "int8".into(),
        DataType::Float4 => "float4".into(),
        DataType::Float8 => "float8".into(),
        DataType::Numeric => "numeric".into(),
        DataType::Varchar(_) => "varchar".into(),
        DataType::Text => "text".into(),
        DataType::Char(_) => "bpchar".into(),
        DataType::Bytea => "bytea".into(),
        DataType::Date => "date".into(),
        DataType::Time => "time".into(),
        DataType::Timestamp => "timestamp".into(),
        DataType::Timestamptz => "timestamptz".into(),
        DataType::Interval => "interval".into(),
        DataType::Uuid => "uuid".into(),
        DataType::Json => "json".into(),
        DataType::Jsonb => "jsonb".into(),
        DataType::Array(inner) => format!("_{}", format_pg_type_name(inner)),
        DataType::Vector(_) => "vector".into(),
    }
}

/// Build a non-PK, non-unique nullable column with default storage —
/// the shape system-view columns take. Reduces the per-column boilerplate
/// from 9 fields to 2.
#[inline]
fn sv_col(name: &str, data_type: DataType) -> Column {
    Column {
        name: name.to_string(),
        data_type,
        nullable: true,
        primary_key: false,
        source_table: None,
        source_table_name: None,
        default_expr: None,
        unique: false,
        storage_mode: ColumnStorageMode::Default,
    }
}

const PG_TABLE_OID_BASE: i32 = 1000;
const PG_INDEX_OID_BASE: i32 = 5000;
const PG_CONSTRAINT_OID_BASE: i32 = 4000;
/// Distinct OID base for sequence relations (pg_class relkind='S'). Kept clear
/// of the table (1000) and index (5000) bases so sequence OIDs never collide.
const PG_SEQ_OID_BASE: i32 = 6000;
const PG_PUBLIC_NAMESPACE_OID: i32 = 2200;

fn pg_table_oid(table_idx: usize) -> i32 {
    PG_TABLE_OID_BASE + table_idx as i32
}

fn pg_table_oid_by_name(tables: &[String], table_name: &str) -> Option<i32> {
    tables
        .iter()
        .position(|name| name.eq_ignore_ascii_case(table_name))
        .map(pg_table_oid)
}

fn sorted_art_indexes(storage: &StorageEngine) -> Vec<(String, String, ArtIndexType, Vec<String>)> {
    let mut indexes = storage.art_indexes().list_indexes();
    indexes.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    indexes
}

fn pg_index_oid(index_idx: usize) -> i32 {
    PG_INDEX_OID_BASE + index_idx as i32
}

fn pg_index_oid_by_name(indexes: &[(String, String, ArtIndexType, Vec<String>)], index_name: &str) -> i32 {
    indexes
        .iter()
        .position(|(name, _, _, _)| name.eq_ignore_ascii_case(index_name))
        .map(pg_index_oid)
        .unwrap_or(0)
}

fn pg_sequence_oid(seq_idx: usize) -> i32 {
    PG_SEQ_OID_BASE + seq_idx as i32
}

/// Map a SERIAL/IDENTITY column's integer type to its synthetic
/// owned-sequence `(data_type, max_value)`. Non-integer columns (shouldn't
/// happen for an identity column) fall back to bigint.
fn serial_type_bounds(dt: &DataType) -> (&'static str, i64) {
    match dt {
        DataType::Int2 => ("smallint", i16::MAX as i64),
        DataType::Int4 => ("integer", i32::MAX as i64),
        _ => ("bigint", i64::MAX),
    }
}

fn pg_attnums(schema: &Schema, columns: &[String]) -> Vec<i32> {
    columns
        .iter()
        .filter_map(|wanted| {
            schema
                .columns
                .iter()
                .position(|col| col.name.eq_ignore_ascii_case(wanted))
                .map(|idx| idx as i32 + 1)
        })
        .collect()
}

fn pg_indkey(schema: &Schema, columns: &[String]) -> String {
    pg_attnums(schema, columns)
        .into_iter()
        .map(|attnum| attnum.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn pg_conkey(schema: &Schema, columns: &[String]) -> Value {
    let attnums = pg_attnums(schema, columns);
    if attnums.is_empty() {
        Value::Null
    } else {
        Value::String(format!(
            "{{{}}}",
            attnums
                .into_iter()
                .map(|attnum| attnum.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

fn pg_fk_action_code(action: crate::sql::ReferentialAction) -> &'static str {
    match action {
        crate::sql::ReferentialAction::NoAction => "a",
        crate::sql::ReferentialAction::Restrict => "r",
        crate::sql::ReferentialAction::Cascade => "c",
        crate::sql::ReferentialAction::SetNull => "n",
        crate::sql::ReferentialAction::SetDefault => "d",
    }
}

#[allow(clippy::too_many_arguments)]
fn push_pg_constraint_row(
    rows: &mut Vec<Tuple>,
    next_oid: &mut i32,
    seen: &mut HashSet<String>,
    name: String,
    contype: &str,
    conrelid: i32,
    confrelid: Option<i32>,
    conindid: i32,
    conkey: Value,
    confkey: Value,
    on_update: Option<crate::sql::ReferentialAction>,
    on_delete: Option<crate::sql::ReferentialAction>,
    deferrable: bool,
    deferred: bool,
    validated: bool,
) {
    let seen_key = format!("{conrelid}:{name}");
    if !seen.insert(seen_key) {
        return;
    }

    rows.push(Tuple::new(vec![
        Value::Int4(*next_oid),
        Value::String(name),
        Value::Int4(PG_PUBLIC_NAMESPACE_OID),
        Value::String(contype.to_string()),
        Value::Int4(conrelid),
        confrelid.map(Value::Int4).unwrap_or(Value::Null),
        Value::Int4(conindid),
        conkey,
        confkey,
        on_update
            .map(pg_fk_action_code)
            .map(|code| Value::String(code.to_string()))
            .unwrap_or(Value::Null),
        on_delete
            .map(pg_fk_action_code)
            .map(|code| Value::String(code.to_string()))
            .unwrap_or(Value::Null),
        if contype == "f" {
            Value::String("s".to_string())
        } else {
            Value::Null
        },
        Value::Boolean(deferrable),
        Value::Boolean(deferred),
        Value::Boolean(validated),
    ]));
    *next_oid += 1;
}

impl SystemViewRegistry {
    /// Get system view schema
    pub fn get_schema(&self, view_name: &str) -> Option<&Schema> {
        self.views.get(view_name).map(|v| &v.schema)
    }

    /// Check if a view is a system view
    pub fn is_system_view(&self, view_name: &str) -> bool {
        self.views.contains_key(view_name)
    }

    /// List all system views
    pub fn list_views(&self) -> Vec<&str> {
        self.views.keys().map(|s| s.as_str()).collect()
    }

    /// Execute a system view query
    ///
    /// Queries storage metadata and returns results based on the view type
    pub fn execute(&self, view_name: &str, storage: &StorageEngine) -> Result<Vec<Tuple>> {
        if !self.is_system_view(view_name) {
            return Err(Error::query_execution(format!("Unknown system view: {}", view_name)));
        }

        match view_name {
            "pg_database_branches" => Self::execute_pg_database_branches(storage),
            "pg_mv_staleness" => Self::execute_pg_mv_staleness(storage),
            "pg_vector_index_stats" => Self::execute_pg_vector_index_stats(storage),
            "pg_compare_branches" => {
                // This view requires parameters (source and target branch names)
                // For now, return error indicating parameters are needed
                Err(Error::query_execution(
                    "pg_compare_branches requires parameters: SELECT * FROM pg_compare_branches('source_branch', 'target_branch')"
                ))
            }
            "pg_branch_stats" => Self::execute_pg_branch_stats(storage),
            "pg_class" => Self::execute_pg_class(storage),
            "pg_attribute" => Self::execute_pg_attribute(storage),
            "pg_type" => Self::execute_pg_type(storage),
            "pg_namespace" => Self::execute_pg_namespace(storage),
            "pg_user" => Self::execute_pg_user(),
            "pg_roles" => Self::execute_pg_roles(),
            "information_schema.tables" => Self::execute_information_schema_tables(storage),
            "information_schema.sequences" => Self::execute_information_schema_sequences(storage),
            "information_schema.table_constraints" => Self::execute_information_schema_table_constraints(storage),
            "information_schema.key_column_usage" => Self::execute_information_schema_key_column_usage(storage),
            "information_schema.constraint_column_usage" => {
                Self::execute_information_schema_constraint_column_usage(storage)
            }
            "information_schema.referential_constraints" => {
                Self::execute_information_schema_referential_constraints(storage)
            }
            "pg_database" => Self::execute_pg_database(),
            // Empty-stub catalogue tables (v3.31.0 slice 5). Schema
            // already registered; rows are always empty because Nano
            // doesn't model these concepts.
            "pg_sequences" => Self::execute_pg_sequences(storage),
            "pg_attrdef" => Self::execute_pg_attrdef(storage),
            "pg_proc" | "pg_description" | "pg_policies" | "pg_policy" | "pg_matviews" | "pg_inherits"
            | "pg_publication" | "pg_statistic_ext" => Ok(vec![]),
            "sqlite_master" => Self::execute_sqlite_master(storage),
            "pg_index" => Self::execute_pg_index(storage),
            "pg_constraint" => Self::execute_pg_constraint(storage),
            "information_schema.columns" => Self::execute_information_schema_columns(storage),
            // Compression monitoring views
            "heliosdb_compression_stats" => Self::execute_heliosdb_compression_stats(storage),
            "heliosdb_pattern_stats" => Self::execute_heliosdb_pattern_stats(storage),
            "heliosdb_compression_events" => Self::execute_heliosdb_compression_events(storage),
            "heliosdb_config" => Self::execute_heliosdb_config(storage),
            // HA Replication monitoring views
            "pg_replication_status" => Self::execute_pg_replication_status(),
            "pg_replication_standbys" => Self::execute_pg_replication_standbys(),
            "pg_replication_primary" => Self::execute_pg_replication_primary(),
            "pg_replication_metrics" => Self::execute_pg_replication_metrics(),
            // ART index monitoring
            "heliosdb_art_indexes" => Self::execute_heliosdb_art_indexes(storage),
            // SIMD capabilities
            "heliosdb_simd_capabilities" => Self::execute_heliosdb_simd_capabilities(),
            // Row cache stats
            "heliosdb_row_cache_stats" => Self::execute_heliosdb_row_cache_stats(storage),
            // Read-hot-path lock-contention census (W3.1)
            "heliosdb_lock_census" => Self::execute_heliosdb_lock_census(),
            // Per-statement-class write-volume census (W3.2)
            "heliosdb_write_volume" => Self::execute_heliosdb_write_volume(),
            // COPY fast-path phase-timing census (W3.4)
            "heliosdb_copy_phase_stats" => Self::execute_heliosdb_copy_phase_stats(),
            // Code-graph track grammar inventory.
            "hdb_code_languages" => Self::execute_hdb_code_languages(),
            // pg_tables — delegate to the legacy SystemViewRegistry
            // which has the schema-namespacing split we want.
            "pg_tables" => Self::execute_pg_tables_compat(storage),
            _ => {
                // Other system views not yet implemented
                Ok(vec![])
            }
        }
    }

    /// Bridge to the legacy SystemViewRegistry's pg_tables
    /// implementation so SELECT * FROM pg_tables works over SQL
    /// (the planner only consults the phase-3 registry).
    fn execute_pg_tables_compat(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let legacy = crate::sql::SystemViewRegistry::new();
        legacy.execute("pg_tables", storage)
    }

    /// Materialise `hdb_code_languages`: one row per static
    /// `SupportedLanguage` variant + one row per
    /// runtime-registered grammar.  Sorted by name for stable
    /// output. Returns an empty set when the `code-graph` feature
    /// isn't compiled in.
    fn execute_hdb_code_languages() -> Result<Vec<Tuple>> {
        #[cfg(feature = "code-graph")]
        {
            use crate::code_graph::{parse, SupportedLanguage};
            let mut rows: Vec<(String, &'static str)> = SupportedLanguage::all()
                .iter()
                .map(|l| (l.as_str().to_string(), "static"))
                .collect();
            for name in parse::registered_grammars() {
                if let Some(idx) = rows.iter().position(|(n, _)| n == &name) {
                    rows[idx].1 = "runtime";
                } else {
                    rows.push((name, "runtime"));
                }
            }
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(rows
                .into_iter()
                .map(|(n, s)| Tuple::new(vec![Value::String(n), Value::String(s.to_string())]))
                .collect())
        }
        #[cfg(not(feature = "code-graph"))]
        {
            Ok(vec![])
        }
    }

    /// Execute pg_database_branches() system view
    ///
    /// Returns information about all database branches
    fn execute_pg_database_branches(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let branches = storage.list_branches()?;
        let mut results = Vec::new();

        for branch in branches {
            let tuple = Tuple::new(vec![
                Value::String(branch.name.clone()),
                Value::Int8(branch.branch_id as i64),
                Value::Int8(branch.parent_id.map(|id| id as i64).unwrap_or(0)),
                Value::Timestamp(chrono::DateTime::from_timestamp(branch.created_at as i64, 0).unwrap_or_default()),
                Value::Int8(branch.created_from_snapshot as i64),
                Value::Int8((branch.stats.storage_bytes / (1024 * 1024)) as i64), // Convert to MB
                Value::String(format!("{:?}", branch.state)),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    /// Execute pg_mv_staleness() system view
    ///
    /// Returns staleness information for all materialized views
    fn execute_pg_mv_staleness(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let mv_catalog = storage.mv_catalog();
        let view_names = mv_catalog.list_views()?;
        let mut results = Vec::new();

        for view_name in view_names {
            match mv_catalog.get_view(&view_name) {
                Ok(metadata) => {
                    // Format base tables as JSON array string
                    let base_tables = format!(
                        "[{}]",
                        metadata
                            .base_tables
                            .iter()
                            .map(|t| format!("\"{}\"", t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );

                    // Calculate staleness
                    let last_update = metadata.last_refresh.map(|dt| dt.timestamp()).unwrap_or(0);

                    let staleness_sec = metadata.staleness_seconds().unwrap_or(0);

                    // Estimate pending changes (0 for now - would need change tracking)
                    let pending_changes = 0i64;

                    // Determine status
                    let status = if metadata.is_stale() {
                        "STALE"
                    } else if staleness_sec > 3600 {
                        "OUTDATED"
                    } else {
                        "FRESH"
                    };

                    let tuple = Tuple::new(vec![
                        Value::String(metadata.view_name.clone()),
                        Value::String(base_tables),
                        Value::Timestamp(chrono::DateTime::from_timestamp(last_update, 0).unwrap_or_default()),
                        Value::Int8(pending_changes),
                        Value::Int8(staleness_sec),
                        Value::String(status.to_string()),
                    ]);
                    results.push(tuple);
                }
                Err(e) => {
                    tracing::warn!("Failed to get metadata for view '{}': {}", view_name, e);
                    continue;
                }
            }
        }

        Ok(results)
    }

    /// Execute pg_vector_index_stats() system view
    ///
    /// Returns statistics for all vector indexes
    fn execute_pg_vector_index_stats(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let vector_indexes = storage.vector_indexes();
        let all_metadata = vector_indexes.list_all_metadata();
        let mut results = Vec::new();

        for metadata in all_metadata {
            // Get statistics for this index
            match vector_indexes.get_index_stats(&metadata.name) {
                Ok(stats) => {
                    let tuple = Tuple::new(vec![
                        Value::String(stats.index_name),
                        Value::Int8(stats.num_vectors),
                        Value::Int4(stats.dimensions),
                        Value::String(stats.quantization),
                        Value::Int8(stats.memory_bytes),
                        stats.recall_at_10.map(Value::Float8).unwrap_or(Value::Null),
                    ]);
                    results.push(tuple);
                }
                Err(e) => {
                    tracing::warn!("Failed to get stats for index '{}': {}", metadata.name, e);
                    continue;
                }
            }
        }

        Ok(results)
    }

    /// Execute pg_branch_stats() system view
    ///
    /// Returns detailed statistics for all database branches
    fn execute_pg_branch_stats(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let branches = storage.list_branches()?;
        let mut results = Vec::new();

        for branch in branches {
            // Calculate compression ratio (storage vs uncompressed estimate)
            // Simple heuristic: assume typical compression of 2:1
            let compression_ratio = if branch.stats.storage_bytes > 0 {
                Some(2.0) // Placeholder - would need actual compression tracking
            } else {
                None
            };

            let last_modified_ts = if branch.stats.last_modified > 0 {
                chrono::DateTime::from_timestamp(branch.stats.last_modified as i64, 0).unwrap_or_default()
            } else {
                chrono::DateTime::from_timestamp(branch.created_at as i64, 0).unwrap_or_default()
            };

            let tuple = Tuple::new(vec![
                Value::String(branch.name.clone()),
                Value::Int8(branch.stats.modified_keys as i64),
                Value::Int8(branch.stats.storage_bytes as i64),
                Value::Int8(branch.stats.commit_count as i64),
                Value::Timestamp(last_modified_ts),
                compression_ratio.map(Value::Float8).unwrap_or(Value::Null),
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    /// Execute pg_class() system view
    ///
    /// Returns information about all tables, indexes, sequences, and views
    fn execute_pg_class(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for (idx, table_name) in tables.iter().enumerate() {
            let oid = pg_table_oid(idx);
            let tuple = Tuple::new(vec![
                Value::Int4(oid),                     // oid
                Value::String(table_name.clone()),    // relname
                Value::Int4(PG_PUBLIC_NAMESPACE_OID), // relnamespace (public schema)
                Value::Int4(oid + 1000),              // reltype
                Value::String("r".to_string()),       // relkind (r = relation/table)
                Value::Int4(oid),                     // relfilenode
                Value::Boolean(false),                // relrowsecurity (Nano RLS is via TenantManager, not pg_catalog)
                Value::Null,                          // relpartbound (Stage-0 flatten keeps no typed bound)
            ]);
            results.push(tuple);
        }

        for (idx, (index_name, _table_name, _index_type, _columns)) in sorted_art_indexes(storage).iter().enumerate() {
            let oid = pg_index_oid(idx);
            results.push(Tuple::new(vec![
                Value::Int4(oid),
                Value::String(index_name.clone()),
                Value::Int4(PG_PUBLIC_NAMESPACE_OID),
                Value::Int4(0),
                Value::String("i".to_string()),
                Value::Int4(oid),
                Value::Boolean(false),
                Value::Null, // relpartbound
            ]));
        }

        // Sequences (relkind='S'): real CREATE SEQUENCE defs first, then
        // SERIAL/IDENTITY synthetic owned-sequence names, deduped by name so
        // pg_class never reports a sequence twice. A monotonic `seq_idx` keeps
        // OIDs stable for a given catalog ordering.
        let mut seq_idx = 0usize;
        let mut seen_seq: HashSet<String> = HashSet::new();
        for def in catalog.list_sequences()? {
            if !seen_seq.insert(def.name.to_lowercase()) {
                continue;
            }
            let oid = pg_sequence_oid(seq_idx);
            seq_idx += 1;
            results.push(Tuple::new(vec![
                Value::Int4(oid),                     // oid
                Value::String(def.name.clone()),      // relname
                Value::Int4(PG_PUBLIC_NAMESPACE_OID), // relnamespace
                Value::Int4(oid),                     // reltype
                Value::String("S".to_string()),       // relkind (S = sequence)
                Value::Int4(oid),                     // relfilenode
                Value::Boolean(false),                // relrowsecurity
                Value::Null,                          // relpartbound
            ]));
        }
        for table_name in &tables {
            for col_name in catalog.list_identity_columns(table_name)? {
                let seq_name = format!("{table_name}_{col_name}_seq");
                if !seen_seq.insert(seq_name.to_lowercase()) {
                    continue;
                }
                let oid = pg_sequence_oid(seq_idx);
                seq_idx += 1;
                results.push(Tuple::new(vec![
                    Value::Int4(oid),
                    Value::String(seq_name),
                    Value::Int4(PG_PUBLIC_NAMESPACE_OID),
                    Value::Int4(oid),
                    Value::String("S".to_string()),
                    Value::Int4(oid),
                    Value::Boolean(false),
                    Value::Null, // relpartbound
                ]));
            }
        }

        Ok(results)
    }

    /// Execute sqlite_master view — SQLite-shaped catalog rows for each
    /// user table / materialised view. The `sql` column is best-effort:
    /// most sqlite3 callers only filter on `type` and `name`.
    fn execute_sqlite_master(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();
        for table_name in tables {
            // Skip internal helios_* bookkeeping tables — sqlite3 apps don't expect them.
            if table_name.starts_with("helios_") || table_name.starts_with("_hdb_") {
                continue;
            }
            let (kind, sql_decl) = if let Some(rest) = table_name.strip_prefix("mv_") {
                ("view", format!("CREATE MATERIALIZED VIEW {rest} AS ..."))
            } else {
                ("table", format!("CREATE TABLE {table_name} (...)"))
            };
            results.push(Tuple::new(vec![
                Value::String(kind.to_string()),
                Value::String(table_name.clone()),
                Value::String(table_name),
                Value::Int4(0),
                Value::String(sql_decl),
            ]));
        }
        Ok(results)
    }

    /// Execute pg_attribute() system view
    ///
    /// Returns information about all table columns and their attributes
    fn execute_pg_attribute(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();
        let oid_counter = 1000i32;

        for (table_idx, table_name) in tables.iter().enumerate() {
            let table_oid = oid_counter + (table_idx as i32);
            // KanttBan #23 phase 2.4: cache the IDENTITY column list
            // for this table so we can set attidentity = 'd' on the
            // right rows. Empty / missing record → no identity cols
            // → every column gets attidentity = ''.
            let identity_cols = catalog.list_identity_columns(table_name).unwrap_or_default();

            match catalog.get_table_schema(table_name) {
                Ok(schema) => {
                    for (col_idx, column) in schema.columns.iter().enumerate() {
                        let type_oid = Self::get_type_oid(&column.data_type);
                        let is_identity_col = identity_cols.iter().any(|c| c.eq_ignore_ascii_case(&column.name));
                        let ndims: i32 = match &column.data_type {
                            DataType::Array(_) => 1,
                            _ => 0,
                        };
                        let has_default = column.default_expr.is_some() || is_identity_col;
                        let tuple = Tuple::new(vec![
                            Value::Int4(table_oid),                                                  // attrelid
                            Value::String(column.name.clone()),                                      // attname
                            Value::Int4(type_oid),                                                   // atttypid
                            Value::Int4(-1),                                                         // attlen
                            Value::Boolean(!column.nullable),                                        // attnotnull
                            Value::Int4((col_idx + 1) as i32),                                       // attnum
                            Value::Int4(-1),                                                         // atttypmod
                            Value::Boolean(false),                                                   // attisdropped
                            Value::Int4(ndims),                                                      // attndims
                            Value::String(if is_identity_col { "d".into() } else { String::new() }), // attidentity
                            Value::String(String::new()),                                            // attgenerated
                            Value::Boolean(has_default),                                             // atthasdef
                            Value::Int4(0), // attcollation (default)
                        ]);
                        results.push(tuple);
                    }
                }
                Err(_) => {
                    // Skip tables we can't read schema for
                    continue;
                }
            }
        }

        Ok(results)
    }

    /// Execute pg_type() system view
    ///
    /// Returns information about all data types
    fn execute_pg_type(_storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let mut results = Vec::new();
        let types = vec![
            ("int4", 23, 4, true, "N", false),
            ("int8", 20, 8, true, "N", false),
            ("text", 25, -1, false, "S", false),
            ("boolean", 16, 1, true, "B", false),
            ("timestamp", 1114, 8, true, "D", false),
            ("float8", 701, 8, true, "N", false),
            ("vector", 3614, -1, false, "U", false),
        ];

        for (type_name, oid, len, byval, category, notnull) in types {
            let tuple = Tuple::new(vec![
                Value::Int4(oid),                     // oid
                Value::String(type_name.to_string()), // typname
                Value::Int4(len),                     // typlen
                Value::Boolean(byval),                // typbyval
                Value::String(category.to_string()),  // typcategory
                Value::Boolean(notnull),              // typnotnull
                // KanttBan #23 phase 2.6: typnamespace + 4 more
                // columns drizzle joins / reads. Every built-in
                // type lives in pg_catalog (OID 11). typtype 'b' =
                // base type. typowner = postgres (10). typrelid /
                // typbasetype = 0 for non-composite/non-domain.
                Value::Int4(11),           // typnamespace
                Value::String("b".into()), // typtype
                Value::Int4(10),           // typowner
                Value::Int4(0),            // typrelid
                Value::Int4(0),            // typbasetype
            ]);
            results.push(tuple);
        }

        Ok(results)
    }

    /// Execute pg_namespace() system view
    ///
    /// Returns information about all schemas (namespaces).
    /// Always exposes `public` + `information_schema`; other
    /// schemas (`_hdb_code` / `_hdb_graph` / user-created) come
    /// from the catalog's `list_schemas()` materialisation.
    /// KanttBan #23 (v3.31.1 phase 2): synthesise pg_sequences rows
    /// for every IDENTITY / SERIAL column registered in the catalog.
    /// Nano doesn't model sequences as separate objects (synthetic
    /// row counters on the column), so we project one synthetic
    /// sequence per identity column with name `<table>_<col>_seq`.
    /// drizzle-kit's sequence-diff path treats these as existing
    /// objects → no spurious "missing sequence" diffs.
    fn execute_pg_sequences(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        // Track the names we've already emitted so a real CREATE SEQUENCE and a
        // SERIAL/IDENTITY synthetic row never both surface the same name —
        // pg_dump would otherwise see doubles.
        let mut seen: HashSet<String> = HashSet::new();

        // (1) REAL sequences from the durable catalog: full fidelity from each
        // PersistedSequence + its high-water state.
        for def in catalog.list_sequences()? {
            // PostgreSQL's `last_value` is the value last OBTAINED, not the
            // reserved CACHE-block end. The durable state stores the block end
            // (`last_reserved`), which would overstate by up to `cache - 1` and,
            // worse, move BACKWARD after a CYCLE wrap. So prefer the exact
            // value this session actually served (tracked in the live runtime);
            // fall back to the durable high-water only when this session has not
            // advanced the sequence (e.g. just after a reopen) — the documented
            // cached-sequence gap. NULL until the first nextval (is_called).
            let last_value = match crate::sql::sequences::peek_last_served(&def.name) {
                Some(v) => Value::Int8(v),
                None => match catalog.get_sequence_state(&def.name)? {
                    Some(st) if st.is_called => Value::Int8(st.last_reserved),
                    _ => Value::Null, // never advanced yet (is_called == false)
                },
            };
            seen.insert(def.name.to_lowercase());
            rows.push(Tuple::new(vec![
                Value::String("public".into()),       // schemaname
                Value::String(def.name.clone()),      // sequencename
                Value::String("postgres".into()),     // sequenceowner
                Value::String(def.data_type.clone()), // data_type
                Value::Int8(def.start_value),         // start_value
                Value::Int8(def.min_value),           // min_value
                Value::Int8(def.max_value),           // max_value
                Value::Int8(def.increment_by),        // increment_by
                Value::Boolean(def.cycle),            // cycle
                Value::Int8(def.cache),               // cache_size
                last_value,                           // last_value
            ]));
        }

        // (2) SYNTHETIC owned sequences for SERIAL / IDENTITY columns, named
        // `<table>_<col>_seq`. Type/start/max reflect the column's real integer
        // type (next_row_id-backed, so last_value is NULL). Skip any name a real
        // sequence already owns.
        for table_name in catalog.list_tables()? {
            let schema = catalog.get_table_schema(&table_name).ok();
            for col_name in catalog.list_identity_columns(&table_name)? {
                let seq_name = format!("{table_name}_{col_name}_seq");
                if !seen.insert(seq_name.to_lowercase()) {
                    continue;
                }
                let (data_type, max_value) = schema
                    .as_ref()
                    .and_then(|s| s.columns.iter().find(|c| c.name.eq_ignore_ascii_case(&col_name)))
                    .map(|c| serial_type_bounds(&c.data_type))
                    .unwrap_or(("bigint", i64::MAX));
                rows.push(Tuple::new(vec![
                    Value::String("public".into()),     // schemaname
                    Value::String(seq_name),            // sequencename
                    Value::String("postgres".into()),   // sequenceowner
                    Value::String(data_type.into()),    // data_type
                    Value::Int8(1),                     // start_value
                    Value::Int8(1),                     // min_value
                    Value::Int8(max_value),             // max_value
                    Value::Int8(1),                     // increment_by
                    Value::Boolean(false),              // cycle
                    Value::Int8(1),                     // cache_size
                    Value::Null,                        // last_value (synthetic counter)
                ]));
            }
        }
        Ok(rows)
    }

    /// information_schema.sequences (SQL-standard, 12 cols). PG excludes
    /// SERIAL/IDENTITY-owned sequences here, so this lists REAL CREATE SEQUENCE
    /// definitions only. Numeric metadata + the value columns are decimal
    /// STRINGS per the standard.
    fn execute_information_schema_sequences(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        for def in catalog.list_sequences()? {
            let precision = match def.data_type.as_str() {
                "smallint" => 16,
                "integer" => 32,
                _ => 64, // bigint (default)
            };
            rows.push(Tuple::new(vec![
                Value::String("heliosdb".into()),              // sequence_catalog
                Value::String("public".into()),               // sequence_schema
                Value::String(def.name.clone()),              // sequence_name
                Value::String(def.data_type.clone()),         // data_type
                Value::Int4(precision),                       // numeric_precision
                Value::Int4(2),                               // numeric_precision_radix
                Value::Int4(0),                               // numeric_scale
                Value::String(def.start_value.to_string()),   // start_value
                Value::String(def.min_value.to_string()),     // minimum_value
                Value::String(def.max_value.to_string()),     // maximum_value
                Value::String(def.increment_by.to_string()),  // increment
                Value::String(if def.cycle { "YES" } else { "NO" }.into()), // cycle_option
            ]));
        }
        Ok(rows)
    }

    /// KanttBan #23 (v3.31.1 phase 2): synthesise pg_attrdef rows for
    /// every IDENTITY / SERIAL column so drizzle's EXISTS subquery
    /// (joins pg_attrdef ON ad.adrelid + ad.adnum, filters where
    /// pg_get_expr(ad.adbin, ad.adrelid) = 'nextval(''<seq>''::regclass)')
    /// resolves to TRUE for those columns. The literal `adsrc` string
    /// matches the format drizzle's CASE arm looks for.
    fn execute_pg_attrdef(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        let mut oid_counter: i32 = 7000;
        for (table_idx, table_name) in catalog.list_tables()?.iter().enumerate() {
            let adrelid = 1000_i32 + table_idx as i32;
            if let Ok(schema) = catalog.get_table_schema(table_name) {
                let identity_cols = catalog.list_identity_columns(table_name)?;
                for (col_idx, col) in schema.columns.iter().enumerate() {
                    // Emit a row for every column that HAS a default: an IDENTITY
                    // column (synthetic owned sequence) OR a column with an
                    // explicit `DEFAULT` expression (e.g. `DEFAULT nextval('seq')`,
                    // which is NOT an identity column). Previously only identity
                    // columns appeared, so pg_get_expr over an explicit-default
                    // column returned nothing.
                    let is_identity = identity_cols.iter().any(|c| c.eq_ignore_ascii_case(&col.name));
                    let adsrc = if is_identity {
                        format!("nextval('{table_name}_{}_seq'::regclass)", col.name)
                    } else if let Some(def) = &col.default_expr {
                        crate::sql::logical_plan::default_expr_json_to_sql(def).unwrap_or_else(|| def.clone())
                    } else {
                        continue;
                    };
                    rows.push(Tuple::new(vec![
                        Value::Int4(oid_counter),          // oid
                        Value::Int4(adrelid),              // adrelid
                        Value::Int2((col_idx + 1) as i16), // adnum (1-indexed)
                        Value::String(adsrc.clone()),      // adbin (node-tree text; same as adsrc for our purposes)
                        Value::String(adsrc),              // adsrc
                    ]));
                    oid_counter += 1;
                }
            }
        }
        Ok(rows)
    }

    /// pg_roles companion to pg_user — different shape (12 cols vs 9)
    /// but same two synthetic roles. drizzle-kit's introspection
    /// queries pg_roles directly during pull.
    fn execute_pg_roles() -> Result<Vec<Tuple>> {
        let role = |oid: i32, name: &str| {
            Tuple::new(vec![
                Value::Int4(oid),           // oid
                Value::String(name.into()), // rolname
                Value::Boolean(true),       // rolsuper
                Value::Boolean(true),       // rolinherit
                Value::Boolean(true),       // rolcreaterole
                Value::Boolean(true),       // rolcreatedb
                Value::Boolean(true),       // rolcanlogin
                Value::Boolean(true),       // rolreplication
                Value::Int4(-1),            // rolconnlimit
                Value::Null,                // rolpassword
                Value::Null,                // rolvaliduntil
                Value::Boolean(true),       // rolbypassrls
            ])
        };
        Ok(vec![role(10, "postgres"), role(11, "helios")])
    }

    /// KanttBan #23 phase 2.8: information_schema.table_constraints —
    /// PK + UNIQUE per table. Mirrors the legacy
    /// `query_information_schema_table_constraints` in
    /// protocol/postgres/catalog.rs.
    fn execute_information_schema_table_constraints(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        for name in catalog.list_tables()? {
            let mut emitted = HashSet::new();
            if let Ok(tschema) = catalog.get_table_schema(&name) {
                if tschema.columns.iter().any(|c| c.primary_key) {
                    let constraint_name = format!("{}_pkey", name);
                    emitted.insert(constraint_name.clone());
                    rows.push(Tuple::new(vec![
                        Value::String("heliosdb".into()),
                        Value::String("public".into()),
                        Value::String(constraint_name),
                        Value::String(name.clone()),
                        Value::String("PRIMARY KEY".into()),
                    ]));
                }
                for col in &tschema.columns {
                    if col.unique && !col.primary_key {
                        let constraint_name = format!("{}_{}_key", name, col.name);
                        emitted.insert(constraint_name.clone());
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(constraint_name),
                            Value::String(name.clone()),
                            Value::String("UNIQUE".into()),
                        ]));
                    }
                }
            }
            if let Ok(constraints) = catalog.load_table_constraints(&name) {
                for unique in constraints.unique_constraints {
                    if emitted.insert(unique.name.clone()) {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(unique.name),
                            Value::String(name.clone()),
                            Value::String(if unique.is_primary_key {
                                "PRIMARY KEY".into()
                            } else {
                                "UNIQUE".into()
                            }),
                        ]));
                    }
                }
                for fk in constraints.foreign_keys {
                    if emitted.insert(fk.name.clone()) {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(fk.name),
                            Value::String(name.clone()),
                            Value::String("FOREIGN KEY".into()),
                        ]));
                    }
                }
                for check in constraints.check_constraints {
                    if emitted.insert(check.name.clone()) {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(check.name),
                            Value::String(name.clone()),
                            Value::String("CHECK".into()),
                        ]));
                    }
                }
            }
        }
        Ok(rows)
    }

    /// KanttBan #23 phase 2.9: information_schema.constraint_column_usage.
    /// For PK/UNIQUE constraints, lists the same columns that form the
    /// constraint (same as KCU). For FK constraints, lists the
    /// REFERENCED (target) columns on the parent table. drizzle reads
    /// this to map FK definitions to their target table+column.
    fn execute_information_schema_constraint_column_usage(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        for name in catalog.list_tables()? {
            // PK + UNIQUE columns (same shape as KCU)
            if let Ok(tschema) = catalog.get_table_schema(&name) {
                for col in &tschema.columns {
                    if col.primary_key {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(name.clone()),
                            Value::String(col.name.clone()),
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_pkey", name)),
                        ]));
                    } else if col.unique {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(name.clone()),
                            Value::String(col.name.clone()),
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_{}_key", name, col.name)),
                        ]));
                    }
                }
            }
            // FK target columns (references_table + references_columns)
            if let Ok(constraints) = catalog.load_table_constraints(&name) {
                for fk in &constraints.foreign_keys {
                    for refcol in &fk.references_columns {
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(fk.references_table.clone()),
                            Value::String(refcol.clone()),
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(fk.name.clone()),
                        ]));
                    }
                }
            }
        }
        Ok(rows)
    }

    /// KanttBan #23 phase 2.8: information_schema.key_column_usage.
    fn execute_information_schema_key_column_usage(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        for name in catalog.list_tables()? {
            let mut emitted = HashSet::new();
            if let Ok(tschema) = catalog.get_table_schema(&name) {
                let mut pos: i32 = 1;
                for col in &tschema.columns {
                    if col.primary_key {
                        emitted.insert((format!("{}_pkey", name), col.name.clone()));
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_pkey", name)),
                            Value::String(name.clone()),
                            Value::String(col.name.clone()),
                            Value::Int4(pos),
                        ]));
                        pos += 1;
                    } else if col.unique {
                        emitted.insert((format!("{}_{}_key", name, col.name), col.name.clone()));
                        rows.push(Tuple::new(vec![
                            Value::String("heliosdb".into()),
                            Value::String("public".into()),
                            Value::String(format!("{}_{}_key", name, col.name)),
                            Value::String(name.clone()),
                            Value::String(col.name.clone()),
                            Value::Int4(1),
                        ]));
                    }
                }
            }
            if let Ok(constraints) = catalog.load_table_constraints(&name) {
                for unique in constraints.unique_constraints {
                    for (idx, col) in unique.columns.iter().enumerate() {
                        if emitted.insert((unique.name.clone(), col.clone())) {
                            rows.push(Tuple::new(vec![
                                Value::String("heliosdb".into()),
                                Value::String("public".into()),
                                Value::String(unique.name.clone()),
                                Value::String(name.clone()),
                                Value::String(col.clone()),
                                Value::Int4((idx + 1) as i32),
                            ]));
                        }
                    }
                }
                for fk in constraints.foreign_keys {
                    for (idx, col) in fk.columns.iter().enumerate() {
                        if emitted.insert((fk.name.clone(), col.clone())) {
                            rows.push(Tuple::new(vec![
                                Value::String("heliosdb".into()),
                                Value::String("public".into()),
                                Value::String(fk.name.clone()),
                                Value::String(name.clone()),
                                Value::String(col.clone()),
                                Value::Int4((idx + 1) as i32),
                            ]));
                        }
                    }
                }
            }
        }
        Ok(rows)
    }

    /// KanttBan #23 phase 2.8: information_schema.referential_constraints
    /// — FK ON UPDATE / ON DELETE rules per constraint.
    fn execute_information_schema_referential_constraints(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let mut rows = Vec::new();
        for table in catalog.list_tables()? {
            let constraints = match catalog.load_table_constraints(&table) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for fk in &constraints.foreign_keys {
                rows.push(Tuple::new(vec![
                    Value::String("heliosdb".into()),
                    Value::String("public".into()),
                    Value::String(fk.name.clone()),
                    Value::String("heliosdb".into()),
                    Value::String("public".into()),
                    Value::String(format!("{}_pkey", fk.references_table)),
                    Value::String("NONE".into()),
                    Value::String(fk.on_update.to_string()),
                    Value::String(fk.on_delete.to_string()),
                ]));
            }
        }
        Ok(rows)
    }

    /// KanttBan #22 (v3.31.0): information_schema.tables backed by
    /// the storage catalogue. Mirrors the legacy
    /// `protocol/postgres/catalog.rs::query_information_schema_tables`
    /// (sans the substring LIKE filter — the planner handles WHERE
    /// LIKE natively now).
    fn execute_information_schema_tables(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let table_names = storage.catalog().list_tables()?;
        let mut rows = Vec::with_capacity(table_names.len());
        for name in &table_names {
            rows.push(Tuple::new(vec![
                Value::String("heliosdb".into()),
                Value::String("public".into()),
                Value::String(name.clone()),
                Value::String("BASE TABLE".into()),
            ]));
        }
        Ok(rows)
    }

    /// KanttBan #22 (v3.31.0): pg_database minimal stub. Only the
    /// implicit `heliosdb` system database is enumerated — surfacing
    /// tenant databases registered via `CREATE DATABASE` needs
    /// `EmbeddedDatabase::tenant_manager` access, which the registry's
    /// `execute(&StorageEngine)` signature doesn't carry. Deferred to
    /// a follow-up that widens the executor context. `\l` already
    /// renders only `heliosdb` (via try_psql_metacommand's own
    /// matcher), so there's no current-behaviour regression.
    fn execute_pg_database() -> Result<Vec<Tuple>> {
        Ok(vec![Tuple::new(vec![
            Value::Int4(1),                   // oid
            Value::String("heliosdb".into()), // datname
            Value::Int4(10),                  // datdba
            Value::Int4(6),                   // encoding = UTF8
            Value::String("C.UTF-8".into()),  // datcollate
            Value::String("C.UTF-8".into()),  // datctype
            Value::Boolean(false),            // datistemplate
            Value::Boolean(true),             // datallowconn
            Value::Int4(-1),                  // datconnlimit
            Value::Int4(1663),                // dattablespace = pg_default
        ])])
    }

    /// KanttBan #22 (v3.31.0): pg_user as a read-only stub. Mirrors
    /// the two hard-coded roles the legacy substring router exposed
    /// via query_pg_roles in `protocol/postgres/catalog.rs`.
    /// usesysid is the value drivers JOIN to nspowner / relowner /
    /// proowner; keep it stable at 10 (postgres) and 11 (helios) so
    /// existing introspection sees the schemas / tables as owned by
    /// the postgres super-user.
    fn execute_pg_user() -> Result<Vec<Tuple>> {
        let role = |name: &str, uid: i32| {
            Tuple::new(vec![
                Value::String(name.into()), // usename
                Value::Int4(uid),           // usesysid
                Value::Boolean(true),       // usecreatedb
                Value::Boolean(true),       // usesuper
                Value::Boolean(true),       // userepl
                Value::Boolean(true),       // usebypassrls
                Value::Null,                // passwd
                Value::Null,                // valuntil
                Value::Null,                // useconfig
            ])
        };
        Ok(vec![role("postgres", 10), role("helios", 11)])
    }

    fn execute_pg_namespace(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        use std::collections::BTreeSet;
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.insert("public".to_string());
        all.insert("information_schema".to_string());
        if let Ok(catalog_schemas) = storage.catalog().list_schemas() {
            for s in catalog_schemas {
                all.insert(s);
            }
        }

        let mut results = Vec::new();
        let mut next_oid = 2200i32;
        for nspname in all {
            let oid = match nspname.as_str() {
                "public" => 2200,
                "information_schema" => 11,
                _ => {
                    next_oid += 1;
                    next_oid
                }
            };
            results.push(Tuple::new(vec![
                Value::Int4(oid),
                Value::String(nspname),
                Value::Int4(10),
            ]));
        }
        Ok(results)
    }

    /// Execute pg_index() system view
    ///
    /// Returns information about all indexes
    fn execute_pg_index(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for (idx, (_index_name, table_name, index_type, columns)) in sorted_art_indexes(storage).iter().enumerate() {
            let Some(table_oid) = pg_table_oid_by_name(&tables, table_name) else {
                continue;
            };
            let schema = match catalog.get_table_schema(table_name) {
                Ok(schema) => schema,
                Err(_) => continue,
            };
            let indkey = pg_indkey(&schema, columns);
            let column_count = columns.len() as i16;
            results.push(Tuple::new(vec![
                Value::Int4(pg_index_oid(idx)),                          // indexrelid
                Value::Int4(table_oid),                                  // indrelid
                Value::Boolean(*index_type == ArtIndexType::PrimaryKey), // indisprimary
                Value::Boolean(matches!(*index_type, ArtIndexType::PrimaryKey | ArtIndexType::Unique)), // indisunique
                Value::Boolean(false),                                   // indisexclusion
                Value::String(indkey),                                   // indkey (space-separated attnums)
                Value::Int2(column_count),                               // indnatts
                Value::Int2(column_count),                               // indnkeyatts
                Value::Boolean(false),                                   // indisclustered
                Value::Boolean(true),                                    // indisvalid
                Value::Boolean(true),                                    // indisready
                Value::Boolean(false),                                   // indisreplident
                Value::Null,                                             // indexprs
                Value::Null,                                             // indpred
            ]));
        }

        Ok(results)
    }

    /// Execute pg_constraint() system view
    ///
    /// Returns information about all constraints
    fn execute_pg_constraint(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let indexes = sorted_art_indexes(storage);
        let mut results = Vec::new();
        let mut constraint_oid = PG_CONSTRAINT_OID_BASE;
        let mut seen = HashSet::new();

        for (table_idx, table_name) in tables.iter().enumerate() {
            let table_oid = pg_table_oid(table_idx);
            let schema = match catalog.get_table_schema(table_name) {
                Ok(schema) => schema,
                Err(_) => continue,
            };

            let pk_columns: Vec<String> = schema
                .columns
                .iter()
                .filter(|col| col.primary_key)
                .map(|col| col.name.clone())
                .collect();
            if !pk_columns.is_empty() {
                let constraint_name = format!("{}_pkey", table_name);
                push_pg_constraint_row(
                    &mut results,
                    &mut constraint_oid,
                    &mut seen,
                    constraint_name.clone(),
                    "p",
                    table_oid,
                    None,
                    pg_index_oid_by_name(&indexes, &constraint_name),
                    pg_conkey(&schema, &pk_columns),
                    Value::Null,
                    None,
                    None,
                    false,
                    false,
                    true,
                );
            }

            for col in &schema.columns {
                if col.unique && !col.primary_key {
                    let constraint_name = format!("{}_{}_key", table_name, col.name);
                    push_pg_constraint_row(
                        &mut results,
                        &mut constraint_oid,
                        &mut seen,
                        constraint_name.clone(),
                        "u",
                        table_oid,
                        None,
                        pg_index_oid_by_name(&indexes, &constraint_name),
                        pg_conkey(&schema, std::slice::from_ref(&col.name)),
                        Value::Null,
                        None,
                        None,
                        false,
                        false,
                        true,
                    );
                }
            }

            let constraints = match catalog.load_table_constraints(table_name) {
                Ok(constraints) => constraints,
                Err(_) => continue,
            };

            for unique in constraints.unique_constraints {
                push_pg_constraint_row(
                    &mut results,
                    &mut constraint_oid,
                    &mut seen,
                    unique.name.clone(),
                    if unique.is_primary_key { "p" } else { "u" },
                    table_oid,
                    None,
                    pg_index_oid_by_name(&indexes, &unique.name),
                    pg_conkey(&schema, &unique.columns),
                    Value::Null,
                    None,
                    None,
                    false,
                    false,
                    true,
                );
            }

            for check in constraints.check_constraints {
                push_pg_constraint_row(
                    &mut results,
                    &mut constraint_oid,
                    &mut seen,
                    check.name,
                    "c",
                    table_oid,
                    None,
                    0,
                    Value::Null,
                    Value::Null,
                    None,
                    None,
                    false,
                    false,
                    true,
                );
            }

            for fk in constraints.foreign_keys {
                let ref_oid = pg_table_oid_by_name(&tables, &fk.references_table);
                let ref_key = match catalog.get_table_schema(&fk.references_table) {
                    Ok(ref_schema) => pg_conkey(&ref_schema, &fk.references_columns),
                    Err(_) => Value::Null,
                };
                push_pg_constraint_row(
                    &mut results,
                    &mut constraint_oid,
                    &mut seen,
                    fk.name.clone(),
                    "f",
                    table_oid,
                    ref_oid,
                    0,
                    pg_conkey(&schema, &fk.columns),
                    ref_key,
                    Some(fk.on_update),
                    Some(fk.on_delete),
                    fk.deferrable,
                    fk.initially_deferred,
                    !matches!(fk.enforcement, crate::sql::ConstraintEnforcement::NotEnforced),
                );
            }
        }

        Ok(results)
    }

    /// Execute information_schema.columns() system view
    ///
    /// Returns ANSI SQL standard view of all table columns
    fn execute_information_schema_columns(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        for table_name in tables.iter() {
            let identity_cols = catalog.list_identity_columns(table_name).unwrap_or_default();
            match catalog.get_table_schema(table_name) {
                Ok(schema) => {
                    for (col_idx, column) in schema.columns.iter().enumerate() {
                        let is_nullable = if column.nullable { "YES" } else { "NO" };
                        let data_type = format!("{:?}", column.data_type);
                        let pg_type_name = format_pg_type_name(&column.data_type);
                        let is_identity_col = identity_cols.iter().any(|c| c.eq_ignore_ascii_case(&column.name));
                        // KanttBan #23 (v3.31.1 phase 2): identity
                        // metadata. We always report BY DEFAULT — we
                        // don't track GENERATED ALWAYS separately
                        // (SERIAL maps to BY DEFAULT, IDENTITY can be
                        // either; defaulting to the user-friendly
                        // form). identity_start / identity_increment
                        // / identity_maximum / identity_minimum
                        // match the bigint defaults (1 / 1 / max /
                        // 1). identity_cycle = NO.
                        let (is_identity_str, id_gen, id_start, id_incr, id_max, id_min, id_cycle) = if is_identity_col
                        {
                            (
                                Value::String("YES".into()),
                                Value::String("BY DEFAULT".into()),
                                Value::String("1".into()),
                                Value::String("1".into()),
                                Value::String(i64::MAX.to_string()),
                                Value::String("1".into()),
                                Value::String("NO".into()),
                            )
                        } else {
                            (
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            )
                        };
                        let column_default = if is_identity_col {
                            Value::String(format!("nextval('{table_name}_{}_seq'::regclass)", column.name,))
                        } else {
                            column
                                .default_expr
                                .as_ref()
                                .map(|s| {
                                    Value::String(
                                        crate::sql::logical_plan::default_expr_json_to_sql(s)
                                            .unwrap_or_else(|| s.clone()),
                                    )
                                })
                                .unwrap_or(Value::Null)
                        };

                        let tuple = Tuple::new(vec![
                            Value::String("public".to_string()),    // table_schema
                            Value::String(table_name.clone()),      // table_name
                            Value::String(column.name.clone()),     // column_name
                            Value::Int4((col_idx + 1) as i32),      // ordinal_position
                            column_default,                         // column_default
                            Value::String(is_nullable.to_string()), // is_nullable
                            Value::String(data_type),               // data_type
                            Value::String(pg_type_name),            // udt_name
                            Value::String("NEVER".into()),          // is_generated
                            Value::Null,                            // generation_expression
                            is_identity_str,                        // is_identity
                            id_gen,                                 // identity_generation
                            id_start,                               // identity_start
                            id_incr,                                // identity_increment
                            id_max,                                 // identity_maximum
                            id_min,                                 // identity_minimum
                            id_cycle,                               // identity_cycle
                        ]);
                        results.push(tuple);
                    }
                }
                Err(_) => {
                    // Skip tables we can't read schema for
                    continue;
                }
            }
        }

        Ok(results)
    }

    /// Helper function to get PostgreSQL type OID for HeliosDB data types
    fn get_type_oid(data_type: &DataType) -> i32 {
        match data_type {
            DataType::Boolean => 16,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Int8 => 20,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Numeric => 1700,
            DataType::Varchar(_) => 1043,
            DataType::Text => 25,
            DataType::Char(_) => 1042,
            DataType::Bytea => 17,
            DataType::Date => 1082,
            DataType::Time => 1083,
            DataType::Timestamp => 1114,
            DataType::Timestamptz => 1184,
            DataType::Interval => 1186,
            DataType::Uuid => 2950,
            DataType::Json => 114,
            DataType::Jsonb => 3802,
            DataType::Array(_) => 2277,
            DataType::Vector(_) => 3614,
        }
    }

    // === Compression Monitoring View Executors ===

    /// Execute heliosdb_compression_stats view
    fn execute_heliosdb_compression_stats(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();

        // Aggregate stats by codec
        let mut alp_stats = (0i64, 0i64, 0i64, 0.0f64); // (uses, bytes_in, bytes_out, total_ratio)
        let mut fsst_stats = (0i64, 0i64, 0i64, 0.0f64);
        let mut none_stats = (0i64, 0i64, 0i64, 0.0f64);

        for table_name in &tables {
            if let Some(stats) = catalog.get_compression_stats(table_name)? {
                for col_stats in stats.column_stats.values() {
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
                Value::Float8(if alp_stats.2 > 0 {
                    alp_stats.1 as f64 / alp_stats.2 as f64
                } else {
                    1.0
                }),
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
                Value::Float8(if fsst_stats.2 > 0 {
                    fsst_stats.1 as f64 / fsst_stats.2 as f64
                } else {
                    1.0
                }),
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

    /// Execute heliosdb_pattern_stats view
    fn execute_heliosdb_pattern_stats(_storage: &StorageEngine) -> Result<Vec<Tuple>> {
        // Pattern detection statistics - returns predefined patterns based on data type affinity
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

    /// Execute heliosdb_compression_events view
    fn execute_heliosdb_compression_events(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();
        let now = chrono::Utc::now();

        for table_name in &tables {
            if table_name.starts_with("helios_") || table_name.starts_with("mv_") {
                continue;
            }

            if let Some(stats) = catalog.get_compression_stats(table_name)? {
                for (col_name, col_stats) in &stats.column_stats {
                    let tuple = Tuple::new(vec![
                        Value::Timestamp(now),
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

    /// Execute heliosdb_config view
    fn execute_heliosdb_config(storage: &StorageEngine) -> Result<Vec<Tuple>> {
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

    // ========== HA Replication System View Execution ==========

    /// Execute pg_replication_status() system view
    ///
    /// Returns current node's HA replication status and configuration
    #[cfg(feature = "ha-tier1")]
    fn execute_pg_replication_status() -> Result<Vec<Tuple>> {
        use crate::replication::ha_state;
        use chrono::{TimeZone, Utc};

        let state = ha_state();
        let config = state.get_config().unwrap_or_default();
        let lsn = state.get_lsn();
        let standby_count = state.standby_count();
        let is_read_only = state.is_read_only();

        let started_at = Utc
            .timestamp_opt(config.started_at, 0)
            .single()
            .map(|dt| Value::Timestamp(dt))
            .unwrap_or(Value::Null);

        Ok(vec![Tuple::new(vec![
            Value::String(config.node_id.to_string()),
            Value::String(config.role.as_str().to_string()),
            Value::String(config.sync_mode.as_str().to_string()),
            Value::Boolean(is_read_only),
            Value::Int8(lsn as i64),
            Value::Int4(standby_count as i32),
            config.primary_host.map(Value::String).unwrap_or(Value::Null),
            Value::String(format!("{}:{}", config.listen_addr, config.port)),
            Value::Int4(config.replication_port as i32),
            started_at,
        ])])
    }

    #[cfg(not(feature = "ha-tier1"))]
    fn execute_pg_replication_status() -> Result<Vec<Tuple>> {
        Ok(vec![Tuple::new(vec![
            Value::String("N/A".to_string()),
            Value::String("standalone".to_string()),
            Value::String("N/A".to_string()),
            Value::Boolean(false),
            Value::Int8(0),
            Value::Int4(0),
            Value::Null,
            Value::String("N/A".to_string()),
            Value::Int4(0),
            Value::Null,
        ])])
    }

    /// Execute pg_replication_standbys() system view
    ///
    /// Returns connected standby nodes (run on primary)
    #[cfg(feature = "ha-tier1")]
    fn execute_pg_replication_standbys() -> Result<Vec<Tuple>> {
        use crate::replication::ha_state;
        use chrono::{TimeZone, Utc};

        let state = ha_state();
        let standbys = state.get_standbys();

        let mut results = Vec::new();
        for standby in standbys {
            let connected_at = Utc
                .timestamp_opt(standby.connected_at, 0)
                .single()
                .map(|dt| Value::Timestamp(dt))
                .unwrap_or(Value::Null);
            let last_heartbeat = Utc
                .timestamp_opt(standby.last_heartbeat, 0)
                .single()
                .map(|dt| Value::Timestamp(dt))
                .unwrap_or(Value::Null);

            results.push(Tuple::new(vec![
                Value::String(standby.node_id.to_string()),
                Value::String(standby.address.clone()),
                Value::String(standby.state.as_str().to_string()),
                Value::String(standby.sync_mode.as_str().to_string()),
                Value::Int8(standby.current_lsn as i64),
                Value::Int8(standby.flush_lsn as i64),
                Value::Int8(standby.apply_lsn as i64),
                Value::Int8(standby.lag_bytes as i64),
                Value::Int8(standby.lag_ms as i64),
                connected_at,
                last_heartbeat,
            ]));
        }

        Ok(results)
    }

    #[cfg(not(feature = "ha-tier1"))]
    fn execute_pg_replication_standbys() -> Result<Vec<Tuple>> {
        Ok(vec![])
    }

    /// Execute pg_replication_primary() system view
    ///
    /// Returns primary connection status (run on standby)
    #[cfg(feature = "ha-tier1")]
    fn execute_pg_replication_primary() -> Result<Vec<Tuple>> {
        use crate::replication::ha_state;
        use chrono::{TimeZone, Utc};

        let state = ha_state();

        if let Some(primary) = state.get_primary() {
            let connected_at = Utc
                .timestamp_opt(primary.connected_at, 0)
                .single()
                .map(|dt| Value::Timestamp(dt))
                .unwrap_or(Value::Null);
            let last_heartbeat = Utc
                .timestamp_opt(primary.last_heartbeat, 0)
                .single()
                .map(|dt| Value::Timestamp(dt))
                .unwrap_or(Value::Null);

            Ok(vec![Tuple::new(vec![
                Value::String(primary.node_id.to_string()),
                Value::String(primary.address.clone()),
                Value::String(primary.state.as_str().to_string()),
                Value::Int8(primary.primary_lsn as i64),
                Value::Int8(primary.local_lsn as i64),
                Value::Int8(primary.lag_bytes as i64),
                Value::Int8(primary.lag_ms as i64),
                Value::Int8(primary.fencing_token as i64),
                connected_at,
                last_heartbeat,
            ])])
        } else {
            // No primary connection - return a row indicating disconnected state
            Ok(vec![Tuple::new(vec![
                Value::Null,
                Value::Null,
                Value::String("disconnected".to_string()),
                Value::Int8(0),
                Value::Int8(state.get_lsn() as i64),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Null,
                Value::Null,
            ])])
        }
    }

    #[cfg(not(feature = "ha-tier1"))]
    fn execute_pg_replication_primary() -> Result<Vec<Tuple>> {
        Ok(vec![Tuple::new(vec![
            Value::Null,
            Value::Null,
            Value::String("N/A".to_string()),
            Value::Int8(0),
            Value::Int8(0),
            Value::Int8(0),
            Value::Int8(0),
            Value::Int8(0),
            Value::Null,
            Value::Null,
        ])])
    }

    /// Execute pg_replication_metrics() system view
    ///
    /// Returns replication performance metrics
    #[cfg(feature = "ha-tier1")]
    fn execute_pg_replication_metrics() -> Result<Vec<Tuple>> {
        use crate::replication::ha_state;

        let state = ha_state();
        let metrics = state.get_metrics();

        let mut results = Vec::new();

        // Add each metric as a row
        let metrics_data = vec![
            ("wal_writes", metrics.wal_writes, "Total WAL write operations"),
            (
                "wal_bytes_written",
                metrics.wal_bytes_written,
                "Total bytes written to WAL",
            ),
            (
                "records_replicated",
                metrics.records_replicated,
                "Total records replicated to standbys",
            ),
            (
                "bytes_replicated",
                metrics.bytes_replicated,
                "Total bytes replicated to standbys",
            ),
            ("heartbeats_sent", metrics.heartbeats_sent, "Total heartbeats sent"),
            (
                "heartbeats_received",
                metrics.heartbeats_received,
                "Total heartbeats received",
            ),
            (
                "reconnect_count",
                metrics.reconnect_count,
                "Number of reconnection attempts",
            ),
            ("current_lsn", state.get_lsn(), "Current Log Sequence Number"),
            (
                "standby_count",
                state.standby_count() as u64,
                "Number of connected standbys",
            ),
        ];

        for (name, value, description) in metrics_data {
            results.push(Tuple::new(vec![
                Value::String(name.to_string()),
                Value::Int8(value as i64),
                Value::String(description.to_string()),
            ]));
        }

        // Add timestamp metrics
        if let Some(last_write) = metrics.last_wal_write {
            results.push(Tuple::new(vec![
                Value::String("last_wal_write_epoch".to_string()),
                Value::Int8(last_write),
                Value::String("Unix timestamp of last WAL write".to_string()),
            ]));
        }

        if let Some(last_repl) = metrics.last_replication {
            results.push(Tuple::new(vec![
                Value::String("last_replication_epoch".to_string()),
                Value::Int8(last_repl),
                Value::String("Unix timestamp of last replication".to_string()),
            ]));
        }

        Ok(results)
    }

    #[cfg(not(feature = "ha-tier1"))]
    fn execute_pg_replication_metrics() -> Result<Vec<Tuple>> {
        Ok(vec![Tuple::new(vec![
            Value::String("ha_enabled".to_string()),
            Value::Int8(0),
            Value::String("HA feature not enabled".to_string()),
        ])])
    }

    /// Execute heliosdb_art_indexes view
    ///
    /// Returns information about all ART indexes
    fn execute_heliosdb_art_indexes(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let art_manager = storage.art_indexes();
        let indexes = art_manager.list_indexes();
        let mut results = Vec::new();

        for (name, table, index_type, columns) in indexes {
            // Get stats without cloning the entire tree
            if let Some(stats) = art_manager.index_stats(&name) {
                let columns_str = columns.join(", ");
                let node_count = stats.node4_count + stats.node16_count + stats.node48_count + stats.node256_count;

                results.push(Tuple::new(vec![
                    Value::String(name),
                    Value::String(table),
                    Value::String(columns_str),
                    Value::String(index_type.to_string()),
                    Value::Int8(stats.key_count as i64),
                    Value::Int8(stats.memory_bytes as i64),
                    Value::Int8(node_count as i64),
                    Value::Int8(stats.lookup_count as i64),
                ]));
            }
        }

        Ok(results)
    }

    /// Execute heliosdb_simd_capabilities view
    ///
    /// Returns information about CPU SIMD capabilities
    fn execute_heliosdb_simd_capabilities() -> Result<Vec<Tuple>> {
        use crate::storage::simd_filter::simd_capabilities;
        let caps = simd_capabilities();
        let mut results = Vec::new();

        // AVX-512
        results.push(Tuple::new(vec![
            Value::String("AVX-512".to_string()),
            Value::Boolean(caps.avx512f),
            Value::Int4(if caps.avx512f { 16 } else { 0 }),
            Value::String("512-bit SIMD (16 x i32/f32)".to_string()),
        ]));

        // AVX2
        results.push(Tuple::new(vec![
            Value::String("AVX2".to_string()),
            Value::Boolean(caps.avx2),
            Value::Int4(if caps.avx2 { 8 } else { 0 }),
            Value::String("256-bit SIMD (8 x i32/f32)".to_string()),
        ]));

        // SSE4.1
        results.push(Tuple::new(vec![
            Value::String("SSE4.1".to_string()),
            Value::Boolean(caps.sse41),
            Value::Int4(if caps.sse41 { 4 } else { 0 }),
            Value::String("128-bit SIMD (4 x i32/f32)".to_string()),
        ]));

        // Best available
        let best_level = caps.best_level();
        results.push(Tuple::new(vec![
            Value::String("BEST_AVAILABLE".to_string()),
            Value::Boolean(true),
            Value::Int4(best_level.i32_width() as i32),
            Value::String(caps.description()),
        ]));

        Ok(results)
    }

    /// Execute heliosdb_row_cache_stats view
    ///
    /// Returns row cache statistics
    fn execute_heliosdb_row_cache_stats(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let row_cache = storage.row_cache();
        let stats = row_cache.stats();
        let mut results = Vec::new();

        results.push(Tuple::new(vec![
            Value::String("lookups".to_string()),
            Value::Int8(stats.lookups as i64),
            Value::String("Total cache lookups".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("hits".to_string()),
            Value::Int8(stats.hits as i64),
            Value::String("Cache hits (found and not expired)".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("misses".to_string()),
            Value::Int8(stats.misses as i64),
            Value::String("Cache misses".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("expirations".to_string()),
            Value::Int8(stats.expirations as i64),
            Value::String("Expired entries encountered".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("evictions".to_string()),
            Value::Int8(stats.evictions as i64),
            Value::String("Entries evicted due to capacity".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("inserts".to_string()),
            Value::Int8(stats.inserts as i64),
            Value::String("Total entries inserted".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("current_entries".to_string()),
            Value::Int8(stats.current_entries as i64),
            Value::String("Current entry count".to_string()),
        ]));

        results.push(Tuple::new(vec![
            Value::String("hit_rate_pct".to_string()),
            Value::Int8((stats.hit_rate() * 100.0) as i64),
            Value::String("Cache hit rate percentage".to_string()),
        ]));

        Ok(results)
    }

    /// Execute heliosdb_lock_census view (W3.1 plateau attribution).
    ///
    /// One row per instrumented read-hot-path lock site: acquisitions,
    /// try-lock-contended count, and cumulative contended-wait nanos. Always
    /// empty unless the binary was built with the `lock-census` feature and
    /// `[performance] lock_census` is enabled — the counters are process-global
    /// (see `crate::lock_census`).
    fn execute_heliosdb_lock_census() -> Result<Vec<Tuple>> {
        let results = crate::lock_census::snapshot()
            .into_iter()
            .map(|site| {
                Tuple::new(vec![
                    Value::String(site.name.to_string()),
                    Value::Int8(site.acquisitions as i64),
                    Value::Int8(site.contended as i64),
                    Value::Int8(site.contended_wait_nanos as i64),
                ])
            })
            .collect();
        Ok(results)
    }

    /// Execute heliosdb_write_volume view (W3.2 version-format quantification).
    ///
    /// One row per statement class: bytes written to `data:`, to the
    /// `v:`/`v_idx:` version chain, and to secondary-index keys, plus a written-
    /// row count. All zero unless `[performance] write_volume_stats` is enabled;
    /// the counters are process-global (see `crate::write_volume`).
    fn execute_heliosdb_write_volume() -> Result<Vec<Tuple>> {
        let results = crate::write_volume::snapshot()
            .into_iter()
            .map(|stat| {
                Tuple::new(vec![
                    Value::String(stat.class.to_string()),
                    Value::Int8(stat.data_bytes as i64),
                    Value::Int8(stat.version_bytes as i64),
                    Value::Int8(stat.index_key_bytes as i64),
                    Value::Int8(stat.rows as i64),
                ])
            })
            .collect();
        Ok(results)
    }

    /// Execute heliosdb_copy_phase_stats view (W3.4 ART-maintenance attribution).
    ///
    /// One row per COPY fast-path funnel phase: cumulative wall nanos, a call
    /// count (COPY batches through the phase), and a processed-row count. All
    /// zero unless `[performance] copy_phase_stats` is enabled; the counters are
    /// process-global (see `crate::copy_phase_stats`).
    fn execute_heliosdb_copy_phase_stats() -> Result<Vec<Tuple>> {
        let results = crate::copy_phase_stats::snapshot()
            .into_iter()
            .map(|stat| {
                Tuple::new(vec![
                    Value::String(stat.phase.to_string()),
                    Value::Int8(stat.total_nanos as i64),
                    Value::Int8(stat.calls as i64),
                    Value::Int8(stat.rows as i64),
                ])
            })
            .collect();
        Ok(results)
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
        assert!(registry.is_system_view("pg_database_branches"));
        assert!(registry.is_system_view("pg_mv_staleness"));
        assert!(registry.is_system_view("pg_vector_index_stats"));
        assert!(!registry.is_system_view("nonexistent_view"));
    }

    #[test]
    fn test_get_schema() {
        let registry = SystemViewRegistry::new();
        let schema = registry.get_schema("pg_database_branches").unwrap();
        assert_eq!(schema.columns.len(), 7);
        assert_eq!(schema.columns[0].name, "branch_name");
    }

    #[test]
    fn test_list_views() {
        let registry = SystemViewRegistry::new();
        let views = registry.list_views();
        assert!(views.len() >= 3);
        assert!(views.contains(&"pg_database_branches"));
    }

    #[test]
    fn test_execute_pg_database_branches() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to create storage");

        // Create a test branch
        storage
            .create_branch("test_branch", Some("main"), crate::storage::BranchOptions::default())
            .expect("Failed to create branch");

        let registry = SystemViewRegistry::new();
        let results = registry
            .execute("pg_database_branches", &storage)
            .expect("Failed to execute pg_database_branches");

        // Should have at least 2 branches (main + test_branch)
        assert!(results.len() >= 2);

        // Verify first result has correct number of columns
        assert_eq!(results[0].values.len(), 7);
    }

    #[test]
    fn test_execute_pg_mv_staleness() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to create storage");

        let registry = SystemViewRegistry::new();
        let results = registry
            .execute("pg_mv_staleness", &storage)
            .expect("Failed to execute pg_mv_staleness");

        // Should return empty results if no materialized views exist
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_execute_pg_vector_index_stats() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to create storage");

        let registry = SystemViewRegistry::new();
        let results = registry
            .execute("pg_vector_index_stats", &storage)
            .expect("Failed to execute pg_vector_index_stats");

        // Should return empty results if no vector indexes exist
        assert_eq!(results.len(), 0);
    }
}
