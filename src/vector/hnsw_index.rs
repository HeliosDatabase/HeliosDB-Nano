//! HNSW index implementation for vector similarity search
//!
//! Uses the hnsw_rs library which implements the HNSW algorithm
//! from "Efficient and robust approximate nearest neighbor search using
//! Hierarchical Navigable Small World graphs" (https://arxiv.org/abs/1603.09320)

#![allow(clippy::similar_names)]
#![allow(unused_variables)]

use crate::{Result, Error};
use super::{Vector, DistanceMetric};
use hnsw_rs::prelude::*;
use parking_lot::RwLock;
use std::sync::Arc;
use serde::{Serialize, Deserialize};

/// HNSW index configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswConfig {
    /// Maximum number of connections per layer (M parameter)
    pub max_connections: usize,
    /// Size of the dynamic candidate list (ef_construction parameter)
    pub ef_construction: usize,
    /// Vector dimension
    pub dimension: usize,
    /// Distance metric
    pub distance_metric: DistanceMetric,
    /// Base ef_search parameter (dynamically adjusted at query time)
    pub ef_search_base: usize,
    /// Enable dynamic ef_search adjustment based on k and index size
    pub dynamic_ef_search: bool,
    /// Minimum ef_search value
    pub ef_search_min: usize,
    /// Maximum ef_search value
    pub ef_search_max: usize,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            max_connections: 16,
            ef_construction: 200,
            dimension: 1536, // Default for OpenAI embeddings
            distance_metric: DistanceMetric::L2,
            // Dynamic ef_search configuration
            ef_search_base: 200,
            dynamic_ef_search: true,
            ef_search_min: 50,
            ef_search_max: 500,
        }
    }
}

/// HNSW index wrapper
pub struct HnswIndex {
    /// The underlying HNSW graph
    index: Arc<RwLock<Hnsw<'static, f32, DistL2>>>,
    /// Index configuration
    config: HnswConfig,
    /// Mapping from internal HNSW id to external row id
    id_mapping: Arc<RwLock<Vec<u64>>>,
    /// Reverse mapping from row id to HNSW id
    reverse_mapping: Arc<RwLock<std::collections::HashMap<u64, usize>>>,
}

