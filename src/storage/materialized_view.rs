//! Materialized View storage and catalog management
//!
//! This module implements metadata storage and catalog management for materialized views.
//! It provides functionality for storing view definitions, tracking staleness, and managing
//! the lifecycle of materialized views.

use super::StorageEngine;
use crate::sql::LogicalPlan;
use crate::{Error, Result, Schema, Tuple};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Metadata for a materialized view
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedViewMetadata {
    /// Unique view name
    pub view_name: String,
    /// SQL query definition that generates the view (for display/debugging)
    pub query_text: String,
    /// Serialized logical plan for re-execution during REFRESH
    /// This stores the bincode-serialized LogicalPlan
    pub query_plan_bytes: Vec<u8>,
    /// List of base tables this view depends on
    pub base_tables: Vec<String>,
    /// Schema of the materialized view result
    pub schema: Schema,
    /// Timestamp when the view was created
    pub created_at: DateTime<Utc>,
    /// Timestamp of last refresh (None if never refreshed)
    pub last_refresh: Option<DateTime<Utc>>,
    /// Refresh strategy: "manual", "auto", "incremental"
    pub refresh_strategy: String,
    /// Number of rows in the materialized view
    pub row_count: Option<u64>,
    /// Additional metadata (options from SQL)
    pub metadata: HashMap<String, String>,
    /// Timestamp of last full refresh (for incremental strategy)
    pub last_full_refresh: Option<DateTime<Utc>>,
    /// Number of deltas applied since last full refresh
    pub delta_count_since_full: u64,
    /// Whether incremental refresh is enabled
    pub incremental_enabled: bool,
}

impl MaterializedViewMetadata {
    /// Create a new materialized view metadata
    pub fn new(
        view_name: String,
        query_text: String,
        query_plan_bytes: Vec<u8>,
        base_tables: Vec<String>,
        schema: Schema,
    ) -> Self {
        Self {
            view_name,
            query_text,
            query_plan_bytes,
            base_tables,
            schema,
            created_at: Utc::now(),
            last_refresh: None,
            refresh_strategy: "manual".to_string(),
            row_count: None,
            metadata: HashMap::new(),
            last_full_refresh: None,
            delta_count_since_full: 0,
            incremental_enabled: false,
        }
    }

    /// Enable incremental refresh for this view
    pub fn enable_incremental(&mut self) {
        self.incremental_enabled = true;
        self.refresh_strategy = "incremental".to_string();
    }

    /// Mark that a full refresh was performed
    pub fn mark_full_refreshed(&mut self, row_count: u64) {
        self.last_refresh = Some(Utc::now());
        self.last_full_refresh = Some(Utc::now());
        self.row_count = Some(row_count);
        self.delta_count_since_full = 0;
    }

    /// Mark that an incremental refresh was performed
    pub fn mark_incremental_refreshed(&mut self, delta_count: u64) {
        self.last_refresh = Some(Utc::now());
        self.delta_count_since_full += delta_count;
    }

    /// Check if incremental refresh is needed
    pub fn needs_full_refresh(&self) -> bool {
        // Force full refresh if:
        // 1. Never had a full refresh
        // 2. Delta count exceeds 50% of row count
        if self.last_full_refresh.is_none() {
            return true;
        }

        if let Some(row_count) = self.row_count {
            if self.delta_count_since_full as f64 > row_count as f64 * 0.5 {
                return true;
            }
        }

        false
    }

    /// Deserialize the stored query plan for re-execution
    pub fn get_query_plan(&self) -> Result<LogicalPlan> {
        bincode::deserialize(&self.query_plan_bytes)
            .map_err(|e| Error::storage(format!("Failed to deserialize query plan: {}", e)))
    }

    /// Check if the view is stale (never been refreshed)
    pub fn is_stale(&self) -> bool {
        self.last_refresh.is_none()
    }

    /// Get staleness in seconds (None if never refreshed)
    pub fn staleness_seconds(&self) -> Option<i64> {
        self.last_refresh.map(|last| {
            let now = Utc::now();
            (now - last).num_seconds()
        })
    }

    /// Update refresh timestamp and row count
    pub fn mark_refreshed(&mut self, row_count: u64) {
        self.last_refresh = Some(Utc::now());
        self.row_count = Some(row_count);
    }
}

/// Materialized view catalog manager
pub struct MaterializedViewCatalog<'a> {
    storage: &'a StorageEngine,
}

