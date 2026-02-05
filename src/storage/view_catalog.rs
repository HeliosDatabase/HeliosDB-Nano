//! Regular View catalog management
//!
//! This module implements metadata storage for regular (non-materialized) views.
//! Regular views are virtual tables whose contents are defined by a SQL query.
//! Unlike materialized views, they don't store data - the query is executed
//! at query time by expanding the view definition.

use crate::{Result, Error, Schema};
use super::StorageEngine;
use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};

/// Key prefix for view metadata in storage
const VIEW_METADATA_PREFIX: &str = "__view_metadata__";

/// Metadata for a regular view
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewMetadata {
    /// Unique view name
    pub view_name: String,
    /// SQL query definition (stored as text for expansion)
    pub query_sql: String,
    /// Schema of the view result (column names and types)
    pub schema: Schema,
    /// Timestamp when the view was created
    pub created_at: DateTime<Utc>,
    /// Whether this view was created with OR REPLACE
    pub or_replace: bool,
}

impl ViewMetadata {
    /// Create a new view metadata
    pub fn new(view_name: String, query_sql: String, schema: Schema) -> Self {
        Self {
            view_name,
            query_sql,
            schema,
            created_at: Utc::now(),
            or_replace: false,
        }
    }

    /// Create with OR REPLACE flag
    pub fn with_or_replace(mut self, or_replace: bool) -> Self {
        self.or_replace = or_replace;
        self
    }
}

/// Regular view catalog manager
pub struct ViewCatalog<'a> {
    storage: &'a StorageEngine,
}

impl<'a> ViewCatalog<'a> {
    /// Create a new view catalog
    pub fn new(storage: &'a StorageEngine) -> Self {
        Self { storage }
    }

    /// Generate the storage key for a view
    fn view_key(view_name: &str) -> Vec<u8> {
        format!("{}{}", VIEW_METADATA_PREFIX, view_name).into_bytes()
    }

    /// Create a new view in the catalog
    pub fn create_view(&self, metadata: ViewMetadata, if_not_exists: bool, or_replace: bool) -> Result<()> {
        let key = Self::view_key(&metadata.view_name);

        // Check if view already exists
        if let Some(_existing) = self.storage.get(&key)? {
            if or_replace {
                // Replace existing view
                tracing::info!("Replacing existing view '{}'", metadata.view_name);
            } else if if_not_exists {
                // Silently ignore
                tracing::debug!("View '{}' already exists, IF NOT EXISTS specified", metadata.view_name);
                return Ok(());
            } else {
                return Err(Error::query_execution(format!(
                    "View '{}' already exists",
                    metadata.view_name
                )));
            }
        }

        // Serialize and store
        let value = bincode::serialize(&metadata)
            .map_err(|e| Error::storage(format!("Failed to serialize view metadata: {}", e)))?;

        self.storage.put(&key, &value)?;
        tracing::info!("Created view '{}'", metadata.view_name);

        Ok(())
    }

    /// Check if a view exists
    pub fn view_exists(&self, view_name: &str) -> Result<bool> {
        let key = Self::view_key(view_name);
        Ok(self.storage.get(&key)?.is_some())
    }

    /// Get view metadata
    pub fn get_view(&self, view_name: &str) -> Result<ViewMetadata> {
        let key = Self::view_key(view_name);
        match self.storage.get(&key)? {
            Some(data) => {
                bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize view metadata: {}", e)))
            }
            None => Err(Error::query_execution(format!(
                "View '{}' does not exist",
                view_name
            ))),
        }
    }

    /// Drop a view
    pub fn drop_view(&self, view_name: &str, if_exists: bool) -> Result<()> {
        let key = Self::view_key(view_name);

        if !self.view_exists(view_name)? {
            if if_exists {
                return Ok(());
            } else {
                return Err(Error::query_execution(format!(
                    "View '{}' does not exist",
                    view_name
                )));
            }
        }

        self.storage.delete(&key)?;
        tracing::info!("Dropped view '{}'", view_name);

        Ok(())
    }

    /// List all views
    pub fn list_views(&self) -> Result<Vec<String>> {
        let prefix = VIEW_METADATA_PREFIX.as_bytes();
        let mut views = Vec::new();

        // Iterate over all keys with the view prefix
        let db = self.storage.db();
        let iter = db.prefix_iterator(prefix);
        for item in iter {
            let (key, _value) = item
                .map_err(|e| Error::storage(format!("Failed to iterate views: {}", e)))?;

            // Extract view name from key
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if let Some(name) = key_str.strip_prefix(VIEW_METADATA_PREFIX) {
                    views.push(name.to_string());
                }
            }
        }

        Ok(views)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, DataType};
    use crate::Config;
    use tempfile::tempdir;

    #[test]
    fn test_create_view() {
        let _dir = tempdir().unwrap();
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let catalog = ViewCatalog::new(&storage);

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
        ]);

        let metadata = ViewMetadata::new(
            "test_view".to_string(),
            "SELECT id, name FROM users WHERE active = true".to_string(),
            schema,
        );

        catalog.create_view(metadata, false, false).unwrap();
        assert!(catalog.view_exists("test_view").unwrap());
    }

    #[test]
    fn test_view_or_replace() {
        let _dir = tempdir().unwrap();
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let catalog = ViewCatalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        let metadata1 = ViewMetadata::new(
            "test_view".to_string(),
            "SELECT id FROM t1".to_string(),
            schema.clone(),
        );
        catalog.create_view(metadata1, false, false).unwrap();

        // Should fail without OR REPLACE
        let metadata2 = ViewMetadata::new(
            "test_view".to_string(),
            "SELECT id FROM t2".to_string(),
            schema.clone(),
        );
        assert!(catalog.create_view(metadata2.clone(), false, false).is_err());

        // Should succeed with OR REPLACE
        catalog.create_view(metadata2, false, true).unwrap();

        let retrieved = catalog.get_view("test_view").unwrap();
        assert!(retrieved.query_sql.contains("t2"));
    }

    #[test]
    fn test_drop_view() {
        let _dir = tempdir().unwrap();
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let catalog = ViewCatalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);
        let metadata = ViewMetadata::new(
            "test_view".to_string(),
            "SELECT id FROM t".to_string(),
            schema,
        );

        catalog.create_view(metadata, false, false).unwrap();
        assert!(catalog.view_exists("test_view").unwrap());

        catalog.drop_view("test_view", false).unwrap();
        assert!(!catalog.view_exists("test_view").unwrap());
    }

    #[test]
    fn test_drop_view_if_exists() {
        let _dir = tempdir().unwrap();
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();
        let catalog = ViewCatalog::new(&storage);

        // Should fail without IF EXISTS
        assert!(catalog.drop_view("nonexistent", false).is_err());

        // Should succeed with IF EXISTS
        catalog.drop_view("nonexistent", true).unwrap();
    }
}
