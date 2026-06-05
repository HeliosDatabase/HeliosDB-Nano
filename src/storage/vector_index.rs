//! Vector index management for storage engine
//!
//! Manages HNSW indexes for vector columns, including creation, maintenance,
//! and query execution.

#![allow(unused_variables)]

use crate::vector::{
    DistanceMetric, HnswConfig, MultiMetricHnswIndex, ProductQuantizerConfig, QuantizedHnswConfig, QuantizedHnswIndex,
    Vector,
};
use crate::{Error, Result};
use bincode;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Vector index type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VectorIndexType {
    /// Standard HNSW index (no quantization)
    Standard(HnswConfig),
    /// Quantized HNSW index with Product Quantization
    Quantized(QuantizedHnswConfig),
    /// RocksDB-backed persistent PQ-HNSW index.
    Persistent(PersistentVectorConfig),
}

/// Persistent vector index metadata stored in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentVectorConfig {
    pub dimension: usize,
    pub distance_metric: DistanceMetric,
    pub pq_enabled: bool,
    pub rerank_precision: String,
}

/// Vector index metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexMetadata {
    /// Index name
    pub name: String,
    /// Table name
    pub table_name: String,
    /// Column name
    pub column_name: String,
    /// Index type and configuration
    pub index_type: VectorIndexType,
}

/// Internal index storage
enum IndexStorage {
    /// Standard HNSW index
    Standard(MultiMetricHnswIndex),
    /// Quantized HNSW index
    Quantized(QuantizedHnswIndex),
    /// Persistent PQ-HNSW index
    #[cfg(feature = "vector-persist")]
    Persistent(crate::vector::persistent::PersistentVectorIndex),
}

/// Stored vector payload for API fetch, metadata filters, and exact filtered search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredVectorRecord {
    pub id: String,
    pub row_id: u64,
    pub vector: Vector,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VectorExternalKey {
    id: String,
    namespace: Option<String>,
}

impl VectorExternalKey {
    fn new(id: impl Into<String>, namespace: Option<&str>) -> Self {
        Self {
            id: id.into(),
            namespace: namespace.map(ToOwned::to_owned),
        }
    }
}

/// Vector index manager
pub struct VectorIndexManager {
    /// Map from index name to index storage
    indexes: Arc<RwLock<HashMap<String, IndexStorage>>>,
    /// Map from index name to metadata
    metadata: Arc<RwLock<HashMap<String, VectorIndexMetadata>>>,
    /// Raw vector payloads by index and internal row id.
    records: Arc<RwLock<HashMap<String, HashMap<u64, StoredVectorRecord>>>>,
    /// External API id + namespace -> row id mapping per index.
    external_ids: Arc<RwLock<HashMap<String, HashMap<VectorExternalKey, u64>>>>,
}