impl<'a> MaterializedViewCatalog<'a> {
    /// Create a new materialized view catalog
    pub fn new(storage: &'a StorageEngine) -> Self {
        Self { storage }
    }

    /// Create a new materialized view in the catalog
    pub fn create_view(&self, metadata: MaterializedViewMetadata) -> Result<()> {
        tracing::info!("Creating materialized view '{}' in catalog", metadata.view_name);

        // Check if view already exists
        if self.view_exists(&metadata.view_name)? {
            return Err(Error::query_execution(format!(
                "Materialized view '{}' already exists",
                metadata.view_name
            )));
        }

        // Store metadata
        let key = Self::mv_metadata_key(&metadata.view_name);
        let value = bincode::serialize(&metadata)
            .map_err(|e| Error::storage(format!("Failed to serialize MV metadata: {}", e)))?;

        self.storage.put(&key, &value)?;

        // W1.3: this name now resolves as a materialized view; bump so the
        // existence cache reclassifies it (fast paths must bail on it).
        self.storage.bump_schema_generation();

        tracing::info!("Successfully created materialized view '{}'", metadata.view_name);
        Ok(())
    }

    /// Check if a materialized view exists
    pub fn view_exists(&self, view_name: &str) -> Result<bool> {
        let key = Self::mv_metadata_key(view_name);
        Ok(self.storage.get(&key)?.is_some())
    }

    /// Get materialized view metadata
    pub fn get_view(&self, view_name: &str) -> Result<MaterializedViewMetadata> {
        tracing::debug!("Retrieving metadata for materialized view '{}'", view_name);

        let key = Self::mv_metadata_key(view_name);
        match self.storage.get(&key)? {
            Some(data) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize MV metadata: {}", e))),
            None => Err(Error::query_execution(format!(
                "Materialized view '{}' does not exist",
                view_name
            ))),
        }
    }

    /// Update materialized view metadata (for refresh tracking)
    pub fn update_view(&self, metadata: &MaterializedViewMetadata) -> Result<()> {
        tracing::debug!("Updating metadata for materialized view '{}'", metadata.view_name);

        let key = Self::mv_metadata_key(&metadata.view_name);
        let value = bincode::serialize(metadata)
            .map_err(|e| Error::storage(format!("Failed to serialize MV metadata: {}", e)))?;

        self.storage.put(&key, &value)
    }

    /// Drop a materialized view from the catalog
    pub fn drop_view(&self, view_name: &str) -> Result<()> {
        tracing::info!("Dropping materialized view '{}'", view_name);

        if !self.view_exists(view_name)? {
            return Err(Error::query_execution(format!(
                "Materialized view '{}' does not exist",
                view_name
            )));
        }

        // Delete metadata
        let key = Self::mv_metadata_key(view_name);
        self.storage.delete(&key)?;

        // Delete the data table (MV results are stored as a regular table)
        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();
        if catalog.table_exists(&data_table)? {
            catalog.drop_table(&data_table)?;
        }

        // Invalidate schema cache for the view name itself (may have been cached
        // by catalog.get_table_schema() which falls back to MV lookup)
        self.storage.invalidate_schema_cache(view_name);

        // W1.3: the view no longer exists; bump so the existence cache
        // recomputes this name to `Missing`.
        self.storage.bump_schema_generation();

        tracing::info!("Successfully dropped materialized view '{}'", view_name);
        Ok(())
    }

    /// List all materialized views
    pub fn list_views(&self) -> Result<Vec<String>> {
        tracing::debug!("Listing all materialized views");

        let prefix = b"meta:mv:";
        let mut views = Vec::new();

        // Seek to the `meta:mv:` prefix instead of scanning from keyspace start.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_slice(), rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                break;
            }

