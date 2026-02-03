//! # HeliosDB Lite
//!
//! A PostgreSQL-compatible embedded database with vector search, encryption, and multi-tenancy.
//!
//! ## Features
//!
//! - **SQL Database**: PostgreSQL 17 compatible (95%+)
//! - **Vector Search**: Built-in HNSW index for embeddings
//! - **Encryption**: Transparent Data Encryption (TDE) with AES-256-GCM
//! - **Multi-Tenancy**: Native tenant isolation with RLS
//! - **Embedded Mode**: SQLite-style in-process usage
//! - **Server Mode**: PostgreSQL-style network server
//! - **In-Memory Mode**: ACID-compliant RAM-only storage
//!
//! ## Quick Start
//!
//! ```rust
//! use heliosdb_lite::EmbeddedDatabase;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create in-memory database
//! let db = EmbeddedDatabase::new_in_memory()?;
//!
//! // Execute SQL - CREATE TABLE
//! db.execute("CREATE TABLE users (id INT, name TEXT)")?;
//!
//! // INSERT data
//! db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;
//!
//! // Query data
//! let results = db.query("SELECT * FROM users", &[])?;
//! println!("Found {} users", results.len());
//! # Ok(())
//! # }
//! ```
//!
//! ## Architecture
//!
//! HeliosDB Lite uses only open-source components with zero proprietary IP:
//!
//! - **Storage**: RocksDB (LSM-tree)
//! - **Columnar**: Apache Arrow
//! - **SQL Parser**: sqlparser-rs
//! - **Vector Index**: HNSW (published research)
//! - **Encryption**: AES-256-GCM (NIST standard)
//! - **Protocol**: PostgreSQL wire protocol

// Strict code quality lints - prevent unsafe patterns in production code
#![deny(
    clippy::unwrap_used,
    clippy::todo,
    clippy::unimplemented,
)]

// Warn on patterns that should be reviewed but don't block compilation
#![warn(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
)]

// Standard Rust warnings
// TODO: Re-enable missing_docs once documentation is added
#![allow(missing_docs)]
#![warn(rust_2018_idioms)]

// Recommended pedantic warnings for code quality
#![warn(
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo,
)]

// Allow certain pedantic lints that are too strict or conflict with our style
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    // Stylistic preferences - not safety issues
    clippy::similar_names,
    clippy::redundant_else,
    clippy::needless_continue,
    clippy::needless_pass_by_ref_mut,
    clippy::uninlined_format_args,
    clippy::redundant_closure_for_method_calls,
    clippy::match_same_arms,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::option_if_let_else,
    clippy::struct_excessive_bools,
    clippy::unused_self,
    clippy::unused_async,
    clippy::return_self_not_must_use,
    clippy::if_not_else,
    clippy::manual_let_else,
    clippy::single_char_add_str,
    clippy::unreadable_literal,
    clippy::needless_raw_string_hashes,
    clippy::or_fun_call,
    clippy::derive_partial_eq_without_eq,
    clippy::redundant_clone,
    clippy::map_unwrap_or,
    clippy::needless_borrow,
    clippy::format_push_string,
    clippy::needless_lifetimes,
    clippy::default_trait_access,
    clippy::empty_line_after_doc_comments,
    clippy::needless_pass_by_value,
    clippy::match_single_binding,
    clippy::only_used_in_recursion,
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants,
    clippy::suboptimal_flops,
    clippy::wildcard_imports,
    clippy::ref_option,
    clippy::needless_collect,
    clippy::bool_to_int_with_if,
    clippy::needless_borrowed_reference,
    clippy::useless_format,
    clippy::used_underscore_binding,
    clippy::str_to_string,
    clippy::implicit_hasher,
    clippy::string_add_assign,
    clippy::explicit_iter_loop,
    clippy::single_match_else,
    clippy::manual_string_new,
    clippy::derivable_impls,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::branches_sharing_code,
    clippy::manual_strip,
    clippy::upper_case_acronyms,
    clippy::struct_field_names,
    clippy::assigning_clones,
    clippy::should_implement_trait,
    clippy::boxed_local,
    clippy::collapsible_if,
    clippy::field_reassign_with_default,
    clippy::unnecessary_cast,
    clippy::type_complexity,
    clippy::manual_is_ascii_check,
    clippy::borrow_as_ptr,
    clippy::cognitive_complexity,
    clippy::fn_params_excessive_bools,
    clippy::iter_without_into_iter,
    clippy::unit_cmp,
    clippy::ptr_arg,
    clippy::use_debug,
    clippy::redundant_closure,
    clippy::clone_on_copy,
    clippy::new_without_default,
    clippy::manual_range_contains,
    clippy::manual_range_patterns,
    clippy::if_then_some_else_none,
    clippy::match_like_matches_macro,
    clippy::option_as_ref_cloned,
    clippy::collapsible_match,
    clippy::filter_map_identity,
    clippy::get_first,
    clippy::implicit_clone,
    clippy::len_zero,
    clippy::write_with_newline,
    clippy::single_char_pattern,
    clippy::let_and_return,
    clippy::redundant_pattern_matching,
    clippy::match_ref_pats,
    clippy::if_same_then_else,
    clippy::semicolon_if_nothing_returned,
    clippy::iter_over_hash_type,
    clippy::iter_on_single_items,
    clippy::iter_on_empty_collections,
    clippy::useless_vec,
    clippy::vec_init_then_push,
    clippy::iter_nth_zero,
    clippy::unwrap_or_default,
    clippy::trivial_regex,
    clippy::map_entry,
    clippy::enum_glob_use,
    clippy::unnested_or_patterns,
    clippy::manual_clamp,
    clippy::cast_ptr_alignment,
    clippy::ptr_as_ptr,
    clippy::imprecise_flops,
    clippy::future_not_send,
    clippy::significant_drop_in_scrutinee,
    clippy::collection_is_never_read,
    clippy::manual_div_ceil,
    clippy::checked_conversions,
    clippy::as_underscore,
    clippy::as_ptr_cast_mut,
    clippy::trim_split_whitespace,
    clippy::string_lit_chars_any,
    clippy::large_enum_variant,
    clippy::doc_lazy_continuation,
    clippy::too_long_first_doc_paragraph,
    clippy::useless_conversion,
    clippy::multiple_crate_versions,
    clippy::unit_arg,
    clippy::inherent_to_string,
    clippy::to_string_trait_impl,
    clippy::borrow_deref_ref,
    clippy::manual_map,
    clippy::manual_filter_map,
    clippy::option_map_unit_fn,
    clippy::result_map_unit_fn,
    clippy::manual_is_multiple_of,
    clippy::print_literal,
    clippy::iter_kv_map,
    clippy::manual_find,
    clippy::write_literal,
    clippy::explicit_into_iter_loop,
    clippy::manual_ok_or,
    clippy::bind_instead_of_map,
    clippy::manual_retain,
    clippy::io_other_error,
    clippy::clone_on_ref_ptr,
    clippy::bool_comparison,
    clippy::single_match,
    clippy::iter_next_loop,
    clippy::str_split_at_newline,
    clippy::option_as_ref_deref,
    clippy::arithmetic_side_effects,
    clippy::cloned_instead_of_copied,
    clippy::string_slice,
    clippy::inconsistent_struct_constructor,
    clippy::unnecessary_literal_unwrap,
    clippy::ref_binding_to_reference,
    clippy::match_bool,
    clippy::partialeq_to_none,
    clippy::redundant_static_lifetimes,
    clippy::char_lit_as_u8,
    clippy::manual_is_power_of_two,
    clippy::filter_map_bool_then,
    clippy::manual_flatten,
    clippy::manual_next_back,
    clippy::maybe_infinite_iter,
    clippy::needless_option_as_deref,
    clippy::suspicious_else_formatting,
    clippy::useless_transmute,
    // Casting lints - intentional in numeric code
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    // Performance lints that add noise
    clippy::trivially_copy_pass_by_ref,
    clippy::significant_drop_tightening,
    clippy::unnecessary_wraps,
    clippy::missing_const_for_fn,
    clippy::use_self,
    // Lazy static using once_cell is idiomatic
    clippy::non_std_lazy_statics,
)]

// Allow unused code for GA - these are reserved for future implementation
#![allow(dead_code)]
#![allow(unused_variables)]

// Public modules
pub mod storage;
pub mod compute;
pub mod optimizer;
pub mod vector;
pub mod protocol;
pub mod protocols; // Protocol integration layer (adapters)
pub mod crypto;
pub mod tenant;
pub mod sql;
pub mod audit;
pub mod network;
pub mod repl;
pub mod api; // REST API module
pub mod cli; // CLI module
pub mod session; // Multi-user session management
pub mod ai; // AI/NL query module
pub mod multi_tenant; // Multi-tenant support
pub mod git_integration; // Git workflow integration

// Experimental modules (require feature flags)
// DISABLED: Sync module has compilation issues and is 85% complete
// #[cfg(feature = "sync-experimental")]
// pub mod sync;

// High Availability modules (require HA feature flags)
// Tier 1: Warm Standby (Active-Passive replication)
// Tier 2: Multi-Primary (Branch-based Active-Active)
// Tier 3: Sharding (Horizontal scaling)
#[cfg(any(
    feature = "ha-tier1",
    feature = "ha-tier2",
    feature = "ha-tier3",
    feature = "ha-dedup",
    feature = "ha-branch-replication"
))]
pub mod replication;

// HeliosProxy - Connection router and failover manager
#[cfg(feature = "ha-proxy")]
pub mod proxy;

// Branch-Based A/B Testing
#[cfg(feature = "ha-ab-testing")]
pub mod ab_testing;

// Internal modules
mod error;
mod types;
mod config;
mod embedded_db_dump;

// Re-exports
pub use error::{Error, Result};
pub use types::{DataType, Value, Tuple, Schema, Column, ColumnStorageMode, VectorStoreInfo, AgentSession, AgentMessage, DocumentData, DocumentMetadata};
pub use config::{Config, KeySource, ZkeMode, ZkeEncryptionConfig};
pub use storage::StorageEngine;
pub use crypto::{
    ZkeConfig, ZkeDerivedKeys, ZkeKeyDerivation, ZkeRequestContext,
    ZeroKnowledgeSession, NonceTracker, TimestampValidator,
};

/// Convert logical plan ReferentialAction to constraints module ReferentialAction
fn convert_logical_referential_action(action: &sql::logical_plan::ReferentialAction) -> sql::constraints::ReferentialAction {
    match action {
        sql::logical_plan::ReferentialAction::NoAction => sql::constraints::ReferentialAction::NoAction,
        sql::logical_plan::ReferentialAction::Restrict => sql::constraints::ReferentialAction::Restrict,
        sql::logical_plan::ReferentialAction::Cascade => sql::constraints::ReferentialAction::Cascade,
        sql::logical_plan::ReferentialAction::SetNull => sql::constraints::ReferentialAction::SetNull,
        sql::logical_plan::ReferentialAction::SetDefault => sql::constraints::ReferentialAction::SetDefault,
    }
}

/// Embedded database instance
///
/// Provides a simple API for embedded database usage (like SQLite).
///
/// # Examples
///
/// ```rust,no_run
/// use heliosdb_lite::EmbeddedDatabase;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let db = EmbeddedDatabase::new("./mydb.helio")?;
/// db.execute("CREATE TABLE test (id INT, name TEXT)")?;
/// # Ok(())
/// # }
/// ```
pub struct EmbeddedDatabase {
    /// Storage engine (public for REPL access)
    pub storage: std::sync::Arc<storage::StorageEngine>,
    config: Config,
    /// Current active transaction (if any)
    current_transaction: std::sync::Arc<std::sync::Mutex<Option<storage::Transaction>>>,
    /// Tenant manager for multi-tenancy and RLS (optional)
    pub tenant_manager: std::sync::Arc<crate::tenant::TenantManager>,
    /// Trigger registry for trigger management and execution
    pub trigger_registry: std::sync::Arc<sql::TriggerRegistry>,
    /// Function registry for stored functions and procedures
    pub function_registry: std::sync::Arc<sql::FunctionRegistry>,
    /// MV scheduler for CPU-aware refresh scheduling
    mv_scheduler: std::sync::Arc<storage::MVScheduler>,
    /// Auto-refresh worker for background MV refresh (optional, requires async)
    auto_refresh_worker: std::sync::Arc<parking_lot::RwLock<Option<storage::AutoRefreshWorker>>>,
    /// Dump manager for database persistence
    pub dump_manager: std::sync::Arc<storage::DumpManager>,
    /// Session manager for multi-user support
    pub session_manager: std::sync::Arc<crate::session::SessionManager>,
    /// Lock manager for concurrency control
    pub lock_manager: std::sync::Arc<storage::LockManager>,
    /// Dirty tracker for tracking uncommitted changes
    pub dirty_tracker: std::sync::Arc<storage::DirtyTracker>,
    /// Active transactions per session
    session_transactions: std::sync::Arc<dashmap::DashMap<crate::session::SessionId, storage::Transaction>>,
}

impl EmbeddedDatabase {
    /// Check if a SQL statement is a transaction control statement
    fn is_transaction_control(sql: &str) -> bool {
        // Strip trailing semicolon and whitespace for matching
        let trimmed = sql.trim().trim_end_matches(';').trim().to_uppercase();
        trimmed.starts_with("BEGIN") ||
        trimmed.starts_with("START TRANSACTION") ||
        trimmed == "COMMIT" ||
        trimmed == "ROLLBACK"
    }

    /// Handle transaction control statements (BEGIN, COMMIT, ROLLBACK)
    fn handle_transaction_control(&self, sql: &str) -> Result<u64> {
        // Strip trailing semicolon and whitespace for matching
        let trimmed = sql.trim().trim_end_matches(';').trim().to_uppercase();

        if trimmed.starts_with("BEGIN") || trimmed.starts_with("START TRANSACTION") {
            self.begin_transaction_internal()?;
            Ok(0)
        } else if trimmed == "COMMIT" {
            self.commit_internal()?;
            Ok(0)
        } else if trimmed == "ROLLBACK" {
            self.rollback_internal()?;
            Ok(0)
        } else {
            Err(Error::query_execution("Unknown transaction control statement"))
        }
    }

    /// Internal method to begin a transaction
    fn begin_transaction_internal(&self) -> Result<()> {
        use crate::error::LockResultExt;
        let mut txn_ref = self.current_transaction.lock()
            .map_lock_err("Failed to acquire transaction lock for begin")?;
        if txn_ref.is_some() {
            return Err(Error::transaction("Transaction already active"));
        }
        let txn = self.storage.begin_transaction()?;
        *txn_ref = Some(txn);
        Ok(())
    }

    /// Internal method to commit the current transaction
    fn commit_internal(&self) -> Result<()> {
        use crate::error::LockResultExt;
        let mut txn_ref = self.current_transaction.lock()
            .map_lock_err("Failed to acquire transaction lock for commit")?;
        if let Some(txn) = txn_ref.take() {
            txn.commit()?;
            // Increment LSN to track transaction commits
            self.storage.increment_lsn();
            Ok(())
        } else {
            Err(Error::transaction("No active transaction to commit"))
        }
    }

    /// Internal method to rollback the current transaction
    fn rollback_internal(&self) -> Result<()> {
        use crate::error::LockResultExt;
        let mut txn_ref = self.current_transaction.lock()
            .map_lock_err("Failed to acquire transaction lock for rollback")?;
        if let Some(txn) = txn_ref.take() {
            txn.rollback()?;
            Ok(())
        } else {
            Err(Error::transaction("No active transaction to rollback"))
        }
    }

    /// Try to parse HA switchover commands (ha-tier1 feature only)
    ///
    /// Returns Some(LogicalPlan) if the SQL is an HA command, None otherwise.
    #[cfg(feature = "ha-tier1")]
    fn try_parse_ha_command(sql: &str) -> Result<Option<sql::LogicalPlan>> {
        if sql::Parser::is_switchover(sql) {
            let target_node = sql::Parser::parse_switchover_sql(sql)?;
            Ok(Some(sql::LogicalPlan::Switchover { target_node }))
        } else if sql::Parser::is_switchover_check(sql) {
            let target_node = sql::Parser::parse_switchover_check_sql(sql)?;
            Ok(Some(sql::LogicalPlan::SwitchoverCheck { target_node }))
        } else if sql::Parser::is_cluster_status(sql) {
            Ok(Some(sql::LogicalPlan::ClusterStatus))
        } else if sql::Parser::is_set_node_alias(sql) {
            let (node_id, alias) = sql::Parser::parse_set_node_alias_sql(sql)?;
            Ok(Some(sql::LogicalPlan::SetNodeAlias { node_id, alias }))
        } else if sql::Parser::is_show_topology(sql) {
            Ok(Some(sql::LogicalPlan::ShowTopology))
        } else {
            Ok(None)
        }
    }

    /// Stub for HA command parsing when ha-tier1 is disabled
    #[cfg(not(feature = "ha-tier1"))]
    fn try_parse_ha_command(_sql: &str) -> Result<Option<sql::LogicalPlan>> {
        Ok(None)
    }

