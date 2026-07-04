//! HNSW index implementation for vector similarity search
//!
//! Uses the hnsw_rs library which implements the HNSW algorithm
//! from "Efficient and robust approximate nearest neighbor search using
//! Hierarchical Navigable Small World graphs" (<https://arxiv.org/abs/1603.09320>)

#![allow(clippy::similar_names)]
#![allow(unused_variables)]

use super::{DistanceMetric, Vector};
use crate::{Error, Result};
use hnsw_rs::prelude::*;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

    /// Bulk-insert a batch of `(row_id, vector)` pairs, building the HNSW
    /// graph in parallel across all available cores.
    ///
    /// This is the fast path for `CREATE INDEX ... USING hnsw` backfill over
    /// a populated table. The naive `insert`-per-row loop is single-threaded
    /// and dominated by the O(log N) graph-link work per vector. Here we:
    ///   1. Take the id-mapping locks once, pre-assign every contiguous
    ///      `hnsw_id`, and update both mappings. This is O(N) but cheap (a
    ///      vec push + hashmap insert per row) and runs under a single short
    ///      critical section instead of N lock acquisitions.
    ///   2. Hand the whole batch to `hnsw_rs::parallel_insert`, which uses
    ///      Rayon to build the graph concurrently. The underlying `Hnsw`
    ///      synchronises its own internal structures, so concurrent inserts
    ///      are correct.
    ///
    /// Result ordering and search results are identical to inserting the same
    /// rows sequentially (data_ids are assigned in input order); only the
    /// wall-clock build time changes.
    pub fn insert_batch(&self, batch: &[(u64, Vector)]) -> Result<()> {
        for (_, vector) in batch {
            if vector.len() != self.config.dimension {
                return Err(Error::query_execution(format!(
                    "Vector dimension mismatch: expected {}, got {}",
                    self.config.dimension,
                    vector.len()
                )));
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();
        let base = id_mapping.len();
        let mut datas: Vec<(&Vec<f32>, usize)> = Vec::with_capacity(batch.len());
        for (offset, (row_id, vector)) in batch.iter().enumerate() {
            let hnsw_id = base + offset;
            id_mapping.push(*row_id);
            reverse_mapping.insert(*row_id, hnsw_id);
            datas.push((vector, hnsw_id));
        }

        let index = self.index.write();
        index.parallel_insert(&datas);

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
        let reverse_mapping = self.reverse_mapping.read();

        // Calculate dynamic ef_search
        let ef_search = self.calculate_ef_search(k, id_mapping.len());

        // Perform search with dynamic ef_search
        let results = index.search(query.as_slice(), k, ef_search);

        // Convert internal ids to row ids
        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).and_then(|&row_id| {
                    (reverse_mapping.get(&row_id) == Some(&hnsw_id)).then_some((row_id, neighbor.distance))
                })
            })
            .collect();

        // Recall guard: hnsw_rs 0.3.3 can leave an early-inserted, high-level
        // point with an empty layer-0 neighbour list (the rank-1 point gets no
        // forward links, and back-links land at the later point's own level).
        // A search that descends onto such a pivot under-returns even though
        // the vectors exist. When the graph returns fewer than the achievable
        // k on a small index, recover exactness by brute force. Capped so a
        // tombstone-heavy large index degrades as before instead of paying an
        // O(N) scan per query.
        let live = reverse_mapping.len();
        if mapped_results.len() < k.min(live) && live <= BRUTE_FORCE_RESCUE_MAX {
            return Ok(brute_force_rescue(&index, query, k, &id_mapping, &reverse_mapping));
        }

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
        let reverse_mapping = self.reverse_mapping.read();

        // Clamp ef_search to valid range
        let ef_search = ef_search.clamp(k, self.config.ef_search_max);

        let results = index.search(query.as_slice(), k, ef_search);

        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).and_then(|&row_id| {
                    (reverse_mapping.get(&row_id) == Some(&hnsw_id)).then_some((row_id, neighbor.distance))
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
        // Physical entry count (includes tombstones): hnsw_rs cannot truly
        // delete, so `delete` only drops the live `reverse_mapping` entry and the
        // vector stays in the graph. `id_mapping` keeps one slot per insert and is
        // never shrunk by delete, so it reflects the tombstone-inclusive size.
        self.id_mapping.read().len()
    }

    /// Live (tombstone-excluded) entry count: one entry per row currently
    /// visible to `search`. `delete` removes the `reverse_mapping` entry, so
    /// its size is the number of vectors a search can still return.
    pub fn live_len(&self) -> usize {
        self.reverse_mapping.read().len()
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.id_mapping.read().is_empty()
    }

    /// Get the dimension of vectors in this index
    pub fn dimension(&self) -> usize {
        self.config.dimension
    }

    /// R4.2: dump the graph via `hnsw_rs` (`{basename}.hnsw.graph` +
    /// `{basename}.hnsw.data` inside `dir`).
    pub fn dump_graph(&self, dir: &std::path::Path, basename: &str) -> Result<()> {
        dump_graph_impl(&self.index.read(), dir, basename)
    }

    /// R4.2: snapshot the row-id mappings for the persistence sidecar.
    pub fn export_mappings(&self) -> (Vec<u64>, Vec<(u64, u64)>) {
        export_mappings_impl(&self.id_mapping.read(), &self.reverse_mapping.read())
    }

    /// R4.2: reconstruct the index from a graph dump + sidecar mappings.
    pub fn reload_from_dump(
        config: HnswConfig,
        dir: &std::path::Path,
        basename: &str,
        id_mapping: Vec<u64>,
        reverse_pairs: &[(u64, u64)],
    ) -> Result<Self> {
        let graph: Hnsw<'static, f32, DistL2> = reload_graph(dir, basename)?;
        Ok(Self {
            index: Arc::new(RwLock::new(graph)),
            config,
            id_mapping: Arc::new(RwLock::new(id_mapping)),
            reverse_mapping: Arc::new(RwLock::new(reverse_pairs_to_map(reverse_pairs))),
        })
    }
}

