//! Catalog management for table metadata
//!
//! Handles table schemas, row IDs, and metadata storage in RocksDB.

use super::compression::{CompressionConfig, CompressionStats};
use super::statistics::TableStatistics;
use super::StorageEngine;
use crate::sql::{TriggerDefinition, TriggerPersistence};
use crate::{DataType, Error, Result, Schema, Value};
use serde::{Deserialize, Serialize};

/// Persisted CREATE INDEX definition.
///
/// The actual ART and non-persistent HNSW structures are rebuilt in memory on
/// open; this record is the version-portable catalog entry that survives a
/// binary swap against the same data directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedIndexDefinition {
    pub table_name: String,
    pub column_name: String,
    pub index_type: Option<String>,
    #[serde(default)]
    pub options: Vec<crate::sql::logical_plan::IndexOption>,
}

fn vector_distance_metric(options: &[crate::sql::logical_plan::IndexOption]) -> crate::vector::DistanceMetric {
    use crate::sql::logical_plan::IndexOption;
    use crate::vector::DistanceMetric;

    options
        .iter()
        .find_map(|option| match option {
            IndexOption::DistanceMetric(name) => match name.as_str() {
                "cosine" => Some(DistanceMetric::Cosine),
                "ip" | "inner_product" => Some(DistanceMetric::InnerProduct),
                _ => Some(DistanceMetric::L2),
            },
            _ => None,
        })
        .unwrap_or(DistanceMetric::L2)
}

/// Catalog manager for table metadata
pub struct Catalog<'a> {
    storage: &'a StorageEngine,
}

impl<'a> Catalog<'a> {
    /// Create a new catalog
    pub fn new(storage: &'a StorageEngine) -> Self {
        Self { storage }
    }