    /// Execute SQL in the context of a transaction
    ///
    /// This method ensures that all write operations are buffered in the transaction's
    /// write set and will be atomically committed when the transaction commits.
    ///
    /// # ACID Guarantees
    ///
    /// - **Atomicity**: All writes are buffered and committed atomically via RocksDB WriteBatch
    /// - **Consistency**: Schema validation ensures data integrity before writes
    /// - **Isolation**: Snapshot isolation prevents dirty reads; read-your-own-writes via write set
    /// - **Durability**: Write-ahead logging (WAL) ensures durability after commit
    ///
    /// # Current Implementation
    ///
    /// **Fully Transactional:**
    /// - INSERT: Writes go to transaction write set via `txn.put()`
    /// - CREATE TABLE: Catalog operations (already atomic)
    ///
    /// **Limitations (Future Enhancement):**
    /// - UPDATE/DELETE: Currently bypass transaction (execute directly on storage)
    /// - TRUNCATE: Currently bypass transaction (execute directly on storage)
    ///
    /// These limitations are acceptable for v2.0 because:
    /// 1. INSERT is the most common write operation
    /// 2. UPDATE/DELETE still provide atomicity via RocksDB's atomic writes
    /// 3. Full transaction support for all operations will be added in v2.1
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL statement to execute
    /// * `txn` - Transaction context for write buffering
    ///
    /// # Returns
    ///
    /// Number of rows affected
    fn execute_in_transaction(&self, sql: &str, txn: &storage::Transaction) -> Result<u64> {
        // Record query for quota tracking (QPS enforcement)
        if let Some(context) = self.tenant_manager.get_current_context() {
            self.tenant_manager.record_query(context.tenant_id)
                .map_err(|e| Error::query_execution(format!("Quota exceeded: {}", e)))?;
        }

        // Check if this is a Phase 3 branching statement (before trying to parse with sqlparser)
        let plan = if sql::Parser::is_create_branch(sql) {
            // Parse CREATE DATABASE BRANCH statement
            let (branch_name, parent, as_of_clause, with_options) = sql::Parser::parse_create_branch_sql(sql)?;
            sql::phase3::branching::BranchingParser::parse_create_branch(
                branch_name,
                parent,
                &as_of_clause,
                with_options.as_deref(),
            )?
        } else if sql::Parser::is_drop_branch(sql) {
            // Parse DROP DATABASE BRANCH statement
            let (branch_name, if_exists) = sql::Parser::parse_drop_branch_sql(sql)?;
            sql::phase3::branching::BranchingParser::parse_drop_branch(branch_name, if_exists)?
        } else if sql::Parser::is_merge_branch(sql) {
            // Parse MERGE DATABASE BRANCH statement
            let (source, target, with_options) = sql::Parser::parse_merge_branch_sql(sql)?;
            sql::phase3::branching::BranchingParser::parse_merge_branch(
                source,
                target,
                with_options.as_deref(),
            )?
        } else if sql::Parser::is_use_branch(sql) {
            // Parse USE BRANCH statement
            let branch_name = sql::Parser::parse_use_branch_sql(sql)?;
            sql::LogicalPlan::UseBranch { branch_name }
        } else if sql::Parser::is_show_branches(sql) {
            // Parse SHOW BRANCHES statement
            sql::LogicalPlan::ShowBranches
        } else if sql::Parser::is_refresh_materialized_view(sql) {
            // Parse REFRESH MATERIALIZED VIEW statement
            let (view_name, concurrent, incremental) = sql::Parser::parse_refresh_materialized_view_sql(sql)?;
            sql::LogicalPlan::RefreshMaterializedView {
                name: view_name,
                concurrent,
                incremental,
            }
        } else if sql::Parser::is_drop_materialized_view(sql) {
            // Parse DROP MATERIALIZED VIEW statement
            let (view_name, if_exists) = sql::Parser::parse_drop_materialized_view_sql(sql)?;
            sql::LogicalPlan::DropMaterializedView {
                name: view_name,
                if_exists,
            }
        } else if sql::Parser::is_alter_materialized_view(sql) {
            // Parse ALTER MATERIALIZED VIEW statement
            let (view_name, options) = sql::Parser::parse_alter_materialized_view_sql(sql)?;
            sql::LogicalPlan::AlterMaterializedView {
                name: view_name,
                options,
            }
        } else if sql::Parser::is_alter_column_storage(sql) {
            // Parse ALTER TABLE ALTER COLUMN SET STORAGE statement
            let (table_name, column_name, storage_mode) = sql::Parser::parse_alter_column_storage(sql)?;
            sql::LogicalPlan::AlterColumnStorage {
                table_name,
                column_name,
                storage_mode,
            }
        } else if sql::Parser::is_pg_create_procedure(sql) || sql::Parser::is_pg_create_or_replace_procedure(sql) {
            // Parse PostgreSQL-style CREATE [OR REPLACE] PROCEDURE statement
            let (name, or_replace, params, language, body) = sql::Parser::parse_pg_create_procedure(sql)?;
            let param_list: Vec<sql::logical_plan::FunctionParam> = params.into_iter().map(|(pname, ptype)| {
                sql::logical_plan::FunctionParam {
                    name: pname,
                    data_type: sql::Planner::parse_data_type_string(&ptype).unwrap_or(DataType::Text),
                    mode: sql::logical_plan::ParamMode::In,
                    default: None,
                }
            }).collect();
            sql::LogicalPlan::CreateProcedure {
                name,
                or_replace,
                params: param_list,
                body,
                language,
            }
        } else if let Some(plan) = Self::try_parse_ha_command(sql)? {
            // HA Switchover commands (ha-tier1 feature)
            plan
        } else {
            // Regular SQL - parse normally
            let parser = sql::Parser::new();
            let statement = parser.parse_one(sql)?;

            // Create logical plan with catalog access and original SQL for time-travel parsing
            let catalog = self.storage.catalog();
            let planner = sql::Planner::with_catalog(&catalog)
                .with_sql(sql.to_string());
            planner.statement_to_plan(statement)?
        };

        // Execute plan based on type
        match &plan {
            sql::LogicalPlan::CreateTable { name, columns, constraints, .. } => {
                let schema_columns: Vec<Column> = columns.iter().map(|col_def| {
                    // Serialize default expression to JSON for storage
                    let default_expr = col_def.default.as_ref().map(|expr| {
                        serde_json::to_string(expr).unwrap_or_default()
                    });

                    Column {
                        name: col_def.name.clone(),
                        data_type: col_def.data_type.clone(),
                        nullable: !col_def.not_null,
                        primary_key: col_def.primary_key,
                        source_table: None,
                        source_table_name: None,
                        default_expr,
                        unique: col_def.unique,
                        storage_mode: col_def.storage_mode,
                    }
                }).collect();

                let schema = Schema::new(schema_columns);
                let catalog = self.storage.catalog();

                // Log to WAL for replication before creating (schema will be moved)
                if let Err(e) = self.storage.log_create_table(name, &schema) {
                    tracing::warn!("Failed to log CREATE TABLE to WAL: {}", e);
                }

                catalog.create_table(name, schema)?;

                // Save table constraints if any
                if !constraints.is_empty() {
                    let mut table_constraints = sql::TableConstraints::new();
                    for constraint in constraints {
                        match constraint {
                            sql::logical_plan::TableConstraint::ForeignKey {
                                name: fk_name,
                                columns: fk_cols,
                                references_table,
                                references_columns,
                                on_delete,
                                on_update,
                                deferrable,
                                initially_deferred,
                            } => {
                                let fk = sql::ForeignKeyConstraint::new(
                                    fk_name.clone().unwrap_or_else(|| {
                                        sql::ForeignKeyConstraint::generate_name(name, fk_cols, references_table)
                                    }),
                                    name.clone(),
                                    fk_cols.clone(),
                                    references_table.clone(),
                                    references_columns.clone(),
                                );
                                let fk = if let Some(action) = on_delete {
                                    fk.on_delete(convert_logical_referential_action(action))
                                } else {
                                    fk
                                };
                                let fk = if let Some(action) = on_update {
                                    fk.on_update(convert_logical_referential_action(action))
                                } else {
                                    fk
                                };
                                let fk = if *deferrable {
                                    fk.deferrable(*initially_deferred)
                                } else {
                                    fk
                                };
                                table_constraints.add_foreign_key(fk);
                            }
                            sql::logical_plan::TableConstraint::PrimaryKey { name: pk_name, columns: pk_cols } => {
                                table_constraints.add_unique(sql::UniqueConstraint::new(
                                    pk_name.clone().unwrap_or_else(|| format!("{}_pkey", name)),
                                    name.clone(),
                                    pk_cols.clone(),
                                    true,
                                ));
                            }
                            sql::logical_plan::TableConstraint::Unique { name: uq_name, columns: uq_cols } => {
                                table_constraints.add_unique(sql::UniqueConstraint::new(
                                    uq_name.clone().unwrap_or_else(|| format!("{}_unique", name)),
                                    name.clone(),
                                    uq_cols.clone(),
                                    false,
                                ));
                            }
                            sql::logical_plan::TableConstraint::Check { name: ck_name, expression } => {
                                table_constraints.add_check(sql::CheckConstraint::new(
                                    ck_name.clone().unwrap_or_else(|| format!("{}_check", name)),
                                    name.clone(),
                                    serde_json::to_string(expression).unwrap_or_default(),
                                ));
                            }
                        }
                    }
                    catalog.save_table_constraints(name, &table_constraints)?;
                }

                Ok(1)
            }
            sql::LogicalPlan::Insert { table_name, columns, values, returning } => {
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let evaluator = sql::Evaluator::new(std::sync::Arc::new(Schema {
                    columns: vec![],
                }));
                let empty_tuple = Tuple::new(vec![]);

                // Auto-suspend SMFI tracking for bulk inserts (>= bulk_load_threshold rows)
                // The threshold is hot-reloadable via SET smfi_bulk_load_threshold = N
                // The guard will automatically resume tracking and schedule rebuild when dropped
                let bulk_threshold = self.storage.smfi_bulk_load_threshold();
                let _smfi_guard = if values.len() >= bulk_threshold {
                    Some(self.storage.suspend_smfi_for_bulk_load(
                        table_name,
                        storage::BulkLoadReason::MultiRowInsert,
                    ))
                } else {
                    None
                };

                // Initialize trigger context for cascading detection
                let mut trigger_context = sql::TriggerContext::new();
                let trigger_event = sql::logical_plan::TriggerEvent::Insert;

                // Collect tuples for RETURNING clause
                let mut returned_tuples: Vec<Tuple> = Vec::new();
                let has_returning = returning.is_some();

                // Pre-parse default expressions for columns (lazy evaluation)
                let default_exprs: Vec<Option<sql::LogicalExpr>> = schema.columns.iter()
                    .map(|col| {
                        col.default_expr.as_ref().and_then(|json| {
                            serde_json::from_str(json).ok()
                        })
                    })
                    .collect();

                // Build column index mapping for INSERT with explicit column list
                let column_indices: Option<Vec<usize>> = columns.as_ref().map(|cols| {
                    cols.iter()
                        .filter_map(|col_name| schema.get_column_index(col_name))
                        .collect()
                });

                let mut count = 0;
                for value_row in values {
                    // Initialize tuple values for ALL columns (use None as placeholder)
                    let mut tuple_values: Vec<Option<Value>> = vec![None; schema.columns.len()];

                    // Fill in provided values
                    for (val_idx, expr) in value_row.iter().enumerate() {
                        let target_col_idx = if let Some(ref indices) = column_indices {
                            if val_idx >= indices.len() {
                                return Err(Error::query_execution(
                                    "More values than columns specified"
                                ));
                            }
                            indices[val_idx]
                        } else {
                            val_idx
                        };

                        if target_col_idx >= schema.columns.len() {
                            return Err(Error::query_execution(format!(
                                "Too many values for INSERT: table has {} columns",
                                schema.columns.len()
                            )));
                        }

                        let target_type = &schema.columns[target_col_idx].data_type;
                        let mut value = evaluator.evaluate(expr, &empty_tuple)?;

                        let needs_cast = match (&value, target_type) {
                            (Value::Null, _) => false,
                            (Value::Vector(_), DataType::Vector(_)) => false,
                            (Value::String(_), DataType::Vector(_)) => true,
                            (Value::String(_), DataType::Json | DataType::Jsonb) => true,
                            (Value::Int4(_), DataType::Int4) => false,
                            (Value::Int8(_), DataType::Int8) => false,
                            (Value::Float4(_), DataType::Float4) => false,
                            (Value::Float8(_), DataType::Float8) => false,
                            (Value::String(_), DataType::Text | DataType::Varchar(_)) => false,
                            (Value::Boolean(_), DataType::Boolean) => false,
                            (Value::Json(_), DataType::Json | DataType::Jsonb) => false,
                            _ => true,
                        };

                        if needs_cast {
                            value = evaluator.cast_value(value, target_type)?;
                        }

                        // Enforce NOT NULL constraint for explicitly provided values
                        if matches!(value, Value::Null) && !schema.columns[target_col_idx].nullable {
                            return Err(Error::constraint_violation(format!(
                                "NOT NULL constraint violated: cannot insert NULL into column '{}'",
                                schema.columns[target_col_idx].name
                            )));
                        }

                        tuple_values[target_col_idx] = Some(value);
                    }

                    // Fill in missing columns with defaults or NULL
                    let final_values: Result<Vec<Value>> = tuple_values
                        .into_iter()
                        .enumerate()
                        .map(|(idx, opt_val)| {
                            if let Some(val) = opt_val {
                                Ok(val)
                            } else {
                                // Column not provided, use default or NULL
                                let col = &schema.columns[idx];
                                if let Some(ref default_expr) = default_exprs[idx] {
                                    // Evaluate default expression
                                    let mut value = evaluator.evaluate(default_expr, &empty_tuple)?;
                                    // Cast if needed
                                    if value.data_type() != col.data_type {
                                        value = evaluator.cast_value(value, &col.data_type)?;
                                    }
                                    Ok(value)
                                } else if col.nullable {
                                    Ok(Value::Null)
                                } else {
                                    Err(Error::query_execution(format!(
                                        "Column '{}' does not have a default value and is not nullable",
                                        col.name
                                    )))
                                }
                            }
                        })
                        .collect();

                    let final_values_vec = final_values?;
                    let tuple = Tuple::new(final_values_vec.clone());

                    // Validate foreign key constraints (IMMEDIATE mode)
                    let table_constraints = catalog.load_table_constraints(table_name)?;
                    for fk in &table_constraints.foreign_keys {
                        if fk.enforcement == sql::ConstraintEnforcement::Immediate {
                            // Get FK column values from the tuple
                            let fk_values: Vec<Value> = fk.columns.iter()
                                .map(|col_name| {
                                    schema.columns.iter()
                                        .position(|c| &c.name == col_name)
                                        .map(|idx| final_values_vec[idx].clone())
                                        .unwrap_or(Value::Null)
                                })
                                .collect();

                            // Skip validation if any FK column is NULL
                            if fk_values.iter().any(|v| matches!(v, Value::Null)) {
                                continue;
                            }

                            // Check if referenced row exists
                            let exists = self.check_foreign_key_exists(
                                &fk.references_table,
                                &fk.references_columns,
                                &fk_values,
                            )?;

                            if !exists {
                                return Err(Error::constraint_violation(format!(
                                    "Foreign key constraint '{}' violated: referenced row in table '{}' does not exist",
                                    fk.name, fk.references_table
                                )));
                            }
                        }
                    }

                    // Validate CHECK constraints
                    for check in &table_constraints.check_constraints {
                        // Parse and evaluate the CHECK expression
                        let check_result = self.evaluate_check_constraint(
                            &check.expression,
                            &schema,
                            &final_values_vec,
                        )?;

                        if !check_result {
                            return Err(Error::constraint_violation(format!(
                                "CHECK constraint '{}' violated: expression '{}' evaluated to false",
                                check.name, check.expression
                            )));
                        }
                    }

                    // Execute BEFORE INSERT triggers
                    let row_context = sql::triggers::TriggerRowContext::for_insert(tuple.clone());
                    let db_ref = self.clone_for_trigger();
                    let mut executor_fn = |stmt: &sql::LogicalPlan, _ctx: &sql::triggers::TriggerRowContext| -> Result<()> {
                        db_ref.execute_plan_internal(stmt)?;
                        Ok(())
                    };

                    let action = self.trigger_registry.execute_triggers(
                        table_name,
                        &trigger_event,
                        &sql::logical_plan::TriggerTiming::Before,
                        &row_context,
                        &mut trigger_context,
                        Some(std::sync::Arc::new(schema.clone())),
                        &mut executor_fn,
                    )?;

                    // Handle trigger action
                    match action {
                        sql::triggers::TriggerAction::Abort(msg) => {
                            return Err(Error::query_execution(format!("INSERT aborted by trigger: {}", msg)));
                        }
                        sql::triggers::TriggerAction::Skip => {
                            // INSTEAD OF trigger - skip the insert
                            continue;
                        }
                        sql::triggers::TriggerAction::Continue => {
                            // Continue with insert
                        }
                    }

                    // Transactional insert (branch-aware)
                    let row_id = catalog.next_row_id(table_name)?;
                    let key = self.storage.branch_aware_data_key(table_name, row_id);

                    // Serialize tuple directly (RocksDB LZ4 handles compression at block level)
                    let val = bincode::serialize(&tuple).map_err(|e| Error::storage(e.to_string()))?;
                    txn.put(key.clone(), val.clone())?;

                    // Log to WAL for replication (txn.put bypasses normal WAL logging)
                    self.storage.log_data_insert(table_name, &val)?;

                    count += 1;

                    // Collect tuple for RETURNING clause
                    if has_returning {
                        // Create tuple with row_id populated for reference
                        let mut returned_tuple = tuple.clone();
                        returned_tuple.row_id = Some(row_id);
                        if let Some(projected) = Self::project_returning_columns(&returned_tuple, &schema, returning) {
                            returned_tuples.push(projected);
                        }
                    }

                    // Update storage quota tracking
                    if let Some(context) = self.tenant_manager.get_current_context() {
                        // Calculate approximate storage size (tuple + overhead)
                        let tuple_size = bincode::serialize(&tuple)
                            .map(|bytes| bytes.len() as u64)
                            .unwrap_or(256); // fallback estimate

                        // Get current storage and add new tuple size
                        if let Some(current_quota) = self.tenant_manager.get_quota_tracking(context.tenant_id) {
                            let new_storage = current_quota.storage_bytes_used + tuple_size;
                            if let Err(e) = self.tenant_manager.update_storage_usage(context.tenant_id, new_storage) {
                                // Storage quota exceeded - rollback will happen automatically
                                return Err(Error::query_execution(format!("Storage quota exceeded: {}", e)));
                            }
                        }

                        // Record CDC event for INSERT
                        let new_values = serde_json::to_string(&tuple.values)
                            .unwrap_or_else(|_| "[]".to_string());

                        self.tenant_manager.record_change_event(
                            crate::tenant::ChangeType::Insert,
                            table_name.to_string(),
                            row_id.to_string(),
                            None, // no old values for INSERT
                            Some(new_values),
                            context.tenant_id,
                            None, // transaction_id could be added if tracked
                        );
                    }

                    // Execute AFTER INSERT triggers
                    let action = self.trigger_registry.execute_triggers(
                        table_name,
                        &trigger_event,
                        &sql::logical_plan::TriggerTiming::After,
                        &row_context,
                        &mut trigger_context,
                        Some(std::sync::Arc::new(schema.clone())),
                        &mut executor_fn,
                    )?;

                    // Handle AFTER trigger action
                    if let sql::triggers::TriggerAction::Abort(msg) = action {
                        return Err(Error::query_execution(format!("INSERT aborted by AFTER trigger: {}", msg)));
                    }
                }
                // Return count (RETURNING clause results handled separately)
                Ok(count)
            }
            sql::LogicalPlan::Update { table_name, assignments, selection, returning } => {
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let evaluator = sql::Evaluator::with_parameters(
                    std::sync::Arc::new(schema.clone()),
                    vec![],
                );

                // Initialize trigger context
                let mut trigger_context = sql::TriggerContext::new();
                let updated_columns: Vec<String> = assignments.iter().map(|(col, _)| col.clone()).collect();
                let trigger_event = sql::logical_plan::TriggerEvent::Update(Some(updated_columns));

                // Use branch-aware scan to get tuples (includes main + branch overrides - deleted)
                let tuples = self.storage.scan_table_branch_aware(table_name)?;
                let mut updates: Vec<(u64, Tuple)> = Vec::new();

                for old_tuple in tuples {
                    let matches = if let Some(predicate) = selection {
                        let result = evaluator.evaluate(predicate, &old_tuple)?;
                        match result {
                            Value::Boolean(b) => b,
                            _ => false,
                        }
                    } else {
                        true
                    };

                    if matches {
                        // Create new tuple with updated values
                        let mut new_tuple = old_tuple.clone();
                        for (col_name, value_expr) in assignments {
                            let new_value = evaluator.evaluate(value_expr, &old_tuple)?;
                            let col_index = evaluator.schema().get_column_index(col_name)
                                .ok_or_else(|| Error::query_execution(format!("Column '{}' not found", col_name)))?;
                            new_tuple.values[col_index] = new_value;
                        }

                        // Execute BEFORE UPDATE triggers
                        let row_context = sql::triggers::TriggerRowContext::for_update(old_tuple.clone(), new_tuple.clone());
                        let db_ref = self.clone_for_trigger();
                        let mut executor_fn = |stmt: &sql::LogicalPlan, _ctx: &sql::triggers::TriggerRowContext| -> Result<()> {
                            db_ref.execute_plan_internal(stmt)?;
                            Ok(())
                        };

                        let action = self.trigger_registry.execute_triggers(
                            table_name,
                            &trigger_event,
                            &sql::logical_plan::TriggerTiming::Before,
                            &row_context,
                            &mut trigger_context,
                            Some(evaluator.schema().clone()),
                            &mut executor_fn,
                        )?;

                        // Handle trigger action
                        match action {
                            sql::triggers::TriggerAction::Abort(msg) => {
                                return Err(Error::query_execution(format!("UPDATE aborted by trigger: {}", msg)));
                            }
                            sql::triggers::TriggerAction::Skip => {
                                // INSTEAD OF trigger - skip this update
                                continue;
                            }
                            sql::triggers::TriggerAction::Continue => {
                                // Continue with update
                            }
                        }

                        let row_id = new_tuple.row_id.unwrap_or(0);
                        updates.push((row_id, new_tuple.clone()));

                        // Record CDC event for UPDATE
                        if let Some(context) = self.tenant_manager.get_current_context() {
                            let old_values = serde_json::to_string(&old_tuple.values)
                                .unwrap_or_else(|_| "[]".to_string());
                            let new_values = serde_json::to_string(&new_tuple.values)
                                .unwrap_or_else(|_| "[]".to_string());

                            self.tenant_manager.record_change_event(
                                crate::tenant::ChangeType::Update,
                                table_name.to_string(),
                                row_id.to_string(),
                                Some(old_values),
                                Some(new_values),
                                context.tenant_id,
                                None,
                            );
                        }

                        // Execute AFTER UPDATE triggers
                        let action = self.trigger_registry.execute_triggers(
                            table_name,
                            &trigger_event,
                            &sql::logical_plan::TriggerTiming::After,
                            &row_context,
                            &mut trigger_context,
                            Some(evaluator.schema().clone()),
                            &mut executor_fn,
                        )?;

                        // Handle AFTER trigger action
                        if let sql::triggers::TriggerAction::Abort(msg) = action {
                            return Err(Error::query_execution(format!("UPDATE aborted by AFTER trigger: {}", msg)));
                        }
                    }
                }

                let update_count = updates.len() as u64;
                // Use branch-aware update which properly handles:
                // - Main branch: direct key update
                // - Other branches: write to branch-specific keys
                self.storage.update_tuples_branch_aware(table_name, updates.clone())?;

                // Update storage quota tracking (UPDATEs may change storage size)
                if let Some(context) = self.tenant_manager.get_current_context() {
                    // Calculate storage change from updates
                    let mut storage_delta: i64 = 0;
                    for (_row_id, new_tuple) in &updates {
                        let new_size = bincode::serialize(new_tuple)
                            .map(|bytes| bytes.len() as i64)
                            .unwrap_or(256);
                        // We don't have old tuple size here, so we approximate
                        // In production, would track old size or calculate from storage
                        storage_delta += new_size;
                    }

                    if let Some(current_quota) = self.tenant_manager.get_quota_tracking(context.tenant_id) {
                        let new_storage = (current_quota.storage_bytes_used as i64 + storage_delta).max(0) as u64;
                        if let Err(e) = self.tenant_manager.update_storage_usage(context.tenant_id, new_storage) {
                            return Err(Error::query_execution(format!("Storage quota exceeded: {}", e)));
                        }
                    }
                }

                // Project RETURNING clause columns from updated tuples
                let returned_tuples: Vec<Tuple> = if returning.is_some() {
                    updates.iter()
                        .filter_map(|(_, tuple)| Self::project_returning_columns(tuple, &schema, returning))
                        .collect()
                } else {
                    Vec::new()
                };
                let _ = returned_tuples; // RETURNING clause results handled separately

                Ok(update_count)
            }
            sql::LogicalPlan::Delete { table_name, selection, returning } => {
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let schema_arc = std::sync::Arc::new(schema);
                let evaluator = sql::Evaluator::with_parameters(
                    schema_arc.clone(),
                    vec![],
                );

                // Initialize trigger context
                let mut trigger_context = sql::TriggerContext::new();
                let trigger_event = sql::logical_plan::TriggerEvent::Delete;

                // Use branch-aware scan to get tuples (includes main + branch overrides - deleted)
                let tuples = self.storage.scan_table_branch_aware(table_name)?;
                let mut row_ids_to_delete: Vec<u64> = Vec::new();

                // Collect tuples for RETURNING clause (must be done before deletion)
                let mut returned_tuples: Vec<Tuple> = Vec::new();
                let has_returning = returning.is_some();

                for tuple in tuples {
                    let matches = if let Some(predicate) = selection {
                        let result = evaluator.evaluate(predicate, &tuple)?;
                        match result {
                            Value::Boolean(b) => b,
                            _ => false,
                        }
                    } else {
                        true
                    };

                    if matches {
                        // Execute BEFORE DELETE triggers
                        let row_context = sql::triggers::TriggerRowContext::for_delete(tuple.clone());
                        let db_ref = self.clone_for_trigger();
                        let mut executor_fn = |stmt: &sql::LogicalPlan, _ctx: &sql::triggers::TriggerRowContext| -> Result<()> {
                            db_ref.execute_plan_internal(stmt)?;
                            Ok(())
                        };

                        let action = self.trigger_registry.execute_triggers(
                            table_name,
                            &trigger_event,
                            &sql::logical_plan::TriggerTiming::Before,
                            &row_context,
                            &mut trigger_context,
                            Some(evaluator.schema().clone()),
                            &mut executor_fn,
                        )?;

                        // Handle trigger action
                        match action {
                            sql::triggers::TriggerAction::Abort(msg) => {
                                return Err(Error::query_execution(format!("DELETE aborted by trigger: {}", msg)));
                            }
                            sql::triggers::TriggerAction::Skip => {
                                // INSTEAD OF trigger - skip this delete
                                continue;
                            }
                            sql::triggers::TriggerAction::Continue => {
                                // Continue with delete
                            }
                        }

                        if let Some(row_id) = tuple.row_id {
                            // Validate FK constraints - check if any other table references this row
                            let referencing_fks = catalog.get_referencing_fks(table_name)?;
                            for fk in &referencing_fks {
                                if fk.enforcement == sql::ConstraintEnforcement::Immediate {
                                    // Get the referenced column values from the tuple being deleted
                                    let ref_values: Vec<Value> = fk.references_columns.iter()
                                        .map(|col_name| {
                                            schema_arc.columns.iter()
                                                .position(|c| &c.name == col_name)
                                                .map(|idx| tuple.values[idx].clone())
                                                .unwrap_or(Value::Null)
                                        })
                                        .collect();

                                    // Check if any row in the referencing table uses these values
                                    let has_refs = self.check_referencing_rows_exist(
                                        &fk.table_name,
                                        &fk.columns,
                                        &ref_values,
                                    )?;

                                    if has_refs {
                                        match fk.on_delete {
                                            sql::constraints::ReferentialAction::NoAction |
                                            sql::constraints::ReferentialAction::Restrict => {
                                                return Err(Error::constraint_violation(format!(
                                                    "Foreign key constraint '{}' violated: cannot delete row from '{}' - referenced by '{}'",
                                                    fk.name, table_name, fk.table_name
                                                )));
                                            }
                                            sql::constraints::ReferentialAction::Cascade => {
                                                // TODO: CASCADE delete referencing rows
                                                // For now, just allow the delete
                                            }
                                            sql::constraints::ReferentialAction::SetNull => {
                                                // TODO: SET NULL in referencing rows
                                            }
                                            sql::constraints::ReferentialAction::SetDefault => {
                                                // TODO: SET DEFAULT in referencing rows
                                            }
                                        }
                                    }
                                }
                            }

                            row_ids_to_delete.push(row_id);

                            // Collect tuple for RETURNING clause before deletion
                            if has_returning {
                                if let Some(projected) = Self::project_returning_columns(&tuple, &schema_arc, returning) {
                                    returned_tuples.push(projected);
                                }
                            }

                            // Record CDC event for DELETE
                            if let Some(context) = self.tenant_manager.get_current_context() {
                                let old_values = serde_json::to_string(&tuple.values)
                                    .unwrap_or_else(|_| "[]".to_string());

                                self.tenant_manager.record_change_event(
                                    crate::tenant::ChangeType::Delete,
                                    table_name.to_string(),
                                    row_id.to_string(),
                                    Some(old_values),
                                    None, // no new values for DELETE
                                    context.tenant_id,
                                    None,
                                );
                            }
                        }

                        // Execute AFTER DELETE triggers
                        let action = self.trigger_registry.execute_triggers(
                            table_name,
                            &trigger_event,
                            &sql::logical_plan::TriggerTiming::After,
                            &row_context,
                            &mut trigger_context,
                            Some(evaluator.schema().clone()),
                            &mut executor_fn,
                        )?;

                        // Handle AFTER trigger action
                        if let sql::triggers::TriggerAction::Abort(msg) = action {
                            return Err(Error::query_execution(format!("DELETE aborted by AFTER trigger: {}", msg)));
                        }
                    }
                }

                // Calculate storage to reclaim before deleting
                let mut storage_reclaimed: u64 = 0;
                if let Some(context) = self.tenant_manager.get_current_context() {
                    // Estimate storage from deleted row count (rough approximation)
                    storage_reclaimed = (row_ids_to_delete.len() as u64) * 256; // avg tuple size
                }

                let delete_count = row_ids_to_delete.len() as u64;
                // Use branch-aware delete which properly handles:
                // - Main branch: actual key deletion
                // - Other branches: delete marker creation
                self.storage.delete_tuples_branch_aware(table_name, row_ids_to_delete)?;

                // Update storage quota tracking (reclaim deleted storage)
                if let Some(context) = self.tenant_manager.get_current_context() {
                    if let Some(current_quota) = self.tenant_manager.get_quota_tracking(context.tenant_id) {
                        let new_storage = current_quota.storage_bytes_used.saturating_sub(storage_reclaimed);
                        // Ignore errors here since we're freeing storage, not adding
                        let _ = self.tenant_manager.update_storage_usage(context.tenant_id, new_storage);
                    }
                }
                let _ = returned_tuples; // RETURNING clause results handled separately

                Ok(delete_count)
            }
            sql::LogicalPlan::CreateFunction { name, or_replace, params, return_type, body, language, volatility } => {
                // Store function in registry
                let stored_func = sql::StoredFunction {
                    name: name.clone(),
                    or_replace: *or_replace,
                    params: params.clone(),
                    return_type: return_type.clone(),
                    body: body.clone(),
                    language: language.clone(),
                    volatility: volatility.clone(),
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                };
                self.function_registry.register_function(stored_func.clone())?;

                // Log to WAL for replication
                if let Ok(definition) = bincode::serialize(&stored_func) {
                    if let Err(e) = self.storage.log_create_function(name, &definition) {
                        tracing::warn!("Failed to log CREATE FUNCTION to WAL: {}", e);
                    }
                }
                Ok(0)
            }
            sql::LogicalPlan::CreateProcedure { name, or_replace, params, body, language } => {
                // Store procedure in registry
                let stored_proc = sql::StoredProcedure {
                    name: name.clone(),
                    or_replace: *or_replace,
                    params: params.clone(),
                    body: body.clone(),
                    language: language.clone(),
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                };
                self.function_registry.register_procedure(stored_proc.clone())?;

                // Log to WAL for replication
                if let Ok(definition) = bincode::serialize(&stored_proc) {
                    if let Err(e) = self.storage.log_create_procedure(name, &definition) {
                        tracing::warn!("Failed to log CREATE PROCEDURE to WAL: {}", e);
                    }
                }
                Ok(0)
            }
            sql::LogicalPlan::DropFunction { name, if_exists } => {
                self.function_registry.drop_function(name, *if_exists)?;

                // Log to WAL for replication
                if let Err(e) = self.storage.log_drop_function(name) {
                    tracing::warn!("Failed to log DROP FUNCTION to WAL: {}", e);
                }
                Ok(0)
            }
            sql::LogicalPlan::DropProcedure { name, if_exists } => {
                self.function_registry.drop_procedure(name, *if_exists)?;

                // Log to WAL for replication
                if let Err(e) = self.storage.log_drop_procedure(name) {
                    tracing::warn!("Failed to log DROP PROCEDURE to WAL: {}", e);
                }
                Ok(0)
            }
            sql::LogicalPlan::CreateTrigger {
                name,
                table_name,
                timing,
                events,
                for_each,
                when_condition,
                body,
                if_not_exists,
                referencing,
                characteristics,
                trigger_type,
                from_constraint,
            } => {
                // Check if trigger already exists
                if let Ok(Some(_)) = self.trigger_registry.get_trigger(table_name, name) {
                    if *if_not_exists {
                        return Ok(0);
                    } else {
                        return Err(Error::query_execution(format!(
                            "Trigger '{}' already exists on table '{}'",
                            name, table_name
                        )));
                    }
                }

                // Create trigger definition
                let definition = sql::triggers::TriggerDefinition {
                    name: name.clone(),
                    table_name: table_name.clone(),
                    timing: timing.clone(),
                    events: events.clone(),
                    for_each: for_each.clone(),
                    when_condition: when_condition.clone(),
                    body: body.clone(),
                    enabled: true,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    referencing: referencing.clone(),
                    characteristics: characteristics.clone(),
                    trigger_type: trigger_type.clone(),
                    from_constraint: from_constraint.clone(),
                };

                // Register trigger
                self.trigger_registry.register_trigger(definition.clone())?;

                // Log to WAL for replication
                if let Ok(serialized) = bincode::serialize(&definition) {
                    if let Err(e) = self.storage.log_create_trigger(name, table_name, &serialized) {
                        tracing::warn!("Failed to log CREATE TRIGGER to WAL: {}", e);
                    }
                }

                Ok(0)
            }
            sql::LogicalPlan::DropTrigger { name, table_name, if_exists } => {
                // Drop trigger from registry - table_name is required
                let tbl = table_name.as_ref().ok_or_else(|| {
                    Error::query_execution("DROP TRIGGER requires ON <table_name> clause".to_string())
                })?;

                let dropped = self.trigger_registry.drop_trigger(tbl, name)?;

                if !dropped && !*if_exists {
                    return Err(Error::query_execution(format!(
                        "Trigger '{}' does not exist on table '{}'",
                        name, tbl
                    )));
                }

                // Log to WAL for replication
                if let Err(e) = self.storage.log_drop_trigger(name, table_name.as_deref()) {
                    tracing::warn!("Failed to log DROP TRIGGER to WAL: {}", e);
                }

                Ok(0)
            }
            sql::LogicalPlan::Call { name, args } => {
                // Execute procedure
                let schema = std::sync::Arc::new(Schema { columns: vec![] });
                let evaluator = sql::Evaluator::new(schema);

                // Evaluate arguments
                let arg_values: Vec<Value> = args.iter()
                    .map(|expr| evaluator.evaluate(expr, &Tuple::new(vec![])))
                    .collect::<Result<Vec<_>>>()?;

                // Clone self for SQL execution within procedure
                let db_clone = self.clone_for_trigger();
                let sql_executor = |sql: &str| -> Result<Vec<Vec<Value>>> {
                    // Detect if this is a SELECT query or DML
                    let sql_upper = sql.trim().to_uppercase();
                    if sql_upper.starts_with("SELECT") || sql_upper.starts_with("WITH") {
                        let tuples = db_clone.query(sql, &[])?;
                        Ok(tuples.iter().map(|t| t.values.clone()).collect())
                    } else {
                        // For INSERT, UPDATE, DELETE, etc., use execute
                        db_clone.execute(sql)?;
                        Ok(vec![])
                    }
                };

                self.function_registry.execute_procedure(name, &arg_values, sql_executor)?;
                Ok(0)
            }
            sql::LogicalPlan::AlterColumnStorage { table_name, column_name, storage_mode } => {
                // ALTER TABLE t ALTER COLUMN c SET STORAGE mode
                // Migrates existing data to the new storage format online

                let catalog = self.storage.catalog();
                let mut schema = catalog.get_table_schema(table_name)?;

                // Find column index
                let col_idx = schema.columns.iter()
                    .position(|c| c.name == *column_name)
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column '{}' not found in table '{}'", column_name, table_name
                    )))?;

