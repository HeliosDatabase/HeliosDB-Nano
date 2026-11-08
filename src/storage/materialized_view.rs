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
use std::collections::{BTreeMap, HashMap};

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
        let table = Self::mv_data_table_name(&metadata.view_name);
        let _guard = self.storage.main_catalog_write(&[&table], "CREATE MATERIALIZED VIEW")?;

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

        let mut ddl = super::DdlBatch::new(self.storage);
        ddl.put(&key, &value)?;
        ddl.log(super::WalOperation::CreateMaterializedView {
            name: metadata.view_name.clone(),
            definition: value,
        });
        ddl.on_commit(|| {
            self.storage.bump_schema_generation();
        });
        ddl.commit()?.wait_replicated();

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

    /// A no-row refresh still makes its refreshed record and logical entry
    /// durable together; a surrounding WAL-capture frame is not its commit.
    pub(crate) fn refresh_metadata_in_one_group(
        &self,
        metadata: &MaterializedViewMetadata,
        concurrent: bool,
        incremental: bool,
    ) -> Result<()> {
        let mut ddl = super::DdlBatch::new(self.storage);
        let bytes = bincode::serialize(metadata).map_err(|e| Error::storage(format!("MV refresh metadata: {e}")))?;
        ddl.put(Self::mv_metadata_key(&metadata.view_name), &bytes)?;
        ddl.log(super::WalOperation::RefreshMaterializedView {
            name: metadata.view_name.clone(),
            concurrent,
            incremental,
        });
        self.stage_applied_delta_cleanup(metadata, &mut ddl)?;
        ddl.on_commit(
            self.storage
                .data_write_attempt_publication(Self::mv_data_table_name(&metadata.view_name)),
        );
        ddl.on_abort(
            self.storage
                .data_write_attempt_publication(Self::mv_data_table_name(&metadata.view_name)),
        );
        ddl.commit()?.wait_replicated();
        Ok(())
    }

    fn stage_applied_delta_cleanup(
        &self,
        metadata: &MaterializedViewMetadata,
        ddl: &mut super::DdlBatch<'_>,
    ) -> Result<()> {
        let Some(refreshed_at) = metadata.last_refresh else {
            return Ok(());
        };
        let cutoff: std::time::SystemTime = refreshed_at.into();
        let prefix = b"delta:";
        let mut opts = rocksdb::ReadOptions::default();
        opts.set_total_order_seek(true);
        for item in self
            .storage
            .db
            .iterator_opt(rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward), opts)
        {
            let (key, value) = item.map_err(|e| Error::storage(format!("MV delta cleanup scan: {e}")))?;
            if !key.starts_with(prefix) {
                break;
            }
            let plain = super::tde::open_ref(self.storage.key_manager_ref(), &key, &value)?;
            let delta: super::mv_delta::Delta =
                bincode::deserialize(&plain).map_err(|e| Error::storage(format!("MV delta cleanup record: {e}")))?;
            if delta.timestamp < cutoff {
                ddl.delete(&key);
            }
        }
        Ok(())
    }

    /// GH#36: restore a materialized view definition from a replicated WAL
    /// entry. Writes the key the reader actually consults (`meta:mv:<name>`)
    /// and reclassifies the name.
    pub fn restore_view_from_wal(&self, view_name: &str, definition: &[u8]) -> Result<()> {
        self.storage.put(&Self::mv_metadata_key(view_name), definition)?;
        self.storage.bump_schema_generation();
        Ok(())
    }

    /// GH#36: drop a materialized view during replay, tolerating an
    /// already-absent record and an already-dropped backing table.
    pub fn drop_view_from_wal(&self, view_name: &str) -> Result<()> {
        let key = Self::mv_metadata_key(view_name);
        if self.storage.get(&key)?.is_some() {
            self.storage.delete(&key)?;
        }
        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();
        if catalog.table_exists(&data_table)? {
            catalog.drop_table(&data_table)?;
        }
        // R5 decision 11: write, bump, invalidate.
        self.storage.bump_schema_generation();
        self.storage.invalidate_schema_cache(view_name);
        Ok(())
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

        // Spec R5.3: the view's record, its data table (MV results are stored as a
        // regular table) and both logical-WAL entries are ONE write group: no
        // reader, standby or crash sees the view without its rows or the rows
        // without the view.
        let key = Self::mv_metadata_key(view_name);
        let data_table = Self::mv_data_table_name(view_name);
        let catalog = self.storage.catalog();
        let mut ddl = super::DdlBatch::new(self.storage);
        ddl.delete(&key);
        // GH#36: represent the drop. The backing table's own `DropTable` entry
        // comes from the table's drop; this names the VIEW.
        ddl.log(super::WalOperation::DropMaterializedView {
            name: view_name.to_string(),
        });
        let dropped = if catalog.main_table_exists(&data_table)? {
            catalog.stage_drop_table(&data_table, &[], &mut ddl)?;
            vec![data_table]
        } else {
            Vec::new()
        };
        catalog.stage_dropped_tables_sidecar_purge(&dropped, &mut ddl);
        // W1.3: the view no longer exists; bump so the existence cache
        // recomputes this name to `Missing`. Then the schema cache of the view
        // name itself (it may have been cached by catalog.get_table_schema(),
        // which falls back to MV lookup), after the bump (R5 decision 11): a
        // reader's fill either ran already, and is removed here, or finds the
        // generation moved and is refused.
        {
            let storage = self.storage;
            let view = view_name.to_string();
            ddl.on_commit(move || {
                storage.bump_schema_generation();
                storage.invalidate_schema_cache(&view);
            });
        }
        ddl.commit()?.wait_replicated();
        catalog.finish_dropped_table_history(&dropped);

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

    /// Create the definition and its populated backing table in the same group.
    pub(crate) fn create_view_with_data(
        &self,
        mut metadata: MaterializedViewMetadata,
        tuples: &[Tuple],
    ) -> Result<u64> {
        self.storage.refuse_statement_on_branch("CREATE MATERIALIZED VIEW")?;
        let table = Self::mv_data_table_name(&metadata.view_name);
        let _guard = self.storage.main_catalog_write(&[&table], "CREATE MATERIALIZED VIEW")?;
        if self.view_exists(&metadata.view_name)? {
            return Err(Error::query_execution(format!(
                "Materialized view '{}' already exists",
                metadata.view_name
            )));
        }
        metadata.mark_refreshed(tuples.len() as u64);
        let schema = metadata.schema.clone();
        self.write_view_data_group(&metadata.view_name, tuples, &schema, Some(&metadata), None, true)
    }

    /// Replace the materialized rows atomically, preserving a compatible backing
    /// table's indexes, storage modes, row-id sequence and version history.
    pub fn store_view_data(&self, view_name: &str, tuples: Vec<Tuple>, schema: &Schema) -> Result<u64> {
        self.store_view_data_mode(view_name, &tuples, schema, false)
    }

    fn store_view_data_mode(
        &self,
        view_name: &str,
        tuples: &[Tuple],
        schema: &Schema,
        concurrent: bool,
    ) -> Result<u64> {
        let table = Self::mv_data_table_name(view_name);
        let _guard = self
            .storage
            .main_catalog_write(&[&table], "MATERIALIZED VIEW population")?;
        let refreshed = if self.view_exists(view_name)? {
            let mut metadata = self.get_view(view_name)?;
            metadata.mark_refreshed(tuples.len() as u64);
            Some(metadata)
        } else {
            None
        };
        self.write_view_data_group(
            view_name,
            tuples,
            schema,
            refreshed.as_ref(),
            refreshed
                .as_ref()
                .map(|_| super::WalOperation::RefreshMaterializedView {
                    name: view_name.to_string(),
                    concurrent,
                    incremental: false,
                }),
            false,
        )
    }

    /// SQL, the scheduler and both storage APIs share the same atomic replacement.
    /// There is no indexed/storage-mode fallback that writes outside the group.
    pub fn refresh_view_in_one_group(
        &self,
        view_name: &str,
        tuples: &[Tuple],
        schema: &Schema,
        refreshed: &MaterializedViewMetadata,
        refresh: super::WalOperation,
    ) -> Result<Option<u64>> {
        self.write_view_data_group(view_name, tuples, schema, Some(refreshed), Some(refresh), false)
            .map(Some)
    }

    /// CONCURRENTLY has the same old-or-new visibility guarantee as plain refresh.
    pub fn store_view_data_concurrent(&self, view_name: &str, tuples: Vec<Tuple>, schema: &Schema) -> Result<u64> {
        self.store_view_data_mode(view_name, &tuples, schema, true)
    }

    fn write_view_data_group(
        &self,
        view_name: &str,
        tuples: &[Tuple],
        requested: &Schema,
        metadata: Option<&MaterializedViewMetadata>,
        refresh: Option<super::WalOperation>,
        create: bool,
    ) -> Result<u64> {
        use super::art_manager::{ArtIndexManager, MergedKeysOutcome, MergedRowChange};
        use super::columnar::{BatchPresence, BatchStats, ColumnBatch, ColumnarStore, BATCH_SIZE};

        self.storage.refuse_statement_on_branch("REFRESH MATERIALIZED VIEW")?;
        super::Catalog::refuse_unstorable_table_name(view_name)?;
        let table = Self::mv_data_table_name(view_name);
        // Freeze main for existing branch children before the first catalog/row
        // read. The guard also excludes every competing writer until publication.
        let _guard = self
            .storage
            .main_catalog_write(&[&table], "MATERIALIZED VIEW population")?;
        let catalog = self.storage.catalog();
        let exists = catalog.main_table_exists(&table)?;
        let current = if exists {
            Some(catalog.main_table_schema(&table)?)
        } else {
            None
        };
        let compatible_schema = current.as_ref().filter(|have| {
            have.columns.len() == requested.columns.len()
                && have
                    .columns
                    .iter()
                    .zip(&requested.columns)
                    .all(|(have, want)| have.name == want.name && have.data_type == want.data_type)
        });
        let compatible = compatible_schema.is_some();
        let schema = compatible_schema.unwrap_or(requested);
        for tuple in tuples {
            if tuple.values.len() != schema.columns.len() {
                return Err(Error::storage("materialized view row does not match its result schema"));
            }
        }
        let old = match &current {
            Some(schema) => self.storage.scan_table_with_schema(&table, schema)?,
            None => Vec::new(),
        };
        let rows: Vec<(u64, Tuple)> = tuples
            .iter()
            .map(|tuple| {
                let id = self.storage.next_row_id_volatile(&table);
                (id, Tuple::with_row_id(tuple.values.clone(), id))
            })
            .collect();

        // Keep index fast paths on scans throughout an eager key claim and its
        // publication/undo, even though this is a DDL batch rather than DML.
        let census = self.storage.begin_transaction()?;
        census.note_index_key_removal(&table);
        // A logical-shape replacement drops the old index definitions. Validate
        // its new schema-owned keys using the same index implementation BEFORE
        // the write; publish the validated trees only after the catalog flip.
        let replacement_indexes = if exists && !compatible {
            let indexes = ArtIndexManager::new();
            Self::register_schema_indexes(&indexes, &table, schema)?;
            indexes
                .on_insert_tuples(&table, schema, &rows)
                .map_err(|e| Error::query_execution(format!("materialized view keys: {e}")))?;
            Some(indexes)
        } else {
            None
        };

        // Vector hooks have not run before commit. Preserve that known-good
        // state on a refused group by proving the actual schema, definitions
        // and logical rows are unchanged; no inference from error text.
        let prior_vector_state = if self.storage.vector_indexes().table_has_indexes(&table) {
            let raw_schema = self.storage.get(&format!("meta:table:{table}").into_bytes())?;
            let definitions: Vec<_> = catalog
                .index_definitions_strict()?
                .into_iter()
                .filter(|(_, definition)| definition.table_name == table)
                .collect();
            Some((
                raw_schema,
                bincode::serialize(&definitions)
                    .map_err(|error| Error::storage(format!("MV prior index definitions: {error}")))?,
            ))
        } else {
            None
        };

        let mut ddl = super::DdlBatch::new(self.storage);
        if !exists {
            catalog.stage_create_table(&table, schema.clone(), &mut ddl)?;
        }
        // Hold an in-flight commit slot until the group is durable and published,
        // so a snapshot allocated after this timestamp cannot scan before it lands.
        struct Inflight {
            registry: std::sync::Arc<super::conflict::WriteConflictRegistry>,
            timestamp: u64,
        }
        impl Drop for Inflight {
            fn drop(&mut self) {
                self.registry.end_commit(self.timestamp);
            }
        }
        let timestamp = self.storage.next_commit_timestamp(true);
        let _inflight = Inflight {
            registry: self.storage.conflict_registry(),
            timestamp,
        };
        if exists && !compatible {
            catalog.stage_materialized_view_replacement(&table, schema, timestamp, &mut ddl)?;
        }
        // Clear even orphan data rows of a missing table, while preserving the
        // monotonically allocated ids so a current row never reuses an old id.
        let prefix = format!("data:{table}:");
        let mut opts = rocksdb::ReadOptions::default();
        opts.set_total_order_seek(true);
        for item in self.storage.db.iterator_opt(
            rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
            opts,
        ) {
            let (key, _) = item.map_err(|e| Error::storage(format!("MV row scan: {e}")))?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let Some(id) = StorageEngine::parse_row_id_after_prefix(&key, prefix.len()) else {
                continue;
            };
            ddl.delete(&key);
            if exists && self.storage.time_travel_enabled() {
                self.storage.snapshot_manager().stage_row_change_history(
                    ddl.batch_mut(),
                    &table,
                    id,
                    timestamp,
                    None,
                )?;
            }
            ddl.log(super::WalOperation::Delete {
                table: table.clone(),
                key: key.to_vec(),
            });
        }

        let changes: Vec<MergedRowChange<'_>> = old
            .iter()
            .filter_map(|tuple| {
                tuple.row_id.map(|row_id| MergedRowChange {
                    row_id,
                    old: Some(tuple),
                    new: None,
                })
            })
            .chain(rows.iter().map(|(row_id, tuple)| MergedRowChange {
                row_id: *row_id,
                old: None,
                new: Some(tuple),
            }))
            .collect();
        let claim = if replacement_indexes.is_none() {
            match self
                .storage
                .art_indexes()
                .claim_merged_table_keys(&table, schema, &changes)
                .map_err(|e| Error::query_execution(format!("materialized view keys: {e}")))?
            {
                MergedKeysOutcome::Claimed(claim) => Some(claim),
                MergedKeysOutcome::HeldBy { constraint, .. } => {
                    return Err(Error::query_execution(format!(
                        "duplicate key value violates unique constraint \"{constraint}\""
                    )));
                }
            }
        } else {
            None
        };

        for (id, tuple) in &rows {
            let logical = bincode::serialize(tuple).map_err(|e| Error::storage(format!("MV tuple: {e}")))?;
            // As in transaction DML, dictionary/CAS columns may hold inline
            // logical values. COLUMNAR requires side data; its row marker and
            // batches are staged here, and history always keeps full values.
            let mut stored_tuple = tuple.clone();
            for (index, column) in schema.columns.iter().enumerate() {
                if column.storage_mode == crate::ColumnStorageMode::Columnar {
                    *stored_tuple
                        .values
                        .get_mut(index)
                        .ok_or_else(|| Error::storage("materialized view row does not match its result schema"))? =
                        crate::Value::ColumnarRef;
                }
            }
            let stored = bincode::serialize(&stored_tuple).map_err(|e| Error::storage(format!("MV tuple: {e}")))?;
            let key = format!("data:{table}:{id}");
            ddl.put(&key, &stored)?;
            if self.storage.time_travel_enabled() {
                let sealed = super::tde::seal(self.storage.key_manager_ref(), &logical)?;
                self.storage.snapshot_manager().stage_rewritten_row_version(
                    ddl.batch_mut(),
                    &table,
                    *id,
                    timestamp,
                    sealed.as_ref(),
                )?;
            }
            ddl.log(super::WalOperation::Insert {
                table: table.clone(),
                key: key.into_bytes(),
                tuple: logical,
            });
        }
        if let Some(highest) = self.storage.row_counter_high_water(&table) {
            ddl.put(
                format!("counter:{table}"),
                &bincode::serialize(&highest).map_err(|e| Error::storage(format!("MV row counter: {e}")))?,
            )?;
        }
        if let Some(metadata) = metadata {
            let mut metadata = metadata.clone();
            metadata.schema = schema.clone();
            let bytes = bincode::serialize(&metadata).map_err(|e| Error::storage(format!("MV metadata: {e}")))?;
            ddl.put(Self::mv_metadata_key(view_name), &bytes)?;
            if !create {
                self.stage_applied_delta_cleanup(&metadata, &mut ddl)?;
            }
            if create {
                ddl.log(super::WalOperation::CreateMaterializedView {
                    name: view_name.to_string(),
                    definition: bytes,
                });
            }
        }
        if let Some(refresh) = refresh {
            ddl.log(refresh);
        }
        if self.storage.time_travel_enabled() {
            let snapshot = self
                .storage
                .snapshot_manager()
                .stage_snapshot_record(ddl.batch_mut(), timestamp, None)?;
            let storage = self.storage;
            let table = table.clone();
            ddl.on_commit(move || storage.snapshot_manager().publish_rewrite_snapshot(&table, snapshot));
        }

        // A population can reach storage before its vector hooks run. Keep
        // old dumped graphs from being resurrected after that uncertain edge:
        // invalidation rides the SAME group, and a refused group preserves it.
        for (name, _) in self.storage.vector_indexes().indexes_on_table(&table) {
            ddl.delete(super::index_snapshot::vec_snapshot_marker_key(&name));
        }

        // Persistent HNSW is a database graph: stage its full replacement in
        // this same row group. DdlBatch settles all its owned graph leases
        // before any registry publication callback or ACK.
        let replaced_persistent = if compatible {
            self.storage
                .vector_indexes()
                .stage_persistent_table_replacement(&table, schema, &rows, &mut ddl)?
        } else {
            Vec::new()
        };

        // Last before commit: the process-global zone-stats lock must not cover
        // query execution, key claims or unrelated catalog work.
        let uses_columnar = schema
            .columns
            .iter()
            .any(|c| c.storage_mode == crate::ColumnStorageMode::Columnar);
        if uses_columnar || !compatible {
            let stats = super::columnar::stats_write_lock();
            if !compatible {
                ColumnarStore::stage_table_sidecar_purge(&self.storage.db, &table, ddl.batch_mut())?;
            }
            let mut presence: BTreeMap<u64, (Vec<u64>, Vec<u64>)> = BTreeMap::new();
            for tuple in &old {
                if let Some(id) = tuple.row_id {
                    presence.entry(id / BATCH_SIZE as u64).or_default().1.push(id);
                }
            }
            for (id, _) in &rows {
                presence.entry(*id / BATCH_SIZE as u64).or_default().0.push(*id);
            }
            for (index, column) in schema.columns.iter().enumerate() {
                if column.storage_mode != crate::ColumnStorageMode::Columnar {
                    continue;
                }
                let mut batches: BTreeMap<u64, Vec<(u64, crate::Value)>> = BTreeMap::new();
                for (id, tuple) in &rows {
                    let value = tuple
                        .values
                        .get(index)
                        .ok_or_else(|| Error::storage("materialized view row does not match its result schema"))?;
                    batches
                        .entry(*id / BATCH_SIZE as u64)
                        .or_default()
                        .push((*id, value.clone()));
                }
                for (batch_id, values) in batches {
                    if compatible {
                        ColumnarStore::apply_batch_values(
                            &self.storage.db,
                            ddl.batch_mut(),
                            &table,
                            &column.name,
                            batch_id,
                            &values,
                        )?;
                    } else {
                        let mut batch = ColumnBatch::new(&column.name, batch_id * BATCH_SIZE as u64);
                        for (id, value) in values {
                            batch.set(id, value);
                        }
                        let stats = BatchStats::from_batch(&batch);
                        let bytes = super::typed_batch::encode_column_batch(&batch, &stats)?;
                        ddl.batch_mut()
                            .put(format!("col:{table}:{}:{batch_id}", column.name), bytes);
                        ddl.batch_mut().put(
                            format!("colz:{table}:{}:{batch_id}", column.name),
                            bincode::serialize(&stats)
                                .map_err(|e| Error::storage(format!("MV columnar stats: {e}")))?,
                        );
                    }
                }
            }
            if uses_columnar {
                for (batch_id, (set, clear)) in presence {
                    if compatible {
                        ColumnarStore::apply_presence_updates(
                            &self.storage.db,
                            ddl.batch_mut(),
                            &table,
                            batch_id,
                            &set,
                            &clear,
                        )?;
                    } else {
                        // The purge is staged, so an RMW against the raw store
                        // would still inherit orphan bits from the old table.
                        let mut present = BatchPresence::new();
                        for id in set {
                            present.set((id % BATCH_SIZE as u64) as usize);
                        }
                        ddl.batch_mut().put(
                            format!("colp:{table}:{batch_id}"),
                            bincode::serialize(&present).map_err(|e| Error::storage(format!("MV presence: {e}")))?,
                        );
                    }
                }
                ddl.batch_mut().put(format!("colpm:{table}"), [1u8]);
            }
            ddl.hold_until_written(stats);
        }
        self.storage.note_committed_write(&table, timestamp);
        let storage = self.storage;
        let published_table = table.clone();
        // Both successful and possibly-applied failed writes fence data caches
        // while the in-flight commit slot is held. Catalog/layout publication
        // alone advances schema generation on actual create/shape replacement.
        ddl.on_commit(storage.data_write_attempt_publication(table.clone()));
        ddl.on_abort(storage.data_write_attempt_publication(table.clone()));
        ddl.on_commit(move || {
            storage.row_locks().note_rows_changed(&published_table);
        });
        ddl.exit_after_write(if create { "create_mv" } else { "refresh_mv" }, view_name);
        // A native write error does not prove that the batch was rejected.
        // Never let claim Drop blindly restore the old memberships after an
        // uncertain result. The exclusive guard, census and in-flight slot
        // remain held until actual storage has been reconciled or quarantined.
        ddl.reconcile_unknown_in_caller();
        let written = match ddl.commit() {
            Ok(written) => written,
            Err(error) => {
                // DdlBatch has settled every graph lease before reconciliation
                // enters manager registries.
                if let Some(claim) = claim {
                    claim.disarm_for_reconciliation();
                }
                self.reconcile_failed_population(view_name, &table, timestamp, &old, prior_vector_state.as_ref())?;
                return Err(error);
            }
        };
        let publication = (|| -> Result<()> {
            if let Some(claim) = claim {
                claim
                    .finish()
                    .map_err(|error| Error::storage(format!("MV key publication: {error}")))?;
            }
            if replacement_indexes.is_some() {
                Self::register_schema_indexes(self.storage.art_indexes(), &table, schema)?;
                self.storage
                    .art_indexes()
                    .on_insert_tuples(&table, schema, &rows)
                    .map_err(|e| Error::storage(format!("MV index publication: {e}")))?;
            }
            Ok(())
        })();
        if let Err(error) = publication {
            self.reconcile_failed_population(view_name, &table, timestamp, &old, prior_vector_state.as_ref())?;
            return Err(error);
        }
        self.storage
            .vector_indexes()
            .forget_replaced_row_records(&replaced_persistent, &old);
        if compatible && self.storage.vector_indexes().table_has_indexes(&table) {
            for tuple in &old {
                if let Some(id) = tuple.row_id {
                    self.storage.vector_indexes().on_row_delete_excluding(
                        &table,
                        id,
                        Some(schema),
                        Some(tuple),
                        &replaced_persistent,
                    );
                }
            }
            for (id, tuple) in &rows {
                self.storage
                    .vector_indexes()
                    .on_row_insert_excluding(&table, *id, schema, tuple, &replaced_persistent);
            }
        }
        drop(census);
        drop(_inflight);
        written.wait_replicated();
        Ok(rows.len() as u64)
    }

    /// Rebuild from actual durable state, never the requested result or an old
    /// schema/row cache. The caller keeps its exclusive table ownership, full
    /// write census and in-flight timestamp through this entire operation.
    fn reconcile_failed_population(
        &self,
        view: &str,
        table: &str,
        timestamp: u64,
        prior_rows: &[Tuple],
        prior_vector_state: Option<&(Option<Vec<u8>>, Vec<u8>)>,
    ) -> Result<()> {
        let storage = self.storage;
        storage.data_write_attempt_publication(table.to_string())();
        storage.invalidate_schema_cache(table);
        storage.invalidate_schema_cache(view);
        storage.invalidate_missing_values(table);
        storage.invalidate_table_constraints_cache(table);
        storage.forget_table_dictionaries(table);
        storage.snapshot_manager().invalidate_cached_versions(table);
        let reconciled = (|| -> Result<()> {
            let catalog = storage.catalog();
            let definition = storage.get(&format!("meta:table:{table}").into_bytes())?;
            let definitions: Vec<_> = catalog
                .index_definitions_strict()?
                .into_iter()
                .filter(|(_, definition)| definition.table_name == table)
                .collect();
            let Some(definition) = definition else {
                if !definitions.is_empty() {
                    return Err(Error::storage("MV indexes reference an absent backing table"));
                }
                storage
                    .art_indexes()
                    .drop_table_indexes(table)
                    .map_err(|error| Error::storage(format!("MV absent-table indexes: {error}")))?;
                if storage.vector_indexes().table_has_indexes(table) {
                    return Err(Error::storage(
                        "MV backing table disappeared while vector indexes remain",
                    ));
                }
                return Ok(());
            };
            let schema: Schema = bincode::deserialize(&definition)
                .map_err(|error| Error::storage(format!("MV actual schema: {error}")))?;
            let mut columns = std::collections::HashSet::new();
            for column in &schema.columns {
                if !columns.insert(column.name.as_str()) {
                    return Err(Error::storage("MV actual schema has duplicate columns"));
                }
            }
            let validate_columns = |names: &[String]| -> Result<()> {
                if names.is_empty() || names.iter().any(|name| schema.get_column(name).is_none()) {
                    return Err(Error::storage(format!(
                        "MV actual index has invalid columns: {names:?}"
                    )));
                }
                Ok(())
            };
            // Decoding resolves CURRENT dictionary/CAS/columnar side data only
            // after layout and dictionary caches have been discarded above.
            let shape = storage.row_shape(table, schema.columns.len())?;
            let prefix = format!("data:{table}:");
            let mut options = rocksdb::ReadOptions::default();
            options.set_total_order_seek(true);
            let mut rows = Vec::new();
            for entry in storage.db.iterator_opt(
                rocksdb::IteratorMode::From(prefix.as_bytes(), rocksdb::Direction::Forward),
                options,
            ) {
                let (key, raw) = entry.map_err(|error| Error::storage(format!("MV actual row scan: {error}")))?;
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                let Some(id) = StorageEngine::parse_row_id_after_prefix(&key, prefix.len()) else {
                    continue;
                };
                let plain = super::tde::open_ref(storage.key_manager_ref(), &key, &raw)?;
                let logical = storage
                    .snapshot_manager()
                    .logical_current_row_for_history(table, id, &plain, None)?;
                let mut tuple: Tuple = bincode::deserialize(&logical)
                    .map_err(|error| Error::storage(format!("MV actual row: {error}")))?;
                if tuple.values.len() > schema.columns.len() {
                    return Err(Error::storage("MV actual row is wider than its durable schema"));
                }
                shape.apply(&mut tuple);
                tuple.row_id = Some(id);
                rows.push((id, tuple));
            }
            let constraints = catalog.main_table_constraints(table)?;
            // Vector reconciliation must not emit persistent-HNSW writes on
            // an uncertain DDL result. Retire/fence the table if a vector
            // manager is involved; reopen rebuilds from the surviving defs.
            if storage.vector_indexes().table_has_indexes(table)
                || definitions.iter().any(|(_, definition)| {
                    super::catalog::index_family(definition.index_type.as_deref())
                        == Some(super::catalog::IndexFamily::Vector)
                })
            {
                let unchanged = if let Some((prior_schema, prior_definitions)) = prior_vector_state {
                    let prior_rows: BTreeMap<_, _> = prior_rows
                        .iter()
                        .map(|tuple| {
                            let id = tuple.row_id.ok_or_else(|| Error::storage("MV prior row lacks an id"))?;
                            Ok((
                                id,
                                bincode::serialize(tuple)
                                    .map_err(|error| Error::storage(format!("MV prior row: {error}")))?,
                            ))
                        })
                        .collect::<Result<_>>()?;
                    let actual_rows: BTreeMap<_, _> = rows
                        .iter()
                        .map(|(id, tuple)| {
                            Ok((
                                *id,
                                bincode::serialize(tuple)
                                    .map_err(|error| Error::storage(format!("MV actual row: {error}")))?,
                            ))
                        })
                        .collect::<Result<_>>()?;
                    prior_schema.as_deref() == Some(definition.as_slice())
                        && *prior_definitions
                            == bincode::serialize(&definitions)
                                .map_err(|error| Error::storage(format!("MV actual index definitions: {error}")))?
                        && prior_rows == actual_rows
                } else {
                    false
                };
                if !unchanged {
                    return Err(Error::storage(
                        "MV vector state requires a fresh rebuild after an uncertain population",
                    ));
                }
            }
            let art = storage.art_indexes();
            art.drop_table_indexes(table)
                .map_err(|error| Error::storage(format!("MV index retirement: {error}")))?;
            // Durable user names precede generated constraint names, exactly
            // as on reopen. No best-effort decoder/registration is used here.
            for (name, definition) in &definitions {
                validate_columns(std::slice::from_ref(&definition.column_name))?;
                match super::catalog::index_family(definition.index_type.as_deref()) {
                    Some(super::catalog::IndexFamily::Art) => {
                        if let Some(columns) = crate::sql::executor::ddl::unique_index_columns(
                            &definition.options,
                            &definition.column_name,
                        ) {
                            validate_columns(&columns)?;
                            art.create_unique_index(table, &columns, Some(name))
                                .map_err(|error| Error::storage(format!("MV actual unique index '{name}': {error}")))?;
                        } else {
                            art.create_manual_index(name, table, std::slice::from_ref(&definition.column_name))
                                .map_err(|error| Error::storage(format!("MV actual index '{name}': {error}")))?;
                        }
                    }
                    Some(super::catalog::IndexFamily::DdlOnly) => {}
                    // The vector state above proved schema/definitions/rows
                    // unchanged; its preserved manager needs no ART structure.
                    Some(super::catalog::IndexFamily::Vector) => {}
                    _ => {
                        return Err(Error::storage(format!(
                            "MV actual index '{name}' has an unsupported family"
                        )))
                    }
                }
            }
            Self::register_schema_indexes(art, table, &schema)?;
            for unique in &constraints.unique_constraints {
                validate_columns(&unique.columns)?;
            }
            catalog.register_unique_constraint_indexes(table, &constraints)?;
            for foreign in &constraints.foreign_keys {
                validate_columns(&foreign.columns)?;
                if foreign.table_name != table || foreign.columns.len() != foreign.references_columns.len() {
                    return Err(Error::storage("MV actual foreign key has inconsistent owner/arity"));
                }
                art.create_fk_index(
                    table,
                    &foreign.columns,
                    &foreign.references_table,
                    &foreign.references_columns,
                    Some(&foreign.name),
                )
                .map_err(|error| Error::storage(format!("MV actual foreign-key index: {error}")))?;
            }
            art.on_insert_tuples(table, &schema, &rows)
                .map_err(|error| Error::storage(format!("MV actual index backfill: {error}")))?;
            // Applied-then-error skipped the ordinary snapshot publication.
            // Only publish the exact record if it actually survived the group.
            let key = format!("snapshot:{timestamp}");
            if let Some(raw) = storage.db.get(key.as_bytes())? {
                storage
                    .snapshot_manager()
                    .ingest_snapshot_record(key.as_bytes(), &raw)?;
            }
            storage.row_locks().note_rows_changed(table);
            Ok(())
        })();
        if let Err(error) = reconciled {
            let quarantined = storage.quarantine_materialized_view(view, table, &error.to_string());
            if let Err(retire) = storage.art_indexes().drop_table_indexes(table) {
                tracing::error!("MV quarantined index retirement for {table}: {retire}");
            }
            storage.vector_indexes().retire_table_indexes(table, &error.to_string());
            return Err(quarantined);
        }
        Ok(())
    }

    fn register_schema_indexes(
        indexes: &super::art_manager::ArtIndexManager,
        table: &str,
        schema: &Schema,
    ) -> Result<()> {
        let primary: Vec<String> = schema
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        if !primary.is_empty() {
            indexes
                .create_pk_index(table, &primary)
                .map_err(|e| Error::storage(format!("MV primary key: {e}")))?;
        }
        for column in schema.columns.iter().filter(|c| c.unique && !c.primary_key) {
            indexes
                .create_constraint_unique_index(table, std::slice::from_ref(&column.name), None)
                .map_err(|e| Error::storage(format!("MV unique key: {e}")))?;
        }
        Ok(())
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