impl HnswIndex {
    /// Create a new HNSW index
    pub fn new(config: HnswConfig) -> Result<Self> {
        let max_nb_connection = config.max_connections;
        let ef_construction = config.ef_construction;

        let index = Hnsw::<f32, DistL2>::new(
            max_nb_connection,
            config.dimension,
            ef_construction,
            100, // max_layer
            DistL2,
        );

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            config,
            id_mapping: Arc::new(RwLock::new(Vec::new())),
            reverse_mapping: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Insert a vector into the index
    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<()> {
        // Validate dimension
        if vector.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                vector.len()
            )));
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();

        // Get internal HNSW id
        let hnsw_id = id_mapping.len();
        id_mapping.push(row_id);
        reverse_mapping.insert(row_id, hnsw_id);

        // Insert into HNSW index
        let index = self.index.write();
        let data_id = DataId::from(hnsw_id);
        index.insert((vector.as_slice(), data_id));

        Ok(())
    }

    /// Search for k nearest neighbors
    ///
    /// Performance optimization: Uses dynamic ef_search adjustment based on:
    /// - k (number of neighbors requested): higher k needs larger ef_search
    /// - Index size: larger indices benefit from slightly higher ef_search
    /// - Recall requirements: ef_search = max(k * multiplier, base)
    pub fn search(&self, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        // Validate dimension
        if query.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Query vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query.len()
            )));
        }

        let index = self.index.read();
        let id_mapping = self.id_mapping.read();

        // Calculate dynamic ef_search
        let ef_search = self.calculate_ef_search(k, id_mapping.len());

        // Perform search with dynamic ef_search
        let results = index.search(query.as_slice(), k, ef_search);

        // Convert internal ids to row ids
        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).map(|&row_id| {
                    (row_id, neighbor.distance)
                })
            })
            .collect();

        Ok(mapped_results)
    }

    /// Calculate optimal ef_search based on k and index size
    fn calculate_ef_search(&self, k: usize, index_size: usize) -> usize {
        if !self.config.dynamic_ef_search {
            return self.config.ef_search_base;
        }

        // Base: at minimum, ef_search should be 2x k for good recall
        let k_based = k * 2;

        // Size factor: larger indices may need slightly higher ef_search
        // log2(size) provides diminishing returns scaling
        let size_factor = if index_size > 1000 {
            (index_size as f64).log2() / 10.0 // ~1.0 at 1K, ~1.3 at 10K, ~1.6 at 100K
        } else {
            1.0
        };

        // Calculate ef_search with size adjustment
        let adjusted = ((self.config.ef_search_base as f64 * size_factor) as usize).max(k_based);

        // Clamp to configured bounds
        adjusted.clamp(self.config.ef_search_min, self.config.ef_search_max)
    }

    /// Search with custom ef_search (for fine-tuned queries)
    pub fn search_with_ef(&self, query: &Vector, k: usize, ef_search: usize) -> Result<Vec<(u64, f32)>> {
        // Validate dimension
        if query.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Query vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query.len()
            )));
        }

        let index = self.index.read();
        let id_mapping = self.id_mapping.read();

        // Clamp ef_search to valid range
        let ef_search = ef_search.clamp(k, self.config.ef_search_max);

        let results = index.search(query.as_slice(), k, ef_search);

        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).map(|&row_id| {
                    (row_id, neighbor.distance)
                })
            })
            .collect();

        Ok(mapped_results)
    }

    /// Delete a vector from the index
    pub fn delete(&self, row_id: u64) -> Result<()> {
        let mut reverse_mapping = self.reverse_mapping.write();

        if let Some(&hnsw_id) = reverse_mapping.get(&row_id) {
            // Remove from reverse mapping
            reverse_mapping.remove(&row_id);

            // Note: hnsw_rs doesn't support true deletion, so we just mark it as deleted
            // In a production system, you'd need to rebuild the index periodically
            // or use a deleted tombstone list

            Ok(())
        } else {
            Err(Error::query_execution(format!(
                "Vector with row_id {} not found in index",
                row_id
            )))
        }
    }

    /// Get the number of vectors in the index
    pub fn len(&self) -> usize {
        self.id_mapping.read().len()
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.id_mapping.read().is_empty()
    }

    /// Get the dimension of vectors in this index
    pub fn dimension(&self) -> usize {
        self.config.dimension
    }
}

/// Multi-metric HNSW index that supports different distance metrics
pub enum MultiMetricHnswIndex {
    L2(HnswIndex),
    Cosine(CosineHnswIndex),
    InnerProduct(InnerProductHnswIndex),
}

impl MultiMetricHnswIndex {
    /// Create a new multi-metric HNSW index
    pub fn new(config: HnswConfig) -> Result<Self> {
        match config.distance_metric {
            DistanceMetric::L2 => Ok(Self::L2(HnswIndex::new(config)?)),
            DistanceMetric::Cosine => Ok(Self::Cosine(CosineHnswIndex::new(config)?)),
            DistanceMetric::InnerProduct => Ok(Self::InnerProduct(InnerProductHnswIndex::new(config)?)),
        }
    }

    /// Insert a vector
    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<()> {
        match self {
            Self::L2(index) => index.insert(row_id, vector),
            Self::Cosine(index) => index.insert(row_id, vector),
            Self::InnerProduct(index) => index.insert(row_id, vector),
        }
    }

    /// Search for k nearest neighbors
    pub fn search(&self, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        match self {
            Self::L2(index) => index.search(query, k),
            Self::Cosine(index) => index.search(query, k),
            Self::InnerProduct(index) => index.search(query, k),
        }
    }

    /// Delete a vector
    pub fn delete(&self, row_id: u64) -> Result<()> {
        match self {
            Self::L2(index) => index.delete(row_id),
            Self::Cosine(index) => index.delete(row_id),
            Self::InnerProduct(index) => index.delete(row_id),
        }
    }

