//! Vector index management for storage engine
//!
//! Manages HNSW indexes for vector columns, including creation, maintenance,
//! and query execution.

#![allow(unused_variables)]

use crate::{Result, Error};
use crate::vector::{
    MultiMetricHnswIndex, HnswConfig, DistanceMetric, Vector,
    QuantizedHnswIndex, QuantizedHnswConfig, ProductQuantizerConfig,
};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use bincode;

/// Vector index type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VectorIndexType {
    /// Standard HNSW index (no quantization)
    Standard(HnswConfig),
    /// Quantized HNSW index with Product Quantization
    Quantized(QuantizedHnswConfig),
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
}

/// Vector index manager
pub struct VectorIndexManager {
    /// Map from index name to index storage
    indexes: Arc<RwLock<HashMap<String, IndexStorage>>>,
    /// Map from index name to metadata
    metadata: Arc<RwLock<HashMap<String, VectorIndexMetadata>>>,
}

impl VectorIndexManager {
    /// Create a new vector index manager
    pub fn new() -> Self {
        Self {
            indexes: Arc::new(RwLock::new(HashMap::new())),
            metadata: Arc::new(RwLock::new(HashMap::new())),
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
            return Err(Error::query_execution(format!(
                "Index '{}' already exists",
                name
            )));
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

        Ok(())
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
            return Err(Error::query_execution(format!(
                "Index '{}' already exists",
                name
            )));
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

        Ok(())
    }

    /// Get an index by name
    pub fn get_index(&self, name: &str) -> Result<Arc<MultiMetricHnswIndex>> {
        let indexes = self.indexes.read();
        indexes.get(name)
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
            }
            Ok(())
        } else {
            Err(Error::query_execution(format!(
                "Index '{}' not found",
                index_name
            )))
        }
    }

    /// Search for nearest neighbors
    pub fn search(&self, index_name: &str, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        let indexes = self.indexes.read();
        if let Some(index) = indexes.get(index_name) {
            match index {
                IndexStorage::Standard(idx) => idx.search(query, k),
                IndexStorage::Quantized(idx) => idx.search(query, k),
            }
        } else {
            Err(Error::query_execution(format!(
                "Index '{}' not found",
                index_name
            )))
        }
    }

    /// Delete a vector from an index
    pub fn delete_vector(&self, index_name: &str, row_id: u64) -> Result<()> {
        let indexes = self.indexes.read();
        if let Some(index) = indexes.get(index_name) {
            match index {
                IndexStorage::Standard(idx) => idx.delete(row_id)?,
                IndexStorage::Quantized(idx) => idx.delete(row_id)?,
            }
            Ok(())
        } else {
            Err(Error::query_execution(format!(
                "Index '{}' not found",
                index_name
            )))
        }
    }

    /// Drop an index
    pub fn drop_index(&self, name: &str) -> Result<()> {
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        if indexes.remove(name).is_none() {
            return Err(Error::query_execution(format!(
                "Index '{}' does not exist",
                name
            )));
        }

        metadata.remove(name);
        Ok(())
    }

    /// Get index metadata
    pub fn get_metadata(&self, name: &str) -> Result<VectorIndexMetadata> {
        let metadata = self.metadata.read();
        metadata.get(name)
            .cloned()
            .ok_or_else(|| Error::query_execution(format!("Index '{}' not found", name)))
    }

    /// List all indexes for a table and column
    pub fn find_indexes(&self, table_name: &str, column_name: &str) -> Vec<String> {
        let metadata = self.metadata.read();
        metadata.values()
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
                IndexStorage::Quantized(idx) => {
                    idx.to_bytes()?
                }
            };

            let persisted = PersistedIndex {
                metadata: meta.clone(),
                index_data,
            };

            bincode::serialize(&persisted)
                .map_err(|e| Error::query_execution(format!("Failed to serialize index: {}", e)))
        } else {
            Err(Error::query_execution(format!(
                "Index '{}' not found",
                name
            )))
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
                return Err(Error::query_execution(
                    "Standard index persistence not yet implemented"
                ));
            }
            VectorIndexType::Quantized(_) => {
                let idx = QuantizedHnswIndex::from_bytes(&persisted.index_data)?;
                IndexStorage::Quantized(idx)
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
                    (num_vectors as i64, dimensions, "Product".to_string(), mem_stats.total_size as i64)
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
            Err(Error::query_execution(format!(
                "Index '{}' not found",
                name
            )))
        }
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
        manager.create_index(
            "test_idx".to_string(),
            "documents".to_string(),
            "embedding".to_string(),
            3,
            DistanceMetric::L2,
        ).unwrap();

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
        manager.create_index(
            "idx1".to_string(),
            "docs".to_string(),
            "embedding".to_string(),
            128,
            DistanceMetric::L2,
        ).unwrap();

        manager.create_index(
            "idx2".to_string(),
            "docs".to_string(),
            "embedding".to_string(),
            256,
            DistanceMetric::Cosine,
        ).unwrap();

        let indexes = manager.find_indexes("docs", "embedding");
        assert_eq!(indexes.len(), 2);
    }
}