            // Extract view name from key
            let view_name = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).to_string();
            views.push(view_name);
        }

        views.sort();
        tracing::debug!("Found {} materialized views", views.len());
        Ok(views)
    }

    /// Store materialized view data
    ///
    /// Stores the query results in a regular table format for easy querying.
    /// The table name is prefixed with "__mv_" to distinguish it from user tables.
    pub fn store_view_data(&self, view_name: &str, tuples: Vec<Tuple>, schema: &Schema) -> Result<u64> {
        tracing::info!(
            "Storing data for materialized view '{}' ({} rows)",
            view_name,
            tuples.len()
        );

        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();

        // Create or recreate the data table. Drop catalog metadata if present, then
        // UNCONDITIONALLY purge any data rows: a prior run may have removed the
        // data-table metadata while leaving `data:__mv_*` rows behind (issue #2 —
        // those orphaned rows were read back instead of the freshly materialized
        // value, so a COUNT(DISTINCT) view returned a stale slice's count). Purging
        // by key range guarantees the re-populated view contains only new rows.
        if catalog.table_exists(&data_table)? {
            catalog.drop_table(&data_table)?;
        }
        self.storage.purge_table_data(&data_table)?;
        catalog.create_table(&data_table, schema.clone())?;

        // Insert all tuples
        let row_count = tuples.len() as u64;
        for tuple in tuples {
            self.storage.insert_tuple(&data_table, tuple)?;
        }

        // Bump AFTER the rows are in place. Readers use `schema_generation` as the
        // "something changed outside my handle" signal for their result caches
        // (`EmbeddedDatabase::reconcile_result_cache_with_storage`); the create/drop
        // above bump it BEFORE the rows land, so without this trailing bump a reader
        // could reconcile against the mid-refresh generation, cache the half-written
        // view, and never invalidate again.
        self.storage.bump_schema_generation();

        tracing::info!(
            "Successfully stored {} rows for materialized view '{}'",
            row_count,
            view_name
        );
        Ok(row_count)
    }

    /// Store materialized view data concurrently (zero downtime refresh)
    ///
    /// This method implements true CONCURRENT refresh using a temporary table
    /// and atomic swap pattern:
    /// 1. Create a temporary table with unique name
    /// 2. Populate the temporary table with new data
    /// 3. Atomically rename: old -> backup, temp -> current
    /// 4. Drop the backup table
    ///
    /// This ensures that queries can continue reading from the old data
    /// during the refresh operation with zero downtime.
    ///
    /// Error handling:
    /// - If any error occurs before the rename, the temporary table is cleaned up
    /// - If rename fails partway through, we attempt to restore the original state
    /// - Cleanup errors are logged but don't fail the operation
    pub fn store_view_data_concurrent(&self, view_name: &str, tuples: Vec<Tuple>, schema: &Schema) -> Result<u64> {
        use chrono::Utc;

        tracing::info!(
            "Storing data for materialized view '{}' CONCURRENTLY ({} rows)",
            view_name,
            tuples.len()
        );

        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();

        // Generate unique temporary table name using timestamp
        let timestamp = Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let temp_table = format!("{}__temp_{}", data_table, timestamp);
        let backup_table = format!("{}__old_{}", data_table, timestamp);

        tracing::debug!("Using temporary table '{}' for concurrent refresh", temp_table);

        // Step 1: Create temporary table with the new data
        if let Err(e) = catalog.create_table(&temp_table, schema.clone()) {
            tracing::error!("Failed to create temporary table '{}': {}", temp_table, e);
            return Err(e);
        }

        // Step 2: Populate temporary table
        let row_count = tuples.len() as u64;
        for (idx, tuple) in tuples.into_iter().enumerate() {
            if let Err(e) = self.storage.insert_tuple(&temp_table, tuple) {
                tracing::error!(
                    "Failed to insert tuple {} into temporary table '{}': {}",
                    idx,
                    temp_table,
                    e
                );

                // Cleanup: drop temporary table
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!(
                        "Failed to cleanup temporary table '{}' after insert error: {}",
                        temp_table,
                        cleanup_err
                    );
                }

                return Err(e);
            }
        }

        tracing::debug!("Populated temporary table '{}' with {} rows", temp_table, row_count);

        // Step 3: Atomic swap using rename operations
        // This is the critical section where we swap the tables

        // Check if the main table exists
        let table_exists = match catalog.table_exists(&data_table) {
            Ok(exists) => exists,
            Err(e) => {
                tracing::error!("Failed to check if table '{}' exists: {}", data_table, e);

                // Cleanup temporary table
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!("Failed to cleanup temporary table '{}': {}", temp_table, cleanup_err);
                }

                return Err(e);
            }
        };

        if table_exists {
            // Rename: old table -> backup table.
            //
            // `rename_table_data_swap`, not `rename_table`: these three renames
            // move a ROW SET under a stable name, they do not rename a relation.
            // The user-visible rename carries the per-table side records
            // (`table_constraints:{t}`, IDENTITY, partition links) with the key,
            // which here would walk whatever `__mv_<view>` owns onto the backup
            // table and drop it with the backup — once per refresh, silently.
            if let Err(e) = catalog.rename_table_data_swap(&data_table, &backup_table) {
                tracing::error!("Failed to rename '{}' to '{}': {}", data_table, backup_table, e);

                // Cleanup temporary table
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!("Failed to cleanup temporary table '{}': {}", temp_table, cleanup_err);
                }

                return Err(e);
            }
            tracing::debug!("Renamed '{}' to '{}'", data_table, backup_table);
        }

        // Rename: temp table -> main table (data swap; see above).
        if let Err(e) = catalog.rename_table_data_swap(&temp_table, &data_table) {
            tracing::error!("CRITICAL: Failed to rename '{}' to '{}': {}", temp_table, data_table, e);

            // Attempt to restore original state if old table was renamed
            if table_exists {
                tracing::info!(
                    "Attempting to restore original table by renaming '{}' back to '{}'",
                    backup_table,
                    data_table
                );

                if let Err(restore_err) = catalog.rename_table_data_swap(&backup_table, &data_table) {
                    tracing::error!(
                        "CRITICAL: Failed to restore original table '{}': {}. Manual intervention may be required.",
                        data_table,
                        restore_err
                    );
                } else {
                    tracing::info!("Successfully restored original table '{}'", data_table);
                }
            }

            // Try to cleanup temporary table if it still exists
            if catalog.table_exists(&temp_table).unwrap_or(false) {
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!("Failed to cleanup temporary table '{}': {}", temp_table, cleanup_err);
                }
            }

            return Err(e);
        }
        tracing::debug!("Renamed '{}' to '{}'", temp_table, data_table);

        // Step 4: Clean up the backup table
        if table_exists {
            if let Err(e) = catalog.drop_table(&backup_table) {
                // Log but don't fail - the refresh succeeded, cleanup is just housekeeping
                tracing::warn!(
                    "Warning: Failed to drop backup table '{}': {}. This may be cleaned up manually.",
                    backup_table,
                    e
                );
            } else {
                tracing::debug!("Dropped backup table '{}'", backup_table);
            }
        }

        // See the trailing bump in `store_view_data`: the LAST generation bump of a
        // refresh must follow the row swap so out-of-handle readers reconcile against
        // a generation that already reflects the new data.
        self.storage.bump_schema_generation();

        tracing::info!(
            "Successfully stored {} rows for materialized view '{}' (CONCURRENT mode)",
            row_count,
            view_name
        );

        Ok(row_count)
    }

    /// Read materialized view data
    pub fn read_view_data(&self, view_name: &str) -> Result<Vec<Tuple>> {
        tracing::debug!("Reading data for materialized view '{}'", view_name);

        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();

        if !catalog.table_exists(&data_table)? {
            return Err(Error::query_execution(format!(
                "Materialized view '{}' has no data (never refreshed)",
                view_name
            )));
        }

        self.storage.scan_table(&data_table)
    }

    /// Get the data table name for a materialized view
    pub fn mv_data_table_name(view_name: &str) -> String {
        format!("__mv_{}", view_name)
    }

    /// Build metadata key for materialized view
    fn mv_metadata_key(view_name: &str) -> Vec<u8> {
        format!("meta:mv:{}", view_name).into_bytes()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, Config, DataType, Value};

    #[test]
    fn test_create_and_get_view() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let mv_catalog = MaterializedViewCatalog::new(&storage);

        let schema = Schema::new(vec![
            Column::new("status", DataType::Text),
            Column::new("count", DataType::Int8),
        ]);

        // Create a dummy plan for testing
        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "users".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let metadata = MaterializedViewMetadata::new(
            "user_summary".to_string(),
            "SELECT status, COUNT(*) FROM users GROUP BY status".to_string(),
            query_plan_bytes,
            vec!["users".to_string()],
            schema.clone(),
        );

        mv_catalog.create_view(metadata.clone()).expect("Failed to create view");

        // Verify view exists
        assert!(mv_catalog
            .view_exists("user_summary")
            .expect("Failed to check if view exists"));

        // Verify metadata
        let retrieved = mv_catalog
            .get_view("user_summary")
            .expect("Failed to get view metadata");
        assert_eq!(retrieved.view_name, "user_summary");
        assert_eq!(retrieved.query_text, metadata.query_text);
        assert_eq!(retrieved.base_tables, vec!["users"]);
    }

    #[test]
    fn test_drop_view() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let mv_catalog = MaterializedViewCatalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        // Create a dummy plan for testing
        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "temp".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let metadata = MaterializedViewMetadata::new(
            "temp_view".to_string(),
            "SELECT id FROM temp".to_string(),
            query_plan_bytes,
            vec!["temp".to_string()],
            schema,
        );

        mv_catalog.create_view(metadata).expect("Failed to create view");

        assert!(mv_catalog
            .view_exists("temp_view")
            .expect("Failed to check if view exists"));

        mv_catalog.drop_view("temp_view").expect("Failed to drop view");

        assert!(!mv_catalog
            .view_exists("temp_view")
            .expect("Failed to check if view exists after drop"));
    }

    #[test]
    fn test_store_and_read_view_data() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let mv_catalog = MaterializedViewCatalog::new(&storage);

        let schema = Schema::new(vec![
            Column::new("name", DataType::Text),
            Column::new("age", DataType::Int4),
        ]);

        // Create a dummy plan for testing
        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "users".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let metadata = MaterializedViewMetadata::new(
            "test_view".to_string(),
            "SELECT name, age FROM users".to_string(),
            query_plan_bytes,
            vec!["users".to_string()],
            schema.clone(),
        );

        mv_catalog.create_view(metadata).expect("Failed to create view");

        // Store test data
        let tuples = vec![
            Tuple::new(vec![Value::String("Alice".to_string()), Value::Int4(30)]),
            Tuple::new(vec![Value::String("Bob".to_string()), Value::Int4(25)]),
        ];

        let row_count = mv_catalog
            .store_view_data("test_view", tuples.clone(), &schema)
            .expect("Failed to store view data");
        assert_eq!(row_count, 2);

        // Read back data
        let retrieved = mv_catalog
            .read_view_data("test_view")
            .expect("Failed to read view data");
        assert_eq!(retrieved.len(), 2);
    }

    /// A CONCURRENT refresh is a three-step key swap: `__mv_v` → `__mv_v__old_<ts>`
    /// (dropped seconds later), `__mv_v__temp_<ts>` → `__mv_v`. It moves the
    /// view's ROW SET under a stable name — it does not rename a relation.
    ///
    /// `Catalog::rename_table` now carries the per-table SIDE records
    /// (`table_constraints:{t}`, the IDENTITY record, the partition links) with
    /// the key, because for a user's `ALTER TABLE … RENAME TO` a record left
    /// behind is a constraint that silently stops enforcing. Applied to this
    /// swap, that same rule walks whatever `__mv_v` owns onto the BACKUP table
    /// and destroys it with the backup — once per refresh, with nothing
    /// reported. So the swap uses `rename_table_data_swap`, which moves
    /// everything that describes the ROWS (schema, counter, data, ART indexes,
    /// triggers, caches) and leaves the records that describe the NAME alone.
    ///
    /// The premise is asserted first: a data table created by `create_table` owns
    /// no side record at all today, so the split changes nothing in practice —
    /// its whole job is that it keeps changing nothing if that ever stops being
    /// true. The planted record stands in for that future.
    #[test]
    fn concurrent_refresh_leaves_the_data_tables_side_records_under_its_own_name() {
        use crate::sql::{TableConstraints, UniqueConstraint};

        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open storage");
        let mv_catalog = MaterializedViewCatalog::new(&storage);
        let catalog = storage.catalog();

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("label", DataType::Text),
        ]);
        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "src".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let metadata = MaterializedViewMetadata::new(
            "swapv".to_string(),
            "SELECT id, label FROM src".to_string(),
            bincode::serialize(&query_plan).unwrap(),
            vec!["src".to_string()],
            schema.clone(),
        );
        mv_catalog.create_view(metadata).expect("create view");

        let data_table = MaterializedViewCatalog::mv_data_table_name("swapv");
        let constraints_key = format!("table_constraints:{}", data_table).into_bytes();

        // First refresh: creates the data table (nothing to swap yet).
        let rows = vec![Tuple::new(vec![Value::Int4(1), Value::String("one".to_string())])];
        assert_eq!(
            mv_catalog
                .store_view_data_concurrent("swapv", rows, &schema)
                .expect("first concurrent refresh"),
            1
        );

        // PREMISE: an MV data table owns no side record of its own. `create_table`
        // writes the schema, the counter and the PK/UNIQUE ART indexes — every
        // writer of a side record is a CREATE/ALTER TABLE arm keyed by a USER
        // table name.
        assert!(
            storage.get(&constraints_key).expect("read back").is_none(),
            "an MV data table gained a constraint record — the swap's assumption no longer holds"
        );
        assert!(
            catalog.list_identity_columns(&data_table).expect("identity").is_empty(),
            "an MV data table gained an IDENTITY record"
        );

        // Now plant one, standing in for any side record a future data table
        // might own, and refresh again — this time through the full
        // data → backup → temp → data swap.
        let mut planted = TableConstraints::new();
        planted.unique_constraints.push(UniqueConstraint::new(
            "mv_swap_marker".to_string(),
            data_table.clone(),
            vec!["id".to_string()],
            false,
        ));
        catalog
            .save_table_constraints(&data_table, &planted)
            .expect("plant a side record");

        let rows = vec![
            Tuple::new(vec![Value::Int4(2), Value::String("two".to_string())]),
            Tuple::new(vec![Value::Int4(3), Value::String("three".to_string())]),
        ];
        assert_eq!(
            mv_catalog
                .store_view_data_concurrent("swapv", rows, &schema)
                .expect("second concurrent refresh"),
            2
        );

        // The refresh itself still works: the new row set is what the view reads.
        let read_back = mv_catalog.read_view_data("swapv").expect("read view data");
        assert_eq!(read_back.len(), 2, "the concurrent swap lost the refreshed rows");
        let mut ids: Vec<i32> = read_back
            .iter()
            .map(|t| match t.values.first() {
                Some(Value::Int4(v)) => *v,
                other => panic!("unexpected id value {other:?}"),
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 3], "the swap did not install the new row set");

        // …and the side record is still under the data table's OWN name — not
        // riding the backup into the drop, and not orphaned under the backup's
        // name either (the backup is dropped, so a record moved there is either
        // destroyed or leaked, once per refresh).
        let records: Vec<(String, Vec<u8>)> = storage
            .meta_blobs_with_prefix("table_constraints:")
            .expect("scan constraint records")
            .into_iter()
            // The view's own family only: `__mv_swapv`, plus the transient
            // `__mv_swapv__old_<ts>` / `__mv_swapv__temp_<ts>` the swap creates.
            .filter(|(owner, _)| owner.starts_with(&data_table))
            .collect();
        let owners: Vec<&str> = records.iter().map(|(suffix, _)| suffix.as_str()).collect();
        assert_eq!(
            owners,
            vec![data_table.as_str()],
            "*** SIDE RECORD LOST *** the MV data table's constraint record followed the rename \
             onto the backup table instead of staying under '{data_table}'"
        );
        let survived: TableConstraints = bincode::deserialize(&records[0].1).expect("decode constraints");
        assert_eq!(
            survived
                .unique_constraints
                .iter()
                .map(|u| u.name.as_str())
                .collect::<Vec<_>>(),
            vec!["mv_swap_marker"],
            "the side record under the data table's name was rewritten by the swap"
        );
    }

    #[test]
    fn test_staleness_tracking() {
        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        // Create a dummy plan for testing
        let query_plan = LogicalPlan::Scan {
            alias: None,
            table_name: "test".to_string(),
            schema: std::sync::Arc::new(schema.clone()),
            projection: None,
            as_of: None,
        };
        let query_plan_bytes = bincode::serialize(&query_plan).unwrap();

        let mut metadata = MaterializedViewMetadata::new(
            "test_view".to_string(),
            "SELECT id FROM test".to_string(),
            query_plan_bytes,
            vec!["test".to_string()],
            schema,
        );

        // Initially stale
        assert!(metadata.is_stale());
        assert!(metadata.staleness_seconds().is_none());

        // Mark as refreshed
        metadata.mark_refreshed(100);
        assert!(!metadata.is_stale());
        assert!(metadata.last_refresh.is_some());
        assert_eq!(metadata.row_count, Some(100));

        // Staleness should be very small (just now)
        let staleness = metadata.staleness_seconds().expect("Should have staleness");
        assert!(staleness >= 0 && staleness < 2); // Less than 2 seconds
    }
}