    /// Get dimension
    pub fn dimension(&self) -> usize {
        match self {
            Self::L2(index) => index.dimension(),
            Self::Cosine(index) => index.dimension(),
            Self::InnerProduct(index) => index.dimension(),
        }
    }

    /// Get number of vectors in the index
    pub fn len(&self) -> usize {
        match self {
            Self::L2(index) => index.len(),
            Self::Cosine(index) => index.len(),
            Self::InnerProduct(index) => index.len(),
        }
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// HNSW index for cosine distance
pub struct CosineHnswIndex {
    index: Arc<RwLock<Hnsw<'static, f32, DistCosine>>>,
    config: HnswConfig,
    id_mapping: Arc<RwLock<Vec<u64>>>,
    reverse_mapping: Arc<RwLock<std::collections::HashMap<u64, usize>>>,
}

impl CosineHnswIndex {
    pub fn new(config: HnswConfig) -> Result<Self> {
        let index = Hnsw::<f32, DistCosine>::new(
            config.max_connections,
            config.dimension,
            config.ef_construction,
            100,
            DistCosine,
        );

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            config,
            id_mapping: Arc::new(RwLock::new(Vec::new())),
            reverse_mapping: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<()> {
        if vector.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                vector.len()
            )));
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();

        let hnsw_id = id_mapping.len();
        id_mapping.push(row_id);
        reverse_mapping.insert(row_id, hnsw_id);

        let index = self.index.write();
        let data_id = DataId::from(hnsw_id);
        index.insert((vector.as_slice(), data_id));

        Ok(())
    }

    pub fn search(&self, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        if query.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Query vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query.len()
            )));
        }

        let index = self.index.read();
        let id_mapping = self.id_mapping.read();

        let results = index.search(query.as_slice(), k, 200);
        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).map(|&row_id| {
                    (row_id, neighbor.distance)
                })
            })
            .collect();

        Ok(mapped_results)
    }

    pub fn delete(&self, row_id: u64) -> Result<()> {
        let mut reverse_mapping = self.reverse_mapping.write();
        if reverse_mapping.remove(&row_id).is_some() {
            Ok(())
        } else {
            Err(Error::query_execution(format!(
                "Vector with row_id {} not found in index",
                row_id
            )))
        }
    }

    pub fn dimension(&self) -> usize {
        self.config.dimension
    }

    pub fn len(&self) -> usize {
        self.id_mapping.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_mapping.read().is_empty()
    }
}

/// HNSW index for inner product (dot product)
pub struct InnerProductHnswIndex {
    index: Arc<RwLock<Hnsw<'static, f32, DistDot>>>,
    config: HnswConfig,
    id_mapping: Arc<RwLock<Vec<u64>>>,
    reverse_mapping: Arc<RwLock<std::collections::HashMap<u64, usize>>>,
}

