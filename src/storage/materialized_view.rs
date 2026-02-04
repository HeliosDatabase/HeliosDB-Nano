//! Materialized View storage and catalog management
//!
//! This module implements metadata storage and catalog management for materialized views.
//! It provides functionality for storing view definitions, tracking staleness, and managing
//! the lifecycle of materialized views.

use crate::{Result, Error, Schema, Tuple};
use crate::sql::LogicalPlan;
use super::StorageEngine;
use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
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
            Some(data) => {
                bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize MV metadata: {}", e)))
            }
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

        tracing::info!("Successfully dropped materialized view '{}'", view_name);
        Ok(())
    }

    /// List all materialized views
    pub fn list_views(&self) -> Result<Vec<String>> {
        tracing::debug!("Listing all materialized views");

        let prefix = b"meta:mv:";
        let mut views = Vec::new();

        let iter = self.storage.db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                if !key.is_empty() && key[0] > prefix[0] {
                    break;
                }
                continue;
            }

            // Extract view name from key
            let view_name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
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
        tracing::info!("Storing data for materialized view '{}' ({} rows)", view_name, tuples.len());

        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();

        // Create or recreate the data table
        if catalog.table_exists(&data_table)? {
            catalog.drop_table(&data_table)?;
        }
        catalog.create_table(&data_table, schema.clone())?;

        // Insert all tuples
        let row_count = tuples.len() as u64;
        for tuple in tuples {
            self.storage.insert_tuple(&data_table, tuple)?;
        }

        tracing::info!("Successfully stored {} rows for materialized view '{}'", row_count, view_name);
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
            view_name, tuples.len()
        );

        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();

        // Generate unique temporary table name using timestamp
        let timestamp = Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let temp_table = format!("{}__temp_{}", data_table, timestamp);
        let backup_table = format!("{}__old_{}", data_table, timestamp);

        tracing::debug!(
            "Using temporary table '{}' for concurrent refresh",
            temp_table
        );

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
                    idx, temp_table, e
                );

                // Cleanup: drop temporary table
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!(
                        "Failed to cleanup temporary table '{}' after insert error: {}",
                        temp_table, cleanup_err
                    );
                }

                return Err(e);
            }
        }

        tracing::debug!(
            "Populated temporary table '{}' with {} rows",
            temp_table, row_count
        );

        // Step 3: Atomic swap using rename operations
        // This is the critical section where we swap the tables

        // Check if the main table exists
        let table_exists = match catalog.table_exists(&data_table) {
            Ok(exists) => exists,
            Err(e) => {
                tracing::error!("Failed to check if table '{}' exists: {}", data_table, e);

                // Cleanup temporary table
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!(
                        "Failed to cleanup temporary table '{}': {}",
                        temp_table, cleanup_err
                    );
                }

                return Err(e);
            }
        };

        if table_exists {
            // Rename: old table -> backup table
            if let Err(e) = catalog.rename_table(&data_table, &backup_table) {
                tracing::error!(
                    "Failed to rename '{}' to '{}': {}",
                    data_table, backup_table, e
                );

                // Cleanup temporary table
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!(
                        "Failed to cleanup temporary table '{}': {}",
                        temp_table, cleanup_err
                    );
                }

                return Err(e);
            }
            tracing::debug!("Renamed '{}' to '{}'", data_table, backup_table);
        }

        // Rename: temp table -> main table
        if let Err(e) = catalog.rename_table(&temp_table, &data_table) {
            tracing::error!(
                "CRITICAL: Failed to rename '{}' to '{}': {}",
                temp_table, data_table, e
            );

            // Attempt to restore original state if old table was renamed
            if table_exists {
                tracing::info!("Attempting to restore original table by renaming '{}' back to '{}'",
                    backup_table, data_table);

                if let Err(restore_err) = catalog.rename_table(&backup_table, &data_table) {
                    tracing::error!(
                        "CRITICAL: Failed to restore original table '{}': {}. Manual intervention may be required.",
                        data_table, restore_err
                    );
                } else {
                    tracing::info!("Successfully restored original table '{}'", data_table);
                }
            }

            // Try to cleanup temporary table if it still exists
            if catalog.table_exists(&temp_table).unwrap_or(false) {
                if let Err(cleanup_err) = catalog.drop_table(&temp_table) {
                    tracing::warn!(
                        "Failed to cleanup temporary table '{}': {}",
                        temp_table, cleanup_err
                    );
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
                    backup_table, e
                );
            } else {
                tracing::debug!("Dropped backup table '{}'", backup_table);
            }
        }

        tracing::info!(
            "Successfully stored {} rows for materialized view '{}' (CONCURRENT mode)",
            row_count, view_name
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
    use crate::{Config, Column, DataType, Value};

    #[test]
    fn test_create_and_get_view() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config)
            .expect("Failed to open storage");
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

        mv_catalog.create_view(metadata.clone())
            .expect("Failed to create view");

        // Verify view exists
        assert!(mv_catalog.view_exists("user_summary")
            .expect("Failed to check if view exists"));

        // Verify metadata
        let retrieved = mv_catalog.get_view("user_summary")
            .expect("Failed to get view metadata");
        assert_eq!(retrieved.view_name, "user_summary");
        assert_eq!(retrieved.query_text, metadata.query_text);
        assert_eq!(retrieved.base_tables, vec!["users"]);
    }

    #[test]
    fn test_drop_view() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config)
            .expect("Failed to open storage");
        let mv_catalog = MaterializedViewCatalog::new(&storage);

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
        ]);

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

        mv_catalog.create_view(metadata)
            .expect("Failed to create view");

        assert!(mv_catalog.view_exists("temp_view")
            .expect("Failed to check if view exists"));

        mv_catalog.drop_view("temp_view")
            .expect("Failed to drop view");

        assert!(!mv_catalog.view_exists("temp_view")
            .expect("Failed to check if view exists after drop"));
    }

    #[test]
    fn test_store_and_read_view_data() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config)
            .expect("Failed to open storage");
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

        mv_catalog.create_view(metadata)
            .expect("Failed to create view");

        // Store test data
        let tuples = vec![
            Tuple::new(vec![
                Value::String("Alice".to_string()),
                Value::Int4(30),
            ]),
            Tuple::new(vec![
                Value::String("Bob".to_string()),
                Value::Int4(25),
            ]),
        ];

        let row_count = mv_catalog.store_view_data("test_view", tuples.clone(), &schema)
            .expect("Failed to store view data");
        assert_eq!(row_count, 2);

        // Read back data
        let retrieved = mv_catalog.read_view_data("test_view")
            .expect("Failed to read view data");
        assert_eq!(retrieved.len(), 2);
    }

    #[test]
    fn test_staleness_tracking() {
        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
        ]);

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