impl VectorIndexManager {
    /// Create a new vector index manager
    pub fn new() -> Self {
        Self {
            indexes: Arc::new(RwLock::new(HashMap::new())),
            metadata: Arc::new(RwLock::new(HashMap::new())),
            records: Arc::new(RwLock::new(HashMap::new())),
            external_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new standard (non-quantized) vector index
    pub fn create_index(
        &self,
        name: String,
        table_name: String,
        column_name: String,
        dimension: usize,
        distance_metric: DistanceMetric,
    ) -> Result<()> {
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        // Check if index already exists
        if indexes.contains_key(&name) {
            return Err(Error::query_execution(format!("Index '{}' already exists", name)));
        }

        // Create HNSW configuration
        let config = HnswConfig {
            dimension,
            distance_metric,
            max_connections: 16,
            ef_construction: 200,
            // Performance optimization: Dynamic ef_search tuning
            ef_search_base: 200,
            dynamic_ef_search: true,
            ef_search_min: 50,
            ef_search_max: 500,
        };

        // Create HNSW index
        let index = MultiMetricHnswIndex::new(config.clone())?;

        // Store metadata
        let meta = VectorIndexMetadata {
            name: name.clone(),
            table_name,
            column_name,
            index_type: VectorIndexType::Standard(config),
        };

        indexes.insert(name.clone(), IndexStorage::Standard(index));
        metadata.insert(name.clone(), meta);
        self.records.write().entry(name.clone()).or_default();
        self.external_ids.write().entry(name).or_default();

        Ok(())
    }

    /// Create a RocksDB-backed persistent vector index.
    #[cfg(feature = "vector-persist")]
    pub fn create_persistent_index(
        &self,
        name: String,
        table_name: String,
        column_name: String,
        dimension: usize,
        distance_metric: DistanceMetric,
        pq_config: Option<ProductQuantizerConfig>,
        rerank_precision: Option<crate::vector::persistent::VectorPrecision>,
        training_vectors: &[Vector],
        db: Arc<rocksdb::DB>,
    ) -> Result<()> {
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        if indexes.contains_key(&name) {
            return Err(Error::query_execution(format!("Index '{}' already exists", name)));
        }

        let index_id = stable_index_id(&name);
        let mut config = crate::vector::persistent::PqHnswConfig::new(dimension, distance_metric);
        config.pq_config = pq_config;
        if let Some(precision) = rerank_precision {
            config.rerank_precision = precision;
        }
        let rerank_precision_name = persistent_precision_name(config.rerank_precision).to_string();

        let pq_enabled = config.pq_config.is_some();
        let index = if pq_enabled {
            crate::vector::persistent::PersistentVectorIndex::create_with_pq(db, index_id, config, training_vectors)?
        } else {
            crate::vector::persistent::PersistentVectorIndex::create(db, index_id, config)?
        };

        let meta = VectorIndexMetadata {
            name: name.clone(),
            table_name,
            column_name,
            index_type: VectorIndexType::Persistent(PersistentVectorConfig {
                dimension,
                distance_metric,
                pq_enabled,
                rerank_precision: rerank_precision_name,
            }),
        };

        indexes.insert(name.clone(), IndexStorage::Persistent(index));
        metadata.insert(name.clone(), meta);
        self.records.write().entry(name.clone()).or_default();
        self.external_ids.write().entry(name).or_default();
        Ok(())
    }

    /// Stub for builds without `vector-persist`.
    #[cfg(not(feature = "vector-persist"))]
    pub fn create_persistent_index(
        &self,
        _name: String,
        _table_name: String,
        _column_name: String,
        _dimension: usize,
        _distance_metric: DistanceMetric,
        _pq_config: Option<ProductQuantizerConfig>,
        _rerank_precision: Option<()>,
        _training_vectors: &[Vector],
        _db: Arc<rocksdb::DB>,
    ) -> Result<()> {
        Err(Error::query_execution(
            "Persistent vector indexes require the 'vector-persist' feature".to_string(),
        ))
    }

    /// Create a new quantized vector index
    pub fn create_quantized_index(
        &self,
        name: String,
        table_name: String,
        column_name: String,
        dimension: usize,
        distance_metric: DistanceMetric,
        pq_config: ProductQuantizerConfig,
        training_vectors: &[Vector],
    ) -> Result<()> {
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        // Check if index already exists
        if indexes.contains_key(&name) {
            return Err(Error::query_execution(format!("Index '{}' already exists", name)));
        }

        // Create Quantized HNSW configuration
        let config = QuantizedHnswConfig {
            max_connections: 16,
            ef_construction: 200,
            ef_search: 200,
            dimension,
            distance_metric,
            pq_config,
            use_pq_storage: true,
        };

        // Train and create Quantized HNSW index
        let index = QuantizedHnswIndex::train(config.clone(), training_vectors)
            .map_err(|e| Error::query_execution(format!("Failed to train PQ index: {}", e)))?;

        // Store metadata
        let meta = VectorIndexMetadata {
            name: name.clone(),
            table_name,
            column_name,
            index_type: VectorIndexType::Quantized(config),
        };

        indexes.insert(name.clone(), IndexStorage::Quantized(index));
        metadata.insert(name.clone(), meta);
        self.records.write().entry(name.clone()).or_default();
        self.external_ids.write().entry(name).or_default();

        Ok(())
    }

    /// Get an index by name
    pub fn get_index(&self, name: &str) -> Result<Arc<MultiMetricHnswIndex>> {
        let indexes = self.indexes.read();
        indexes
            .get(name)
            .map(|idx| {
                // We can't directly clone Arc<MultiMetricHnswIndex> because it's behind RwLock
                // For now, we'll return an error - in production, we'd need a different approach
                Err(Error::query_execution("Vector index access not yet fully implemented"))
            })
            .unwrap_or_else(|| Err(Error::query_execution(format!("Index '{}' not found", name))))
    }

    /// Insert a vector into an index
    pub fn insert_vector(&self, index_name: &str, row_id: u64, vector: &Vector) -> Result<()> {
        let indexes = self.indexes.read();
        if let Some(index) = indexes.get(index_name) {
            match index {
                IndexStorage::Standard(idx) => idx.insert(row_id, vector)?,
                IndexStorage::Quantized(idx) => idx.insert(row_id, vector)?,
                #[cfg(feature = "vector-persist")]
                IndexStorage::Persistent(idx) => {
                    idx.insert(row_id, vector)?;
                }
            }
            Ok(())
        } else {
            Err(Error::query_execution(format!("Index '{}' not found", index_name)))
        }
    }

    /// Bulk-insert a batch of `(row_id, vector)` pairs into an index.
    ///
    /// For the standard (non-quantized) HNSW backend this builds the graph in
    /// parallel across all cores via `parallel_insert`, which is dramatically
    /// faster than the per-row `insert_vector` loop for `CREATE INDEX`
    /// backfill. Quantized and persistent backends don't expose a parallel
    /// build path here, so they fall back to the sequential per-row insert —
    /// correctness is identical either way.
    pub fn insert_vectors_batch(&self, index_name: &str, batch: &[(u64, Vector)]) -> Result<()> {
        let indexes = self.indexes.read();
        let Some(index) = indexes.get(index_name) else {
            return Err(Error::query_execution(format!("Index '{}' not found", index_name)));
        };
        match index {
            IndexStorage::Standard(idx) => idx.insert_batch(batch),
            IndexStorage::Quantized(idx) => {
                for (row_id, vector) in batch {
                    idx.insert(*row_id, vector)?;
                }
                Ok(())
            }
            #[cfg(feature = "vector-persist")]
            IndexStorage::Persistent(idx) => {
                for (row_id, vector) in batch {
                    idx.insert(*row_id, vector)?;
                }
                Ok(())
            }
        }
    }

    /// Search for nearest neighbors
    pub fn search(&self, index_name: &str, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        let indexes = self.indexes.read();
        if let Some(index) = indexes.get(index_name) {
            match index {
                IndexStorage::Standard(idx) => idx.search(query, k),
                IndexStorage::Quantized(idx) => idx.search(query, k),
                #[cfg(feature = "vector-persist")]
                IndexStorage::Persistent(idx) => idx.search(query, k, k.max(200)),
            }
        } else {
            Err(Error::query_execution(format!("Index '{}' not found", index_name)))
        }
    }

    /// Delete a vector from an index
    pub fn delete_vector(&self, index_name: &str, row_id: u64) -> Result<()> {
        let indexes = self.indexes.read();
        if let Some(index) = indexes.get(index_name) {
            match index {
                IndexStorage::Standard(idx) => idx.delete(row_id)?,
                IndexStorage::Quantized(idx) => idx.delete(row_id)?,
                #[cfg(feature = "vector-persist")]
                IndexStorage::Persistent(idx) => {
                    idx.remove(row_id)?;
                }
            }
            if let Some(records) = self.records.write().get_mut(index_name) {
                if let Some(record) = records.remove(&row_id) {
                    if let Some(ids) = self.external_ids.write().get_mut(index_name) {
                        ids.remove(&VectorExternalKey::new(&record.id, record.namespace.as_deref()));
                    }
                }
            }
            Ok(())
        } else {
            Err(Error::query_execution(format!("Index '{}' not found", index_name)))
        }
    }

    /// Drop an index
    pub fn drop_index(&self, name: &str) -> Result<()> {
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        let Some(index) = indexes.remove(name) else {
            return Err(Error::query_execution(format!("Index '{}' does not exist", name)));
        };

        #[cfg(feature = "vector-persist")]
        if let IndexStorage::Persistent(index) = index {
            index.drop_storage()?;
        }

        metadata.remove(name);
        self.records.write().remove(name);
        self.external_ids.write().remove(name);
        Ok(())
    }

    /// Insert a vector and retain API-visible id, metadata, namespace, and raw values.
    pub fn insert_vector_record(
        &self,
        index_name: &str,
        row_id: u64,
        external_id: String,
        vector: &Vector,
        metadata: Option<HashMap<String, serde_json::Value>>,
        namespace: Option<String>,
    ) -> Result<()> {
        let key = VectorExternalKey::new(&external_id, namespace.as_deref());
        let existing_row = self
            .external_ids
            .read()
            .get(index_name)
            .and_then(|ids| ids.get(&key).copied());
        if let Some(existing_row) = existing_row {
            if existing_row != row_id {
                self.delete_vector(index_name, existing_row)?;
            }
        }

        self.insert_vector(index_name, row_id, vector)?;
        let record = StoredVectorRecord {
            id: external_id.clone(),
            row_id,
            vector: vector.clone(),
            metadata,
            namespace,
        };
        self.records
            .write()
            .entry(index_name.to_string())
            .or_default()
            .insert(row_id, record);
        self.external_ids
            .write()
            .entry(index_name.to_string())
            .or_default()
            .insert(key, row_id);
        Ok(())
    }

    /// Fetch vector records by external id.
    pub fn fetch_records(&self, index_name: &str, ids: &[String]) -> Result<Vec<StoredVectorRecord>> {
        self.fetch_records_in_namespace(index_name, ids, None)
    }

    /// Fetch vector records by external id, optionally scoped to a namespace.
    pub fn fetch_records_in_namespace(
        &self,
        index_name: &str,
        ids: &[String],
        namespace: Option<&str>,
    ) -> Result<Vec<StoredVectorRecord>> {
        let external_ids = self.external_ids.read();
        let records = self.records.read();
        let id_map = external_ids.get(index_name).ok_or_else(|| {
            if self.index_exists(index_name) {
                Error::query_execution(format!("Index '{}' has no vector records", index_name))
            } else {
                Error::query_execution(format!("Index '{}' not found", index_name))
            }
        })?;
        let Some(record_map) = records.get(index_name) else {
            return Ok(Vec::new());
        };
        Ok(ids
            .iter()
            .flat_map(|id| {
                id_map
                    .iter()
                    .filter(move |(key, _)| {
                        key.id == *id && namespace.map_or(true, |ns| key.namespace.as_deref() == Some(ns))
                    })
                    .filter_map(|(_, row_id)| record_map.get(row_id).cloned())
            })
            .filter(|record| namespace.map_or(true, |ns| record.namespace.as_deref() == Some(ns)))
            .collect())
    }

    /// Delete vector records by external id.
    pub fn delete_records(&self, index_name: &str, ids: &[String]) -> Result<usize> {
        self.delete_records_in_namespace(index_name, ids, None)
    }

    /// Delete vector records by external id, optionally scoped to a namespace.
    pub fn delete_records_in_namespace(
        &self,
        index_name: &str,
        ids: &[String],
        namespace: Option<&str>,
    ) -> Result<usize> {
        let row_ids: Vec<u64> = {
            let external_ids = self.external_ids.read();
            let Some(id_map) = external_ids.get(index_name) else {
                if self.index_exists(index_name) {
                    return Ok(0);
                }
                return Err(Error::query_execution(format!("Index '{}' not found", index_name)));
            };
            id_map
                .iter()
                .filter_map(|(key, row_id)| {
                    if ids.iter().any(|id| id == &key.id)
                        && namespace.map_or(true, |ns| key.namespace.as_deref() == Some(ns))
                    {
                        Some(*row_id)
                    } else {
                        None
                    }
                })
                .collect()
        };
        let mut deleted = 0;
        for row_id in row_ids {
            self.delete_vector(index_name, row_id)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Filter-aware search. For metadata or namespace filters, exact scan the stored
    /// vector payloads so filters are applied before top-k selection.
    pub fn search_with_filters(
        &self,
        index_name: &str,
        query: &Vector,
        k: usize,
        filter: Option<&HashMap<String, serde_json::Value>>,
        namespace: Option<&str>,
    ) -> Result<Vec<StoredVectorSearchResult>> {
        if filter.is_none() && namespace.is_none() {
            let rows = self.search(index_name, query, k)?;
            let records = self.records.read();
            let record_map = records.get(index_name);
            return Ok(rows
                .into_iter()
                .filter_map(|(row_id, score)| match record_map {
                    Some(records) => records.get(&row_id).map(|record| StoredVectorSearchResult {
                        id: record.id.clone(),
                        row_id,
                        score,
                        vector: Some(record.vector.clone()),
                        metadata: record.metadata.clone(),
                        namespace: record.namespace.clone(),
                    }),
                    None => Some(StoredVectorSearchResult {
                        id: format!("vec_{}", row_id),
                        row_id,
                        score,
                        vector: None,
                        metadata: None,
                        namespace: None,
                    }),
                })
                .collect());
        }

        let meta = self.get_metadata(index_name)?;
        let metric = meta.distance_metric();
        let records = self.records.read();
        let Some(record_map) = records.get(index_name) else {
            return Ok(Vec::new());
        };
        let mut scored: Vec<_> = record_map
            .values()
            .filter(|record| namespace.map_or(true, |ns| record.namespace.as_deref() == Some(ns)))
            .filter(|record| metadata_matches(record.metadata.as_ref(), filter))
            .map(|record| StoredVectorSearchResult {
                id: record.id.clone(),
                row_id: record.row_id,
                score: exact_distance(metric, query, &record.vector),
                vector: Some(record.vector.clone()),
                metadata: record.metadata.clone(),
                namespace: record.namespace.clone(),
            })
            .collect();
        scored.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    /// Get index metadata
    pub fn get_metadata(&self, name: &str) -> Result<VectorIndexMetadata> {
        let metadata = self.metadata.read();
        metadata
            .get(name)
            .cloned()
            .ok_or_else(|| Error::query_execution(format!("Index '{}' not found", name)))
    }

    /// List all indexes for a table and column
    pub fn find_indexes(&self, table_name: &str, column_name: &str) -> Vec<String> {
        let metadata = self.metadata.read();
        metadata
            .values()
            .filter(|meta| meta.table_name == table_name && meta.column_name == column_name)
            .map(|meta| meta.name.clone())
            .collect()
    }

    /// Check if an index exists
    pub fn index_exists(&self, name: &str) -> bool {
        let indexes = self.indexes.read();
        indexes.contains_key(name)
    }

    /// List all index metadata
    pub fn list_all_metadata(&self) -> Vec<VectorIndexMetadata> {
        let metadata = self.metadata.read();
        metadata.values().cloned().collect()
    }

    /// Save index to bytes for persistence
    pub fn save_index(&self, name: &str) -> Result<Vec<u8>> {
        let indexes = self.indexes.read();
        let metadata = self.metadata.read();

        if let (Some(index), Some(meta)) = (indexes.get(name), metadata.get(name)) {
            #[derive(Serialize, Deserialize)]
            struct PersistedIndex {
                metadata: VectorIndexMetadata,
                index_data: Vec<u8>,
            }

            let index_data = match index {
                IndexStorage::Standard(_) => {
                    // For standard indexes, we'd need to implement serialization
                    // For now, return empty
                    Vec::new()
                }
                IndexStorage::Quantized(idx) => idx.to_bytes()?,
                #[cfg(feature = "vector-persist")]
                IndexStorage::Persistent(_) => Vec::new(),
            };

            let persisted = PersistedIndex {
                metadata: meta.clone(),
                index_data,
            };

            bincode::serialize(&persisted)
                .map_err(|e| Error::query_execution(format!("Failed to serialize index: {}", e)))
        } else {
            Err(Error::query_execution(format!("Index '{}' not found", name)))
        }
    }

    /// Load index from bytes
    pub fn load_index(&self, bytes: &[u8]) -> Result<()> {
        #[derive(Serialize, Deserialize)]
        struct PersistedIndex {
            metadata: VectorIndexMetadata,
            index_data: Vec<u8>,
        }

        let persisted: PersistedIndex = bincode::deserialize(bytes)
            .map_err(|e| Error::query_execution(format!("Failed to deserialize index: {}", e)))?;

        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        // Check if index already exists
        if indexes.contains_key(&persisted.metadata.name) {
            return Err(Error::query_execution(format!(
                "Index '{}' already exists",
                persisted.metadata.name
            )));
        }

        let index_storage = match &persisted.metadata.index_type {
            VectorIndexType::Standard(_) => {
                return Err(Error::query_execution("Standard index persistence not yet implemented"));
            }
            VectorIndexType::Quantized(_) => {
                let idx = QuantizedHnswIndex::from_bytes(&persisted.index_data)?;
                IndexStorage::Quantized(idx)
            }
            VectorIndexType::Persistent(_) => {
                return Err(Error::query_execution(
                    "Persistent index loading is handled by RocksDB-backed vector-persist metadata",
                ));
            }
        };

        indexes.insert(persisted.metadata.name.clone(), index_storage);
        metadata.insert(persisted.metadata.name.clone(), persisted.metadata);

        Ok(())
    }

    /// Get statistics for a specific index
    pub fn get_index_stats(&self, name: &str) -> Result<VectorIndexStats> {
        let indexes = self.indexes.read();
        let metadata = self.metadata.read();

        if let (Some(index), Some(meta)) = (indexes.get(name), metadata.get(name)) {
            let (num_vectors, dimensions, quantization, memory_bytes) = match index {
                IndexStorage::Standard(idx) => {
                    let num_vectors = idx.len();
                    let dimensions = match &meta.index_type {
                        VectorIndexType::Standard(config) => config.dimension,
                        _ => 0,
                    } as i32;
                    let memory_bytes = (num_vectors as i64) * (dimensions as i64) * 4 + 1024;
                    (num_vectors as i64, dimensions, "None".to_string(), memory_bytes)
                }
                IndexStorage::Quantized(idx) => {
                    let num_vectors = idx.len();
                    let dimensions = match &meta.index_type {
                        VectorIndexType::Quantized(config) => config.dimension,
                        _ => 0,
                    } as i32;
                    let mem_stats = idx.memory_stats();
                    (
                        num_vectors as i64,
                        dimensions,
                        "Product".to_string(),
                        mem_stats.total_size as i64,
                    )
                }
                #[cfg(feature = "vector-persist")]
                IndexStorage::Persistent(idx) => {
                    let num_vectors = idx.len();
                    let dimensions = match &meta.index_type {
                        VectorIndexType::Persistent(config) => config.dimension,
                        _ => 0,
                    } as i32;
                    let quantization = match &meta.index_type {
                        VectorIndexType::Persistent(config) if config.pq_enabled => "ProductPersistent",
                        _ => "Persistent",
                    };
                    (num_vectors as i64, dimensions, quantization.to_string(), 0)
                }
            };

            // Recall metrics would need benchmark data
            let recall_at_10 = None;

            Ok(VectorIndexStats {
                index_name: name.to_string(),
                num_vectors,
                dimensions,
                quantization,
                memory_bytes,
                recall_at_10,
            })
        } else {
            Err(Error::query_execution(format!("Index '{}' not found", name)))
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredVectorSearchResult {
    pub id: String,
    pub row_id: u64,
    pub score: f32,
    pub vector: Option<Vector>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    pub namespace: Option<String>,
}

impl VectorIndexMetadata {
    pub fn dimension(&self) -> usize {
        match &self.index_type {
            VectorIndexType::Standard(config) => config.dimension,
            VectorIndexType::Quantized(config) => config.dimension,
            VectorIndexType::Persistent(config) => config.dimension,
        }
    }

    pub fn distance_metric(&self) -> DistanceMetric {
        match &self.index_type {
            VectorIndexType::Standard(config) => config.distance_metric,
            VectorIndexType::Quantized(config) => config.distance_metric,
            VectorIndexType::Persistent(config) => config.distance_metric,
        }
    }
}

#[cfg(feature = "vector-persist")]
fn persistent_precision_name(precision: crate::vector::persistent::VectorPrecision) -> &'static str {
    match precision {
        crate::vector::persistent::VectorPrecision::F32 => "f32",
        crate::vector::persistent::VectorPrecision::F16 => "f16",
        crate::vector::persistent::VectorPrecision::I8 => "i8",
    }
}

fn metadata_matches(
    metadata: Option<&HashMap<String, serde_json::Value>>,
    filter: Option<&HashMap<String, serde_json::Value>>,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let Some(metadata) = metadata else {
        return filter.is_empty();
    };
    filter.iter().all(|(key, value)| metadata.get(key) == Some(value))
}

fn exact_distance(metric: DistanceMetric, a: &Vector, b: &Vector) -> f32 {
    if a.len() != b.len() {
        return f32::INFINITY;
    }
    match metric {
        DistanceMetric::L2 => a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = x - y;
                d * d
            })
            .sum::<f32>()
            .sqrt(),
        DistanceMetric::Cosine => {
            let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
            let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            if na == 0.0 || nb == 0.0 {
                1.0
            } else {
                1.0 - dot / (na * nb)
            }
        }
        DistanceMetric::InnerProduct => -a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>(),
    }
}

fn stable_index_id(name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let id = hasher.finish();
    if id == 0 {
        1
    } else {
        id
    }
}

/// Vector index statistics
#[derive(Debug, Clone)]
pub struct VectorIndexStats {
    /// Index name
    pub index_name: String,
    /// Number of vectors in the index
    pub num_vectors: i64,
    /// Vector dimensions
    pub dimensions: i32,
    /// Quantization method (e.g., "None", "PQ", "SQ")
    pub quantization: String,
    /// Memory used by the index in bytes
    pub memory_bytes: i64,
    /// Recall@10 metric (if available)
    pub recall_at_10: Option<f64>,
}

impl Default for VectorIndexManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_create_index() {
        let manager = VectorIndexManager::new();
        let result = manager.create_index(
            "test_idx".to_string(),
            "documents".to_string(),
            "embedding".to_string(),
            384,
            DistanceMetric::L2,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_insert_and_search() {
        let manager = VectorIndexManager::new();
        manager
            .create_index(
                "test_idx".to_string(),
                "documents".to_string(),
                "embedding".to_string(),
                3,
                DistanceMetric::L2,
            )
            .unwrap();

        // Insert vectors
        manager.insert_vector("test_idx", 1, &vec![1.0, 0.0, 0.0]).unwrap();
        manager.insert_vector("test_idx", 2, &vec![0.0, 1.0, 0.0]).unwrap();
        manager.insert_vector("test_idx", 3, &vec![0.0, 0.0, 1.0]).unwrap();

        // Search
        let query = vec![1.0, 0.1, 0.0];
        let results = manager.search("test_idx", &query, 2).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1); // Closest to [1,0,0]
    }

    #[test]
    fn test_find_indexes() {
        let manager = VectorIndexManager::new();
        manager
            .create_index(
                "idx1".to_string(),
                "docs".to_string(),
                "embedding".to_string(),
                128,
                DistanceMetric::L2,
            )
            .unwrap();

        manager
            .create_index(
                "idx2".to_string(),
                "docs".to_string(),
                "embedding".to_string(),
                256,
                DistanceMetric::Cosine,
            )
            .unwrap();

        let indexes = manager.find_indexes("docs", "embedding");
        assert_eq!(indexes.len(), 2);
    }
}