impl InnerProductHnswIndex {
    pub fn new(config: HnswConfig) -> Result<Self> {
        let index = Hnsw::<f32, DistDot>::new(
            config.max_connections,
            config.dimension,
            config.ef_construction,
            100,
            DistDot,
        );

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            config,
            id_mapping: Arc::new(RwLock::new(Vec::new())),
            reverse_mapping: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<()> {
        if vector.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                vector.len()
            )));
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();

        let hnsw_id = id_mapping.len();
        id_mapping.push(row_id);
        reverse_mapping.insert(row_id, hnsw_id);

        let index = self.index.write();
        let data_id = DataId::from(hnsw_id);
        index.insert((vector.as_slice(), data_id));

        Ok(())
    }

    pub fn search(&self, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        if query.len() != self.config.dimension {
            return Err(Error::query_execution(format!(
                "Query vector dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query.len()
            )));
        }

        let index = self.index.read();
        let id_mapping = self.id_mapping.read();

        let results = index.search(query.as_slice(), k, 200);
        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).map(|&row_id| {
                    (row_id, neighbor.distance)
                })
            })
            .collect();

        Ok(mapped_results)
    }

    pub fn delete(&self, row_id: u64) -> Result<()> {
        let mut reverse_mapping = self.reverse_mapping.write();
        if reverse_mapping.remove(&row_id).is_some() {
            Ok(())
        } else {
            Err(Error::query_execution(format!(
                "Vector with row_id {} not found in index",
                row_id
            )))
        }
    }

    pub fn dimension(&self) -> usize {
        self.config.dimension
    }

    pub fn len(&self) -> usize {
        self.id_mapping.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_mapping.read().is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_hnsw_basic() {
        let config = HnswConfig {
            dimension: 3,
            max_connections: 16,
            ef_construction: 200,
            distance_metric: DistanceMetric::L2,
            ef_search_base: 200,
            dynamic_ef_search: true,
            ef_search_min: 50,
            ef_search_max: 500,
        };

        let index = HnswIndex::new(config).unwrap();

        // Insert vectors
        index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();
        index.insert(2, &vec![0.0, 1.0, 0.0]).unwrap();
        index.insert(3, &vec![0.0, 0.0, 1.0]).unwrap();

        // Search
        let query = vec![1.0, 0.1, 0.0];
        let results = index.search(&query, 2).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1); // Closest to [1,0,0]
    }

    #[test]
    fn test_dimension_validation() {
        let config = HnswConfig {
            dimension: 3,
            ..Default::default()
        };

        let index = HnswIndex::new(config).unwrap();

        // Wrong dimension should fail
        let result = index.insert(1, &vec![1.0, 0.0]);
        assert!(result.is_err());
    }

    #[test]
    fn test_multi_metric_index() {
        let config = HnswConfig {
            dimension: 2,
            distance_metric: DistanceMetric::Cosine,
            ..Default::default()
        };

        let index = MultiMetricHnswIndex::new(config).unwrap();

        index.insert(1, &vec![1.0, 0.0]).unwrap();
        index.insert(2, &vec![0.0, 1.0]).unwrap();

        let results = index.search(&vec![0.7, 0.7], 1).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_vector_count_tracking() {
        // Test all three metric types
        let test_configs = vec![
            (DistanceMetric::L2, "L2"),
            (DistanceMetric::Cosine, "Cosine"),
            (DistanceMetric::InnerProduct, "InnerProduct"),
        ];

        for (metric, name) in test_configs {
            let config = HnswConfig {
                dimension: 3,
                distance_metric: metric,
                ..Default::default()
            };

            let index = MultiMetricHnswIndex::new(config).unwrap();

            // Initially empty
            assert_eq!(index.len(), 0, "{} index should start empty", name);
            assert!(index.is_empty(), "{} index should be empty", name);

            // Insert vectors
            index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();
            assert_eq!(index.len(), 1, "{} index should have 1 vector", name);
            assert!(!index.is_empty(), "{} index should not be empty", name);

            index.insert(2, &vec![0.0, 1.0, 0.0]).unwrap();
            assert_eq!(index.len(), 2, "{} index should have 2 vectors", name);

            index.insert(3, &vec![0.0, 0.0, 1.0]).unwrap();
            assert_eq!(index.len(), 3, "{} index should have 3 vectors", name);

            // Delete a vector
            index.delete(2).unwrap();
            assert_eq!(index.len(), 3, "{} index length should remain 3 (tombstone)", name);
        }
    }

    #[test]
    fn test_index_len_methods() {
        // Test that individual index types track length correctly
        let config = HnswConfig {
            dimension: 2,
            max_connections: 16,
            ef_construction: 200,
            distance_metric: DistanceMetric::L2,
            ef_search_base: 200,
            dynamic_ef_search: true,
            ef_search_min: 50,
            ef_search_max: 500,
        };

        let index = HnswIndex::new(config).unwrap();
        assert_eq!(index.len(), 0);
        assert!(index.is_empty());

        index.insert(1, &vec![1.0, 0.0]).unwrap();
        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());

        index.insert(2, &vec![0.0, 1.0]).unwrap();
        assert_eq!(index.len(), 2);
    }
}