    /// Get a reference to the storage engine
    pub fn storage(&self) -> &'a StorageEngine {
        self.storage
    }

    /// Create a table with the given schema
    pub fn create_table(&self, table_name: &str, schema: Schema) -> Result<()> {
        // Check if table already exists
        if self.table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' already exists", table_name)));
        }

        // Log CreateTable to WAL first (for replication to standbys)
        // This must happen before the actual table creation so standbys
        // receive and apply the operation in the correct order.
        self.storage.log_create_table(table_name, &schema)?;

        // Store schema
        let key = Self::table_metadata_key(table_name);
        let value =
            bincode::serialize(&schema).map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;

        self.storage.put(&key, &value)?;

        // Update in-memory schema cache
        self.storage.cache_schema(table_name, schema.clone());

        // Initialize row counter to 0
        let counter_key = Self::table_counter_key(table_name);
        let counter_value =
            bincode::serialize(&0u64).map_err(|e| Error::storage(format!("Failed to serialize counter: {}", e)))?;
        self.storage.put(&counter_key, &counter_value)?;

        // Auto-create ART indexes for PRIMARY KEY and UNIQUE constraints
        let art_manager = self.storage.art_indexes();

        // Collect PRIMARY KEY columns
        let pk_columns: Vec<String> = schema
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();

        if !pk_columns.is_empty() {
            if let Err(e) = art_manager.create_pk_index(table_name, &pk_columns) {
                tracing::warn!("Failed to create PK ART index for table '{}': {}", table_name, e);
            } else {
                tracing::debug!(
                    "Created PK ART index for table '{}' on columns {:?}",
                    table_name,
                    pk_columns
                );
            }
        }

        // Collect UNIQUE columns (non-PK) and create individual UNIQUE indexes
        for col in schema.columns.iter() {
            if col.unique && !col.primary_key {
                let unique_columns = vec![col.name.clone()];
                if let Err(e) = art_manager.create_unique_index(table_name, &unique_columns, Some(&col.name)) {
                    tracing::warn!(
                        "Failed to create UNIQUE ART index for table '{}' column '{}': {}",
                        table_name,
                        col.name,
                        e
                    );
                } else {
                    tracing::debug!(
                        "Created UNIQUE ART index for table '{}' on column '{}'",
                        table_name,
                        col.name
                    );
                }
            }
        }

        Ok(())
    }

    /// Check if a table exists
    pub fn table_exists(&self, table_name: &str) -> Result<bool> {
        let key = Self::table_metadata_key(table_name);
        Ok(self.storage.get(&key)?.is_some())
    }

    /// Get table schema
    ///
    /// This method first checks if the table exists as a regular table.
    /// If not found, it checks if it exists as a materialized view and
    /// returns the MV's schema if found.
    pub fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        // Resolve the raw schema (in-memory cache -> on-disk metadata -> materialized view).
        let mut schema = if let Some(schema) = self.storage.get_cached_schema(table_name) {
            schema
        } else {
            let key = Self::table_metadata_key(table_name);
            match self.storage.get(&key)? {
                Some(data) => {
                    let schema: Schema = bincode::deserialize(&data)
                        .map_err(|e| Error::storage(format!("Failed to deserialize schema: {}", e)))?;
                    // Cache for future lookups
                    self.storage.cache_schema(table_name, schema.clone());
                    schema
                }
                None => {
                    // Table not found, check if it's a materialized view
                    let mv_catalog = self.storage.mv_catalog();
                    if mv_catalog.view_exists(table_name)? {
                        let mv_metadata = mv_catalog.get_view(table_name)?;
                        // Cache MV schema too
                        self.storage.cache_schema(table_name, mv_metadata.schema.clone());
                        mv_metadata.schema
                    } else {
                        return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
                    }
                }
            }
        };

        // B31 backward-compat: a column always belongs to its table, but tables
        // created by older binaries persisted columns with `source_table_name = None`.
        // That breaks table-qualified column resolution (e.g. `SELECT t.col`) on the
        // schema-derivation path used to build the extended-query RowDescription
        // (`derive_result_schema` -> `LogicalPlan::schema()`): freshly-created tables
        // resolve, but old ones raise `Column 't.col' not found in schema`. Stamp the
        // canonical table name here so every consumer (Describe, planner, evaluator)
        // resolves `t.col` regardless of the on-disk format.
        for col in &mut schema.columns {
            if col.source_table_name.is_none() {
                col.source_table_name = Some(table_name.to_string());
            }
        }
        Ok(schema)
    }

    /// Update table schema (for ALTER TABLE operations)
    ///
    /// Updates the schema metadata for an existing table.
    /// This is used by ALTER TABLE ALTER COLUMN SET STORAGE to
    /// persist storage mode changes.
    pub fn update_table_schema(&self, table_name: &str, schema: &Schema) -> Result<()> {
        // Verify table exists
        if !self.table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }

        // Store updated schema
        let key = Self::table_metadata_key(table_name);
        let value =
            bincode::serialize(schema).map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;

        self.storage.put(&key, &value)?;

        // Update in-memory schema cache
        self.storage.cache_schema(table_name, schema.clone());

        Ok(())
    }

    /// Drop a table
    pub fn drop_table(&self, table_name: &str) -> Result<()> {
        if !self.table_exists(table_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", table_name)));
        }

        // Log DropTable to WAL first (for replication to standbys)
        self.storage.log_drop_table(table_name)?;

        // Drop all ART indexes for this table
        let art_manager = self.storage.art_indexes();
        if let Err(e) = art_manager.drop_table_indexes(table_name) {
            tracing::warn!("Failed to drop ART indexes for table '{}': {}", table_name, e);
        }

        // Invalidate statistics cache
        let cache = self.storage.statistics_cache();
        cache.invalidate(table_name)?;

        // Invalidate schema cache
        self.storage.invalidate_schema_cache(table_name);

        // Batch-delete all metadata keys in a single RocksDB write
        {
            let mut batch = rocksdb::WriteBatch::default();
            batch.delete(Self::table_metadata_key(table_name));
            batch.delete(Self::table_counter_key(table_name));
            batch.delete(Self::compression_config_key(table_name));
            batch.delete(Self::compression_stats_key(table_name));
            self.storage
                .db
                .write(batch)
                .map_err(|e| crate::Error::storage(format!("Batch delete failed: {}", e)))?;
        }

        // Delete all data rows using prefix seek (jumps directly to table's key range)
        // Key format: data:{table_name}:{row_id}
        let data_prefix = format!("data:{}:", table_name);
        let prefix_bytes = data_prefix.as_bytes();

        // Collect all keys to delete using prefix seek (O(rows_in_table), not O(all_keys))
        let mut keys_to_delete = Vec::new();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(false); // Enable prefix-based seek
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix_bytes, rocksdb::Direction::Forward),
            read_opts,
        );

        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix_bytes) {
                break; // Past the prefix range
            }
            keys_to_delete.push(key.to_vec());
        }

        // Delete all collected data row keys
        for key in keys_to_delete {
            self.storage.delete(&key)?;
        }

        // R3.3: purge columnar sidecars (col:/colz:/colzm:/colp:/colpm:) so a
        // re-created table with the same name never reads stale batches, zone
        // stats or live-row presence. No-op prefix seeks for row-only tables.
        super::ColumnarStore::purge_table_sidecars(&self.storage.db, table_name)?;

        Ok(())
    }

    /// Get next row ID for a table
    pub fn next_row_id(&self, table_name: &str) -> Result<u64> {
        self.storage.next_row_id(table_name)
    }

    /// List every catalogued table as `(schema, name)`.
    /// Default schema is `"public"`; `_hdb_code_*` and `_hdb_graph_*`
    /// flat-prefix tables are reported under their dotted-form
    /// schemas (`_hdb_code` / `_hdb_graph`).  Stable order, sorted
    /// lexicographically.
    pub fn list_tables_qualified(&self) -> Result<Vec<(String, String)>> {
        let names = self.list_tables()?;
        let mut out: Vec<(String, String)> = names
            .into_iter()
            .map(|n| {
                if let Some(rest) = n.strip_prefix("_hdb_code_") {
                    ("_hdb_code".to_string(), rest.to_string())
                } else if let Some(rest) = n.strip_prefix("_hdb_graph_") {
                    ("_hdb_graph".to_string(), rest.to_string())
                } else if let Some(idx) = n.find('.') {
                    let (s, t) = n.split_at(idx);
                    (s.to_string(), t[1..].to_string())
                } else {
                    ("public".to_string(), n)
                }
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// List the distinct schemas seen across every catalogued
    /// table.  Useful for `pg_namespace` materialisation.
    pub fn list_schemas(&self) -> Result<Vec<String>> {
        use std::collections::BTreeSet;
        let mut s: BTreeSet<String> = BTreeSet::new();
        for (sch, _) in self.list_tables_qualified()? {
            s.insert(sch);
        }
        Ok(s.into_iter().collect())
    }

    /// List all tables in the database
    pub fn list_tables(&self) -> Result<Vec<String>> {
        let prefix = b"meta:table:";
        let mut tables = Vec::new();

        // Use prefix seek to jump directly to "meta:table:" range
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break; // Past the prefix range
            }
            let table_name = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).to_string();
            tables.push(table_name);
        }

        tables.sort();
        Ok(tables)
    }

    /// Persist a user-created index definition in the catalog.
    pub fn save_index_definition(&self, index_name: &str, definition: &PersistedIndexDefinition) -> Result<()> {
        let key = Self::index_metadata_key(index_name);
        let value = bincode::serialize(definition)
            .map_err(|e| Error::storage(format!("Failed to serialize index definition: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Drop a persisted user-created index definition.
    pub fn drop_index_definition(&self, index_name: &str) -> Result<()> {
        let key = Self::index_metadata_key(index_name);
        self.storage.delete(&key)
    }

    /// List persisted CREATE INDEX definitions.
    pub fn list_index_definitions(&self) -> Result<Vec<(String, PersistedIndexDefinition)>> {
        let prefix = b"meta:index:";
        let mut indexes = Vec::new();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );

        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;
            if !key.starts_with(prefix) {
                break;
            }
            let index_name = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).to_string();

            let definition = match bincode::deserialize::<PersistedIndexDefinition>(&value) {
                Ok(definition) => definition,
                Err(_) => {
                    // Backward-compatible decode for older WAL replay records
                    // that stored `(table, column, index_type, options_bytes)`.
                    let (table_name, column_name, index_type, options_bytes): (
                        String,
                        String,
                        Option<String>,
                        Vec<u8>,
                    ) = bincode::deserialize(&value).map_err(|e| {
                        Error::storage(format!(
                            "Failed to deserialize index definition '{}': {}",
                            index_name, e
                        ))
                    })?;
                    let options = if options_bytes.is_empty() {
                        Vec::new()
                    } else {
                        bincode::deserialize(&options_bytes).map_err(|e| {
                            Error::storage(format!("Failed to deserialize index options '{}': {}", index_name, e))
                        })?
                    };
                    PersistedIndexDefinition {
                        table_name,
                        column_name,
                        index_type,
                        options,
                    }
                }
            };
            indexes.push((index_name, definition));
        }

        indexes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(indexes)
    }

    /// Re-register and re-populate every ART index from on-disk state.
    ///
    /// Called once at engine startup so that a fresh process attaching to an
    /// existing data directory has the same in-memory ART state any process
    /// that created the rows would have. Without this, PK / UNIQUE constraint
    /// checks fall back to scans, and INSERT … ON CONFLICT can't find rows
    /// committed by a previous process.
    ///
    /// Behaviour:
    /// - PK and UNIQUE indexes are registered from the persisted schema.
    /// - FK indexes are registered from the persisted `table_constraints`.
    /// - Every row in every user table is replayed through `on_insert` to
    ///   populate the indexes.
    /// - Errors are logged but do not abort startup; a missing or corrupt
    ///   index will fall back to a scan path at query time.
    ///
    /// Cost is `O(rows + indexes)` at startup with no extra cost on the OLTP
    /// hot path. This is the v3.21 interim; persistent index pages backed by
    /// a RocksDB column family are tracked separately for v3.22+.
    pub fn rebuild_all_indexes(&self) -> Result<()> {
        let started = std::time::Instant::now();
        let art_manager = self.storage.art_indexes();
        let mut total_rows: u64 = 0;
        let mut total_tables: u64 = 0;
        let persisted_indexes = self.list_index_definitions()?;

        for table_name in self.list_tables()? {
            // Skip system / internal bookkeeping tables — they have no
            // user-facing constraint indexes and rebuilding them just
            // wastes time at startup.
            if table_name.starts_with("helios_") {
                continue;
            }

            let schema = match self.get_table_schema(&table_name) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        "Index rebuild: skipping table {} — schema load failed: {}",
                        table_name,
                        e
                    );
                    continue;
                }
            };

            // (Re)register the PK index structure if the table has one.
            let pk_columns: Vec<String> = schema
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect();
            if !pk_columns.is_empty() {
                if let Err(e) = art_manager.create_pk_index(&table_name, &pk_columns) {
                    // IndexAlreadyExists is expected if create_table ran in
                    // the same process; log at debug, continue.
                    tracing::debug!("Index rebuild: PK index for {} already registered: {}", table_name, e);
                }
            }

            // (Re)register UNIQUE indexes (one per UNIQUE non-PK column).
            for col in &schema.columns {
                if col.unique && !col.primary_key {
                    let cols = vec![col.name.clone()];
                    if let Err(e) = art_manager.create_unique_index(&table_name, &cols, Some(&col.name)) {
                        tracing::debug!(
                            "Index rebuild: UNIQUE index for {}.{} already registered: {}",
                            table_name,
                            col.name,
                            e
                        );
                    }
                }
            }

            // (Re)register FK indexes from persisted constraints.
            if let Ok(constraints) = self.load_table_constraints(&table_name) {
                for fk in &constraints.foreign_keys {
                    if let Err(e) = art_manager.create_fk_index(
                        &fk.table_name,
                        &fk.columns,
                        &fk.references_table,
                        &fk.references_columns,
                        Some(&fk.name),
                    ) {
                        tracing::debug!("Index rebuild: FK index {} already registered: {}", fk.name, e);
                    }
                }
            }

            // (Re)register user-created scalar secondary indexes from the
            // durable index catalog.
            for (index_name, definition) in persisted_indexes
                .iter()
                .filter(|(_, definition)| definition.table_name == table_name)
            {
                if matches!(definition.index_type.as_deref(), None | Some("art" | "btree" | "hash")) {
                    let columns = vec![definition.column_name.clone()];
                    if let Err(e) = art_manager.create_manual_index(index_name, &table_name, &columns) {
                        tracing::debug!("Index rebuild: manual index {} already registered: {}", index_name, e);
                    }
                }
            }

            // Replay every existing row through on_insert so the indexes
            // know about pre-existing data.
            let tuples = match self.storage.scan_table_with_schema(&table_name, &schema) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("Index rebuild: scan failed for table {}: {}", table_name, e);
                    continue;
                }
            };

            for tuple in &tuples {
                let row_id = match tuple.row_id {
                    Some(id) => id,
                    None => continue,
                };
                let mut col_values = std::collections::HashMap::with_capacity(schema.columns.len());
                for (idx, col) in schema.columns.iter().enumerate() {
                    if let Some(v) = tuple.values.get(idx) {
                        col_values.insert(col.name.clone(), v.clone());
                    }
                }
                if let Err(e) = art_manager.on_insert(&table_name, row_id, &col_values) {
                    tracing::debug!(
                        "Index rebuild: on_insert skipped for {} row {}: {}",
                        table_name,
                        row_id,
                        e
                    );
                }
                total_rows += 1;
            }

            total_tables += 1;
        }

        self.rebuild_vector_indexes(&persisted_indexes)?;

        tracing::info!(
            "Index rebuild complete: {} tables, {} rows, {:.1}ms",
            total_tables,
            total_rows,
            started.elapsed().as_secs_f64() * 1000.0,
        );
        Ok(())
    }

    fn rebuild_vector_indexes(&self, indexes: &[(String, PersistedIndexDefinition)]) -> Result<()> {
        for (index_name, definition) in indexes {
            if !matches!(
                definition.index_type.as_deref(),
                Some("hnsw" | "persistent_hnsw" | "hnsw_pq")
            ) {
                continue;
            }

            let schema = match self.get_table_schema(&definition.table_name) {
                Ok(schema) => schema,
                Err(e) => {
                    tracing::warn!(
                        "Vector index rebuild: skipping {} — schema load failed: {}",
                        index_name,
                        e
                    );
                    continue;
                }
            };
            let Some(column) = schema.get_column(&definition.column_name) else {
                tracing::warn!(
                    "Vector index rebuild: skipping {} — column {}.{} not found",
                    index_name,
                    definition.table_name,
                    definition.column_name
                );
                continue;
            };
            let DataType::Vector(dimension) = &column.data_type else {
                tracing::warn!(
                    "Vector index rebuild: skipping {} — {}.{} is not VECTOR",
                    index_name,
                    definition.table_name,
                    definition.column_name
                );
                continue;
            };

            let vector_indexes = self.storage.vector_indexes();
            if vector_indexes.index_exists(index_name) {
                continue;
            }
            let metric = vector_distance_metric(&definition.options);
            if let Err(e) = vector_indexes.create_index(
                index_name.clone(),
                definition.table_name.clone(),
                definition.column_name.clone(),
                *dimension,
                metric,
            ) {
                tracing::warn!("Vector index rebuild: create {} failed: {}", index_name, e);
                continue;
            }

            let vectors = match self.collect_existing_vectors(
                &schema,
                &definition.table_name,
                &definition.column_name,
                *dimension,
            ) {
                Ok(vectors) => vectors,
                Err(e) => {
                    let _ = vector_indexes.drop_index(index_name);
                    tracing::warn!("Vector index rebuild: scan {} failed: {}", index_name, e);
                    continue;
                }
            };
            if let Err(e) = vector_indexes.insert_vectors_batch(index_name, &vectors) {
                let _ = vector_indexes.drop_index(index_name);
                tracing::warn!("Vector index rebuild: backfill {} failed: {}", index_name, e);
            }
        }
        Ok(())
    }

    fn collect_existing_vectors(
        &self,
        schema: &Schema,
        table_name: &str,
        column_name: &str,
        dimension: usize,
    ) -> Result<Vec<(u64, crate::vector::Vector)>> {
        let col_idx = schema
            .get_column_index(column_name)
            .ok_or_else(|| Error::query_execution(format!("Column '{}' not found in schema", column_name)))?;
        let tuples = self
            .storage
            .scan_table_with_schema_columns(table_name, schema, &[col_idx])?;
        let mut vectors = Vec::with_capacity(tuples.len());

        for tuple in tuples {
            match tuple.values.get(col_idx) {
                Some(Value::Vector(vec)) => {
                    if vec.len() != dimension {
                        return Err(Error::query_execution(format!(
                            "Vector dimension mismatch while rebuilding '{}.{}': expected {}, got {}",
                            table_name,
                            column_name,
                            dimension,
                            vec.len()
                        )));
                    }
                    let row_id = tuple.row_id.ok_or_else(|| {
                        Error::query_execution(format!(
                            "Cannot rebuild vector index on '{}.{}' from tuple without row_id",
                            table_name, column_name
                        ))
                    })?;
                    vectors.push((row_id, vec.clone()));
                }
                Some(Value::Null) | None => {}
                Some(other) => {
                    return Err(Error::query_execution(format!(
                        "Cannot rebuild vector index on '{}.{}' from non-vector value {:?}",
                        table_name, column_name, other
                    )))
                }
            }
        }

        Ok(vectors)
    }

    /// Rename a table atomically
    ///
    /// This operation renames a table by updating its metadata and moving all data rows
    /// to use the new table name. This is used for concurrent materialized view refresh.
    pub fn rename_table(&self, old_name: &str, new_name: &str) -> Result<()> {
        // Check that old table exists
        if !self.table_exists(old_name)? {
            return Err(Error::query_execution(format!("Table '{}' does not exist", old_name)));
        }

        // Check that new table name is not already in use
        if self.table_exists(new_name)? {
            return Err(Error::query_execution(format!("Table '{}' already exists", new_name)));
        }

        // Get the schema from old table
        let schema = self.get_table_schema(old_name)?;

        // Get current row counter
        let old_counter_key = Self::table_counter_key(old_name);
        let counter_value = match self.storage.get(&old_counter_key)? {
            Some(data) => data,
            None => {
                // Default to 0 if counter doesn't exist
                bincode::serialize(&0u64).map_err(|e| Error::storage(format!("Failed to serialize counter: {}", e)))?
            }
        };

        // Get compression config if it exists
        let compression_config = self.get_compression_config(old_name)?;

        // Get compression stats if they exist
        let compression_stats = self.get_compression_stats(old_name)?;

        // Create new table metadata with the same schema
        let new_metadata_key = Self::table_metadata_key(new_name);
        let schema_bytes =
            bincode::serialize(&schema).map_err(|e| Error::storage(format!("Failed to serialize schema: {}", e)))?;
        self.storage.put(&new_metadata_key, &schema_bytes)?;

        // Create new counter
        let new_counter_key = Self::table_counter_key(new_name);
        self.storage.put(&new_counter_key, &counter_value)?;

        // Copy compression config to new table
        if let Some(config) = compression_config {
            self.set_compression_config(new_name, &config)?;
        }

        // Copy compression stats to new table
        if let Some(stats) = compression_stats {
            self.set_compression_stats(new_name, &stats)?;
        }

        // Move all data rows from old table to new table
        let old_data_prefix = format!("data:{}:", old_name);
        let old_prefix_bytes = old_data_prefix.as_bytes();
        let new_data_prefix = format!("data:{}:", new_name);

        // Collect all old keys and their values. Seek to the table's data
        // prefix instead of scanning from the start of the keyspace.
        let mut rows_to_move = Vec::new();
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(old_prefix_bytes, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(old_prefix_bytes) {
                break;
            }
            // Extract row_id from old key: data:{old_name}:{row_id}
            let key_str = String::from_utf8_lossy(&key);
            if let Some(row_id_str) = key_str.strip_prefix(&old_data_prefix) {
                rows_to_move.push((row_id_str.to_string(), value.to_vec()));
            }
        }

        // Write new rows and delete old rows
        for (row_id, value) in rows_to_move {
            // Write to new location
            let new_key = format!("{}{}", new_data_prefix, row_id).into_bytes();
            self.storage.put(&new_key, &value)?;

            // Delete from old location
            let old_key = format!("{}{}", old_data_prefix, row_id).into_bytes();
            self.storage.delete(&old_key)?;
        }

        // Rename compression manager resources (no-op - compression handled by RocksDB LZ4)
        super::CompressionManager::new().rename_table(old_name, new_name)?;

        // Rename ART indexes
        let art_manager = self.storage.art_indexes();
        if let Err(e) = art_manager.rename_table_indexes(old_name, new_name) {
            tracing::warn!(
                "Failed to rename ART indexes from '{}' to '{}': {}",
                old_name,
                new_name,
                e
            );
        }

        // Delete old table metadata
        let old_metadata_key = Self::table_metadata_key(old_name);
        self.storage.delete(&old_metadata_key)?;

        // Delete old counter
        self.storage.delete(&old_counter_key)?;

        // Delete old compression config and stats
        let old_compression_config_key = Self::compression_config_key(old_name);
        self.storage.delete(&old_compression_config_key)?;

        let old_compression_stats_key = Self::compression_stats_key(old_name);
        self.storage.delete(&old_compression_stats_key)?;

        // Update schema cache: remove old, add new
        self.storage.invalidate_schema_cache(old_name);
        self.storage.cache_schema(new_name, schema);

        Ok(())
    }

    /// Build metadata key for table schema
    fn table_metadata_key(table_name: &str) -> Vec<u8> {
        format!("meta:table:{}", table_name).into_bytes()
    }

    fn index_metadata_key(index_name: &str) -> Vec<u8> {
        format!("meta:index:{}", index_name).into_bytes()
    }

    // -------------------------------------------------------------------
    // KanttBan #20 (v3.31.0) — CREATE TYPE … AS ENUM
    //
    // Storage: each registered enum type lives at
    // `meta:enum_type:<name>` → bincode-encoded `Vec<String>` of labels.
    // Persistence + lookup happens here at the catalog layer so that
    // plan-time `CREATE TABLE foo (col my_enum)` can resolve labels
    // and synthesize an implicit CHECK (col IN (labels…)) constraint.
    // The column itself is stored as TEXT.
    // -------------------------------------------------------------------

    fn enum_type_key(name: &str) -> Vec<u8> {
        format!("meta:enum_type:{}", name.to_lowercase()).into_bytes()
    }

    /// Register an enum type with its labels. Overwrites any existing
    /// entry — callers that need IF NOT EXISTS semantics should check
    /// `enum_type_exists` first.
    pub fn register_enum_type(&self, name: &str, labels: &[String]) -> Result<()> {
        let key = Self::enum_type_key(name);
        let value = bincode::serialize(labels).map_err(|e| Error::query_execution(format!("enum serialize: {e}")))?;
        self.storage.put(&key, &value)
    }

    /// Look up the labels for a registered enum type. Returns None if
    /// the name isn't registered.
    pub fn get_enum_labels(&self, name: &str) -> Result<Option<Vec<String>>> {
        let key = Self::enum_type_key(name);
        match self.storage.get(&key)? {
            Some(bytes) => {
                let labels: Vec<String> = bincode::deserialize(&bytes)
                    .map_err(|e| Error::query_execution(format!("enum deserialize: {e}")))?;
                Ok(Some(labels))
            }
            None => Ok(None),
        }
    }

    /// True if an enum with this name exists.
    pub fn enum_type_exists(&self, name: &str) -> Result<bool> {
        Ok(self.get_enum_labels(name)?.is_some())
    }

    /// Drop a registered enum type. No-op if it doesn't exist
    /// (callers wanting strict semantics should pre-check).
    pub fn drop_enum_type(&self, name: &str) -> Result<()> {
        let key = Self::enum_type_key(name);
        self.storage.delete(&key)
    }

    // -------------------------------------------------------------------
    // KanttBan #23 (v3.31.1 phase 2) — IDENTITY column tracking.
    //
    // Stored as `meta:identity:<table>` → bincode `Vec<String>` of
    // column names. Could've added the bool to the Column struct
    // itself, but that propagates through 200+ struct literals
    // across the codebase. A side table keeps the change additive.
    // Callers: pg_sequences / pg_attrdef / information_schema.columns
    // executors enumerate identity columns to populate their rows.
    // -------------------------------------------------------------------

    fn identity_key(table_name: &str) -> Vec<u8> {
        format!("meta:identity:{}", table_name.to_lowercase()).into_bytes()
    }

    /// Record that the given column on the given table was declared
    /// with `GENERATED [ALWAYS | BY DEFAULT] AS IDENTITY`. Overwrites
    /// any previous list for that table — callers passing the full
    /// post-CREATE-TABLE column set is the expected shape.
    pub fn register_identity_columns(&self, table_name: &str, columns: &[String]) -> Result<()> {
        let key = Self::identity_key(table_name);
        if columns.is_empty() {
            // Avoid storing empty entries — keeps the prefix scan
            // narrow when many tables have no identity columns.
            self.storage.delete(&key)?;
            return Ok(());
        }
        let value =
            bincode::serialize(columns).map_err(|e| Error::query_execution(format!("identity serialize: {e}")))?;
        self.storage.put(&key, &value)
    }

    /// List the identity columns recorded for a table. Returns an
    /// empty Vec when the table has none (or when no record exists).
    pub fn list_identity_columns(&self, table_name: &str) -> Result<Vec<String>> {
        let key = Self::identity_key(table_name);
        match self.storage.get(&key)? {
            Some(bytes) => {
                bincode::deserialize(&bytes).map_err(|e| Error::query_execution(format!("identity deserialize: {e}")))
            }
            None => Ok(Vec::new()),
        }
    }

    /// True if the given (table, column) pair was marked IDENTITY.
    pub fn is_identity_column(&self, table_name: &str, column_name: &str) -> Result<bool> {
        let cols = self.list_identity_columns(table_name)?;
        Ok(cols.iter().any(|c| c.eq_ignore_ascii_case(column_name)))
    }

    /// Drop the identity record for a table (called from DROP TABLE).
    pub fn drop_identity_columns(&self, table_name: &str) -> Result<()> {
        let key = Self::identity_key(table_name);
        self.storage.delete(&key)
    }

    /// Build counter key for table row IDs
    fn table_counter_key(table_name: &str) -> Vec<u8> {
        format!("counter:{}", table_name).into_bytes()
    }

    /// Build compression config key for a table
    fn compression_config_key(table_name: &str) -> Vec<u8> {
        format!("compression:config:{}", table_name).into_bytes()
    }

    /// Build compression stats key for a table
    fn compression_stats_key(table_name: &str) -> Vec<u8> {
        format!("compression:stats:{}", table_name).into_bytes()
    }

    /// Build statistics key for a table
    fn table_statistics_key(table_name: &str) -> Vec<u8> {
        format!("statistics:table:{}", table_name).into_bytes()
    }

    /// Set compression configuration for a table
    pub fn set_compression_config(&self, table_name: &str, config: &CompressionConfig) -> Result<()> {
        let key = Self::compression_config_key(table_name);
        let value = bincode::serialize(config)
            .map_err(|e| Error::storage(format!("Failed to serialize compression config: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Get compression configuration for a table
    pub fn get_compression_config(&self, table_name: &str) -> Result<Option<CompressionConfig>> {
        let key = Self::compression_config_key(table_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let config = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize compression config: {}", e)))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Set compression statistics for a table
    pub fn set_compression_stats(&self, table_name: &str, stats: &CompressionStats) -> Result<()> {
        let key = Self::compression_stats_key(table_name);
        let value = bincode::serialize(stats)
            .map_err(|e| Error::storage(format!("Failed to serialize compression stats: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Get compression statistics for a table
    pub fn get_compression_stats(&self, table_name: &str) -> Result<Option<CompressionStats>> {
        let key = Self::compression_stats_key(table_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let stats = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize compression stats: {}", e)))?;
                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    /// Set table statistics
    pub fn set_table_statistics(&self, table_name: &str, stats: &TableStatistics) -> Result<()> {
        let key = Self::table_statistics_key(table_name);
        let value = bincode::serialize(stats)
            .map_err(|e| Error::storage(format!("Failed to serialize table statistics: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Get table statistics
    pub fn get_table_statistics(&self, table_name: &str) -> Result<Option<TableStatistics>> {
        // Try cache first
        let cache = self.storage.statistics_cache();
        if let Some(cached_stats) = cache.get(table_name)? {
            return Ok(Some((*cached_stats).clone()));
        }

        // Cache miss - load from storage
        let key = Self::table_statistics_key(table_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let stats: TableStatistics = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize table statistics: {}", e)))?;

                // Store in cache
                cache.put(table_name.to_string(), stats.clone())?;

                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    /// Analyze a table and update statistics
    ///
    /// Performs a full table scan to collect statistics for query planning.
    /// This should be called periodically or after significant data changes.
    pub fn analyze_table(&self, table_name: &str) -> Result<()> {
        use super::statistics::StatisticsAnalyzer;

        // Get table schema
        let schema = self.get_table_schema(table_name)?;

        // Scan all tuples
        let tuples = self.storage.scan_table(table_name)?;

        // Analyze and collect statistics
        let stats = StatisticsAnalyzer::analyze_table(table_name, &tuples, &schema)?;

        // Invalidate cache before storing new statistics
        let cache = self.storage.statistics_cache();
        cache.invalidate(table_name)?;

        // Store statistics
        self.set_table_statistics(table_name, &stats)?;

        tracing::info!("Analyzed table '{}' and invalidated statistics cache", table_name);

        Ok(())
    }

    // === Trigger Persistence Methods ===

    /// Build trigger metadata key
    fn trigger_metadata_key(table_name: &str, trigger_name: &str) -> Vec<u8> {
        format!("trigger:{}:{}", table_name, trigger_name).into_bytes()
    }

    /// Save a trigger to persistent storage
    pub fn save_trigger(&self, definition: &crate::sql::TriggerDefinition) -> Result<()> {
        let key = Self::trigger_metadata_key(&definition.table_name, &definition.name);
        let value = bincode::serialize(definition)
            .map_err(|e| Error::storage(format!("Failed to serialize trigger definition: {}", e)))?;
        self.storage.put(&key, &value)
    }

    /// Load a trigger from persistent storage
    pub fn load_trigger(&self, table_name: &str, trigger_name: &str) -> Result<Option<crate::sql::TriggerDefinition>> {
        let key = Self::trigger_metadata_key(table_name, trigger_name);
        match self.storage.get(&key)? {
            Some(data) => {
                let definition = bincode::deserialize(&data)
                    .map_err(|e| Error::storage(format!("Failed to deserialize trigger definition: {}", e)))?;
                Ok(Some(definition))
            }
            None => Ok(None),
        }
    }

    /// Delete a trigger from persistent storage
    pub fn delete_trigger(&self, table_name: &str, trigger_name: &str) -> Result<()> {
        let key = Self::trigger_metadata_key(table_name, trigger_name);
        self.storage.delete(&key)
    }

    /// Load all triggers from persistent storage
    pub fn load_all_triggers(&self) -> Result<Vec<crate::sql::TriggerDefinition>> {
        let prefix = b"trigger:";
        let mut triggers = Vec::new();

        // Seek to the `trigger:` prefix instead of scanning from keyspace start.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_slice(), rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                break;
            }

            // Deserialize trigger definition
            let definition: crate::sql::TriggerDefinition = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize trigger: {}", e)))?;
            triggers.push(definition);
        }

        Ok(triggers)
    }

    /// Delete all triggers for a table (called when table is dropped)
    pub fn delete_table_triggers(&self, table_name: &str) -> Result<usize> {
        let prefix = format!("trigger:{}:", table_name);
        let prefix_bytes = prefix.as_bytes();
        let mut keys_to_delete = Vec::new();

        // Seek to the `trigger:{table}:` prefix instead of scanning from start.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix_bytes, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, _) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix_bytes) {
                break;
            }
            keys_to_delete.push(key.to_vec());
        }

        let count = keys_to_delete.len();
        for key in keys_to_delete {
            self.storage.delete(&key)?;
        }

        Ok(count)
    }

    // === Constraint Persistence Methods ===

    /// Build constraint metadata key
    fn constraint_key(table_name: &str, constraint_name: &str) -> Vec<u8> {
        format!("constraint:{}:{}", table_name, constraint_name).into_bytes()
    }

    /// Build table constraints key (for storing all constraints of a table)
    fn table_constraints_key(table_name: &str) -> Vec<u8> {
        format!("table_constraints:{}", table_name).into_bytes()
    }

    /// Save table constraints to persistent storage
    pub fn save_table_constraints(&self, table_name: &str, constraints: &crate::sql::TableConstraints) -> Result<()> {
        let key = Self::table_constraints_key(table_name);
        let value = bincode::serialize(constraints)
            .map_err(|e| Error::storage(format!("Failed to serialize table constraints: {}", e)))?;
        self.storage.put(&key, &value)?;
        self.storage.cache_table_constraints(table_name, constraints.clone());
        self.storage.clear_referencing_fk_cache();
        Ok(())
    }

    /// Load table constraints from persistent storage
    pub fn load_table_constraints(&self, table_name: &str) -> Result<crate::sql::TableConstraints> {
        if let Some(constraints) = self.storage.get_cached_table_constraints(table_name) {
            return Ok(constraints);
        }

        let key = Self::table_constraints_key(table_name);
        let constraints = match self.storage.get(&key)? {
            Some(data) => bincode::deserialize(&data)
                .map_err(|e| Error::storage(format!("Failed to deserialize table constraints: {}", e)))?,
            None => crate::sql::TableConstraints::default(),
        };
        self.storage.cache_table_constraints(table_name, constraints.clone());
        Ok(constraints)
    }

    /// Add a foreign key constraint to a table
    pub fn add_foreign_key(&self, fk: crate::sql::ForeignKeyConstraint) -> Result<()> {
        let mut constraints = self.load_table_constraints(&fk.table_name)?;
        constraints.add_foreign_key(fk.clone());
        self.save_table_constraints(&fk.table_name, &constraints)?;

        // Auto-create ART index for FK lookups
        let art_manager = self.storage.art_indexes();
        if let Err(e) = art_manager.create_fk_index(
            &fk.table_name,
            &fk.columns,
            &fk.references_table,
            &fk.references_columns,
            Some(&fk.name),
        ) {
            tracing::warn!("Failed to create FK ART index for constraint '{}': {}", fk.name, e);
        } else {
            tracing::debug!(
                "Created FK ART index for constraint '{}' on table '{}'",
                fk.name,
                fk.table_name
            );
        }

        Ok(())
    }

    /// Get all foreign key constraints referencing a specific table
    pub fn get_referencing_fks(&self, referenced_table: &str) -> Result<Vec<crate::sql::ForeignKeyConstraint>> {
        if let Some(cached) = self.storage.get_cached_referencing_fks(referenced_table) {
            return Ok(cached);
        }

        let mut result = Vec::new();
        let prefix = b"table_constraints:";

        // Seek directly to the `table_constraints:` prefix instead of iterating
        // from the start of the keyspace. With `IteratorMode::Start` this walked
        // every `data:` row key (which sort before `table_constraints:`), making
        // every reverse-FK lookup -- and therefore every DELETE -- O(table rows).
        // `total_order_seek` is required because the DB uses a 5-byte prefix extractor.
        let mut read_opts = rocksdb::ReadOptions::default();
        read_opts.set_total_order_seek(true);
        let iter = self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            read_opts,
        );
        for item in iter {
            let (key, value) = item.map_err(|e| Error::storage(format!("Iterator error: {}", e)))?;

            if !key.starts_with(prefix) {
                // We seeked to `table_constraints:`; the first key that no longer
                // shares the prefix means the whole group has been read.
                break;
            }

            let constraints: crate::sql::TableConstraints = bincode::deserialize(&value)
                .map_err(|e| Error::storage(format!("Failed to deserialize constraints: {}", e)))?;

            for fk in constraints.foreign_keys {
                if fk.references_table == referenced_table {
                    result.push(fk);
                }
            }
        }

        self.storage.cache_referencing_fks(referenced_table, result.clone());
        Ok(result)
    }

    /// Delete all constraints for a table (called when table is dropped)
    pub fn delete_table_constraints(&self, table_name: &str) -> Result<()> {
        let key = Self::table_constraints_key(table_name);
        self.storage.delete(&key)?;
        self.storage.invalidate_table_constraints_cache(table_name);
        Ok(())
    }

    /// Drop a specific constraint by name
    pub fn drop_constraint(&self, table_name: &str, constraint_name: &str) -> Result<bool> {
        let mut constraints = self.load_table_constraints(table_name)?;
        let initial_fk_len = constraints.foreign_keys.len();
        let initial_unique_len = constraints.unique_constraints.len();
        let initial_check_len = constraints.check_constraints.len();

        // Find FK constraint to drop its ART index
        let fk_to_drop = constraints
            .foreign_keys
            .iter()
            .find(|fk| fk.name == constraint_name)
            .cloned();

        // Find unique constraint to drop its ART index
        let unique_to_drop = constraints
            .unique_constraints
            .iter()
            .find(|u| u.name == constraint_name)
            .cloned();

        constraints.foreign_keys.retain(|fk| fk.name != constraint_name);
        constraints.check_constraints.retain(|c| c.name != constraint_name);
        constraints.unique_constraints.retain(|u| u.name != constraint_name);

        let final_len =
            constraints.foreign_keys.len() + constraints.check_constraints.len() + constraints.unique_constraints.len();
        let initial_len = initial_fk_len + initial_check_len + initial_unique_len;

        if initial_len != final_len {
            self.save_table_constraints(table_name, &constraints)?;

            // Drop associated ART indexes
            let art_manager = self.storage.art_indexes();

            // Drop FK ART index if constraint was a foreign key
            if let Some(fk) = fk_to_drop {
                let fk_index_name = format!("fk_{}_{}", fk.table_name, fk.name);
                if let Err(e) = art_manager.drop_index(&fk_index_name) {
                    tracing::warn!("Failed to drop FK ART index '{}': {}", fk_index_name, e);
                }
            }

            // Drop UNIQUE ART index if constraint was a unique constraint
            if let Some(unique) = unique_to_drop {
                let unique_index_name = format!("unique_{}_{}", table_name, unique.name);
                if let Err(e) = art_manager.drop_index(&unique_index_name) {
                    tracing::warn!("Failed to drop UNIQUE ART index '{}': {}", unique_index_name, e);
                }
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Implement TriggerPersistence trait for Catalog
impl TriggerPersistence for Catalog<'_> {
    fn save_trigger(&self, definition: &TriggerDefinition) -> Result<()> {
        self.save_trigger(definition)
    }

    fn load_trigger(&self, table_name: &str, trigger_name: &str) -> Result<Option<TriggerDefinition>> {
        self.load_trigger(table_name, trigger_name)
    }

    fn delete_trigger(&self, table_name: &str, trigger_name: &str) -> Result<()> {
        self.delete_trigger(table_name, trigger_name)
    }

    fn load_all_triggers(&self) -> Result<Vec<TriggerDefinition>> {
        self.load_all_triggers()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Column, Config, DataType};

    #[test]
    fn test_create_table() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int4,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]);

        catalog
            .create_table("users", schema.clone())
            .expect("Failed to create table");

        // Verify table exists
        assert!(catalog.table_exists("users").expect("Failed to check if table exists"));

        // Verify schema. B31: get_table_schema now stamps the owning table name on
        // every column (so qualified `t.col` resolution works regardless of the
        // on-disk format), so compare structurally and assert the stamp rather than
        // requiring exact equality with the unstamped input.
        let retrieved_schema = catalog.get_table_schema("users").expect("Failed to get table schema");
        assert_eq!(retrieved_schema.columns.len(), schema.columns.len());
        for (got, want) in retrieved_schema.columns.iter().zip(schema.columns.iter()) {
            assert_eq!(got.name, want.name);
            assert_eq!(got.data_type, want.data_type);
            assert_eq!(got.source_table_name.as_deref(), Some("users"));
        }
    }

    #[test]
    fn b31_get_table_schema_stamps_source_table_name() {
        // Regression for B31: tables persisted by older binaries stored columns with
        // `source_table_name = None`, which broke `SELECT t.col` resolution on the
        // extended-query schema-derivation path (`derive_result_schema` ->
        // `LogicalPlan::schema()`). get_table_schema must stamp the owning table name
        // so qualified resolution succeeds regardless of the on-disk format.
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        // Old-format schema: every column has source_table_name = None.
        let schema = Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "email".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
                default_expr: None,
                unique: false,
                storage_mode: crate::ColumnStorageMode::Default,
            },
        ]);
        catalog.create_table("leads", schema).expect("Failed to create table");

        let loaded = catalog.get_table_schema("leads").expect("Failed to get table schema");
        assert!(
            loaded.get_qualified_column_index(Some("leads"), "id").is_some(),
            "qualified resolution of leads.id must succeed after B31 stamping"
        );
        assert!(
            loaded.get_qualified_column_index(Some("leads"), "email").is_some(),
            "qualified resolution of leads.email must succeed after B31 stamping"
        );
        for col in &loaded.columns {
            assert_eq!(col.source_table_name.as_deref(), Some("leads"));
        }
    }

    #[test]
    fn test_next_row_id() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        catalog.create_table("test", schema).expect("Failed to create table");

        // Get sequential row IDs
        assert_eq!(catalog.next_row_id("test").expect("Failed to get row ID 1"), 1);
        assert_eq!(catalog.next_row_id("test").expect("Failed to get row ID 2"), 2);
        assert_eq!(catalog.next_row_id("test").expect("Failed to get row ID 3"), 3);
    }

    #[test]
    fn test_drop_table() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![Column::new("id", DataType::Int4)]);

        catalog.create_table("temp", schema).expect("Failed to create table");
        assert!(catalog.table_exists("temp").expect("Failed to check if table exists"));

        catalog.drop_table("temp").expect("Failed to drop table");
        assert!(!catalog
            .table_exists("temp")
            .expect("Failed to check if table exists after drop"));
    }

    #[test]
    fn test_drop_table_deletes_data_rows() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).expect("Failed to open in-memory storage");
        let catalog = Catalog::new(&storage);

        let schema = Schema::new(vec![
            Column::new("id", DataType::Int4),
            Column::new("name", DataType::Text),
        ]);

        // Create table and insert some data
        catalog.create_table("users", schema).expect("Failed to create table");

        // Insert test data rows using the storage engine
        use crate::Value;
        let tuple1 = crate::Tuple::new(vec![Value::Int4(1), Value::String("Alice".to_string())]);
        let tuple2 = crate::Tuple::new(vec![Value::Int4(2), Value::String("Bob".to_string())]);

        storage.insert_tuple("users", tuple1).expect("Failed to insert tuple 1");
        storage.insert_tuple("users", tuple2).expect("Failed to insert tuple 2");

        // Verify data exists before drop
        let data_before = storage.scan_table("users").expect("Failed to scan table before drop");
        assert_eq!(data_before.len(), 2, "Should have 2 rows before drop");

        // Drop the table
        catalog.drop_table("users").expect("Failed to drop table");

        // Verify metadata is gone
        assert!(!catalog.table_exists("users").expect("Failed to check if table exists"));

        // Verify data rows are actually deleted by checking the raw database
        let data_prefix = b"data:users:";
        let iter = storage.db.iterator(rocksdb::IteratorMode::Start);
        let mut orphaned_keys = Vec::new();

        for item in iter {
            let (key, _) = item.expect("Iterator error");
            if key.starts_with(data_prefix) {
                orphaned_keys.push(String::from_utf8_lossy(&key).to_string());
            }
        }

        assert_eq!(
            orphaned_keys.len(),
            0,
            "Should have no orphaned data rows, found: {:?}",
            orphaned_keys
        );
    }
}
