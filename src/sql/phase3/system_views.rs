//! System Views Registry
//!
//! Provides schemas and execution for Phase 3 system views:
//! - pg_database_branches() - List all database branches
//! - pg_mv_staleness() - Materialized view staleness info
//! - pg_mv_cpu_usage() - MV refresh CPU usage
//! - pg_vector_index_stats() - Vector index statistics
//! - pg_current_scn() - Current System Change Number
//! - pg_compare_branches() - Compare two branches

use crate::{Result, Error, Schema, Column, DataType, Value, Tuple, ColumnStorageMode};
use crate::storage::StorageEngine;
use std::collections::HashMap;

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
        let mut registry = Self {
            views: HashMap::new(),
        };

        registry.register_phase3_views();
        registry
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
            schema: Schema { columns: vec![
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
            ] },
            description: "PG-compat sequences view (empty — Nano uses synthetic counters)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_proc".to_string(),
            schema: Schema { columns: vec![
                sv_col("oid", DataType::Int4),
                sv_col("proname", DataType::Text),
                sv_col("pronamespace", DataType::Int4),
                sv_col("proowner", DataType::Int4),
                sv_col("prolang", DataType::Int4),
                sv_col("prorettype", DataType::Int4),
                sv_col("proargtypes", DataType::Text),
                sv_col("prosrc", DataType::Text),
            ] },
            description: "PG-compat procedures catalogue (empty stub)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_description".to_string(),
            schema: Schema { columns: vec![
                sv_col("objoid", DataType::Int4),
                sv_col("classoid", DataType::Int4),
                sv_col("objsubid", DataType::Int4),
                sv_col("description", DataType::Text),
            ] },
            description: "PG-compat object descriptions (empty — Nano doesn't store COMMENT ON)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_policies".to_string(),
            schema: Schema { columns: vec![
                sv_col("schemaname", DataType::Text),
                sv_col("tablename", DataType::Text),
                sv_col("policyname", DataType::Text),
                sv_col("permissive", DataType::Text),
                sv_col("roles", DataType::Text),
                sv_col("cmd", DataType::Text),
                sv_col("qual", DataType::Text),
                sv_col("with_check", DataType::Text),
            ] },
            description: "PG-compat RLS policies view (empty — Nano RLS via TenantManager)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_policy".to_string(),
            schema: Schema { columns: vec![
                sv_col("oid", DataType::Int4),
                sv_col("polname", DataType::Text),
                sv_col("polrelid", DataType::Int4),
                sv_col("polcmd", DataType::Char(1)),
                sv_col("polpermissive", DataType::Boolean),
                sv_col("polroles", DataType::Text),
                sv_col("polqual", DataType::Text),
                sv_col("polwithcheck", DataType::Text),
            ] },
            description: "PG-compat RLS policy catalogue (empty stub)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_matviews".to_string(),
            schema: Schema { columns: vec![
                sv_col("schemaname", DataType::Text),
                sv_col("matviewname", DataType::Text),
                sv_col("matviewowner", DataType::Text),
                sv_col("tablespace", DataType::Text),
                sv_col("hasindexes", DataType::Boolean),
                sv_col("ispopulated", DataType::Boolean),
                sv_col("definition", DataType::Text),
            ] },
            description: "PG-compat matview view (empty — use pg_mv_staleness instead)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_inherits".to_string(),
            schema: Schema { columns: vec![
                sv_col("inhrelid", DataType::Int4),
                sv_col("inhparent", DataType::Int4),
                sv_col("inhseqno", DataType::Int4),
            ] },
            description: "PG-compat inheritance catalogue (empty — Nano has no inheritance)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_publication".to_string(),
            schema: Schema { columns: vec![
                sv_col("oid", DataType::Int4),
                sv_col("pubname", DataType::Text),
                sv_col("pubowner", DataType::Int4),
                sv_col("puballtables", DataType::Boolean),
                sv_col("pubinsert", DataType::Boolean),
                sv_col("pubupdate", DataType::Boolean),
                sv_col("pubdelete", DataType::Boolean),
                sv_col("pubtruncate", DataType::Boolean),
            ] },
            description: "PG-compat logical replication publications (empty stub)".to_string(),
        });

        self.register_view(SystemViewSchema {
            name: "pg_statistic_ext".to_string(),
            schema: Schema { columns: vec![
                sv_col("oid", DataType::Int4),
                sv_col("stxrelid", DataType::Int4),
                sv_col("stxnamespace", DataType::Int4),
                sv_col("stxname", DataType::Text),
                sv_col("stxkeys", DataType::Text),
                sv_col("stxkind", DataType::Text),
                sv_col("stxstattarget", DataType::Int4),
            ] },
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
            schema: Schema { columns: vec![
                sv_col("oid", DataType::Int4),
                sv_col("adrelid", DataType::Int4),
                sv_col("adnum", DataType::Int2),
                sv_col("adbin", DataType::Text),
                sv_col("adsrc", DataType::Text),
            ] },
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
            description: "Lists every tree-sitter grammar the indexer can parse"
                .to_string(),
        });
    }

    /// Register a system view
    fn register_view(&mut self, view: SystemViewSchema) {
        self.views.insert(view.name.clone(), view);
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
            return Err(Error::query_execution(format!(
                "Unknown system view: {}",
                view_name
            )));
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
            "pg_database" => Self::execute_pg_database(),
            // Empty-stub catalogue tables (v3.31.0 slice 5). Schema
            // already registered; rows are always empty because Nano
            // doesn't model these concepts.
            "pg_sequences"
            | "pg_proc"
            | "pg_description"
            | "pg_policies"
            | "pg_policy"
            | "pg_matviews"
            | "pg_inherits"
            | "pg_publication"
            | "pg_statistic_ext"
            | "pg_attrdef" => Ok(vec![]),
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
                .map(|(n, s)| {
                    Tuple::new(vec![
                        Value::String(n),
                        Value::String(s.to_string()),
                    ])
                })
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
                    let base_tables = format!("[{}]",
                        metadata.base_tables
                            .iter()
                            .map(|t| format!("\"{}\"", t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );

                    // Calculate staleness
                    let last_update = metadata.last_refresh
                        .map(|dt| dt.timestamp())
                        .unwrap_or(0);

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
                chrono::DateTime::from_timestamp(branch.stats.last_modified as i64, 0)
                    .unwrap_or_default()
            } else {
                chrono::DateTime::from_timestamp(branch.created_at as i64, 0)
                    .unwrap_or_default()
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
        let oid_counter = 1000i32;

        for (idx, table_name) in tables.iter().enumerate() {
            let oid = oid_counter + (idx as i32);
            let tuple = Tuple::new(vec![
                Value::Int4(oid),                      // oid
                Value::String(table_name.clone()),     // relname
                Value::Int4(2200),                     // relnamespace (public schema)
                Value::Int4(oid + 1000),               // reltype
                Value::String("r".to_string()),        // relkind (r = relation/table)
                Value::Int4(oid),                      // relfilenode
                Value::Boolean(false),                 // relrowsecurity (Nano RLS is via TenantManager, not pg_catalog)
            ]);
            results.push(tuple);
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

            match catalog.get_table_schema(table_name) {
                Ok(schema) => {
                    for (col_idx, column) in schema.columns.iter().enumerate() {
                        let type_oid = Self::get_type_oid(&column.data_type);
                        let tuple = Tuple::new(vec![
                            Value::Int4(table_oid),                        // attrelid
                            Value::String(column.name.clone()),            // attname
                            Value::Int4(type_oid),                         // atttypid
                            Value::Int4(-1),                               // attlen
                            Value::Boolean(!column.nullable),              // attnotnull
                            Value::Int4((col_idx + 1) as i32),             // attnum
                            Value::Int4(-1),                               // atttypmod
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
                Value::Int4(oid),                          // oid
                Value::String(type_name.to_string()),      // typname
                Value::Int4(len),                          // typlen
                Value::Boolean(byval),                     // typbyval
                Value::String(category.to_string()),       // typcategory
                Value::Boolean(notnull),                   // typnotnull
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
    /// pg_roles companion to pg_user — different shape (12 cols vs 9)
    /// but same two synthetic roles. drizzle-kit's introspection
    /// queries pg_roles directly during pull.
    fn execute_pg_roles() -> Result<Vec<Tuple>> {
        let role = |oid: i32, name: &str| Tuple::new(vec![
            Value::Int4(oid),                  // oid
            Value::String(name.into()),        // rolname
            Value::Boolean(true),              // rolsuper
            Value::Boolean(true),              // rolinherit
            Value::Boolean(true),              // rolcreaterole
            Value::Boolean(true),              // rolcreatedb
            Value::Boolean(true),              // rolcanlogin
            Value::Boolean(true),              // rolreplication
            Value::Int4(-1),                   // rolconnlimit
            Value::Null,                       // rolpassword
            Value::Null,                       // rolvaliduntil
            Value::Boolean(true),              // rolbypassrls
        ]);
        Ok(vec![role(10, "postgres"), role(11, "helios")])
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
            Value::Int4(1),                       // oid
            Value::String("heliosdb".into()),     // datname
            Value::Int4(10),                      // datdba
            Value::Int4(6),                       // encoding = UTF8
            Value::String("C.UTF-8".into()),      // datcollate
            Value::String("C.UTF-8".into()),      // datctype
            Value::Boolean(false),                // datistemplate
            Value::Boolean(true),                 // datallowconn
            Value::Int4(-1),                      // datconnlimit
            Value::Int4(1663),                    // dattablespace = pg_default
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
        let role = |name: &str, uid: i32| Tuple::new(vec![
            Value::String(name.into()),     // usename
            Value::Int4(uid),                // usesysid
            Value::Boolean(true),            // usecreatedb
            Value::Boolean(true),            // usesuper
            Value::Boolean(true),            // userepl
            Value::Boolean(true),            // usebypassrls
            Value::Null,                     // passwd
            Value::Null,                     // valuntil
            Value::Null,                     // useconfig
        ]);
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
        let mut index_oid = 5000i32;
        let mut table_oid = 1000i32;

        for _table_name in tables.iter() {
            table_oid += 1;
            // For each table, create a placeholder primary key index
            let tuple = Tuple::new(vec![
                Value::Int4(index_oid),                // indexrelid
                Value::Int4(table_oid),                // indrelid
                Value::Boolean(true),                  // indisprimary
                Value::Boolean(false),                 // indisunique
                Value::Boolean(false),                 // indisexclusion
                Value::String("1".to_string()),        // indkey (column 1)
            ]);
            results.push(tuple);
            index_oid += 1;
        }

        Ok(results)
    }

    /// Execute pg_constraint() system view
    ///
    /// Returns information about all constraints
    fn execute_pg_constraint(storage: &StorageEngine) -> Result<Vec<Tuple>> {
        let catalog = storage.catalog();
        let tables = catalog.list_tables()?;
        let mut results = Vec::new();
        let mut constraint_oid = 4000i32;
        let mut table_oid = 1000i32;

        for _table_name in tables.iter() {
            table_oid += 1;
            // Add a primary key constraint for each table
            let tuple = Tuple::new(vec![
                Value::Int4(constraint_oid),           // oid
                Value::String(format!("pk_{}", table_oid)), // conname
                Value::Int4(2200),                     // connamespace
                Value::String("p".to_string()),        // contype (p = primary key)
                Value::Int4(table_oid),                // conrelid
                Value::Null,                           // confrelid (no foreign key)
            ]);
            results.push(tuple);
            constraint_oid += 1;
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
            match catalog.get_table_schema(table_name) {
                Ok(schema) => {
                    for (col_idx, column) in schema.columns.iter().enumerate() {
                        let is_nullable = if column.nullable { "YES" } else { "NO" };
                        let data_type = format!("{:?}", column.data_type);

                        let tuple = Tuple::new(vec![
                            Value::String("public".to_string()),           // table_schema
                            Value::String(table_name.clone()),             // table_name
                            Value::String(column.name.clone()),            // column_name
                            Value::Int4((col_idx + 1) as i32),             // ordinal_position
                            Value::Null,                                   // column_default
                            Value::String(is_nullable.to_string()),        // is_nullable
                            Value::String(data_type),                      // data_type
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
        let mut alp_stats = (0i64, 0i64, 0i64, 0.0f64);  // (uses, bytes_in, bytes_out, total_ratio)
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
        use chrono::{Utc, TimeZone};

        let state = ha_state();
        let config = state.get_config().unwrap_or_default();
        let lsn = state.get_lsn();
        let standby_count = state.standby_count();
        let is_read_only = state.is_read_only();

        let started_at = Utc.timestamp_opt(config.started_at, 0)
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
        use chrono::{Utc, TimeZone};

        let state = ha_state();
        let standbys = state.get_standbys();

        let mut results = Vec::new();
        for standby in standbys {
            let connected_at = Utc.timestamp_opt(standby.connected_at, 0)
                .single()
                .map(|dt| Value::Timestamp(dt))
                .unwrap_or(Value::Null);
            let last_heartbeat = Utc.timestamp_opt(standby.last_heartbeat, 0)
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
        use chrono::{Utc, TimeZone};

        let state = ha_state();

        if let Some(primary) = state.get_primary() {
            let connected_at = Utc.timestamp_opt(primary.connected_at, 0)
                .single()
                .map(|dt| Value::Timestamp(dt))
                .unwrap_or(Value::Null);
            let last_heartbeat = Utc.timestamp_opt(primary.last_heartbeat, 0)
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
            ("wal_bytes_written", metrics.wal_bytes_written, "Total bytes written to WAL"),
            ("records_replicated", metrics.records_replicated, "Total records replicated to standbys"),
            ("bytes_replicated", metrics.bytes_replicated, "Total bytes replicated to standbys"),
            ("heartbeats_sent", metrics.heartbeats_sent, "Total heartbeats sent"),
            ("heartbeats_received", metrics.heartbeats_received, "Total heartbeats received"),
            ("reconnect_count", metrics.reconnect_count, "Number of reconnection attempts"),
            ("current_lsn", state.get_lsn(), "Current Log Sequence Number"),
            ("standby_count", state.standby_count() as u64, "Number of connected standbys"),
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
                let node_count = stats.node4_count + stats.node16_count
                    + stats.node48_count + stats.node256_count;

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
        let storage = StorageEngine::open_in_memory(&config)
            .expect("Failed to create storage");

        // Create a test branch
        storage.create_branch(
            "test_branch",
            Some("main"),
            crate::storage::BranchOptions::default(),
        ).expect("Failed to create branch");

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_database_branches", &storage)
            .expect("Failed to execute pg_database_branches");

        // Should have at least 2 branches (main + test_branch)
        assert!(results.len() >= 2);

        // Verify first result has correct number of columns
        assert_eq!(results[0].values.len(), 7);
    }

    #[test]
    fn test_execute_pg_mv_staleness() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config)
            .expect("Failed to create storage");

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_mv_staleness", &storage)
            .expect("Failed to execute pg_mv_staleness");

        // Should return empty results if no materialized views exist
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_execute_pg_vector_index_stats() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config)
            .expect("Failed to create storage");

        let registry = SystemViewRegistry::new();
        let results = registry.execute("pg_vector_index_stats", &storage)
            .expect("Failed to execute pg_vector_index_stats");

        // Should return empty results if no vector indexes exist
        assert_eq!(results.len(), 0);
    }
}