// ---- R4.2 graph persistence helpers (shared by the three metric wrappers) ----

fn dump_graph_impl<D>(index: &Hnsw<'static, f32, D>, dir: &std::path::Path, basename: &str) -> Result<()>
where
    D: Distance<f32> + Send + Sync,
{
    use hnsw_rs::api::AnnT;
    let dumped = index
        .file_dump(dir, basename)
        .map_err(|e| Error::storage(format!("HNSW graph dump failed: {e}")))?;
    if dumped != basename {
        // file_dump uniquifies the basename instead of overwriting when the
        // datamap option is active; we always build in-memory graphs, so this
        // signals an unexpected configuration rather than a partial dump.
        return Err(Error::storage(format!(
            "HNSW graph dump wrote basename '{dumped}' instead of '{basename}'"
        )));
    }
    Ok(())
}

fn export_mappings_impl(
    id_mapping: &[u64],
    reverse_mapping: &std::collections::HashMap<u64, usize>,
) -> (Vec<u64>, Vec<(u64, u64)>) {
    let reverse = reverse_mapping.iter().map(|(row, hnsw)| (*row, *hnsw as u64)).collect();
    (id_mapping.to_vec(), reverse)
}

fn reverse_pairs_to_map(pairs: &[(u64, u64)]) -> std::collections::HashMap<u64, usize> {
    pairs.iter().map(|(row, hnsw)| (*row, *hnsw as usize)).collect()
}

/// Upper bound on live entries for the under-return brute-force rescue in the
/// search paths. The layer-0-isolation pathology (see `brute_force_rescue`) is
/// a small-index phenomenon; beyond this size an under-full result is almost
/// always tombstone filtering, which brute force would turn into a per-query
/// O(N) scan.
const BRUTE_FORCE_RESCUE_MAX: usize = 10_000;

/// Exact k-NN over every live point — the recall rescue for hnsw_rs 0.3.3's
/// layer-0 isolation defect: an early-inserted point that drew a high level
/// can end up with an empty layer-0 neighbour list (the rank-1 point gets no
/// forward links; back-links land at the later point's own level), making it
/// unreachable from a search that descends onto it, which then under-returns
/// even though the vectors exist. Uses the same distance functor as the graph
/// search so scores are directly comparable. Caller must already hold the
/// index/mapping read locks (guards are passed in, not re-taken — parking_lot
/// locks are not reentrant under a queued writer).
fn brute_force_rescue<D>(
    index: &Hnsw<'static, f32, D>,
    query: &Vector,
    k: usize,
    id_mapping: &[u64],
    reverse_mapping: &std::collections::HashMap<u64, usize>,
) -> Vec<(u64, f32)>
where
    D: Distance<f32> + Default + Send + Sync,
{
    let dist = D::default();
    let mut all: Vec<(u64, f32)> = index
        .get_point_indexation()
        .into_iter()
        .filter_map(|point| {
            let hnsw_id = point.get_origin_id();
            let row_id = *id_mapping.get(hnsw_id)?;
            // Same tombstone rule as the graph-result filter.
            (reverse_mapping.get(&row_id) == Some(&hnsw_id))
                .then(|| (row_id, dist.eval(query.as_slice(), point.get_v())))
        })
        .collect();
    all.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    all.truncate(k);
    all
}

/// Reload a dumped graph. The `HnswIo` loader owns buffers the reloaded
/// graph borrows from, so it is intentionally leaked to give the graph the
/// `'static` lifetime the wrappers require — one small leak per index per
/// process open (the reload path runs only at startup).
fn reload_graph<D>(dir: &std::path::Path, basename: &str) -> Result<Hnsw<'static, f32, D>>
where
    D: Distance<f32> + Default + Send + Sync,
{
    let io: &'static mut hnsw_rs::hnswio::HnswIo = Box::leak(Box::new(hnsw_rs::hnswio::HnswIo::new(dir, basename)));
    io.load_hnsw::<f32, D>()
        .map_err(|e| Error::storage(format!("HNSW graph reload failed: {e}")))
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

    /// Bulk-insert a batch of `(row_id, vector)` pairs, building the graph in
    /// parallel. Used by `CREATE INDEX` backfill; see [`HnswIndex::insert_batch`].
    pub fn insert_batch(&self, batch: &[(u64, Vector)]) -> Result<()> {
        match self {
            Self::L2(index) => index.insert_batch(batch),
            Self::Cosine(index) => index.insert_batch(batch),
            Self::InnerProduct(index) => index.insert_batch(batch),
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

    /// Live (tombstone-excluded) entry count — see [`HnswIndex::live_len`].
    pub fn live_len(&self) -> usize {
        match self {
            Self::L2(index) => index.live_len(),
            Self::Cosine(index) => index.live_len(),
            Self::InnerProduct(index) => index.live_len(),
        }
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// R4.2: dump the graph to `{dir}/{basename}.hnsw.{graph,data}`.
    pub fn dump_graph(&self, dir: &std::path::Path, basename: &str) -> Result<()> {
        match self {
            Self::L2(index) => index.dump_graph(dir, basename),
            Self::Cosine(index) => index.dump_graph(dir, basename),
            Self::InnerProduct(index) => index.dump_graph(dir, basename),
        }
    }

    /// R4.2: snapshot the row-id mappings for the persistence sidecar.
    pub fn export_mappings(&self) -> (Vec<u64>, Vec<(u64, u64)>) {
        match self {
            Self::L2(index) => index.export_mappings(),
            Self::Cosine(index) => index.export_mappings(),
            Self::InnerProduct(index) => index.export_mappings(),
        }
    }

    /// R4.2: reconstruct an index from a graph dump + sidecar mappings.
    /// The metric is taken from `config.distance_metric` and must match the
    /// metric the graph was built with (the caller persists it in the sidecar).
    pub fn reload_from_dump(
        config: HnswConfig,
        dir: &std::path::Path,
        basename: &str,
        id_mapping: Vec<u64>,
        reverse_pairs: &[(u64, u64)],
    ) -> Result<Self> {
        match config.distance_metric {
            DistanceMetric::L2 => Ok(Self::L2(HnswIndex::reload_from_dump(
                config,
                dir,
                basename,
                id_mapping,
                reverse_pairs,
            )?)),
            DistanceMetric::Cosine => Ok(Self::Cosine(CosineHnswIndex::reload_from_dump(
                config,
                dir,
                basename,
                id_mapping,
                reverse_pairs,
            )?)),
            DistanceMetric::InnerProduct => Ok(Self::InnerProduct(InnerProductHnswIndex::reload_from_dump(
                config,
                dir,
                basename,
                id_mapping,
                reverse_pairs,
            )?)),
        }
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

    /// Bulk-insert a batch, building the cosine HNSW graph in parallel.
    /// See [`HnswIndex::insert_batch`] for the rationale and correctness notes.
    pub fn insert_batch(&self, batch: &[(u64, Vector)]) -> Result<()> {
        for (_, vector) in batch {
            if vector.len() != self.config.dimension {
                return Err(Error::query_execution(format!(
                    "Vector dimension mismatch: expected {}, got {}",
                    self.config.dimension,
                    vector.len()
                )));
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();
        let base = id_mapping.len();
        let mut datas: Vec<(&Vec<f32>, usize)> = Vec::with_capacity(batch.len());
        for (offset, (row_id, vector)) in batch.iter().enumerate() {
            let hnsw_id = base + offset;
            id_mapping.push(*row_id);
            reverse_mapping.insert(*row_id, hnsw_id);
            datas.push((vector, hnsw_id));
        }

        let index = self.index.write();
        index.parallel_insert(&datas);

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
        let reverse_mapping = self.reverse_mapping.read();

        let results = index.search(query.as_slice(), k, 200);
        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).and_then(|&row_id| {
                    (reverse_mapping.get(&row_id) == Some(&hnsw_id)).then_some((row_id, neighbor.distance))
                })
            })
            .collect();

        // Recall guard — see `brute_force_rescue`.
        let live = reverse_mapping.len();
        if mapped_results.len() < k.min(live) && live <= BRUTE_FORCE_RESCUE_MAX {
            return Ok(brute_force_rescue(&index, query, k, &id_mapping, &reverse_mapping));
        }

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
        // Physical entry count (includes tombstones): hnsw_rs cannot truly
        // delete, so `delete` only drops the live `reverse_mapping` entry and the
        // vector stays in the graph. `id_mapping` keeps one slot per insert and is
        // never shrunk by delete, so it reflects the tombstone-inclusive size.
        self.id_mapping.read().len()
    }

    /// Live (tombstone-excluded) entry count — see [`HnswIndex::live_len`].
    pub fn live_len(&self) -> usize {
        self.reverse_mapping.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_mapping.read().is_empty()
    }

    /// R4.2: dump the graph — see [`HnswIndex::dump_graph`].
    pub fn dump_graph(&self, dir: &std::path::Path, basename: &str) -> Result<()> {
        dump_graph_impl(&self.index.read(), dir, basename)
    }

    /// R4.2: snapshot the row-id mappings — see [`HnswIndex::export_mappings`].
    pub fn export_mappings(&self) -> (Vec<u64>, Vec<(u64, u64)>) {
        export_mappings_impl(&self.id_mapping.read(), &self.reverse_mapping.read())
    }

    /// R4.2: reconstruct from a graph dump — see [`HnswIndex::reload_from_dump`].
    pub fn reload_from_dump(
        config: HnswConfig,
        dir: &std::path::Path,
        basename: &str,
        id_mapping: Vec<u64>,
        reverse_pairs: &[(u64, u64)],
    ) -> Result<Self> {
        let graph: Hnsw<'static, f32, DistCosine> = reload_graph(dir, basename)?;
        Ok(Self {
            index: Arc::new(RwLock::new(graph)),
            config,
            id_mapping: Arc::new(RwLock::new(id_mapping)),
            reverse_mapping: Arc::new(RwLock::new(reverse_pairs_to_map(reverse_pairs))),
        })
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

    /// Bulk-insert a batch, building the inner-product HNSW graph in parallel.
    /// See [`HnswIndex::insert_batch`] for the rationale and correctness notes.
    pub fn insert_batch(&self, batch: &[(u64, Vector)]) -> Result<()> {
        for (_, vector) in batch {
            if vector.len() != self.config.dimension {
                return Err(Error::query_execution(format!(
                    "Vector dimension mismatch: expected {}, got {}",
                    self.config.dimension,
                    vector.len()
                )));
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();
        let base = id_mapping.len();
        let mut datas: Vec<(&Vec<f32>, usize)> = Vec::with_capacity(batch.len());
        for (offset, (row_id, vector)) in batch.iter().enumerate() {
            let hnsw_id = base + offset;
            id_mapping.push(*row_id);
            reverse_mapping.insert(*row_id, hnsw_id);
            datas.push((vector, hnsw_id));
        }

        let index = self.index.write();
        index.parallel_insert(&datas);

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
        let reverse_mapping = self.reverse_mapping.read();

        let results = index.search(query.as_slice(), k, 200);
        let mapped_results: Vec<(u64, f32)> = results
            .into_iter()
            .filter_map(|neighbor| {
                let hnsw_id = neighbor.d_id as usize;
                id_mapping.get(hnsw_id).and_then(|&row_id| {
                    (reverse_mapping.get(&row_id) == Some(&hnsw_id)).then_some((row_id, neighbor.distance))
                })
            })
            .collect();

        // Recall guard — see `brute_force_rescue`.
        let live = reverse_mapping.len();
        if mapped_results.len() < k.min(live) && live <= BRUTE_FORCE_RESCUE_MAX {
            return Ok(brute_force_rescue(&index, query, k, &id_mapping, &reverse_mapping));
        }

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
        // Physical entry count (includes tombstones): hnsw_rs cannot truly
        // delete, so `delete` only drops the live `reverse_mapping` entry and the
        // vector stays in the graph. `id_mapping` keeps one slot per insert and is
        // never shrunk by delete, so it reflects the tombstone-inclusive size.
        self.id_mapping.read().len()
    }

    /// Live (tombstone-excluded) entry count — see [`HnswIndex::live_len`].
    pub fn live_len(&self) -> usize {
        self.reverse_mapping.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_mapping.read().is_empty()
    }

    /// R4.2: dump the graph — see [`HnswIndex::dump_graph`].
    pub fn dump_graph(&self, dir: &std::path::Path, basename: &str) -> Result<()> {
        dump_graph_impl(&self.index.read(), dir, basename)
    }

    /// R4.2: snapshot the row-id mappings — see [`HnswIndex::export_mappings`].
    pub fn export_mappings(&self) -> (Vec<u64>, Vec<(u64, u64)>) {
        export_mappings_impl(&self.id_mapping.read(), &self.reverse_mapping.read())
    }

    /// R4.2: reconstruct from a graph dump — see [`HnswIndex::reload_from_dump`].
    pub fn reload_from_dump(
        config: HnswConfig,
        dir: &std::path::Path,
        basename: &str,
        id_mapping: Vec<u64>,
        reverse_pairs: &[(u64, u64)],
    ) -> Result<Self> {
        let graph: Hnsw<'static, f32, DistDot> = reload_graph(dir, basename)?;
        Ok(Self {
            index: Arc::new(RwLock::new(graph)),
            config,
            id_mapping: Arc::new(RwLock::new(id_mapping)),
            reverse_mapping: Arc::new(RwLock::new(reverse_pairs_to_map(reverse_pairs))),
        })
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

        // Insert vectors. The query's nearest neighbor (id 1) goes LAST:
        // hnsw_rs 0.3.3 gives the first-inserted point no forward links, and
        // when its (OS-entropy-seeded) level draw puts it above the later
        // points it can end up layer-0-isolated — search then returns 1 of 2
        // results (~1/800 runs). A non-first insert always gets layer-0 links.
        index.insert(2, &vec![0.0, 1.0, 0.0]).unwrap();
        index.insert(3, &vec![0.0, 0.0, 1.0]).unwrap();
        index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();

        // Search
        let query = vec![1.0, 0.1, 0.0];
        let results = index.search(&query, 2).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1); // Closest to [1,0,0]
    }

    #[test]
    fn test_first_insert_isolation_rescued() {
        // Historical flake shape: nearest vector inserted FIRST. In ~1/800
        // seeds hnsw_rs leaves it layer-0-isolated and the graph search
        // under-returns; the brute-force rescue must make k=2 exact every
        // time. Loop enough that a regression reappears at a visible rate.
        for _ in 0..800 {
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
            index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();
            index.insert(2, &vec![0.0, 1.0, 0.0]).unwrap();
            index.insert(3, &vec![0.0, 0.0, 1.0]).unwrap();

            let results = index.search(&vec![1.0, 0.1, 0.0], 2).unwrap();
            assert_eq!(results.len(), 2, "under-returned despite rescue");
            assert_eq!(results[0].0, 1);
        }
    }

    #[test]
    fn test_rescue_respects_tombstones() {
        let config = HnswConfig {
            dimension: 3,
            ..Default::default()
        };
        let index = HnswIndex::new(config).unwrap();
        index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();
        index.insert(2, &vec![0.0, 1.0, 0.0]).unwrap();
        index.delete(1).unwrap();

        // k=2 but only 1 live vector: must return exactly the live one —
        // the rescue must not resurrect the tombstoned row.
        let results = index.search(&vec![1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 2);
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