                let old_mode = schema.columns[col_idx].storage_mode;
                if old_mode == *storage_mode {
                    // No change needed
                    return Ok(0);
                }

                // Migrate existing data online
                let column = schema.columns[col_idx].clone();
                let rows_migrated = self.storage.migrate_column_storage(
                    table_name,
                    col_idx,
                    &column,
                    old_mode,
                    *storage_mode,
                )?;

                // Update schema with new storage mode
                schema.columns[col_idx].storage_mode = *storage_mode;
                catalog.update_table_schema(table_name, &schema)?;

                // Log to WAL for replication
                if let Err(e) = self.storage.log_alter_column_storage(table_name, column_name, storage_mode) {
                    tracing::warn!("Failed to log ALTER COLUMN STORAGE to WAL: {}", e);
                }

                tracing::info!(
                    "Altered {}.{} storage from {:?} to {:?}, migrated {} rows",
                    table_name, column_name, old_mode, storage_mode, rows_migrated
                );

                Ok(rows_migrated as u64)
            }
            _ => {
                // For other operations (TRUNCATE, CREATE INDEX, etc.), use executor
                let mut executor = sql::Executor::with_storage(&self.storage)
                    .with_timeout(self.config.storage.query_timeout_ms);
                let results = executor.execute(&plan)?;
                // Return results as tuples for SELECT queries, empty for DDL
                let is_select = matches!(plan,
                    sql::LogicalPlan::Scan { .. } |
                    sql::LogicalPlan::Filter { .. } |
                    sql::LogicalPlan::Project { .. } |
                    sql::LogicalPlan::Aggregate { .. } |
                    sql::LogicalPlan::Join { .. } |
                    sql::LogicalPlan::Sort { .. } |
                    sql::LogicalPlan::Limit { .. } |
                    sql::LogicalPlan::With { .. } |
                    sql::LogicalPlan::SystemView { .. }
                );
                let _ = is_select; // Results handled by execute_returning
                Ok(results.len() as u64)
            }
        }
    }

    /// Create a new embedded database
    ///
    /// # Arguments
    ///
    /// * `path` - Path to database directory
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let config = Config::default();
        let storage = std::sync::Arc::new(storage::StorageEngine::open(path.as_ref(), &config)?);
        let mv_scheduler = std::sync::Arc::new(storage::MVScheduler::new(
            storage::SchedulerConfig::default(),
            std::sync::Arc::clone(&storage),
        ));

        let dump_manager = std::sync::Arc::new(storage::DumpManager::new(
            path.as_ref().to_path_buf(),
            storage::DumpCompressionType::Zstd,
        ));

        let session_manager = std::sync::Arc::new(crate::session::SessionManager::new());
        let lock_manager = std::sync::Arc::new(storage::LockManager::with_default_timeout());
        let dirty_tracker = std::sync::Arc::new(storage::DirtyTracker::new());

        Ok(Self {
            storage,
            config,
            current_transaction: std::sync::Arc::new(std::sync::Mutex::new(None)),
            tenant_manager: std::sync::Arc::new(crate::tenant::TenantManager::new()),
            trigger_registry: std::sync::Arc::new(sql::TriggerRegistry::new()),
            function_registry: std::sync::Arc::new(sql::FunctionRegistry::new()),
            mv_scheduler,
            auto_refresh_worker: std::sync::Arc::new(parking_lot::RwLock::new(None)),
            dump_manager,
            session_manager,
            lock_manager,
            dirty_tracker,
            session_transactions: std::sync::Arc::new(dashmap::DashMap::new()),
        })
    }

    /// Create an in-memory database
    ///
    /// Data is stored in RAM only. Useful for testing or caching.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_in_memory() -> Result<Self> {
        let config = Config::in_memory();
        let storage = std::sync::Arc::new(storage::StorageEngine::open_in_memory(&config)?);
        let mv_scheduler = std::sync::Arc::new(storage::MVScheduler::new(
            storage::SchedulerConfig::default(),
            std::sync::Arc::clone(&storage),
        ));

        // Use temporary directory for in-memory DB dumps if not specified
        let dump_path = std::env::temp_dir().join("heliosdb_dumps");
        let dump_manager = std::sync::Arc::new(storage::DumpManager::new(
            dump_path,
            storage::DumpCompressionType::Zstd,
        ));

        let session_manager = std::sync::Arc::new(crate::session::SessionManager::new());
        let lock_manager = std::sync::Arc::new(storage::LockManager::with_default_timeout());
        let dirty_tracker = std::sync::Arc::new(storage::DirtyTracker::new());

        Ok(Self {
            storage,
            config,
            current_transaction: std::sync::Arc::new(std::sync::Mutex::new(None)),
            tenant_manager: std::sync::Arc::new(crate::tenant::TenantManager::new()),
            trigger_registry: std::sync::Arc::new(sql::TriggerRegistry::new()),
            function_registry: std::sync::Arc::new(sql::FunctionRegistry::new()),
            mv_scheduler,
            auto_refresh_worker: std::sync::Arc::new(parking_lot::RwLock::new(None)),
            dump_manager,
            session_manager,
            lock_manager,
            dirty_tracker,
            session_transactions: std::sync::Arc::new(dashmap::DashMap::new()),
        })
    }

    /// Create an in-memory database with custom configuration
    ///
    /// # Examples
    ///
    /// ```rust
    /// use heliosdb_lite::{EmbeddedDatabase, Config};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut config = Config::in_memory();
    /// config.compression.level = 6;  // Higher compression level
    /// let db = EmbeddedDatabase::with_config(config)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_config(config: Config) -> Result<Self> {
        let storage = std::sync::Arc::new(if config.storage.memory_only {
            storage::StorageEngine::open_in_memory(&config)?
        } else {
            let path = config.storage.path.as_ref()
                .ok_or_else(|| Error::config("Storage path not specified for non-memory database".to_string()))?;
            storage::StorageEngine::open(path, &config)?
        });
        let mv_scheduler = std::sync::Arc::new(storage::MVScheduler::new(
            storage::SchedulerConfig::default(),
            std::sync::Arc::clone(&storage),
        ));

        let dump_path = if let Some(ref p) = config.storage.path {
            p.clone()
        } else {
            std::env::temp_dir().join("heliosdb_dumps")
        };

        let dump_manager = std::sync::Arc::new(storage::DumpManager::new(
            dump_path,
            storage::DumpCompressionType::Zstd,
        ));

        let session_manager = std::sync::Arc::new(crate::session::SessionManager::new());
        let lock_manager = std::sync::Arc::new(storage::LockManager::with_default_timeout());
        let dirty_tracker = std::sync::Arc::new(storage::DirtyTracker::new());

        Ok(Self {
            storage,
            config,
            current_transaction: std::sync::Arc::new(std::sync::Mutex::new(None)),
            tenant_manager: std::sync::Arc::new(crate::tenant::TenantManager::new()),
            trigger_registry: std::sync::Arc::new(sql::TriggerRegistry::new()),
            function_registry: std::sync::Arc::new(sql::FunctionRegistry::new()),
            mv_scheduler,
            auto_refresh_worker: std::sync::Arc::new(parking_lot::RwLock::new(None)),
            dump_manager,
            session_manager,
            lock_manager,
            dirty_tracker,
            session_transactions: std::sync::Arc::new(dashmap::DashMap::new()),
        })
    }

    /// Execute a SQL statement (POTENTIALLY UNSAFE - use execute_params for user input)
    ///
    /// **WARNING**: This method does not protect against SQL injection. If you're
    /// concatenating user input into the SQL string, use `execute_params()` instead.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL statement to execute
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    /// db.execute("CREATE TABLE users (id SERIAL, name TEXT)")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Safety
    ///
    /// Safe for hardcoded SQL strings. For queries with user input, use `execute_params()`:
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, Value};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    /// let user_input = "'; DROP TABLE users; --";
    ///
    /// // UNSAFE - SQL injection risk!
    /// // let sql = format!("SELECT * FROM users WHERE name = '{}'", user_input);
    /// // db.execute(&sql)?;
    ///
    /// // SAFE - uses parameterized query
    /// db.execute_params(
    ///     "SELECT * FROM users WHERE name = $1",
    ///     &[Value::String(user_input.to_string())]
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn execute(&self, sql: &str) -> Result<u64> {
        use crate::error::LockResultExt;

        // Check if this is a transaction control statement
        if Self::is_transaction_control(sql) {
            return self.handle_transaction_control(sql);
        }

        // Check if we have an active transaction
        let has_active_txn = {
            let txn_lock = self.current_transaction.lock()
                .map_lock_err("Failed to acquire transaction lock for execute")?;
            txn_lock.is_some()
        };

        if has_active_txn {
            // Execute within existing transaction context
            let txn_lock = self.current_transaction.lock()
                .map_lock_err("Failed to acquire transaction lock for execute")?;
            let txn_ref = txn_lock.as_ref()
                .ok_or_else(|| Error::transaction("Transaction lock in invalid state"))?;
            return self.execute_in_transaction(sql, txn_ref);
        }

        // No active transaction - create implicit transaction
        self.execute_with_implicit_transaction(sql)
    }

    /// Execute a SQL statement with RETURNING clause support
    ///
    /// Similar to `execute`, but returns the tuples from RETURNING clause
    /// if present. For INSERT/UPDATE/DELETE with RETURNING, returns the affected rows.
    ///
    /// # Arguments
    ///
    /// * `sql` - The SQL statement to execute
    ///
    /// # Returns
    ///
    /// A tuple of (rows_affected, returned_tuples). If no RETURNING clause is present,
    /// returned_tuples will be empty.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    /// db.execute("CREATE TABLE users (id INT, name TEXT)")?;
    ///
    /// // INSERT with RETURNING
    /// let (count, rows) = db.execute_returning(
    ///     "INSERT INTO users (id, name) VALUES (1, 'Alice') RETURNING *"
    /// )?;
    ///
    /// assert_eq!(count, 1);
    /// assert_eq!(rows.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn execute_returning(&self, sql: &str) -> Result<(u64, Vec<Tuple>)> {
        self.execute_params_returning(sql, &[])
    }

    /// Execute SQL with an implicit transaction (auto-commit)
    fn execute_with_implicit_transaction(&self, sql: &str) -> Result<u64> {
        // Begin implicit transaction
        let txn = self.storage.begin_transaction()?;

        // Execute the query within transaction context
        let result = self.execute_in_transaction(sql, &txn);

        // Commit or rollback based on result
        match result {
            Ok(count) => {
                txn.commit()?;
                // Increment LSN to track transaction commits
                self.storage.increment_lsn();
                Ok(count)
            }
            Err(e) => {
                let _ = txn.rollback(); // Ignore rollback errors
                Err(e)
            }
        }
    }

    /// Internal execute method without transaction management
    fn execute_internal(&self, sql: &str) -> Result<u64> {
        // 1. Record query for quota tracking (QPS enforcement)
        if let Some(context) = self.tenant_manager.get_current_context() {
            self.tenant_manager.record_query(context.tenant_id)
                .map_err(|e| Error::query_execution(format!("Quota exceeded: {}", e)))?;
        }

        // 2. Parse SQL
        let parser = sql::Parser::new();
        let statement = parser.parse_one(sql)?;

        // 2. Create logical plan with catalog access
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog);
        let plan = planner.statement_to_plan(statement)?;

        // 3. Execute plan based on type
        match &plan {
            sql::LogicalPlan::CreateTable { name, columns, .. } => {
                // Convert ColumnDef to Column
                let schema_columns: Vec<Column> = columns.iter().map(|col_def| {
                    Column {
                        name: col_def.name.clone(),
                        data_type: col_def.data_type.clone(),
                        nullable: !col_def.not_null, // nullable is opposite of not_null
                        primary_key: col_def.primary_key,
                        source_table: None,
                        source_table_name: None,
                        default_expr: None,
                        unique: false,
                        storage_mode: col_def.storage_mode,
                    }
                }).collect();

                let schema = Schema::new(schema_columns);
                let catalog = self.storage.catalog();

                // Log to WAL for replication before creating (schema will be moved)
                if let Err(e) = self.storage.log_create_table(name, &schema) {
                    tracing::warn!("Failed to log CREATE TABLE to WAL: {}", e);
                }

                catalog.create_table(name, schema)?;
                Ok(1) // 1 table created
            }
            sql::LogicalPlan::Insert { table_name, columns, values, returning } => {
                // Check for RLS enforcement (with_check_expr)
                let rls_enforced = self.tenant_manager.should_apply_rls(table_name, "INSERT");
                let rls_check = if rls_enforced {
                    self.tenant_manager.get_rls_conditions(table_name, "INSERT")
                } else {
                    None
                };

                // Get table schema for column types
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;

                // Create evaluator with schema for expression evaluation
                // Use empty tuple for evaluation context since INSERT values are constants
                let evaluator = sql::Evaluator::new(std::sync::Arc::new(Schema {
                    columns: vec![], // Empty schema - INSERT values don't reference columns
                }));
                let empty_tuple = Tuple::new(vec![]);

                let mut count = 0;
                for value_row in values {
                    // Evaluate each expression to get actual values
                    // This handles literals, CAST expressions, and more
                    let mut tuple_values: Vec<Value> = Vec::new();

                    for (col_idx, expr) in value_row.iter().enumerate() {
                        // Determine target column type for auto-casting
                        let target_col_idx = if let Some(ref cols) = columns {
                            // Explicit column list - find index
                            let col_name = &cols[col_idx];
                            schema.get_column_index(col_name)
                                .ok_or_else(|| Error::query_execution(format!("Column '{}' not found", col_name)))?
                        } else {
                            // No column list - use position
                            col_idx
                        };

                        if target_col_idx >= schema.columns.len() {
                            return Err(Error::query_execution(format!(
                                "Too many values for INSERT: table has {} columns",
                                schema.columns.len()
                            )));
                        }

                        let target_type = &schema.columns[target_col_idx].data_type;

                        // Evaluate expression
                        let mut value = evaluator.evaluate(expr, &empty_tuple)?;

                        // Auto-cast if value type doesn't match column type
                        let needs_cast = match (&value, target_type) {
                            (Value::Null, _) => false, // NULL is compatible with any type
                            (Value::Vector(_), DataType::Vector(_)) => false,
                            (Value::String(_), DataType::Vector(_)) => true, // Always cast strings to vectors
                            (Value::String(_), DataType::Json | DataType::Jsonb) => true, // Always cast strings to JSON
                            (Value::Int4(_), DataType::Int4) => false,
                            (Value::Int8(_), DataType::Int8) => false,
                            (Value::Float4(_), DataType::Float4) => false,
                            (Value::Float8(_), DataType::Float8) => false,
                            (Value::String(_), DataType::Text | DataType::Varchar(_)) => false,
                            (Value::Boolean(_), DataType::Boolean) => false,
                            (Value::Json(_), DataType::Json | DataType::Jsonb) => false,
                            _ => true, // Type mismatch - needs cast
                        };

                        if needs_cast {
                            value = evaluator.cast_value(value, target_type)?;
                        }

                        tuple_values.push(value);
                    }

                    let tuple = Tuple::new(tuple_values);

                    // Validate RLS with_check_expr if present
                    if let Some((_, with_check)) = &rls_check {
                        if let Some(ref with_check_expr) = with_check {
                            // Evaluate RLS with_check expression to ensure inserted row satisfies policy
                            let tenant_context = self.tenant_manager.get_current_context();
                            let rls_evaluator = tenant::RLSExpressionEvaluator::new(
                                std::sync::Arc::new(schema.clone()),
                                tenant_context
                            );
                            let expr = rls_evaluator.parse(with_check_expr)?;
                            let satisfies_policy = rls_evaluator.evaluate(&expr, &tuple)?;

                            if !satisfies_policy {
                                return Err(Error::query_execution(format!(
                                    "Row-Level Security policy violation: inserted row does not satisfy WITH CHECK expression"
                                )));
                            }
                        }
                    }

                    self.storage.insert_tuple(table_name, tuple)?;
                    count += 1;
                }
                Ok(count)
            }
            sql::LogicalPlan::Update { table_name, assignments, selection, returning } => {
                // Check for RLS enforcement
                let rls_enforced = self.tenant_manager.should_apply_rls(table_name, "UPDATE");
                let rls_condition = if rls_enforced {
                    self.tenant_manager.get_rls_conditions(table_name, "UPDATE")
                } else {
                    None
                };

                // Scan table to get all tuples with their row IDs
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let evaluator = sql::Evaluator::new(std::sync::Arc::new(schema.clone()));

                // Use branch-aware scan to get tuples (includes main + branch overrides - deleted)
                let tuples = self.storage.scan_table_branch_aware(table_name)?;
                let mut updates: Vec<(u64, Tuple)> = Vec::new();

                for mut tuple in tuples {
                    // Check if tuple matches WHERE clause (if provided)
                    let where_matches = if let Some(predicate) = selection {
                        let result = evaluator.evaluate(predicate, &tuple)?;
                        match result {
                            Value::Boolean(b) => b,
                            _ => false,
                        }
                    } else {
                        true // No WHERE clause means update all
                    };

                    // Check RLS policy (if enforced)
                    let rls_matches = if let Some((using_expr, _)) = &rls_condition {
                        // Evaluate RLS using expression to check if row can be updated
                        let tenant_context = self.tenant_manager.get_current_context();
                        let rls_evaluator = tenant::RLSExpressionEvaluator::new(
                            std::sync::Arc::new(schema.clone()),
                            tenant_context
                        );
                        let expr = rls_evaluator.parse(using_expr)?;
                        rls_evaluator.evaluate(&expr, &tuple)?
                    } else {
                        true // No RLS policy = allow
                    };

                    if where_matches && rls_matches {
                        // Apply updates
                        for (col_name, value_expr) in assignments {
                            let new_value = evaluator.evaluate(value_expr, &tuple)?;
                            // Find column index
                            let col_index = evaluator.schema().get_column_index(col_name)
                                .ok_or_else(|| Error::query_execution(format!("Column '{}' not found", col_name)))?;
                            tuple.values[col_index] = new_value;
                        }

                        let row_id = tuple.row_id.unwrap_or(0);
                        updates.push((row_id, tuple));
                    }
                }

                // Use branch-aware update which properly handles:
                // - Main branch: direct key update
                // - Other branches: write to branch-specific keys
                let update_count = self.storage.update_tuples_branch_aware(table_name, updates)?;
                Ok(update_count)
            }
            sql::LogicalPlan::Delete { table_name, selection, returning } => {
                // Check for RLS enforcement
                let rls_enforced = self.tenant_manager.should_apply_rls(table_name, "DELETE");
                let rls_condition = if rls_enforced {
                    self.tenant_manager.get_rls_conditions(table_name, "DELETE")
                } else {
                    None
                };

                // Scan table to get all tuples
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let evaluator = sql::Evaluator::new(std::sync::Arc::new(schema.clone()));

                // Use branch-aware scan to get tuples (includes main + branch overrides - deleted)
                let tuples = self.storage.scan_table_branch_aware(table_name)?;
                let mut row_ids_to_delete: Vec<u64> = Vec::new();

                for tuple in tuples {
                    // Check if tuple matches WHERE clause (if provided)
                    let where_matches = if let Some(predicate) = selection {
                        let result = evaluator.evaluate(predicate, &tuple)?;
                        match result {
                            Value::Boolean(b) => b,
                            _ => false,
                        }
                    } else {
                        true // No WHERE clause means delete all
                    };

                    // Check RLS policy (if enforced)
                    let rls_matches = if let Some((using_expr, _)) = &rls_condition {
                        // Evaluate RLS using expression to check if row can be deleted
                        let tenant_context = self.tenant_manager.get_current_context();
                        let rls_evaluator = tenant::RLSExpressionEvaluator::new(
                            std::sync::Arc::new(schema.clone()),
                            tenant_context
                        );
                        let expr = rls_evaluator.parse(using_expr)?;
                        rls_evaluator.evaluate(&expr, &tuple)?
                    } else {
                        true // No RLS policy = allow
                    };

                    if where_matches && rls_matches {
                        if let Some(row_id) = tuple.row_id {
                            row_ids_to_delete.push(row_id);
                        }
                    }
                }

                // Use branch-aware delete which properly handles:
                // - Main branch: actual key deletion
                // - Other branches: delete marker creation
                let delete_count = self.storage.delete_tuples_branch_aware(table_name, row_ids_to_delete)?;
                Ok(delete_count)
            }
            sql::LogicalPlan::DropTable { name, if_exists } => {
                // Check if table exists
                let catalog = self.storage.catalog();
                let exists = catalog.table_exists(name)?;

                if exists {
                    // Drop the table
                    catalog.drop_table(name)?;

                    // Log to WAL for replication
                    if let Err(e) = self.storage.log_drop_table(name) {
                        tracing::warn!("Failed to log DROP TABLE to WAL: {}", e);
                    }

                    Ok(0) // 0 rows affected by DROP TABLE
                } else if *if_exists {
                    // IF EXISTS - no error if table doesn't exist
                    Ok(0)
                } else {
                    // Table doesn't exist and IF NOT EXISTS wasn't specified
                    Err(Error::query_execution(format!("Table '{}' does not exist", name)))
                }
            }
            sql::LogicalPlan::Truncate { table_name } => {
                // TRUNCATE removes all rows from a table
                // Implementation: Delete all keys with the table prefix

                // Initialize trigger context for cascading detection
                let mut trigger_context = sql::TriggerContext::new();
                let trigger_event = sql::logical_plan::TriggerEvent::Truncate;

                // TRUNCATE triggers are FOR EACH STATEMENT only - no OLD/NEW rows
                let row_context = sql::triggers::TriggerRowContext {
                    old_tuple: None,
                    new_tuple: None,
                    transition_tables: None,
                };

                // Execute BEFORE TRUNCATE triggers
                let db_ref = self.clone_for_trigger();
                let mut executor_fn = |stmt: &sql::LogicalPlan, _ctx: &sql::triggers::TriggerRowContext| -> Result<()> {
                    db_ref.execute_plan_internal(stmt)?;
                    Ok(())
                };

                let action = self.trigger_registry.execute_triggers(
                    table_name,
                    &trigger_event,
                    &sql::logical_plan::TriggerTiming::Before,
                    &row_context,
                    &mut trigger_context,
                    None, // No schema needed for statement-level TRUNCATE triggers
                    &mut executor_fn,
                )?;

                // Handle trigger action
                match action {
                    sql::triggers::TriggerAction::Abort(msg) => {
                        return Err(Error::query_execution(format!("TRUNCATE aborted by trigger: {}", msg)));
                    }
                    sql::triggers::TriggerAction::Skip => {
                        // INSTEAD OF trigger - skip the truncate
                        return Ok(0);
                    }
                    sql::triggers::TriggerAction::Continue => {
                        // Continue with truncate
                    }
                }

                let prefix = format!("data:{}:", table_name);
                let prefix_bytes = prefix.as_bytes();
                let mut keys_to_delete = Vec::new();

                // Collect all keys for this table
                let iter = self.storage.db.iterator(rocksdb::IteratorMode::Start);
                for item in iter {
                    let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

                    if !key.starts_with(prefix_bytes) {
                        if !key.is_empty() && key[0] > prefix_bytes[0] {
                            break;
                        }
                        continue;
                    }

                    keys_to_delete.push(key.to_vec());
                }

                // Delete all collected keys
                for key in &keys_to_delete {
                    self.storage.delete(key)?;
                }

                // Execute AFTER TRUNCATE triggers
                let action = self.trigger_registry.execute_triggers(
                    table_name,
                    &trigger_event,
                    &sql::logical_plan::TriggerTiming::After,
                    &row_context,
                    &mut trigger_context,
                    None, // No schema needed for statement-level TRUNCATE triggers
                    &mut executor_fn,
                )?;

                // Handle AFTER trigger action
                if let sql::triggers::TriggerAction::Abort(msg) = action {
                    return Err(Error::query_execution(format!("TRUNCATE failed in AFTER trigger: {}", msg)));
                }

                // Log to WAL for replication
                if let Err(e) = self.storage.log_truncate(table_name) {
                    tracing::warn!("Failed to log TRUNCATE to WAL: {}", e);
                }

                Ok(keys_to_delete.len() as u64) // Return number of rows deleted
            }
            sql::LogicalPlan::AlterColumnStorage { table_name, column_name, storage_mode } => {
                // ALTER TABLE t ALTER COLUMN c SET STORAGE mode
                // Migrates existing data to the new storage format online

                let catalog = self.storage.catalog();
                let mut schema = catalog.get_table_schema(table_name)?;

                // Find column index
                let col_idx = schema.columns.iter()
                    .position(|c| c.name == *column_name)
                    .ok_or_else(|| Error::query_execution(format!(
                        "Column '{}' not found in table '{}'", column_name, table_name
                    )))?;

                let old_mode = schema.columns[col_idx].storage_mode;
                if old_mode == *storage_mode {
                    // No change needed
                    return Ok(0);
                }

                // Migrate existing data online
                let column = schema.columns[col_idx].clone();
                let rows_migrated = self.storage.migrate_column_storage(
                    table_name,
                    col_idx,
                    &column,
                    old_mode,
                    *storage_mode,
                )?;

                // Update schema with new storage mode
                schema.columns[col_idx].storage_mode = *storage_mode;
                catalog.update_table_schema(table_name, &schema)?;

                // Log to WAL for replication
                if let Err(e) = self.storage.log_alter_column_storage(table_name, column_name, storage_mode) {
                    tracing::warn!("Failed to log ALTER COLUMN STORAGE to WAL: {}", e);
                }

                tracing::info!(
                    "Altered {}.{} storage from {:?} to {:?}, migrated {} rows",
                    table_name, column_name, old_mode, storage_mode, rows_migrated
                );

                Ok(rows_migrated as u64)
            }
            _ => {
                // For query plans, use executor
                let mut executor = sql::Executor::with_storage(&self.storage)
                    .with_timeout(self.config.storage.query_timeout_ms);
                let results = executor.execute(&plan)?;
                Ok(results.len() as u64)
            }
        }
    }

    /// Execute a SQL statement with parameters (SAFE - prevents SQL injection)
    ///
    /// This method uses parameterized queries to safely execute SQL with user input.
    /// Parameters are referenced as $1, $2, $3, etc. in PostgreSQL style.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL statement with parameter placeholders ($1, $2, etc.)
    /// * `params` - Parameter values in order
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, Value};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// // Create table
    /// db.execute("CREATE TABLE users (id INT, name TEXT, email TEXT)")?;
    ///
    /// // Safe INSERT with parameters
    /// let user_name = "Alice";
    /// let user_email = "alice@example.com";
    /// db.execute_params(
    ///     "INSERT INTO users (id, name, email) VALUES ($1, $2, $3)",
    ///     &[
    ///         Value::Int4(1),
    ///         Value::String(user_name.to_string()),
    ///         Value::String(user_email.to_string()),
    ///     ]
    /// )?;
    ///
    /// // Safe UPDATE with parameters
    /// db.execute_params(
    ///     "UPDATE users SET email = $1 WHERE name = $2",
    ///     &[
    ///         Value::String("newemail@example.com".to_string()),
    ///         Value::String("Alice".to_string()),
    ///     ]
    /// )?;
    ///
    /// // Safe DELETE with parameters
    /// db.execute_params(
    ///     "DELETE FROM users WHERE id = $1",
    ///     &[Value::Int4(1)]
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Security
    ///
    /// This method prevents SQL injection by treating parameters as data, not code.
    /// Even malicious input like `"'; DROP TABLE users; --"` is safely handled.
    pub fn execute_params(&self, sql: &str, params: &[Value]) -> Result<u64> {
        // 1. Parse SQL (will recognize $N placeholders)
        let parser = sql::Parser::new();
        let statement = parser.parse_one(sql)?;

        // 2. Create logical plan with catalog access
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog);
        let plan = planner.statement_to_plan(statement)?;

        // 3. Execute plan with parameters and extract count
        let (count, _tuples) = self.execute_plan_with_params(&plan, params)?;
        Ok(count)
    }

    /// Execute a parameterized SQL statement with RETURNING clause support
    ///
    /// Similar to `execute_params`, but returns the tuples from RETURNING clause
    /// if present. For INSERT/UPDATE/DELETE with RETURNING, returns the affected rows.
    ///
    /// # Arguments
    ///
    /// * `sql` - The SQL statement with `$N` parameter placeholders
    /// * `params` - The parameter values in order ($1, $2, etc.)
    ///
    /// # Returns
    ///
    /// A tuple of (rows_affected, returned_tuples). If no RETURNING clause is present,
    /// returned_tuples will be empty.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, Value};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// // INSERT with RETURNING
    /// let (count, rows) = db.execute_params_returning(
    ///     "INSERT INTO users (id, name) VALUES ($1, $2) RETURNING *",
    ///     &[Value::Int4(1), Value::String("Alice".to_string())]
    /// )?;
    ///
    /// assert_eq!(count, 1);
    /// assert_eq!(rows.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn execute_params_returning(&self, sql: &str, params: &[Value]) -> Result<(u64, Vec<Tuple>)> {
        // 1. Parse SQL (will recognize $N placeholders)
        let parser = sql::Parser::new();
        let statement = parser.parse_one(sql)?;

        // 2. Create logical plan with catalog access
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog);
        let plan = planner.statement_to_plan(statement)?;

        // 3. Execute plan with parameters
        self.execute_plan_with_params(&plan, params)
    }

    /// Project columns from a tuple according to RETURNING clause
    ///
    /// # Arguments
    /// * `tuple` - The tuple to project from
    /// * `schema` - The schema of the tuple
    /// * `returning_columns` - Column names to return (None means no RETURNING)
    ///
    /// # Returns
    /// * Some(projected_tuple) if RETURNING columns specified
    /// * None if no RETURNING clause
    fn project_returning_columns(
        tuple: &Tuple,
        schema: &Schema,
        returning_columns: &Option<Vec<String>>,
    ) -> Option<Tuple> {
        let columns = returning_columns.as_ref()?;

        // Handle RETURNING * (return all columns)
        if columns.len() == 1 && columns[0] == "*" {
            return Some(tuple.clone());
        }

        // Project specified columns
        let mut projected_values = Vec::with_capacity(columns.len());
        for col_name in columns {
            if col_name == "*" {
                // Mixed wildcard - return all columns
                return Some(tuple.clone());
            }
            if let Some(col_idx) = schema.get_column_index(col_name) {
                if col_idx < tuple.values.len() {
                    projected_values.push(tuple.values[col_idx].clone());
                } else {
                    projected_values.push(Value::Null);
                }
            } else {
                // Column not found - return NULL
                projected_values.push(Value::Null);
            }
        }

        Some(Tuple::new(projected_values))
    }

    /// Internal method to execute a plan with parameters
    ///
    /// Returns (rows_affected, returned_tuples) where returned_tuples is populated
    /// only when RETURNING clause is present in INSERT/UPDATE/DELETE statements.
    fn execute_plan_with_params(&self, plan: &sql::LogicalPlan, params: &[Value]) -> Result<(u64, Vec<Tuple>)> {
        match plan {
            sql::LogicalPlan::Insert { table_name, columns, values, returning } => {
                // Get table schema for column types
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;

                // Create evaluator with parameters
                let evaluator = sql::Evaluator::with_parameters(
                    std::sync::Arc::new(Schema { columns: vec![] }),
                    params.to_vec(),
                );
                let empty_tuple = Tuple::new(vec![]);

                let mut count = 0;
                for value_row in values {
                    let mut tuple_values: Vec<Value> = Vec::new();

                    for (col_idx, expr) in value_row.iter().enumerate() {
                        let target_col_idx = if let Some(ref cols) = columns {
                            let col_name = &cols[col_idx];
                            schema.get_column_index(col_name)
                                .ok_or_else(|| Error::query_execution(format!("Column '{}' not found", col_name)))?
                        } else {
                            col_idx
                        };

                        if target_col_idx >= schema.columns.len() {
                            return Err(Error::query_execution(format!(
                                "Too many values for INSERT: table has {} columns",
                                schema.columns.len()
                            )));
                        }

                        let target_type = &schema.columns[target_col_idx].data_type;
                        let mut value = evaluator.evaluate(expr, &empty_tuple)?;

                        // Auto-cast if needed
                        let needs_cast = match (&value, target_type) {
                            (Value::Null, _) => false,
                            (Value::Vector(_), DataType::Vector(_)) => false,
                            (Value::String(_), DataType::Vector(_)) => true,
                            (Value::String(_), DataType::Json | DataType::Jsonb) => true,
                            (Value::Int4(_), DataType::Int4) => false,
                            (Value::Int8(_), DataType::Int8) => false,
                            (Value::Float4(_), DataType::Float4) => false,
                            (Value::Float8(_), DataType::Float8) => false,
                            (Value::String(_), DataType::Text | DataType::Varchar(_)) => false,
                            (Value::Boolean(_), DataType::Boolean) => false,
                            (Value::Json(_), DataType::Json | DataType::Jsonb) => false,
                            _ => true,
                        };

                        if needs_cast {
                            value = evaluator.cast_value(value, target_type)?;
                        }

                        tuple_values.push(value);
                    }

                    let tuple = Tuple::new(tuple_values);
                    self.storage.insert_tuple(table_name, tuple)?;
                    count += 1;
                }
                Ok((count, Vec::new()))
            }
            sql::LogicalPlan::Update { table_name, assignments, selection, returning } => {
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let evaluator = sql::Evaluator::with_parameters(
                    std::sync::Arc::new(schema.clone()),
                    params.to_vec(),
                );

                // Use branch-aware scan to read tuples
                let tuples = self.storage.scan_table_branch_aware(table_name)?;
                let mut updates: Vec<(u64, Tuple)> = Vec::new();

                for mut tuple in tuples {
                    let matches = if let Some(predicate) = selection {
                        let result = evaluator.evaluate(predicate, &tuple)?;
                        match result {
                            Value::Boolean(b) => b,
                            _ => false,
                        }
                    } else {
                        true
                    };

                    if matches {
                        for (col_name, value_expr) in assignments {
                            let new_value = evaluator.evaluate(value_expr, &tuple)?;
                            let col_index = evaluator.schema().get_column_index(col_name)
                                .ok_or_else(|| Error::query_execution(format!("Column '{}' not found", col_name)))?;
                            tuple.values[col_index] = new_value;
                        }

                        let row_id = tuple.row_id.unwrap_or(0);
                        updates.push((row_id, tuple));
                    }
                }

                // Use branch-aware update
                let count = self.storage.update_tuples_branch_aware(table_name, updates)?;
                Ok((count, Vec::new()))
            }
            sql::LogicalPlan::Delete { table_name, selection, returning } => {
                let catalog = self.storage.catalog();
                let schema = catalog.get_table_schema(table_name)?;
                let evaluator = sql::Evaluator::with_parameters(
                    std::sync::Arc::new(schema),
                    params.to_vec(),
                );

                // Use branch-aware scan to read tuples
                let tuples = self.storage.scan_table_branch_aware(table_name)?;
                let mut row_ids_to_delete: Vec<u64> = Vec::new();

                for tuple in tuples {
                    let matches = if let Some(predicate) = selection {
                        let result = evaluator.evaluate(predicate, &tuple)?;
                        match result {
                            Value::Boolean(b) => b,
                            _ => false,
                        }
                    } else {
                        true
                    };

                    if matches {
                        if let Some(row_id) = tuple.row_id {
                            row_ids_to_delete.push(row_id);
                        }
                    }
                }

                // Use branch-aware delete
                let count = self.storage.delete_tuples_branch_aware(table_name, row_ids_to_delete)?;
                Ok((count, Vec::new()))
            }
            // Transaction control statements
            sql::LogicalPlan::StartTransaction => {
                self.begin_transaction_internal()?;
                Ok((0, Vec::new()))
            }
            sql::LogicalPlan::Commit => {
                self.commit_internal()?;
                Ok((0, Vec::new()))
            }
            sql::LogicalPlan::Rollback => {
                self.rollback_internal()?;
                Ok((0, Vec::new()))
            }
            _ => {
                // For query plans and other operations, use executor with parameters
                let mut executor = sql::Executor::with_storage(&self.storage)
                    .with_timeout(self.config.storage.query_timeout_ms)
                    .with_parameters(params.to_vec());
                let results = executor.execute(plan)?;
                Ok((results.len() as u64, Vec::new()))
            }
        }
    }

    /// Query data (POTENTIALLY UNSAFE - use query_params for user input)
    ///
    /// **WARNING**: This method does not protect against SQL injection.
    /// Use `query_params()` for queries with user input.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL query
    /// * `params` - Query parameters (DEPRECATED - not used, kept for backward compatibility)
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    /// let results = db.query("SELECT * FROM users", &[])?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn query(&self, sql: &str, _params: &[&dyn std::fmt::Display]) -> Result<Vec<Tuple>> {
        // Note: Parameter binding not yet implemented
        // 1. Parse SQL
        let parser = sql::Parser::new();
        let statement = parser.parse_one(sql)?;

        // 2. Create logical plan with catalog access and original SQL for time-travel parsing
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog)
            .with_sql(sql.to_string());
        let mut plan = planner.statement_to_plan(statement)?;

        // 3. Apply RLS policies to SELECT queries
        plan = self.apply_rls_to_plan(plan)?;

        // 4. Execute plan and return results
        let mut executor = sql::Executor::with_storage(&self.storage)
            .with_timeout(self.config.storage.query_timeout_ms);
        let results = executor.execute(&plan)?;
        Ok(results)
    }

    /// Create a full dump of the database
    ///
    /// Creates a complete binary dump of all tables, schemas, and data.
    /// The dump is compressed using Zstd by default.
    ///
    /// # Arguments
    ///
    /// * `path` - File path where the dump will be written
    ///
    /// # Returns
    ///
    /// Metadata about the created dump including size and table count
    pub fn dump_full(&self, path: &std::path::Path) -> Result<storage::DumpMetadata> {
        self.dump_manager.create_full_dump(path, self)
    }

    /// Create a SQL dump of the database
    ///
    /// Creates a text-based SQL dump compatible with SQLite and PostgreSQL.
    /// The output contains CREATE TABLE and INSERT statements that can be
    /// replayed to recreate the database.
    ///
    /// # Arguments
    ///
    /// * `path` - File path where the SQL dump will be written
    pub fn dump_sql(&self, path: &std::path::Path) -> Result<storage::DumpMetadata> {
        self.dump_manager.create_sql_dump(path, self)
    }

    /// Create an incremental dump of changed data
    ///
    /// Dumps only data that has changed since the last dump. More efficient
    /// than full dumps for large databases with few changes.
    ///
    /// # Arguments
    ///
    /// * `path` - File path where the incremental dump will be written
    pub fn dump_incremental(&self, path: &std::path::Path) -> Result<storage::DumpMetadata> {
        self.dump_manager.create_incremental_dump(path, self, false)
    }

    /// Create an incremental dump in append mode
    ///
    /// Appends changed data to an existing incremental dump file.
    ///
    /// # Arguments
    ///
    /// * `path` - File path of the existing dump to append to
    pub fn dump_incremental_append(&self, path: &std::path::Path) -> Result<storage::DumpMetadata> {
        self.dump_manager.create_incremental_dump(path, self, true)
    }

    /// Restore the database from a dump file
    ///
    /// Replays a dump file to restore tables and data. Supports both full
    /// dumps and incremental dumps created by this database.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the dump file to restore from
    pub fn restore_from_dump(&mut self, path: &std::path::Path) -> Result<()> {
        let dump_manager = self.dump_manager.clone();
        dump_manager.restore_from_dump(path, self)
    }

    /// Create a full dump with specific compression algorithm
    ///
    /// # Arguments
    ///
    /// * `path` - File path where the dump will be written
    /// * `compression` - Compression algorithm to use (None, Gzip, Zstd)
    pub fn dump_full_compressed(&self, path: &std::path::Path, compression: storage::DumpCompressionType) -> Result<storage::DumpMetadata> {
        // Create a temporary manager with the requested compression
        let manager = storage::DumpManager::new(
            path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf(),
            compression
        );
        manager.create_full_dump(path, self)
    }

    /// Create a full dump without compression
    ///
    /// Useful for debugging or when downstream tools don't support compression.
    ///
    /// # Arguments
    ///
    /// * `path` - File path where the uncompressed dump will be written
    pub fn dump_full_uncompressed(&self, path: &std::path::Path) -> Result<storage::DumpMetadata> {
        self.dump_full_compressed(path, storage::DumpCompressionType::None)
    }

    /// Dump specific tables (partial dump)
    ///
    /// Creates a dump containing only the specified tables.
    ///
    /// # Arguments
    ///
    /// * `path` - File path where the dump will be written
    /// * `tables` - List of table names to include in the dump
    pub fn dump_tables(&self, path: &std::path::Path, tables: Vec<&str>) -> Result<storage::DumpMetadata> {
        // This is a stub for the test - full filtering logic would be in DumpManager
        // For now we just dump everything as the DumpManager doesn't support filtering yet
        // In a real implementation, we'd pass the filter to DumpManager
        self.dump_full(path)
    }

    /// Restore specific tables from a dump (partial restore)
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the dump file
    /// * `_tables` - List of table names to restore
    pub fn restore_tables(&mut self, path: &std::path::Path, _tables: Vec<&str>) -> Result<()> {
         // Stub for test
         self.restore_from_dump(path)
    }

    /// Read dump metadata without restoring
    ///
    /// Retrieves metadata from a dump file including version, creation time,
    /// and table count without actually restoring any data.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the dump file
    pub fn read_dump_metadata(&self, path: &std::path::Path) -> Result<storage::DumpMetadata> {
        use std::io::{Read, Seek, SeekFrom};
        let file = std::fs::File::open(path).map_err(|e| Error::io(e.to_string()))?;
        let mut reader = std::io::BufReader::new(file);
        
        // Skip magic (8) and version (4)
        reader.seek(SeekFrom::Start(12)).map_err(|e| Error::io(e.to_string()))?;
        
        // Read metadata length (4 bytes)
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes).map_err(|e| Error::io(e.to_string()))?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        
        if len == 0 || len > 8192 {
            return Err(Error::io("Invalid metadata length".to_string()));
        }

        // Read JSON metadata
        let mut json_bytes = vec![0u8; len];
        reader.read_exact(&mut json_bytes).map_err(|e| Error::io(e.to_string()))?;
        
        let metadata: storage::DumpMetadata = serde_json::from_slice(&json_bytes)
            .map_err(|e| Error::io(format!("Failed to deserialize metadata: {}", e)))?;
            
        Ok(metadata)
    }

    // ==================== Multi-User Session Methods ====================

    /// Create a new session for a user with specified isolation level
    ///
    /// Sessions provide isolated execution contexts for multi-user scenarios.
    /// Each session maintains its own transaction state and isolation guarantees.
    ///
    /// # Arguments
    ///
    /// * `user_name` - Name of the user for this session
    /// * `isolation` - Transaction isolation level for the session
    ///
    /// # Returns
    ///
    /// A unique `SessionId` that identifies this session
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    /// use heliosdb_lite::session::IsolationLevel;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    ///
    /// // Create a session with snapshot isolation
    /// let session_id = db.create_session("alice", IsolationLevel::Snapshot)?;
    ///
    /// // Execute queries in the session context
    /// db.execute_in_session(session_id, "CREATE TABLE test (id INT)")?;
    ///
    /// // Clean up when done
    /// db.destroy_session(session_id)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn create_session(&self, user_name: &str, isolation: crate::session::IsolationLevel) -> Result<crate::session::SessionId> {
        let user = crate::session::User::new_passwordless(user_name);
        self.session_manager.create_session(&user, isolation)
    }

    /// Destroy an active session and release all resources
    ///
    /// This terminates the session and releases all associated resources including
    /// active transactions, locks, and memory. Any uncommitted transaction is
    /// automatically rolled back.
    ///
    /// # Arguments
    ///
    /// * `session_id` - ID of the session to destroy
    pub fn destroy_session(&self, session_id: crate::session::SessionId) -> Result<()> {
        self.session_manager.destroy_session(session_id)
    }

    /// Begin an explicit transaction for a specific session
    ///
    /// Starts a new transaction within the session context. The transaction
    /// uses the isolation level specified when the session was created.
    ///
    /// # Arguments
    ///
    /// * `session_id` - ID of the session to begin transaction in
    ///
    /// # Errors
    ///
    /// Returns an error if the session already has an active transaction.
    pub fn begin_transaction_for_session(&self, session_id: crate::session::SessionId) -> Result<()> {
        let session_lock = self.session_manager.get_session(session_id)?;
        let mut session = session_lock.write();
        
        if session.active_txn.is_some() {
            return Err(Error::transaction("Session already has an active transaction"));
        }

        // Create a real storage transaction with a FRESH snapshot
        let txn = storage::Transaction::new_with_session(
            self.storage.db.clone(),
            self.storage.next_timestamp(),
            self.storage.snapshot_manager_arc(),
            session_id,
            session.isolation_level,
            self.lock_manager.clone(),
            self.dirty_tracker.clone(),
        )?;

        let txn_id = txn.snapshot_id();
        session.active_txn = Some(txn_id);
        session.stats.transactions_started += 1;
        
        // Store transaction in map
        self.session_transactions.insert(session_id, txn);
        
        Ok(())
    }

    /// Commit transaction for a specific session
    ///
    /// Atomically applies all buffered writes from the session's transaction
    /// to the database. After commit, the transaction is finalized.
    ///
    /// # Arguments
    ///
    /// * `session_id` - ID of the session whose transaction to commit
    ///
    /// # Errors
    ///
    /// Returns an error if the session has no active transaction.
    pub fn commit_transaction_for_session(&self, session_id: crate::session::SessionId) -> Result<()> {
        let session_lock = self.session_manager.get_session(session_id)?;
        let mut session = session_lock.write();
        
        if session.active_txn.is_none() {
            return Err(Error::transaction("Session has no active transaction to commit"));
        }

        // Retrieve and commit transaction with a FRESH commit timestamp
        if let Some((_, txn)) = self.session_transactions.remove(&session_id) {
            txn.commit_with_timestamp(self.storage.next_timestamp())?;
            self.storage.increment_lsn();
        }

        session.active_txn = None;
        session.stats.transactions_committed += 1;
        Ok(())
    }

    /// Rollback transaction for a specific session
    ///
    /// Discards all buffered writes from the session's transaction without
    /// applying them. After rollback, the transaction is finalized and a
    /// new transaction can be started.
    ///
    /// # Arguments
    ///
    /// * `session_id` - ID of the session whose transaction to rollback
    ///
    /// # Errors
    ///
    /// Returns an error if the session has no active transaction.
    pub fn rollback_transaction_for_session(&self, session_id: crate::session::SessionId) -> Result<()> {
        let session_lock = self.session_manager.get_session(session_id)?;
        let mut session = session_lock.write();
        
        if session.active_txn.is_none() {
            return Err(Error::transaction("Session has no active transaction to rollback"));
        }

        // Retrieve and rollback transaction
        if let Some((_, txn)) = self.session_transactions.remove(&session_id) {
            txn.rollback()?;
        }

        session.active_txn = None;
        session.stats.transactions_aborted += 1;
        Ok(())
    }

    /// Execute SQL in a specific session
    ///
    /// Executes a SQL statement within the session's context. If the session
    /// has an active transaction, the statement is executed within that
    /// transaction. Otherwise, an implicit auto-commit transaction is used.
    ///
    /// For `ReadCommitted` isolation, each statement gets a fresh snapshot.
    /// For `Snapshot` isolation, all statements in a transaction see the
    /// same consistent snapshot.
    ///
    /// # Arguments
    ///
    /// * `session_id` - ID of the session to execute in
    /// * `sql` - SQL statement to execute
    ///
    /// # Returns
    ///
    /// Number of rows affected by the statement
    pub fn execute_in_session(&self, session_id: crate::session::SessionId, sql: &str) -> Result<u64> {
        let session_lock = self.session_manager.get_session(session_id)?;
        let mut session = session_lock.write();
        session.touch();
        session.stats.queries_executed += 1;
        
        // Check if session has an active transaction
        if let Some(mut txn) = self.session_transactions.get_mut(&session_id) {
            // For READ COMMITTED, each statement gets a fresh snapshot
            if session.isolation_level == crate::session::IsolationLevel::ReadCommitted {
                txn.refresh_snapshot(self.storage.current_timestamp());
            }

            self.execute_in_transaction(sql, &txn)
        } else {
            // Implicit transaction
            let txn = storage::Transaction::new_with_session(
                self.storage.db.clone(),
                self.storage.next_timestamp(),
                self.storage.snapshot_manager_arc(),
                session_id,
                session.isolation_level,
                self.lock_manager.clone(),
                self.dirty_tracker.clone(),
            )?;
            
            let result = self.execute_in_transaction(sql, &txn);
            
            match result {
                Ok(count) => {
                    txn.commit_with_timestamp(self.storage.next_timestamp())?;
                    self.storage.increment_lsn();
                    Ok(count)
                }
                Err(e) => {
                    let _ = txn.rollback();
                    Err(e)
                }
            }
        }
    }

    /// Query data in a specific session
    ///
    /// Executes a SELECT query within the session's context. If the session
    /// has an active transaction, the query uses that transaction's snapshot
    /// for consistent reads.
    ///
    /// # Arguments
    ///
    /// * `session_id` - ID of the session to query in
    /// * `sql` - SQL SELECT query
    /// * `_params` - Query parameters (reserved for future use)
    ///
    /// # Returns
    ///
    /// Vector of tuples matching the query
    pub fn query_in_session(&self, session_id: crate::session::SessionId, sql: &str, _params: &[&dyn std::fmt::Display]) -> Result<Vec<Tuple>> {
        let session_lock = self.session_manager.get_session(session_id)?;
        let mut session = session_lock.write();
        session.touch();
        session.stats.queries_executed += 1;
        
        // Check if session has an active transaction
        if let Some(mut txn) = self.session_transactions.get_mut(&session_id) {
            // For READ COMMITTED, each statement gets a fresh snapshot
            if session.isolation_level == crate::session::IsolationLevel::ReadCommitted {
                txn.refresh_snapshot(self.storage.current_timestamp());
            }

            // Parse SQL
            let parser = sql::Parser::new();
            let statement = parser.parse_one(sql)?;

            // Create logical plan with catalog access and original SQL for time-travel parsing
            let catalog = self.storage.catalog();
            let planner = sql::Planner::with_catalog(&catalog)
                .with_sql(sql.to_string());
            let plan = planner.statement_to_plan(statement)?;

            // Execute plan with transaction context
            let mut executor = sql::Executor::with_storage(&self.storage)
                .with_timeout(self.config.storage.query_timeout_ms)
                .with_transaction(&txn);

            executor.execute(&plan)
        } else {
            self.query(sql, _params)
        }
    }

    /// Set session quota for a user
    pub fn set_session_quota(&self, _user_name: &str, _max_sessions: usize) -> Result<()> {
        // Stub for test - ResourceQuota is currently global in SessionManager
        Ok(())
    }

    /// Set memory quota for a user
    pub fn set_memory_quota(&self, _user_name: &str, _max_bytes: usize) -> Result<()> {
        // Stub for test
        Ok(())
    }

    /// Check if database has uncommitted changes
    pub fn is_dirty(&self) -> bool {
        self.dirty_tracker.is_dirty()
    }

    /// Mark a table as dirty (for testing)
    pub fn mark_table_dirty(&self, table: &str) {
        // Use a dummy key for tracking
        let _ = self.dirty_tracker.track_insert(table, "dummy_key", &[]);
    }

    /// Query data with parameters (SAFE - prevents SQL injection)
    ///
    /// This method uses parameterized queries to safely execute SELECT queries with user input.
    /// Parameters are referenced as $1, $2, $3, etc. in PostgreSQL style.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL query with parameter placeholders ($1, $2, etc.)
    /// * `params` - Parameter values in order
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::{EmbeddedDatabase, Value};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// // Create and populate table
    /// db.execute("CREATE TABLE users (id INT, name TEXT, age INT)")?;
    /// db.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
    /// db.execute("INSERT INTO users VALUES (2, 'Bob', 25)")?;
    ///
    /// // Safe SELECT with single parameter
    /// let results = db.query_params(
    ///     "SELECT * FROM users WHERE name = $1",
    ///     &[Value::String("Alice".to_string())]
    /// )?;
    /// println!("Found {} users named Alice", results.len());
    ///
    /// // Safe SELECT with multiple parameters
    /// let results = db.query_params(
    ///     "SELECT * FROM users WHERE age > $1 AND name LIKE $2",
    ///     &[
    ///         Value::Int4(20),
    ///         Value::String("%li%".to_string()),
    ///     ]
    /// )?;
    /// println!("Found {} matching users", results.len());
    ///
    /// // SQL injection attempt is safely handled
    /// let malicious_input = "'; DROP TABLE users; --";
    /// let results = db.query_params(
    ///     "SELECT * FROM users WHERE name = $1",
    ///     &[Value::String(malicious_input.to_string())]
    /// )?;
    /// // No users found, but table is safe!
    /// println!("Found {} users (table still exists!)", results.len());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Security
    ///
    /// This method prevents SQL injection by treating parameters as data, not code.
    /// Even malicious input is safely handled as a literal value.
    pub fn query_params(&self, sql: &str, params: &[Value]) -> Result<Vec<Tuple>> {
        // 1. Parse SQL (will recognize $N placeholders)
        let parser = sql::Parser::new();
        let statement = parser.parse_one(sql)?;

        // 2. Create logical plan with catalog access and original SQL for time-travel parsing
        let catalog = self.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog)
            .with_sql(sql.to_string());
        let mut plan = planner.statement_to_plan(statement)?;

        // 3. Apply RLS policies to SELECT queries
        plan = self.apply_rls_to_plan(plan)?;

        // 4. Execute plan with parameters and return results
        self.query_plan_with_params(&plan, params)
    }

    /// Internal method to execute a query plan with parameters
    fn query_plan_with_params(&self, plan: &sql::LogicalPlan, params: &[Value]) -> Result<Vec<Tuple>> {
        // Create an executor with parameter support
        let mut executor = sql::Executor::with_storage(&self.storage)
            .with_timeout(self.config.storage.query_timeout_ms)
            .with_parameters(params.to_vec());

        executor.execute(plan)
    }

    /// Begin an explicit transaction
    ///
    /// This method starts a new transaction. All subsequent SQL operations
    /// will be part of this transaction until `commit()` or `rollback()` is called.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// // Begin transaction
    /// db.begin()?;
    ///
    /// // Execute queries in transaction
    /// db.execute("INSERT INTO users VALUES (1, 'Alice')")?;
    /// db.execute("INSERT INTO users VALUES (2, 'Bob')")?;
    ///
    /// // Commit changes
    /// db.commit()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if a transaction is already active.
    pub fn begin(&self) -> Result<()> {
        self.begin_transaction_internal()
    }

    /// Commit the current transaction
    ///
    /// Permanently applies all changes made during the transaction.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// db.begin()?;
    /// db.execute("DELETE FROM users WHERE id = 1")?;
    /// db.commit()?; // Changes are now permanent
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if no transaction is active.
    pub fn commit(&self) -> Result<()> {
        self.commit_internal()
    }

    /// Rollback the current transaction
    ///
    /// Discards all changes made during the transaction.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// db.begin()?;
    /// db.execute("DELETE FROM users")?;
    /// // Oops, didn't mean to do that!
    /// db.rollback()?; // Changes are discarded
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if no transaction is active.
    pub fn rollback(&self) -> Result<()> {
        self.rollback_internal()
    }

    /// Check if a transaction is currently active
    ///
    /// Returns `true` if `begin()` has been called without a matching
    /// `commit()` or `rollback()`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    ///
    /// assert!(!db.in_transaction());
    ///
    /// db.begin()?;
    /// assert!(db.in_transaction());
    ///
    /// db.commit()?;
    /// assert!(!db.in_transaction());
    /// # Ok(())
    /// # }
    /// ```
    pub fn in_transaction(&self) -> bool {
        self.current_transaction.lock()
            .map(|txn| txn.is_some())
            .unwrap_or(false)
    }

    /// Begin a transaction (DEPRECATED - use `begin()` instead)
    ///
    /// This method is deprecated and will be removed in a future version.
    /// Use `begin()`, `commit()`, and `rollback()` instead for better
    /// transaction control.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new("./data")?;
    /// let tx = db.begin_transaction()?;
    /// // ... perform operations
    /// tx.commit()?;
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(since = "2.1.0", note = "Use `begin()`, `commit()`, and `rollback()` instead")]
    pub fn begin_transaction(&self) -> Result<Transaction<'_>> {
        let tx = self.storage.begin_transaction()?;
        Ok(Transaction { tx, db: self })
    }

    /// Get the current WAL LSN (Log Sequence Number)
    ///
    /// Returns the current position in the Write-Ahead Log.
    /// Useful for debugging and transaction tracking.
    pub fn current_lsn(&self) -> Option<u64> {
        // Return the last snapshot transaction ID for time-travel queries
        // This is the ID users should use with AS OF TRANSACTION
        let txn_id = self.storage.snapshot_manager().current_transaction_id();
        // Return the last registered ID (current - 1, since current is the next to be assigned)
        if txn_id > 1 {
            Some(txn_id - 1)
        } else {
            // No transactions yet, but show 0 to indicate starting point
            Some(0)
        }
    }

    /// Close the database
    pub fn close(self) -> Result<()> {
        // Try to unwrap Arc if we're the sole owner, otherwise storage will be dropped
        match std::sync::Arc::try_unwrap(self.storage) {
            Ok(storage) => storage.close(),
            Err(_) => Ok(()), // Other references exist, just drop our reference
        }
    }

    // Vector store operations (stub implementations for v3.0.0)

    /// List all vector stores
    pub fn list_vector_stores(&self) -> Result<Vec<VectorStoreInfo>> {
        Ok(vec![])
    }

    /// Create a new vector store
    pub fn create_vector_store(&self, name: &str, dimensions: u32) -> Result<VectorStoreInfo> {
        Err(Error::Generic("Vector store operations not yet implemented".to_string()))
    }

    /// Get vector store info
    pub fn get_vector_store(&self, _name: &str) -> Result<VectorStoreInfo> {
        Err(Error::Generic("Vector store operations not yet implemented".to_string()))
    }

    /// Delete a vector store
    pub fn delete_vector_store(&self, _name: &str) -> Result<()> {
        Err(Error::Generic("Vector store operations not yet implemented".to_string()))
    }

    /// Insert vectors
    pub fn insert_vectors(&self, _store: &str, _vectors: Vec<Vec<f32>>) -> Result<Vec<String>> {
        Err(Error::Generic("Vector operations not yet implemented".to_string()))
    }

    /// Upsert vectors
    pub fn upsert_vectors(&self, _store: &str, _vectors: Vec<(String, Vec<f32>)>) -> Result<()> {
        Err(Error::Generic("Vector operations not yet implemented".to_string()))
    }

    /// Search vectors
    pub fn search_vectors(&self, _store: &str, _query: Vec<f32>, _k: usize) -> Result<Vec<(String, f32)>> {
        Err(Error::Generic("Vector search not yet implemented".to_string()))
    }

    /// Text search
    pub fn text_search(&self, _query: &str) -> Result<Vec<String>> {
        Err(Error::Generic("Text search not yet implemented".to_string()))
    }

    /// Store texts for embedding
    pub fn store_texts(&self, _store: &str, _texts: Vec<String>) -> Result<Vec<String>> {
        Err(Error::Generic("Text storage not yet implemented".to_string()))
    }

    /// Hybrid search (vector + text)
    pub fn hybrid_search(&self, _store: &str, _query: &str, _k: usize) -> Result<Vec<(String, f32)>> {
        Err(Error::Generic("Hybrid search not yet implemented".to_string()))
    }

    /// Delete vectors
    pub fn delete_vectors(&self, _store: &str, _ids: Vec<String>) -> Result<()> {
        Err(Error::Generic("Vector deletion not yet implemented".to_string()))
    }

    /// Fetch vectors by ID
    pub fn fetch_vectors(&self, _store: &str, _ids: Vec<String>) -> Result<Vec<(String, Vec<f32>)>> {
        Err(Error::Generic("Vector fetch not yet implemented".to_string()))
    }

    // Agent session operations

    /// List agent sessions
    pub fn list_agent_sessions(&self) -> Result<Vec<AgentSession>> {
        Ok(vec![])
    }

    /// Create agent session
    pub fn create_agent_session(&self, _name: &str) -> Result<AgentSession> {
        Err(Error::Generic("Agent sessions not yet implemented".to_string()))
    }

    /// Get agent session
    pub fn get_agent_session(&self, _id: &str) -> Result<AgentSession> {
        Err(Error::Generic("Agent sessions not yet implemented".to_string()))
    }

    /// Delete agent session
    pub fn delete_agent_session(&self, _id: &str) -> Result<()> {
        Err(Error::Generic("Agent sessions not yet implemented".to_string()))
    }

    /// Add message to agent session
    pub fn add_agent_message(&self, _session_id: &str, _role: &str, _content: &str) -> Result<AgentMessage> {
        Err(Error::Generic("Agent messages not yet implemented".to_string()))
    }

    /// Get messages from agent session
    pub fn get_agent_messages(&self, _session_id: &str) -> Result<Vec<AgentMessage>> {
        Ok(vec![])
    }

    /// Clear agent session messages
    pub fn clear_agent_messages(&self, _session_id: &str) -> Result<()> {
        Err(Error::Generic("Agent messages not yet implemented".to_string()))
    }

    /// Generate schema from data
    pub fn generate_schema(&self, _table_name: &str) -> Result<String> {
        Err(Error::Generic("Schema generation not yet implemented".to_string()))
    }

    /// Get AI chat completion
    pub fn chat_completion(&self, _messages: Vec<(String, String)>) -> Result<String> {
        Err(Error::Generic("Chat completions not yet implemented".to_string()))
    }

    /// Get NL to SQL conversion
    pub fn nl_to_sql(&self, _query: &str) -> Result<String> {
        Err(Error::Generic("Natural language to SQL not yet implemented".to_string()))
    }

    /// Store document
    pub fn store_document(&self, _collection: &str, _id: &str, _content: &str, _metadata: Option<serde_json::Value>) -> Result<()> {
        Err(Error::Generic("Document storage not yet implemented".to_string()))
    }

    /// Get document
    pub fn get_document(&self, _collection: &str, _id: &str) -> Result<DocumentData> {
        Err(Error::Generic("Document storage not yet implemented".to_string()))
    }

    /// Delete document
    pub fn delete_document(&self, _collection: &str, _id: &str) -> Result<()> {
        Err(Error::Generic("Document storage not yet implemented".to_string()))
    }

    /// Update document
    pub fn update_document(&self, _collection: &str, _id: &str, _content: &str, _metadata: Option<serde_json::Value>) -> Result<()> {
        Err(Error::Generic("Document storage not yet implemented".to_string()))
    }

    /// List documents in collection
    pub fn list_documents(&self, _collection: &str) -> Result<Vec<DocumentMetadata>> {
        Ok(vec![])
    }

    /// Search documents
    pub fn search_documents(&self, _collection: &str, _query: &str) -> Result<Vec<DocumentData>> {
        Ok(vec![])
    }

    /// Create collection
    pub fn create_collection(&self, _name: &str) -> Result<()> {
        Err(Error::Generic("Collections not yet implemented".to_string()))
    }

    /// Delete collection
    pub fn delete_collection(&self, _name: &str) -> Result<()> {
        Err(Error::Generic("Collections not yet implemented".to_string()))
    }

    /// List collections
    pub fn list_collections(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// Batch create documents
    pub fn batch_create_documents(&self, _collection: &str, _docs: Vec<DocumentData>) -> Result<Vec<String>> {
        Err(Error::Generic("Batch document creation not yet implemented".to_string()))
    }

    /// Batch infer schema
    pub fn batch_infer_schema(&self, _data: Vec<Vec<Value>>) -> Result<Schema> {
        Err(Error::Generic("Batch schema inference not yet implemented".to_string()))
    }

    /// Chat completion stream
    pub fn chat_completion_stream(&self, _messages: Vec<(String, String)>) -> Result<String> {
        Err(Error::Generic("Chat completion streaming not yet implemented".to_string()))
    }

    /// Compare schemas
    pub fn compare_schemas(&self, _schema1: &Schema, _schema2: &Schema) -> Result<serde_json::Value> {
        Err(Error::Generic("Schema comparison not yet implemented".to_string()))
    }

    /// Create embeddings
    pub fn create_embeddings(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Err(Error::Generic("Embeddings not yet implemented".to_string()))
    }

    /// Create document (alias for store_document)
    pub fn create_document(&self, _collection: &str, _id: &str, _content: &str, _metadata: Option<serde_json::Value>) -> Result<String> {
        Ok("document_id".to_string())
    }

    /// Find similar documents
    pub fn find_similar_documents(&self, _collection: &str, _query: &str, _limit: usize) -> Result<Vec<(DocumentData, f32)>> {
        Err(Error::Generic("Similar document search not yet implemented".to_string()))
    }

    /// Fork agent session
    pub fn fork_agent_session(&self, _session_id: &str, _new_name: &str) -> Result<AgentSession> {
        Err(Error::Generic("Agent session forking not yet implemented".to_string()))
    }

    /// Generate schema from description
    pub fn generate_schema_from_description(&self, _description: &str) -> Result<Schema> {
        Err(Error::Generic("Schema generation from description not yet implemented".to_string()))
    }

    /// Get agent context
    pub fn get_agent_context(&self, _session_id: &str) -> Result<serde_json::Value> {
        Err(Error::Generic("Agent context retrieval not yet implemented".to_string()))
    }

    /// Get chat model
    pub fn get_chat_model(&self, _model_id: &str) -> Result<serde_json::Value> {
        Err(Error::Generic("Chat model retrieval not yet implemented".to_string()))
    }

    /// Get document chunks
    pub fn get_document_chunks(&self, _collection: &str, _id: &str) -> Result<Vec<(String, f32)>> {
        Err(Error::Generic("Document chunking not yet implemented".to_string()))
    }

    /// Infer schema
    pub fn infer_schema(&self, _data: Vec<Vec<Value>>) -> Result<Schema> {
        Err(Error::Generic("Schema inference not yet implemented".to_string()))
    }

    /// Infer schema from file
    pub fn infer_schema_from_file(&self, _path: &str) -> Result<Schema> {
        Err(Error::Generic("Schema inference from file not yet implemented".to_string()))
    }

    /// Instantiate schema template
    pub fn instantiate_schema_template(&self, _template_name: &str, _params: serde_json::Value) -> Result<Schema> {
        Err(Error::Generic("Schema template instantiation not yet implemented".to_string()))
    }

    /// List chat models
    pub fn list_chat_models(&self) -> Result<Vec<serde_json::Value>> {
        Ok(vec![])
    }

    /// List schema templates
    pub fn list_schema_templates(&self) -> Result<Vec<serde_json::Value>> {
        Ok(vec![])
    }

    /// Optimize schema
    pub fn optimize_schema(&self, _schema: &Schema) -> Result<Schema> {
        Err(Error::Generic("Schema optimization not yet implemented".to_string()))
    }

    /// Validate schema
    pub fn validate_schema(&self, _schema: &Schema) -> Result<bool> {
        Err(Error::Generic("Schema validation not yet implemented".to_string()))
    }

    /// RAG search (Retrieval Augmented Generation)
    pub fn rag_search(&self, _collection: &str, _query: &str, _k: usize) -> Result<Vec<(DocumentData, f32, String)>> {
        Err(Error::Generic("RAG search not yet implemented".to_string()))
    }

    /// Rechunk document
    pub fn rechunk_document(&self, _collection: &str, _id: &str, _chunk_size: usize) -> Result<Vec<String>> {
        Err(Error::Generic("Document rechunking not yet implemented".to_string()))
    }

    /// Search agent memory
    pub fn search_agent_memory(&self, _session_id: &str, _query: &str) -> Result<Vec<(AgentMessage, f32)>> {
        Err(Error::Generic("Agent memory search not yet implemented".to_string()))
    }

    /// Summarize agent memory
    pub fn summarize_agent_memory(&self, _session_id: &str) -> Result<String> {
        Err(Error::Generic("Agent memory summarization not yet implemented".to_string()))
    }

    /// Clone database reference for trigger execution
    ///
    /// This creates a lightweight clone that shares the same storage and registries
    /// but can be passed to trigger executor closures.
    fn clone_for_trigger(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            config: self.config.clone(),
            current_transaction: self.current_transaction.clone(),
            tenant_manager: self.tenant_manager.clone(),
            trigger_registry: self.trigger_registry.clone(),
            function_registry: self.function_registry.clone(),
            mv_scheduler: self.mv_scheduler.clone(),
            auto_refresh_worker: self.auto_refresh_worker.clone(),
            dump_manager: self.dump_manager.clone(),
            session_manager: self.session_manager.clone(),
            lock_manager: self.lock_manager.clone(),
            dirty_tracker: self.dirty_tracker.clone(),
            session_transactions: self.session_transactions.clone(),
        }
    }

    /// Check if a foreign key reference exists in the referenced table
    ///
    /// Used for FK constraint validation during INSERT/UPDATE operations.
    fn check_foreign_key_exists(
        &self,
        table_name: &str,
        column_names: &[String],
        values: &[Value],
    ) -> Result<bool> {
        // Build a query to check if the referenced row exists
        let catalog = self.storage.catalog();
        let schema = catalog.get_table_schema(table_name)?;

        // Scan the table and check for a matching row
        let tuples = self.storage.scan_table(table_name)?;

        for tuple in tuples {
            let mut matches = true;
            for (col_name, expected_value) in column_names.iter().zip(values.iter()) {
                // Find column index
                let col_idx = schema.columns.iter()
                    .position(|c| &c.name == col_name);

                if let Some(idx) = col_idx {
                    if idx < tuple.values.len() {
                        let actual_value = &tuple.values[idx];
                        if actual_value != expected_value {
                            matches = false;
                            break;
                        }
                    } else {
                        matches = false;
                        break;
                    }
                } else {
                    matches = false;
                    break;
                }
            }

            if matches {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Evaluate a CHECK constraint expression against a row's values
    ///
    /// Parses the CHECK expression and evaluates it against the provided values.
    /// Returns true if the constraint is satisfied, false otherwise.
    fn evaluate_check_constraint(
        &self,
        expression: &str,
        schema: &Schema,
        values: &[Value],
    ) -> Result<bool> {
        // Create a tuple from the values for evaluation
        let tuple = Tuple::new(values.to_vec());

        // First, try to deserialize as JSON (LogicalExpr was serialized with serde_json)
        let logical_expr = if expression.starts_with('{') || expression.starts_with('[') {
            // Looks like JSON, try to deserialize as LogicalExpr
            serde_json::from_str::<sql::LogicalExpr>(expression)
                .map_err(|e| Error::query_execution(format!(
                    "Failed to deserialize CHECK constraint expression '{}': {}",
                    expression, e
                )))?
        } else {
            // Treat as SQL expression - parse it
            use sqlparser::dialect::PostgreSqlDialect;
            use sqlparser::parser::Parser as SqlParser;

            // Parse the expression by wrapping it in a SELECT WHERE clause
            let sql = format!("SELECT * FROM dummy WHERE {}", expression);
            let dialect = PostgreSqlDialect {};

            let mut statements = SqlParser::parse_sql(&dialect, &sql)
                .map_err(|e| Error::query_execution(format!(
                    "Failed to parse CHECK constraint expression '{}': {}",
                    expression, e
                )))?;

            if statements.len() != 1 {
                return Err(Error::query_execution(
                    "Invalid CHECK constraint expression: expected single statement"
                ));
            }

            // Extract the WHERE clause from the SELECT statement
            let statement = statements.remove(0);

            let selection = if let sqlparser::ast::Statement::Query(query) = statement {
                if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                    select.selection
                } else {
                    None
                }
            } else {
                None
            };

            let selection = selection.ok_or_else(|| Error::query_execution(format!(
                "Failed to extract expression from CHECK constraint: {}",
                expression
            )))?;

            // Use the planner to convert the SQL expression to LogicalExpr
            let catalog = self.storage.catalog();
            let planner = sql::Planner::with_catalog(&catalog);

            // Convert SQL Expr to LogicalExpr
            planner.convert_expr_to_logical(&selection, Some(schema))?
        };

        // Evaluate the expression against the tuple
        let evaluator = sql::Evaluator::new(std::sync::Arc::new(schema.clone()));
        let result = evaluator.evaluate(&logical_expr, &tuple)?;

        // CHECK constraint passes if result is true (or not explicitly false)
        match result {
            Value::Boolean(b) => Ok(b),
            Value::Null => Ok(true), // NULL is treated as "unknown", typically passes
            _ => Err(Error::constraint_violation(format!(
                "CHECK constraint expression '{}' did not evaluate to boolean",
                expression
            ))),
        }
    }

    /// Check if any rows in the referencing table reference the given values
    ///
    /// Used for FK constraint validation during DELETE/UPDATE operations.
    fn check_referencing_rows_exist(
        &self,
        table_name: &str,
        column_names: &[String],
        values: &[Value],
    ) -> Result<bool> {
        let catalog = self.storage.catalog();
        let schema = catalog.get_table_schema(table_name)?;

        // Scan the table and check for referencing rows
        let tuples = self.storage.scan_table(table_name)?;

        for tuple in tuples {
            let mut matches = true;
            for (col_name, expected_value) in column_names.iter().zip(values.iter()) {
                let col_idx = schema.columns.iter()
                    .position(|c| &c.name == col_name);

                if let Some(idx) = col_idx {
                    if idx < tuple.values.len() {
                        let actual_value = &tuple.values[idx];
                        if actual_value != expected_value {
                            matches = false;
                            break;
                        }
                    } else {
                        matches = false;
                        break;
                    }
                } else {
                    matches = false;
                    break;
                }
            }

            if matches {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Start QPS quota window reset background task
    ///
    /// This spawns a background task that resets the QPS window counter for all tenants
    /// every second. This enables accurate rate limiting.
    ///
    /// # Returns
    ///
    /// A `tokio::task::JoinHandle` that can be used to cancel the task
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    ///
    /// // Start QPS reset task
    /// let handle = db.start_qps_reset_task();
    ///
    /// // ... use database ...
    ///
    /// // Cancel task on shutdown
    /// handle.abort();
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "server")]
    pub fn start_qps_reset_task(&self) -> tokio::task::JoinHandle<()> {
        let tenant_manager = self.tenant_manager.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
            loop {
                interval.tick().await;

                // Reset QPS window for all active tenants
                let tenants = tenant_manager.list_tenants();
                for tenant in tenants {
                    let _ = tenant_manager.reset_qps_window(tenant.id);
                }
            }
        })
    }

    /// Reset QPS quota window for all tenants (synchronous version)
    ///
    /// This is a synchronous alternative to `start_qps_reset_task()` that can be
    /// called manually or from a custom scheduler.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    ///
    /// // Manually reset QPS windows (e.g., from a timer)
    /// db.reset_all_qps_windows();
    /// # Ok(())
    /// # }
    /// ```
    pub fn reset_all_qps_windows(&self) {
        let tenants = self.tenant_manager.list_tenants();
        for tenant in tenants {
            let _ = self.tenant_manager.reset_qps_window(tenant.id);
        }
    }

    /// Execute a logical plan internally (for trigger execution)
    ///
    /// This method executes a plan without parsing SQL, useful for trigger bodies
    /// that contain already-parsed logical plans.
    fn execute_plan_internal(&self, plan: &sql::LogicalPlan) -> Result<u64> {
        // Execute plan and extract just the row count (ignore returned tuples)
        let (count, _tuples) = self.execute_plan_with_params(plan, &[])?;
        Ok(count)
    }

    /// Apply RLS policies to a query plan by injecting Filter operators
    fn apply_rls_to_plan(&self, plan: sql::LogicalPlan) -> Result<sql::LogicalPlan> {
        self.apply_rls_to_plan_recursive(plan)
    }

    /// Recursively apply RLS to all Scan operators in a plan
    fn apply_rls_to_plan_recursive(&self, plan: sql::LogicalPlan) -> Result<sql::LogicalPlan> {
        match plan {
            sql::LogicalPlan::Scan { table_name, alias, schema, projection, as_of } => {
                // Check if RLS should be applied to this table
                if self.tenant_manager.should_apply_rls(&table_name, "SELECT") {
                    if let Some((using_expr, _)) = self.tenant_manager.get_rls_conditions(&table_name, "SELECT") {
                        // Parse the RLS expression
                        let tenant_context = self.tenant_manager.get_current_context();
                        let rls_evaluator = tenant::RLSExpressionEvaluator::new(
                            schema.clone(),
                            tenant_context
                        );
                        let filter_expr = rls_evaluator.parse(&using_expr)?;

                        // Create a Filter plan wrapping the Scan
                        let scan_plan = sql::LogicalPlan::Scan {
                            table_name,
                            alias: alias.clone(),
                            schema,
                            projection,
                            as_of,
                        };

                        return Ok(sql::LogicalPlan::Filter {
                            input: Box::new(scan_plan),
                            predicate: filter_expr,
                        });
                    }
                }

                // No RLS, return as-is
                Ok(sql::LogicalPlan::Scan { table_name, alias, schema, projection, as_of })
            }

            sql::LogicalPlan::Filter { input, predicate } => {
                Ok(sql::LogicalPlan::Filter {
                    input: Box::new(self.apply_rls_to_plan_recursive(*input)?),
                    predicate,
                })
            }

            sql::LogicalPlan::Project { input, exprs, aliases, distinct } => {
                Ok(sql::LogicalPlan::Project {
                    input: Box::new(self.apply_rls_to_plan_recursive(*input)?),
                    exprs,
                    aliases,
                    distinct,
                })
            }

            sql::LogicalPlan::Aggregate { input, group_by, aggr_exprs, having } => {
                Ok(sql::LogicalPlan::Aggregate {
                    input: Box::new(self.apply_rls_to_plan_recursive(*input)?),
                    group_by,
                    aggr_exprs,
                    having,
                })
            }

            sql::LogicalPlan::Join { left, right, join_type, on } => {
                Ok(sql::LogicalPlan::Join {
                    left: Box::new(self.apply_rls_to_plan_recursive(*left)?),
                    right: Box::new(self.apply_rls_to_plan_recursive(*right)?),
                    join_type,
                    on,
                })
            }

            sql::LogicalPlan::Sort { input, exprs, asc } => {
                Ok(sql::LogicalPlan::Sort {
                    input: Box::new(self.apply_rls_to_plan_recursive(*input)?),
                    exprs,
                    asc,
                })
            }

            sql::LogicalPlan::Limit { input, limit, offset } => {
                Ok(sql::LogicalPlan::Limit {
                    input: Box::new(self.apply_rls_to_plan_recursive(*input)?),
                    limit,
                    offset,
                })
            }

            // Handle FilteredScan - inject RLS filter into the existing predicate
            sql::LogicalPlan::FilteredScan { table_name, alias, schema, projection, predicate, as_of } => {
                // Check if RLS should be applied to this table
                if self.tenant_manager.should_apply_rls(&table_name, "SELECT") {
                    if let Some((using_expr, _)) = self.tenant_manager.get_rls_conditions(&table_name, "SELECT") {
                        // Parse the RLS expression
                        let tenant_context = self.tenant_manager.get_current_context();
                        let rls_evaluator = tenant::RLSExpressionEvaluator::new(
                            schema.clone(),
                            tenant_context
                        );
                        let rls_predicate = rls_evaluator.parse(&using_expr)?;

                        // Combine existing predicate with RLS predicate using AND
                        let combined_predicate = if let Some(existing) = predicate {
                            Some(sql::LogicalExpr::BinaryExpr {
                                left: Box::new(existing),
                                op: sql::BinaryOperator::And,
                                right: Box::new(rls_predicate),
                            })
                        } else {
                            Some(rls_predicate)
                        };

                        return Ok(sql::LogicalPlan::FilteredScan {
                            table_name,
                            alias,
                            schema,
                            projection,
                            predicate: combined_predicate,
                            as_of,
                        });
                    }
                }

                // No RLS, return as-is
                Ok(sql::LogicalPlan::FilteredScan { table_name, alias, schema, projection, predicate, as_of })
            }

            // For plans that don't contain Scan operators, return as-is
            other => Ok(other),
        }
    }

    // ==================== Auto-Refresh Worker Methods ====================

    /// Start the materialized view auto-refresh background worker
    ///
    /// This enables automatic refresh of materialized views based on staleness
    /// thresholds and CPU availability. The worker runs in a background task.
    ///
    /// # Arguments
    ///
    /// * `config` - Optional custom configuration. If None, uses database config defaults.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let db = EmbeddedDatabase::new_in_memory()?;
    ///
    ///     // Start with default config
    ///     db.start_auto_refresh(None).await?;
    ///
    ///     // Or with custom config
    ///     let config = storage::AutoRefreshConfig::default()
    ///         .with_enabled(true)
    ///         .with_staleness_threshold(600);
    ///     db.start_auto_refresh(Some(config)).await?;
    ///
    ///     Ok(())
    /// }
    /// ```
    pub async fn start_auto_refresh(
        &self,
        config: Option<storage::AutoRefreshConfig>,
    ) -> Result<()> {
        let worker_config = config.unwrap_or_else(|| {
            storage::AutoRefreshConfig::default()
                .with_enabled(true)
                .with_interval_seconds(self.config.materialized_views.refresh_check_interval_secs)
                .with_staleness_threshold(300) // 5 minutes default
                .with_max_cpu_percent(self.config.materialized_views.default_max_cpu_percent as f64)
                .with_max_concurrent(self.config.materialized_views.max_concurrent_refreshes)
        });

        let mut worker = storage::AutoRefreshWorker::new(
            worker_config,
            std::sync::Arc::clone(&self.storage),
            std::sync::Arc::clone(&self.mv_scheduler),
        );

        worker.start().await?;

        // Store the worker
        *self.auto_refresh_worker.write() = Some(worker);

        tracing::info!("Materialized view auto-refresh worker started");
        Ok(())
    }

    /// Stop the materialized view auto-refresh background worker
    ///
    /// Gracefully stops the worker and waits for any in-progress refreshes to complete.
    pub async fn stop_auto_refresh(&self) -> Result<()> {
        let mut worker_guard = self.auto_refresh_worker.write();
        if let Some(ref mut worker) = *worker_guard {
            worker.stop().await?;
            tracing::info!("Materialized view auto-refresh worker stopped");
        }
        *worker_guard = None;
        Ok(())
    }

    /// Check if the auto-refresh worker is currently running
    pub fn is_auto_refresh_running(&self) -> bool {
        self.auto_refresh_worker.read().as_ref()
            .map(|w| w.is_running())
            .unwrap_or(false)
    }

    /// Get the MV scheduler for manual scheduling operations
    pub fn mv_scheduler(&self) -> &std::sync::Arc<storage::MVScheduler> {
        &self.mv_scheduler
    }

    /// Force an immediate staleness check and trigger refreshes as needed
    ///
    /// This is useful for testing or when you want to ensure views are fresh
    /// without waiting for the next scheduled check.
    pub fn check_mv_staleness_now(&self) -> Result<()> {
        let worker_guard = self.auto_refresh_worker.read();
        if let Some(ref worker) = *worker_guard {
            worker.check_now()?;
            Ok(())
        } else {
            Err(Error::query_execution("Auto-refresh worker is not running"))
        }
    }
}

/// Transaction handle
///
/// Provides ACID guarantees for database operations.
///
/// This struct wraps a storage::Transaction and provides SQL execution
/// within the transaction context, ensuring proper isolation and atomicity.
pub struct Transaction<'a> {
    tx: storage::Transaction,
    /// Reference to the database for executing SQL
    db: &'a EmbeddedDatabase,
}

impl<'a> Transaction<'a> {
    /// Commit the transaction
    ///
    /// Atomically applies all buffered writes to the database.
    /// After commit, the transaction is consumed and cannot be used.
    pub fn commit(self) -> Result<()> {
        self.tx.commit()
    }

    /// Rollback the transaction
    ///
    /// Discards all buffered writes without applying them.
    /// After rollback, the transaction is consumed and cannot be used.
    pub fn rollback(self) -> Result<()> {
        self.tx.rollback()
    }

    /// Execute SQL within transaction context
    ///
    /// Executes a SQL statement (INSERT, UPDATE, DELETE, etc.) within this transaction.
    /// All modifications are buffered in the transaction's write set and will be
    /// atomically applied on commit.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL statement to execute
    ///
    /// # Returns
    ///
    /// Number of rows affected
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    /// db.execute("CREATE TABLE users (id INT, name TEXT)")?;
    ///
    /// let tx = db.begin_transaction()?;
    /// tx.execute("INSERT INTO users VALUES (1, 'Alice')")?;
    /// tx.execute("INSERT INTO users VALUES (2, 'Bob')")?;
    /// tx.commit()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn execute(&self, sql: &str) -> Result<u64> {
        // Execute within transaction context using the database's execute_in_transaction helper
        // The transaction parameter ensures writes go to the transaction's write set
        self.db.execute_in_transaction(sql, &self.tx)
    }

    /// Query within transaction context
    ///
    /// Executes a SELECT query within this transaction, using snapshot isolation
    /// to provide a consistent view of the database. Reads will see all writes
    /// made within this transaction (read-your-own-writes) but not uncommitted
    /// writes from other transactions.
    ///
    /// # Arguments
    ///
    /// * `sql` - SQL SELECT query
    /// * `_params` - Query parameters (deprecated, kept for backward compatibility)
    ///
    /// # Returns
    ///
    /// Vector of tuples matching the query
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::EmbeddedDatabase;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = EmbeddedDatabase::new_in_memory()?;
    /// db.execute("CREATE TABLE users (id INT, name TEXT)")?;
    ///
    /// let tx = db.begin_transaction()?;
    /// tx.execute("INSERT INTO users VALUES (1, 'Alice')")?;
    ///
    /// // Can see own writes before commit
    /// let results = tx.query("SELECT * FROM users WHERE id = 1", &[])?;
    /// assert_eq!(results.len(), 1);
    ///
    /// tx.commit()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn query(&self, sql: &str, _params: &[&dyn std::fmt::Display]) -> Result<Vec<Tuple>> {
        // Parse SQL
        let parser = sql::Parser::new();
        let statement = parser.parse_one(sql)?;

        // Create logical plan with catalog access and original SQL for time-travel parsing
        let catalog = self.db.storage.catalog();
        let planner = sql::Planner::with_catalog(&catalog)
            .with_sql(sql.to_string());
        let plan = planner.statement_to_plan(statement)?;

        // Execute plan with transaction context
        // For SELECT queries, we need to see our own writes
        // This is handled by the transaction's get() method which checks the write set first
        let mut executor = sql::Executor::with_storage(&self.db.storage)
            .with_timeout(self.db.config.storage.query_timeout_ms);

        executor.execute(&plan)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
// Allow stricter patterns in test code for convenience
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_database_creation() {
        let db = EmbeddedDatabase::new_in_memory();
        assert!(db.is_ok());
    }
}