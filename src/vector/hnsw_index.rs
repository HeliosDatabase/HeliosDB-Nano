//! HNSW index implementation for vector similarity search
//!
//! Uses the hnsw_rs library which implements the HNSW algorithm
//! from "Efficient and robust approximate nearest neighbor search using
//! Hierarchical Navigable Small World graphs" (<https://arxiv.org/abs/1603.09320>)
//!
//! # What this wrapper does around hnsw_rs 0.3.3 (Sprinter 746342a0bfbb, 66aeb488f312)
//!
//! * **Construction.** The crate's constructor is
//!   `Hnsw::new(max_nb_connection, max_elements, max_layer, ef_construction, f)`
//!   (hnsw.rs:766). The wrapper used to call it as `(M, dimension,
//!   ef_construction, 100)`: every graph was built with ef_construction 100 and
//!   max_layer `min(16, configured ef_construction)`, and a configured
//!   ef_construction below 16 made every dump fail. A dump needs exactly 16
//!   layers (`Description::dump`, hnswio.rs:893), so the layer count is the
//!   fixed [`HNSW_MAX_LAYER`], not a knob.
//! * **Dumps.** `load_hnsw` marks every reloaded graph as memory-mapped
//!   (hnswio.rs:517) and `file_dump` then refuses to overwrite an existing dump,
//!   writing `{basename}-{rand}` instead (api.rs:73, hnswio.rs:156). A dump is
//!   therefore written into a fresh staging directory, fsynced, and renamed
//!   into place ([`GraphIndex::dump_graph`]); the crate never sees an existing
//!   file.
//! * **Recall.** The crate adds a back-link at the NEW point's top layer only
//!   (`reverse_update_neighborhood_simple`, hnsw.rs:1245), so a point drawn
//!   above layer 0 gets layer-0 in-edges only from later points that select it;
//!   and a query descends the upper layers one greedy hop per layer
//!   (`search_filter`, hnsw.rs:1499). A row can therefore be unreachable by its
//!   own vector. At or below [`EXACT_SEARCH_MAX_POINTS`] points `search` is an
//!   exact scan, which there costs no more distance evaluations than the graph
//!   search at the default ef_search; above it the graph is built to make the
//!   defect rarer, not impossible (`BuildOptions`).
//!
//! Lock order on every path: `id_mapping`, then `reverse_mapping`, then
//! `index`. `search` used to take `index` first while `insert` held both
//! mappings and waited for `index`, so a search and an insert on the same index
//! could deadlock.

#![allow(clippy::similar_names)]

use super::{DistanceMetric, Vector};
use crate::{Error, Result};
use hnsw_rs::prelude::*;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Layer count every graph is built with: hnsw_rs 0.3.3's `NB_LAYER_MAX`
/// (hnsw.rs:41, crate-private). `Description::dump` refuses any other count
/// (hnswio.rs:893), so a graph built with fewer layers can never be dumped.
/// Sprinter 746342a0bfbb.
pub const HNSW_MAX_LAYER: usize = 16;

/// `Hnsw::new`'s `max_elements`. It only pre-sizes the per-layer point vectors
/// (`PointIndexation::new`, hnsw.rs:447) and is not a limit; it used to receive
/// the vector dimension.
const MAX_ELEMENTS_HINT: usize = 1024;

/// Smallest accepted `M`. Below 2 the crate's level law (scale `1 / ln M`)
/// degenerates: every point lands on a random layer.
pub const MIN_MAX_CONNECTIONS: usize = 2;
/// Largest accepted `M`. hnsw_rs calls `std::process::exit(1)` above 256
/// (hnsw.rs:779) and dumps `M` as a `u8` (hnswio.rs:888), so 256 would reload
/// as 0. Sprinter 746342a0bfbb.
pub const MAX_MAX_CONNECTIONS: usize = 255;

/// Sprinter 66aeb488f312: [`GraphIndex::search`] is an exact scan while the
/// graph holds at most this many points (tombstones included) AND at most
/// [`EXACT_SEARCH_MAX_FLOATS`] vector components.
///
/// A graph search at the default ef_search (200, rising to ~265 at 8k points)
/// expands at least ef candidates and evaluates up to 2·M = 32 layer-0
/// neighbours of each: up to ~6-8k distance evaluations, each with hash-map
/// and heap work. An exact scan of 8192 low-dimensional points costs no more,
/// and unlike the graph it cannot miss a row the crate left unreachable
/// (module docs, "Recall"). The recall flakes ran on 400 x 4, 3000 x 3 and
/// 4000 x 3.
pub const EXACT_SEARCH_MAX_POINTS: usize = 8192;

/// The second bound on the exact scan (see [`EXACT_SEARCH_MAX_POINTS`]):
/// points x dimension. One query of the scan is that many scalar
/// multiply-adds (anndists is built without SIMD), ~1-2 ms at 2M; at 1536
/// dimensions it allows ~1300 points, where 8192 would be 12.6M per query.
pub const EXACT_SEARCH_MAX_FLOATS: usize = 2_000_000;

/// Sprinter 66aeb488f312: whether new graphs fill neighbour lists with the
/// candidates the selection heuristic pruned (`Hnsw::set_keeping_pruned`).
/// The one switch for it; `tests::measure_build_cost_at_100k` measures both.
pub const DEFAULT_KEEP_PRUNED: bool = true;

/// Upper bound on live entries for the under-return rescue in
/// [`GraphIndex::search`] above [`EXACT_SEARCH_MAX_POINTS`]: a graph answer
/// shorter than the achievable k is recomputed exactly. Beyond this size an
/// under-full result is almost always tombstone filtering, which an exact scan
/// would turn into a per-query O(N) cost.
const BRUTE_FORCE_RESCUE_MAX: usize = 10_000;

/// `dumped_generation` of a graph that no dump describes.
const NEVER_DUMPED: u64 = u64::MAX;

/// The two files of one dump are `{basename}{GRAPH_FILE_SUFFIX}` and
/// `{basename}{DATA_FILE_SUFFIX}` (names fixed by hnsw_rs, hnswio.rs:161,193).
pub const GRAPH_FILE_SUFFIX: &str = ".hnsw.graph";
/// See [`GRAPH_FILE_SUFFIX`].
pub const DATA_FILE_SUFFIX: &str = ".hnsw.data";
/// Prefix of the staging directories [`GraphIndex::dump_graph`] writes into
/// before renaming a dump into place. A leftover one is a crashed dump.
pub const DUMP_STAGING_PREFIX: &str = ".hnsw-staging-";

/// How a graph is built (Sprinter 66aeb488f312). Every index uses
/// `BuildOptions::DEFAULT`; the test module measures the alternatives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuildOptions {
    /// `Hnsw::set_keeping_pruned`: fill a neighbour list up to its limit with
    /// the candidates the selection heuristic pruned. In low dimensions the
    /// heuristic keeps one or two neighbours of a point on a line; fuller lists
    /// give a point drawn above layer 0 more chances of a layer-0 in-edge.
    keep_pruned: bool,
    /// Insert the first vector of a batch into an EMPTY graph on its own before
    /// the parallel rest. Two concurrent first inserts can each find no entry
    /// point and return unlinked (hnsw.rs:1085-1099), leaving one of them
    /// unreachable for good.
    seed_empty_batch: bool,
    /// Insert a batch in a fixed pseudo-random order instead of row order: rows
    /// that arrive sorted along a direction otherwise build a chain.
    shuffle_batch: bool,
}

impl BuildOptions {
    const DEFAULT: Self = Self {
        keep_pruned: DEFAULT_KEEP_PRUNED,
        seed_empty_batch: true,
        shuffle_batch: true,
    };
}

/// A graph distance, and how a search reports it.
///
/// hnsw_rs needs a non-negative distance (it asserts so in `search_layer`,
/// hnsw.rs:965-966). L2 and cosine are reported as the graph computes them;
/// inner product is mapped onto a non-negative scale for the graph
/// ([`DistNegDot`]) and reported as `<#>` computes it.
pub trait GraphMetric: Distance<f32> + Default + Send + Sync {
    /// The reported distance for a graph distance.
    fn reported(graph_distance: f32) -> f32 {
        graph_distance
    }

    /// The reported distance between `a` and `b`, computed directly (the
    /// exact scan).
    fn exact(&self, a: &[f32], b: &[f32]) -> f32 {
        self.eval(a, b)
    }
}

impl GraphMetric for DistL2 {}
impl GraphMetric for DistCosine {}

/// Sprinter 17d901194e59: inner-product distance for the graph.
///
/// anndists' `DistDot` returns `1 - dot` and asserts it is non-negative, so it
/// accepts unit vectors only: any pair with a dot product above 1 (or a unit
/// self-match rounded above 1) panicked, in insert and in search. Nothing
/// normalizes vectors here. This distance takes `x = -dot`, exactly what `<#>`
/// evaluates to (`crate::vector::inner_product_distance`), and maps it
/// monotonically onto (0, inf) — `1 / (1 - x)` below 0, `1 + x` from 0 — so the
/// graph orders candidates as `<#>` does for any vectors. Searches report `x`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DistNegDot;

impl DistNegDot {
    fn to_graph(x: f32) -> f32 {
        if x.is_nan() {
            f32::MAX
        } else if x < 0.0 {
            1.0 / (1.0 - x)
        } else {
            (1.0 + x).min(f32::MAX)
        }
    }

    fn from_graph(distance: f32) -> f32 {
        if distance < 1.0 {
            1.0 - 1.0 / distance
        } else {
            distance - 1.0
        }
    }
}

impl Distance<f32> for DistNegDot {
    fn eval(&self, va: &[f32], vb: &[f32]) -> f32 {
        Self::to_graph(super::inner_product_distance(va, vb))
    }
}

impl GraphMetric for DistNegDot {
    fn reported(graph_distance: f32) -> f32 {
        Self::from_graph(graph_distance)
    }

    fn exact(&self, a: &[f32], b: &[f32]) -> f32 {
        super::inner_product_distance(a, b)
    }
}

/// What one [`GraphIndex::dump_graph`] wrote, captured under the same locks as
/// the files, so the mappings describe exactly the dumped graph.
#[derive(Debug, Clone)]
pub struct GraphDump {
    /// The index's mutation count when it was dumped; hand it back to
    /// [`GraphIndex::mark_dumped`] once the dump is durably referenced.
    pub generation: u64,
    /// hnsw-internal id -> row id (physical, tombstones included).
    pub id_mapping: Vec<u64>,
    /// Live `(row_id, hnsw_id)` pairs (tombstones excluded).
    pub reverse_mapping: Vec<(u64, u64)>,
}

/// One HNSW graph over `f32` vectors under the distance `D`, with the row-id
/// mappings around it. [`HnswIndex`], [`CosineHnswIndex`] and
/// [`InnerProductHnswIndex`] are this type for the three metrics (they were
/// three copies of the same code).
pub struct GraphIndex<D>
where
    D: GraphMetric,
{
    /// The underlying HNSW graph
    index: RwLock<Hnsw<'static, f32, D>>,
    /// Index configuration
    config: HnswConfig,
    /// Mapping from internal HNSW id to external row id
    id_mapping: RwLock<Vec<u64>>,
    /// Reverse mapping from row id to HNSW id
    reverse_mapping: RwLock<HashMap<u64, usize>>,
    /// Sprinter 746342a0bfbb: mutations so far (insert, batch, delete), bumped
    /// under the write locks the mutation holds.
    generation: AtomicU64,
    /// `generation` as of the dump that describes the graph (or the reload it
    /// came from); `NEVER_DUMPED` when no dump does.
    dumped_generation: AtomicU64,
    build: BuildOptions,
    /// [`EXACT_SEARCH_MAX_POINTS`]; the tests lower it to drive small graphs
    /// through the graph path.
    exact_max_points: usize,
}

/// HNSW index under L2 distance.
pub type HnswIndex = GraphIndex<DistL2>;
/// HNSW index for cosine distance
pub type CosineHnswIndex = GraphIndex<DistCosine>;
/// HNSW index for inner product, ordered as `<#>` (negative dot product)
pub type InnerProductHnswIndex = GraphIndex<DistNegDot>;

impl<D> GraphIndex<D>
where
    D: GraphMetric,
{
    /// Create an empty index. Refuses an `M` outside
    /// [`MIN_MAX_CONNECTIONS`]..=[`MAX_MAX_CONNECTIONS`] and an ef_construction
    /// of 0, which hnsw_rs cannot build or dump.
    pub fn new(config: HnswConfig) -> Result<Self> {
        Self::with_build_options(config, BuildOptions::DEFAULT)
    }

    fn with_build_options(config: HnswConfig, build: BuildOptions) -> Result<Self> {
        validate_construction_params(&config)?;
        // Sprinter 746342a0bfbb: the crate's argument order is
        // (max_nb_connection, max_elements, max_layer, ef_construction, f).
        let mut graph = Hnsw::<f32, D>::new(
            config.max_connections,
            MAX_ELEMENTS_HINT,
            HNSW_MAX_LAYER,
            config.ef_construction,
            D::default(),
        );
        graph.set_keeping_pruned(build.keep_pruned);
        Ok(Self::from_graph(
            graph,
            config,
            Vec::new(),
            HashMap::new(),
            NEVER_DUMPED,
            build,
        ))
    }

    fn from_graph(
        graph: Hnsw<'static, f32, D>,
        config: HnswConfig,
        id_mapping: Vec<u64>,
        reverse_mapping: HashMap<u64, usize>,
        dumped_generation: u64,
        build: BuildOptions,
    ) -> Self {
        Self {
            index: RwLock::new(graph),
            config,
            id_mapping: RwLock::new(id_mapping),
            reverse_mapping: RwLock::new(reverse_mapping),
            generation: AtomicU64::new(0),
            dumped_generation: AtomicU64::new(dumped_generation),
            build,
            exact_max_points: EXACT_SEARCH_MAX_POINTS,
        }
    }

    /// Whether `search` answers by exact scan at the current size.
    fn answers_exactly(&self, points: usize) -> bool {
        points <= self.exact_max_points && points.saturating_mul(self.config.dimension) <= EXACT_SEARCH_MAX_FLOATS
    }

    fn check_dimension(&self, vector: &[f32], what: &str) -> Result<()> {
        if vector.len() == self.config.dimension {
            return Ok(());
        }
        Err(Error::query_execution(format!(
            "{what} dimension mismatch: expected {}, got {}",
            self.config.dimension,
            vector.len()
        )))
    }

    /// Record a mutation. Called with the mutation's write locks held, so a
    /// dump (which holds the read locks) sees either all of it or none of it.
    fn note_mutation(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Insert a vector into the index
    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<()> {
        self.check_dimension(vector, "Vector")?;

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();
        let index = self.index.write();

        // Get internal HNSW id
        let hnsw_id = id_mapping.len();
        id_mapping.push(row_id);
        reverse_mapping.insert(row_id, hnsw_id);
        index.insert((vector.as_slice(), hnsw_id));
        self.note_mutation();
        Ok(())
    }

    /// Bulk-insert a batch of `(row_id, vector)` pairs, building the HNSW
    /// graph in parallel across all available cores.
    ///
    /// This is the fast path for `CREATE INDEX ... USING hnsw` backfill over a
    /// populated table and for the rebuild at open. Every `hnsw_id` is
    /// pre-assigned in input order under one critical section, then the batch
    /// goes to `hnsw_rs::parallel_insert`, which synchronises its own internal
    /// structures. Before that (`BuildOptions::DEFAULT`) the batch is put in a
    /// fixed shuffled order (ids stay with their vectors), and into an empty
    /// graph its first vector goes in alone.
    pub fn insert_batch(&self, batch: &[(u64, Vector)]) -> Result<()> {
        for (_, vector) in batch {
            self.check_dimension(vector, "Vector")?;
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut id_mapping = self.id_mapping.write();
        let mut reverse_mapping = self.reverse_mapping.write();
        let index = self.index.write();
        let base = id_mapping.len();
        let mut datas: Vec<(&Vec<f32>, usize)> = Vec::with_capacity(batch.len());
        for (offset, (row_id, vector)) in batch.iter().enumerate() {
            let hnsw_id = base + offset;
            id_mapping.push(*row_id);
            reverse_mapping.insert(*row_id, hnsw_id);
            datas.push((vector, hnsw_id));
        }
        if self.build.shuffle_batch {
            shuffle_in_place(&mut datas);
        }

        let mut rest: &[(&Vec<f32>, usize)] = &datas;
        if self.build.seed_empty_batch && index.get_nb_point() == 0 {
            if let Some(((first, first_id), tail)) = datas.split_first() {
                index.insert((first.as_slice(), *first_id));
                rest = tail;
            }
        }
        if !rest.is_empty() {
            index.parallel_insert(rest);
        }
        self.note_mutation();
        Ok(())
    }

    /// Search for k nearest neighbors.
    ///
    /// Within [`EXACT_SEARCH_MAX_POINTS`] graph points and
    /// [`EXACT_SEARCH_MAX_FLOATS`] components the answer is an exact scan
    /// (Sprinter 66aeb488f312). Beyond either the graph answers with a dynamic
    /// ef_search (scaled with k and the index size), and a graph answer
    /// shorter than the achievable k is recomputed exactly while the index has
    /// at most `BRUTE_FORCE_RESCUE_MAX` live entries.
    pub fn search(&self, query: &Vector, k: usize) -> Result<Vec<(u64, f32)>> {
        self.check_dimension(query, "Query vector")?;

        let id_mapping = self.id_mapping.read();
        let reverse_mapping = self.reverse_mapping.read();
        let index = self.index.read();

        if self.answers_exactly(id_mapping.len()) {
            return Ok(exact_search(&index, query, k, &id_mapping, &reverse_mapping));
        }

        let ef_search = self.calculate_ef_search(k, id_mapping.len());
        let results = graph_search(&index, query, k, ef_search, &id_mapping, &reverse_mapping);
        let live = reverse_mapping.len();
        if results.len() < k.min(live) && live <= BRUTE_FORCE_RESCUE_MAX {
            return Ok(exact_search(&index, query, k, &id_mapping, &reverse_mapping));
        }
        Ok(results)
    }

    /// Calculate optimal ef_search based on k and index size
    fn calculate_ef_search(&self, k: usize, index_size: usize) -> usize {
        if !self.config.dynamic_ef_search {
            return self.config.ef_search_base;
        }

        // Base: at minimum, ef_search should be 2x k for good recall
        let k_based = k.saturating_mul(2);

        // Size factor: larger indices may need slightly higher ef_search
        // log2(size) provides diminishing returns scaling
        let size_factor = if index_size > 1000 {
            (index_size as f64).log2() / 10.0 // ~1.0 at 1K, ~1.3 at 10K, ~1.6 at 100K
        } else {
            1.0
        };

        // Calculate ef_search with size adjustment
        let adjusted = ((self.config.ef_search_base as f64 * size_factor) as usize).max(k_based);

        // Clamp to configured bounds (never panicking on a min above the max)
        adjusted
            .max(self.config.ef_search_min)
            .min(self.config.ef_search_max.max(self.config.ef_search_min))
    }

    /// The graph alone, with an explicit ef_search: no exact path and no
    /// rescue, whatever the size. For fine-tuned queries and for measuring
    /// what the graph itself returns (tests/hnsw_recall_measurement.rs).
    pub fn search_with_ef(&self, query: &Vector, k: usize, ef_search: usize) -> Result<Vec<(u64, f32)>> {
        self.check_dimension(query, "Query vector")?;

        let id_mapping = self.id_mapping.read();
        let reverse_mapping = self.reverse_mapping.read();
        let index = self.index.read();

        // Clamp ef_search to [k, ef_search_max] (k wins when it is larger).
        let ef_search = ef_search.min(self.config.ef_search_max).max(k);
        Ok(graph_search(&index, query, k, ef_search, &id_mapping, &reverse_mapping))
    }

    /// Delete a vector from the index
    pub fn delete(&self, row_id: u64) -> Result<()> {
        let mut reverse_mapping = self.reverse_mapping.write();

        // hnsw_rs cannot remove a point: the vector stays in the graph as a
        // tombstone that the result filter (and the exact scan) skips.
        if reverse_mapping.remove(&row_id).is_some() {
            self.note_mutation();
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

    /// Sprinter 746342a0bfbb: true while the graph and its mappings are exactly
    /// what the last recorded dump (or the dump this index was reloaded from)
    /// holds, so a checkpoint need not dump it again.
    pub fn is_unchanged_since_dump(&self) -> bool {
        self.dumped_generation.load(Ordering::Acquire) == self.generation.load(Ordering::Acquire)
    }

    /// Record that the dump taken at `generation` ([`GraphDump::generation`])
    /// is now the one that describes this index. A mutation after that dump
    /// keeps the index changed.
    pub fn mark_dumped(&self, generation: u64) {
        self.dumped_generation.store(generation, Ordering::Release);
    }

    /// R4.2: dump the graph to `{dir}/{basename}.hnsw.graph` and
    /// `{basename}.hnsw.data`, replacing a dump of that name.
    ///
    /// Sprinter 746342a0bfbb: hnsw_rs writes into a fresh staging directory
    /// inside `dir`, both files are fsynced, renamed into `dir`, and the
    /// directory is fsynced; the staging directory is removed. Each rename is
    /// atomic, the pair is not: a caller that must never expose a mixed pair
    /// dumps under a fresh basename and switches its reference afterwards
    /// (`StorageEngine::persist_index_snapshots`). An empty graph cannot be
    /// dumped (hnsw_rs needs an entry point) and is an error.
    pub fn dump_graph(&self, dir: &Path, basename: &str) -> Result<GraphDump> {
        let id_mapping = self.id_mapping.read();
        let reverse_mapping = self.reverse_mapping.read();
        let index = self.index.read();
        write_graph_dump(&index, dir, basename)?;
        Ok(GraphDump {
            generation: self.generation.load(Ordering::Acquire),
            id_mapping: id_mapping.clone(),
            reverse_mapping: export_reverse(&reverse_mapping),
        })
    }

    /// R4.2: snapshot the row-id mappings for the persistence sidecar.
    pub fn export_mappings(&self) -> (Vec<u64>, Vec<(u64, u64)>) {
        let id_mapping = self.id_mapping.read();
        let reverse_mapping = self.reverse_mapping.read();
        (id_mapping.clone(), export_reverse(&reverse_mapping))
    }

    /// R4.2: reconstruct the index from a graph dump + sidecar mappings. The
    /// reloaded index counts as unchanged since its dump. A dump whose point
    /// count or dimension disagrees with the sidecar is refused.
    pub fn reload_from_dump(
        config: HnswConfig,
        dir: &Path,
        basename: &str,
        id_mapping: Vec<u64>,
        reverse_pairs: &[(u64, u64)],
    ) -> Result<Self> {
        let mut graph: Hnsw<'static, f32, D> = reload_graph(dir, basename)?;
        let points = graph.get_nb_point();
        if points != id_mapping.len() {
            return Err(Error::storage(format!(
                "HNSW graph dump '{basename}' holds {points} points but its sidecar maps {}",
                id_mapping.len()
            )));
        }
        let dimension = graph.get_point_indexation().get_data_dimension();
        if points > 0 && dimension != config.dimension {
            return Err(Error::storage(format!(
                "HNSW graph dump '{basename}' holds {dimension}-dim vectors, the index is {}-dim",
                config.dimension
            )));
        }
        // `load_hnsw` sets keep_pruned = false and extend_candidates = true
        // (hnswio.rs:507-518) where `Hnsw::new` sets extend_candidates = false:
        // later inserts into a reloaded graph build like a fresh graph's.
        graph.set_keeping_pruned(BuildOptions::DEFAULT.keep_pruned);
        graph.set_extend_candidates(false);
        Ok(Self::from_graph(
            graph,
            config,
            id_mapping,
            reverse_pairs_to_map(reverse_pairs),
            0,
            BuildOptions::DEFAULT,
        ))
    }
}

// ---- construction ------------------------------------------------------------

/// Sprinter 746342a0bfbb: refuse what hnsw_rs cannot build or dump, before it
/// is handed to the crate (which exits the process for `M > 256`).
fn validate_construction_params(config: &HnswConfig) -> Result<()> {
    if !(MIN_MAX_CONNECTIONS..=MAX_MAX_CONNECTIONS).contains(&config.max_connections) {
        return Err(Error::query_execution(format!(
            "HNSW m must be between {MIN_MAX_CONNECTIONS} and {MAX_MAX_CONNECTIONS}, got {}",
            config.max_connections
        )));
    }
    if config.ef_construction == 0 {
        return Err(Error::query_execution("HNSW ef_construction must be at least 1"));
    }
    Ok(())
}

/// Fisher-Yates over a fixed SplitMix64 stream: the same batch is always
/// inserted in the same order.
fn shuffle_in_place<T>(items: &mut [T]) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15 ^ items.len() as u64;
    for i in (1..items.len()).rev() {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let j = (z % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

// ---- search --------------------------------------------------------------------

/// The graph's answer, mapped to row ids, tombstones dropped, distances as
/// reported.
fn graph_search<D>(
    index: &Hnsw<'static, f32, D>,
    query: &[f32],
    k: usize,
    ef_search: usize,
    id_mapping: &[u64],
    reverse_mapping: &HashMap<u64, usize>,
) -> Vec<(u64, f32)>
where
    D: GraphMetric,
{
    index
        .search(query, k, ef_search)
        .into_iter()
        .filter_map(|neighbor| {
            let hnsw_id = neighbor.d_id;
            let row_id = *id_mapping.get(hnsw_id)?;
            (reverse_mapping.get(&row_id) == Some(&hnsw_id)).then_some((row_id, D::reported(neighbor.distance)))
        })
        .collect()
}

/// Call `f` on every point of the graph, layer by layer, over all
/// `get_max_level()` layers. (The crate's own point iterator stops at the entry
/// point's layer and unwraps a missing entry point.) The caller holds the graph
/// read lock, so no insert runs meanwhile.
fn for_each_point<D, F>(index: &Hnsw<'static, f32, D>, mut f: F)
where
    D: Distance<f32> + Send + Sync,
    F: FnMut(&Point<'static, f32>),
{
    let points = index.get_point_indexation();
    for layer in 0..index.get_max_level() {
        if points.get_layer_nb_point(layer) == 0 {
            continue;
        }
        for point in points.get_layer_iterator(layer) {
            f(point.as_ref());
        }
    }
}

/// Exact k-NN over every live point, with the graph's own metric so scores
/// match graph results ([`GraphMetric::exact`]). Ties break on row id. The
/// caller holds the mapping and graph read locks (guards are passed in, not
/// re-taken: parking_lot locks are not reentrant under a queued writer).
fn exact_search<D>(
    index: &Hnsw<'static, f32, D>,
    query: &[f32],
    k: usize,
    id_mapping: &[u64],
    reverse_mapping: &HashMap<u64, usize>,
) -> Vec<(u64, f32)>
where
    D: GraphMetric,
{
    if k == 0 || reverse_mapping.is_empty() || index.get_nb_point() == 0 {
        return Vec::new();
    }
    let distance = index.get_distance();
    let mut all: Vec<(u64, f32)> = Vec::with_capacity(reverse_mapping.len());
    for_each_point(index, |point| {
        let hnsw_id = point.get_origin_id();
        if let Some(&row_id) = id_mapping.get(hnsw_id) {
            // Same tombstone rule as the graph-result filter.
            if reverse_mapping.get(&row_id) == Some(&hnsw_id) {
                all.push((row_id, distance.exact(query, point.get_v())));
            }
        }
    });
    let by_distance = |a: &(u64, f32), b: &(u64, f32)| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0));
    if all.len() > k {
        all.select_nth_unstable_by(k - 1, by_distance);
        all.truncate(k);
    }
    all.sort_unstable_by(by_distance);
    all
}

// ---- persistence ---------------------------------------------------------------

fn export_reverse(reverse_mapping: &HashMap<u64, usize>) -> Vec<(u64, u64)> {
    reverse_mapping.iter().map(|(row, hnsw)| (*row, *hnsw as u64)).collect()
}

fn reverse_pairs_to_map(pairs: &[(u64, u64)]) -> HashMap<u64, usize> {
    pairs.iter().map(|(row, hnsw)| (*row, *hnsw as usize)).collect()
}

/// Dump `index` as `{dir}/{basename}.hnsw.*` through a staging directory (see
/// [`GraphIndex::dump_graph`]). The staging directory is removed on every path.
fn write_graph_dump<D>(index: &Hnsw<'static, f32, D>, dir: &Path, basename: &str) -> Result<()>
where
    D: Distance<f32> + Send + Sync,
{
    if index.get_nb_point() == 0 {
        return Err(Error::storage(format!(
            "HNSW graph dump '{basename}': the graph is empty (hnsw_rs cannot dump a graph without an entry point)"
        )));
    }
    let staging = create_staging_dir(dir, basename)?;
    let published = dump_and_publish(index, &staging, dir, basename);
    if let Err(e) = std::fs::remove_dir_all(&staging) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "HNSW graph dump: could not remove staging directory {}: {}",
                staging.display(),
                e
            );
        }
    }
    published
}

fn dump_and_publish<D>(index: &Hnsw<'static, f32, D>, staging: &Path, dir: &Path, basename: &str) -> Result<()>
where
    D: Distance<f32> + Send + Sync,
{
    // hnsw_rs panics instead of failing on some I/O errors (`DumpInit::new`,
    // hnswio.rs:206); this runs on the close path, so a panic becomes an error.
    let dumped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| index.file_dump(staging, basename)))
        .map_err(|_| Error::storage(format!("HNSW graph dump of '{basename}' panicked inside hnsw_rs")))?
        .map_err(|e| Error::storage(format!("HNSW graph dump failed: {e}")))?;
    if dumped != basename {
        // The staging directory was empty, so hnsw_rs had nothing to avoid
        // overwriting; a different name means something else wrote there.
        return Err(Error::storage(format!(
            "HNSW graph dump wrote basename '{dumped}' instead of '{basename}'"
        )));
    }
    for suffix in [GRAPH_FILE_SUFFIX, DATA_FILE_SUFFIX] {
        let staged = staging.join(format!("{basename}{suffix}"));
        sync_file(&staged).map_err(|e| dump_io_error("fsync", &staged, e))?;
    }
    for suffix in [GRAPH_FILE_SUFFIX, DATA_FILE_SUFFIX] {
        let staged = staging.join(format!("{basename}{suffix}"));
        let target = dir.join(format!("{basename}{suffix}"));
        std::fs::rename(&staged, &target).map_err(|e| dump_io_error("rename into place", &target, e))?;
    }
    sync_dir(dir).map_err(|e| dump_io_error("fsync", dir, e))
}

fn dump_io_error(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::storage(format!("HNSW graph dump: {what} {}: {e}", path.display()))
}

/// A new, empty directory inside `dir` for one dump.
fn create_staging_dir(dir: &Path, basename: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::storage(format!("HNSW graph dump: cannot create {}: {e}", dir.display())))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let process = std::process::id();
    for attempt in 0..8_u32 {
        let staging = dir.join(format!("{DUMP_STAGING_PREFIX}{basename}-{process}-{stamp:x}-{attempt}"));
        match std::fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(Error::storage(format!(
                    "HNSW graph dump: cannot create staging directory {}: {e}",
                    staging.display()
                )));
            }
        }
    }
    Err(Error::storage(format!(
        "HNSW graph dump: no free staging directory name in {}",
        dir.display()
    )))
}

/// fsync one file. Opened for writing (without truncation) because Windows
/// refuses to flush a read-only handle.
fn sync_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// fsync a directory so the renames into it are durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Directories cannot be opened for fsync through std on this platform.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Reload a dumped graph. The `HnswIo` loader owns buffers the reloaded
/// graph borrows from, so it is intentionally leaked to give the graph the
/// `'static` lifetime the wrappers require — one small leak per index per
/// process open (the reload path runs only at startup).
fn reload_graph<D>(dir: &Path, basename: &str) -> Result<Hnsw<'static, f32, D>>
where
    D: Distance<f32> + Default + Send + Sync,
{
    // hnsw_rs asserts and unwraps while reading (hnswio.rs:416, 454, 1226). The
    // caller digest-checks the files first; a panic still becomes an error (and
    // a rebuild from rows) rather than a failed open. The loader is created and
    // leaked INSIDE the closure: the graph borrows it, so it must be a
    // `&'static mut` the closure owns, not a captured one it would reborrow.
    let load = move || {
        let io: &'static mut hnsw_rs::hnswio::HnswIo = Box::leak(Box::new(hnsw_rs::hnswio::HnswIo::new(dir, basename)));
        io.load_hnsw::<f32, D>()
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(load)) {
        Ok(Ok(graph)) => Ok(graph),
        Ok(Err(e)) => Err(Error::storage(format!("HNSW graph reload failed: {e}"))),
        Err(_) => Err(Error::storage(format!(
            "HNSW graph reload of '{basename}' panicked inside hnsw_rs"
        ))),
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

    /// Bulk-insert a batch of `(row_id, vector)` pairs, building the graph in
    /// parallel. Used by `CREATE INDEX` backfill; see [`GraphIndex::insert_batch`].
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

    /// Live (tombstone-excluded) entry count — see [`GraphIndex::live_len`].
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

    /// See [`GraphIndex::is_unchanged_since_dump`].
    pub fn is_unchanged_since_dump(&self) -> bool {
        match self {
            Self::L2(index) => index.is_unchanged_since_dump(),
            Self::Cosine(index) => index.is_unchanged_since_dump(),
            Self::InnerProduct(index) => index.is_unchanged_since_dump(),
        }
    }

    /// See [`GraphIndex::mark_dumped`].
    pub fn mark_dumped(&self, generation: u64) {
        match self {
            Self::L2(index) => index.mark_dumped(generation),
            Self::Cosine(index) => index.mark_dumped(generation),
            Self::InnerProduct(index) => index.mark_dumped(generation),
        }
    }

    /// R4.2: dump the graph to `{dir}/{basename}.hnsw.{graph,data}` — see
    /// [`GraphIndex::dump_graph`].
    pub fn dump_graph(&self, dir: &Path, basename: &str) -> Result<GraphDump> {
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
        dir: &Path,
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_precision_loss
)]
mod tests {
    use super::*;
    use hnsw_rs::prelude::{AnnT, DistDot, DistL2, Distance};

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
        // under-returns; `search` must make k=2 exact every time (now the
        // exact path at or below EXACT_SEARCH_MAX_POINTS). Loop enough that a
        // regression reappears at a visible rate.
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

    // ---- R4 (Sprinter 746342a0bfbb / 66aeb488f312) ------------------------------

    /// SplitMix64 over multiples of 1/1024: seeded, exact in f32.
    struct Rows(u64);

    impl Rows {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn take(seed: u64, n: usize, dimension: usize) -> Vec<(u64, Vector)> {
            let mut rng = Rows(seed);
            (0..n)
                .map(|i| {
                    let v = (0..dimension)
                        .map(|_| (rng.next_u64() % 1024) as f32 / 1024.0)
                        .collect();
                    (i as u64 + 1, v)
                })
                .collect()
        }
    }

    fn config(dimension: usize) -> HnswConfig {
        HnswConfig {
            dimension,
            ..Default::default()
        }
    }

    fn assert_built_with<D>(index: &GraphIndex<D>, m: usize, ef_construction: usize, what: &str)
    where
        D: GraphMetric,
    {
        let graph = index.index.read();
        assert_eq!(usize::from(graph.get_max_nb_connection()), m, "{what}: M");
        assert_eq!(graph.get_ef_construction(), ef_construction, "{what}: ef_construction");
        assert_eq!(graph.get_max_level(), HNSW_MAX_LAYER, "{what}: layers");
    }

    /// Reload what `dump_graph` wrote, as the storage layer does from a sidecar.
    fn reload(config: HnswConfig, dir: &Path, basename: &str, dump: GraphDump) -> HnswIndex {
        HnswIndex::reload_from_dump(config, dir, basename, dump.id_mapping, &dump.reverse_mapping).unwrap()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Item 3: the graph is built with the configured M and ef_construction and
    /// hnsw_rs's 16 layers. Red before: ef_construction was always 100 and the
    /// layer count min(16, ef_construction).
    #[test]
    fn a_built_graph_reports_the_configured_m_and_ef_construction() {
        for (m, ef_construction) in [(12, 37), (16, 200), (8, 8)] {
            let config = HnswConfig {
                dimension: 4,
                max_connections: m,
                ef_construction,
                ..Default::default()
            };
            let what = format!("M = {m}, ef_construction = {ef_construction}");
            let l2 = HnswIndex::new(config.clone()).unwrap();
            assert_built_with(&l2, m, ef_construction, &format!("L2 {what}"));
            let cosine = CosineHnswIndex::new(HnswConfig {
                distance_metric: DistanceMetric::Cosine,
                ..config.clone()
            })
            .unwrap();
            assert_built_with(&cosine, m, ef_construction, &format!("cosine {what}"));
            let inner = InnerProductHnswIndex::new(HnswConfig {
                distance_metric: DistanceMetric::InnerProduct,
                ..config.clone()
            })
            .unwrap();
            assert_built_with(&inner, m, ef_construction, &format!("inner product {what}"));

            // The dump carries them: a reloaded graph reports the same.
            for (id, v) in Rows::take(m as u64, 40, 4) {
                l2.insert(id, &v).unwrap();
            }
            let dir = tempfile::tempdir().unwrap();
            let dump = l2.dump_graph(dir.path(), "params").unwrap();
            let reloaded = reload(config, dir.path(), "params", dump);
            assert_built_with(&reloaded, m, ef_construction, &format!("reloaded {what}"));
        }
    }

    /// Item 3: an ef_construction below 16 used to become the layer count, and
    /// `Description::dump` refuses anything but 16 layers, so every dump failed.
    #[test]
    fn a_graph_with_an_ef_construction_below_16_can_be_dumped_and_reloaded() {
        let config = HnswConfig {
            dimension: 4,
            ef_construction: 8,
            ..Default::default()
        };
        let index = HnswIndex::new(config.clone()).unwrap();
        let rows = Rows::take(3, 60, 4);
        index.insert_batch(&rows).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dump = index.dump_graph(dir.path(), "small_ef").expect("dumps with 16 layers");
        let reloaded = reload(config, dir.path(), "small_ef", dump);
        for (id, v) in &rows {
            let hit = reloaded.search(v, 1).unwrap()[0];
            assert!(hit.0 == *id || hit.1 == 0.0, "row {id} found by its own vector");
        }
    }

    /// Item 3: parameters hnsw_rs cannot build or dump are refused up front
    /// (M above 256 made the crate call `std::process::exit(1)`).
    #[test]
    fn construction_parameters_hnsw_rs_cannot_handle_are_refused() {
        for m in [0, 1, 256, 300] {
            let error = HnswIndex::new(HnswConfig {
                dimension: 4,
                max_connections: m,
                ..Default::default()
            })
            .err()
            .unwrap_or_else(|| panic!("M = {m} must be refused"))
            .to_string();
            assert!(error.contains("HNSW m must be between 2 and 255"), "{error}");
        }
        let error = HnswIndex::new(HnswConfig {
            dimension: 4,
            ef_construction: 0,
            ..Default::default()
        })
        .err()
        .expect("ef_construction = 0 must be refused")
        .to_string();
        assert!(error.contains("ef_construction must be at least 1"), "{error}");
        assert!(HnswIndex::new(HnswConfig {
            dimension: 4,
            max_connections: 255,
            ef_construction: 1,
            ..Default::default()
        })
        .is_ok());
    }

    /// Item 1: a RELOADED graph dumps again under its own name, in place, with
    /// nothing left beside it. Red before: hnsw_rs flags reloaded graphs as
    /// memory-mapped, refused to overwrite, wrote `reload-{rand}.hnsw.*`, and
    /// the wrapper reported the dump as failed.
    #[test]
    fn a_reloaded_graph_dumps_again_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(config(4)).unwrap();
        for (id, v) in Rows::take(7, 64, 4) {
            index.insert(id, &v).unwrap();
        }
        let first = index.dump_graph(dir.path(), "reload").unwrap();
        assert_eq!(entries(dir.path()), vec!["reload.hnsw.data", "reload.hnsw.graph"]);

        let reloaded = reload(config(4), dir.path(), "reload", first);
        reloaded.insert(1000, &vec![0.5, 0.25, 0.125, 2.0]).unwrap();
        let second = reloaded
            .dump_graph(dir.path(), "reload")
            .expect("a reloaded graph dumps in place");
        assert_eq!(
            entries(dir.path()),
            vec!["reload.hnsw.data", "reload.hnsw.graph"],
            "no `-{{rand}}` copy and no staging directory is left behind"
        );

        let again = reload(config(4), dir.path(), "reload", second);
        assert_eq!(again.len(), 65);
        assert_eq!(again.search(&vec![0.5, 0.25, 0.125, 2.0], 1).unwrap()[0], (1000, 0.0));
    }

    /// Item 1: a reload is refused, not trusted, when the sidecar's mapping or
    /// dimension does not match the dump.
    #[test]
    fn a_dump_that_disagrees_with_its_sidecar_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(config(4)).unwrap();
        index.insert_batch(&Rows::take(9, 30, 4)).unwrap();
        let dump = index.dump_graph(dir.path(), "count").unwrap();
        let mut short = dump.id_mapping.clone();
        short.pop();
        let error = HnswIndex::reload_from_dump(config(4), dir.path(), "count", short, &dump.reverse_mapping)
            .err()
            .expect("a mismatched sidecar is refused")
            .to_string();
        assert!(error.contains("holds 30 points but its sidecar maps 29"), "{error}");
        let other_dimension = HnswIndex::reload_from_dump(config(5), dir.path(), "count", dump.id_mapping, &[]);
        assert!(other_dimension.is_err(), "a dimension mismatch is refused");
    }

    /// Item 1: an empty graph is not dumpable (hnsw_rs has no entry point to
    /// write) and says so without leaving a staging directory.
    #[test]
    fn an_empty_graph_is_not_dumped() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(config(4)).unwrap();
        let error = index.dump_graph(dir.path(), "empty").err().unwrap().to_string();
        assert!(error.contains("the graph is empty"), "{error}");
        assert!(entries(dir.path()).is_empty());
    }

    /// The per-index dump state: changed by every mutation, unchanged after a
    /// recorded dump and after a reload, and a mutation that lands between a
    /// dump and its record keeps the index changed.
    #[test]
    fn the_dump_state_follows_every_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(config(4)).unwrap();
        assert!(!index.is_unchanged_since_dump(), "no dump describes a new index");

        let rows = Rows::take(5, 20, 4);
        for (id, v) in &rows[..10] {
            index.insert(*id, v).unwrap();
        }
        let dump = index.dump_graph(dir.path(), "state").unwrap();
        assert!(!index.is_unchanged_since_dump(), "dumped, not yet recorded");
        index.mark_dumped(dump.generation);
        assert!(index.is_unchanged_since_dump());

        index.insert(rows[10].0, &rows[10].1).unwrap();
        assert!(!index.is_unchanged_since_dump(), "insert");
        index.mark_dumped(index.dump_graph(dir.path(), "state").unwrap().generation);
        index.delete(rows[0].0).unwrap();
        assert!(!index.is_unchanged_since_dump(), "delete");
        index.mark_dumped(index.dump_graph(dir.path(), "state").unwrap().generation);
        index.insert_batch(&rows[11..15]).unwrap();
        assert!(!index.is_unchanged_since_dump(), "batch");

        let raced = index.dump_graph(dir.path(), "state").unwrap();
        index.insert(rows[15].0, &rows[15].1).unwrap();
        index.mark_dumped(raced.generation);
        assert!(
            !index.is_unchanged_since_dump(),
            "a mutation after the dump stays unrecorded"
        );

        let current = index.dump_graph(dir.path(), "state").unwrap();
        index.mark_dumped(current.generation);
        assert!(index.delete(424_242).is_err());
        assert!(index.is_unchanged_since_dump(), "a failed delete changes nothing");

        let reloaded = reload(config(4), dir.path(), "state", current);
        assert!(reloaded.is_unchanged_since_dump(), "a reload is its dump");
        assert_eq!(reloaded.live_len(), 15);
    }

    /// Recall: at or below EXACT_SEARCH_MAX_POINTS `search` returns the exact
    /// k nearest live rows, ordered by (distance, row id).
    #[test]
    fn search_is_exact_at_or_below_the_exact_threshold() {
        let rows = Rows::take(11, 500, 4);
        let index = HnswIndex::new(config(4)).unwrap();
        index.insert_batch(&rows).unwrap();
        index.delete(rows[0].0).unwrap();
        let live = &rows[1..];

        let middle = vec![0.5; 4];
        for (k, query) in [(1, &rows[10].1), (10, &rows[200].1), (7, &middle), (600, &rows[300].1)] {
            let mut expected: Vec<(u64, f32)> = live
                .iter()
                .map(|(id, v)| (*id, Distance::<f32>::eval(&DistL2, query, v)))
                .collect();
            expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            expected.truncate(k);
            assert_eq!(index.search(query, k).unwrap(), expected, "k = {k}");
        }
        for (id, v) in live {
            let hit = index.search(v, 1).unwrap()[0];
            assert!(
                hit.1.abs() <= f32::EPSILON,
                "row {id} found by its own vector: {}",
                hit.1
            );
        }
        assert!(index
            .search(&rows[0].1, 3)
            .unwrap()
            .iter()
            .all(|(id, _)| *id != rows[0].0));
        assert!(index.search(&rows[5].1, 0).unwrap().is_empty());
    }

    /// The lock order: concurrent inserts and searches on one index finish.
    /// (Search used to take the graph lock first and insert the mappings
    /// first — an inversion.)
    #[test]
    fn concurrent_inserts_and_searches_complete() {
        let index = std::sync::Arc::new(HnswIndex::new(config(4)).unwrap());
        let rows = Rows::take(13, 500, 4);
        index.insert_batch(&rows[..200]).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for part in [&rows[200..350], &rows[350..500]] {
            let index = std::sync::Arc::clone(&index);
            let part = part.to_vec();
            let done = done.clone();
            threads.push(std::thread::spawn(move || {
                for (id, v) in &part {
                    index.insert(*id, v).unwrap();
                }
                done.send(()).unwrap();
            }));
        }
        for t in 0..4_usize {
            let index = std::sync::Arc::clone(&index);
            let queries: Vec<Vector> = rows.iter().skip(t).step_by(4).map(|(_, v)| v.clone()).collect();
            let done = done.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..3 {
                    for q in &queries {
                        index.search(q, 3).unwrap();
                    }
                }
                done.send(()).unwrap();
            }));
        }
        for _ in 0..threads.len() {
            finished
                .recv_timeout(std::time::Duration::from_secs(300))
                .expect("an insert and a search on one index deadlocked");
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(index.live_len(), 500);
    }

    /// Sprinter 17d901194e59: an inner-product index takes unnormalized vectors
    /// (dot products far above 1), incrementally and in a batch, and orders
    /// rows as `<#>` does (ascending `-dot`), reporting `-dot`, on the exact
    /// path and the graph path. Red before: anndists' `DistDot` asserted
    /// `1 - dot >= 0` and panicked on the first such pair.
    #[test]
    fn an_inner_product_index_takes_unnormalized_vectors() {
        use crate::vector::inner_product_distance;
        let rows: Vec<(u64, Vector)> = (0..300_u64)
            .map(|i| {
                let v = vec![
                    (i % 7) as f32 + 1.0,
                    (i % 11) as f32 * 0.5 + 1.0,
                    (i % 13) as f32 * 2.0 + 1.0,
                ];
                (i + 1, v)
            })
            .collect();
        let config = HnswConfig {
            dimension: 3,
            distance_metric: DistanceMetric::InnerProduct,
            ..Default::default()
        };
        let incremental = InnerProductHnswIndex::new(config.clone()).unwrap();
        for (id, v) in &rows {
            incremental.insert(*id, v).unwrap();
        }
        let batch = InnerProductHnswIndex::new(config.clone()).unwrap();
        batch.insert_batch(&rows).unwrap();

        let query = vec![1.0, 2.0, 3.0];
        let truth = |id: u64| inner_product_distance(&query, &rows[(id - 1) as usize].1);
        let mut expected: Vec<(u64, f32)> = rows.iter().map(|(id, _)| (*id, truth(*id))).collect();
        expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        expected.truncate(10);
        for (what, index) in [("incremental", &incremental), ("batch", &batch)] {
            assert_eq!(index.search(&query, 10).unwrap(), expected, "{what}: exact scan");
            let graph = index.search_with_ef(&query, 10, 200).unwrap();
            assert_eq!(graph.len(), 10, "{what}: graph");
            assert!(graph.windows(2).all(|pair| pair[0].1 <= pair[1].1), "{what}: {graph:?}");
            for (id, reported) in &graph {
                let exact = truth(*id);
                assert!(
                    (reported - exact).abs() <= 1e-3 * exact.abs().max(1.0),
                    "{what}: row {id} reported {reported}, <#> is {exact}"
                );
            }
        }

        // It dumps and reloads under its own distance name; a dump written
        // with the old `DistDot` is refused (and so rebuilt from rows once).
        let dir = tempfile::tempdir().unwrap();
        let dump = batch.dump_graph(dir.path(), "ip").unwrap();
        let reloaded = InnerProductHnswIndex::reload_from_dump(
            config.clone(),
            dir.path(),
            "ip",
            dump.id_mapping,
            &dump.reverse_mapping,
        )
        .unwrap();
        assert_eq!(reloaded.search(&query, 10).unwrap(), expected);
        let old = Hnsw::<f32, DistDot>::new(16, 16, HNSW_MAX_LAYER, 100, DistDot);
        old.insert(([1.0_f32, 0.0, 0.0].as_slice(), 0));
        old.insert(([0.0_f32, 1.0, 0.0].as_slice(), 1));
        old.file_dump(dir.path(), "old_ip").unwrap();
        let pairs = [(1, 0), (2, 1)];
        let refused = InnerProductHnswIndex::reload_from_dump(config, dir.path(), "old_ip", vec![1, 2], &pairs);
        assert!(refused.is_err(), "a DistDot dump is not loaded as DistNegDot");
    }

    /// The graph path, asserted (review of R4): 20k rows is past
    /// EXACT_SEARCH_MAX_POINTS, so `search` and `search_with_ef` answer from
    /// the graph. A seeded build must return a row for its own vector at a
    /// top-1 rate of at least 98% (a floor for regressions, not a target —
    /// `tests/hnsw_recall_measurement.rs` and `measure_build_cost_at_100k`
    /// measure the real rate).
    #[test]
    fn the_graph_path_finds_rows_by_their_own_vector_at_20k_rows() {
        const ROWS: usize = 20_000;
        let rows = Rows::take(0x2000_0000, ROWS, 4);
        let index = HnswIndex::new(config(4)).unwrap();
        index.insert_batch(&rows).unwrap();
        assert!(!index.answers_exactly(index.len()), "20k rows must take the graph path");

        let sample: Vec<&(u64, Vector)> = rows.iter().step_by(40).collect();
        let found = |hits: &[(u64, f32)], id: u64| hits.first().is_some_and(|&(hit, d)| hit == id || d == 0.0);
        let mut graph_hits = 0_usize;
        let mut search_hits = 0_usize;
        for (id, v) in &sample {
            graph_hits += usize::from(found(&index.search_with_ef(v, 1, 200).unwrap(), *id));
            search_hits += usize::from(found(&index.search(v, 1).unwrap(), *id));
        }
        let floor = sample.len() * 98 / 100;
        assert!(
            graph_hits >= floor,
            "graph-only top-1: {graph_hits} of {}",
            sample.len()
        );
        assert!(search_hits >= floor, "search top-1: {search_hits} of {}", sample.len());
    }

    /// The under-return rescue on the graph path: with the exact limit set just
    /// below the index size and all but five rows deleted, the graph's nearest
    /// candidates to a deleted row's vector are tombstones, so the graph alone
    /// returns fewer than k — and `search` still returns exactly the five live
    /// rows, in order.
    #[test]
    fn search_rescues_an_under_full_graph_answer_above_the_exact_limit() {
        let rows = Rows::take(0x5E5C_0E00, 400, 2);
        let mut index = HnswIndex::new(config(2)).unwrap();
        index.exact_max_points = 399;
        index.insert_batch(&rows).unwrap();
        assert!(!index.answers_exactly(index.len()));

        // Keep the five rows farthest from the query; delete the rest.
        let query = vec![0.0, 0.0];
        let mut by_distance: Vec<(u64, f32)> = rows
            .iter()
            .map(|(id, v)| (*id, Distance::<f32>::eval(&DistL2, &query, v)))
            .collect();
        by_distance.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let (deleted, kept) = by_distance.split_at(by_distance.len() - 5);
        for (id, _) in deleted {
            index.delete(*id).unwrap();
        }

        assert!(
            index.search_with_ef(&query, 5, 200).unwrap().len() < 5,
            "the graph's nearest candidates are tombstones"
        );
        assert_eq!(
            index.search(&query, 5).unwrap(),
            kept.to_vec(),
            "the rescue answers exactly"
        );
    }

    /// Review of R4 (Sprinter 66aeb488f312): what ef_construction 100 vs 200
    /// and keep_pruned off vs on cost and buy at scale, to choose the defaults
    /// from numbers. 100k uniform seeded rows x 128 dims, built as one batch
    /// (CREATE INDEX on a populated table, the rebuild at open, a standby's
    /// hydration); graph-only recall at ef 200: top-1 for 1000 rows' own
    /// vectors, top-10 against brute force for 200 fresh query vectors. Run:
    /// `CARGO_TARGET_DIR=target/r4-measure cargo test --release --lib
    /// vector::hnsw_index::tests::measure_build_cost_at_100k -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement (Sprinter 66aeb488f312): minutes in release, prints, asserts nothing"]
    fn measure_build_cost_at_100k() {
        use rayon::prelude::*;
        const N: usize = 100_000;
        const DIM: usize = 128;
        let rows = Rows::take(0x1000_0000, N, DIM);
        let queries: Vec<Vector> = Rows::take(0x2000_0001, 200, DIM).into_iter().map(|(_, v)| v).collect();
        let truth: Vec<Vec<u64>> = queries
            .par_iter()
            .map(|q| {
                let mut all: Vec<(u64, f32)> = rows
                    .iter()
                    .map(|(id, v)| (*id, Distance::<f32>::eval(&DistL2, q, v)))
                    .collect();
                all.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
                all.iter().take(10).map(|(id, _)| *id).collect()
            })
            .collect();
        let pre_r4 = BuildOptions {
            keep_pruned: false,
            seed_empty_batch: false,
            shuffle_batch: false,
        };
        let pruned = |keep_pruned: bool| BuildOptions {
            keep_pruned,
            ..BuildOptions::DEFAULT
        };
        let variants = [
            ("pre-R4 (row order)", 100, pre_r4),
            ("ef 100, keep_pruned off", 100, pruned(false)),
            ("ef 100, keep_pruned on", 100, pruned(true)),
            ("ef 200, keep_pruned off", 200, pruned(false)),
            ("ef 200, keep_pruned on", 200, pruned(true)),
        ];
        let own: Vec<&(u64, Vector)> = rows.iter().step_by(N / 1000).collect();
        println!("\n{N} rows x {DIM} dims, M = 16, batch build; graph-only search at ef = 200");
        for (label, ef_construction, options) in variants {
            let config = HnswConfig {
                dimension: DIM,
                ef_construction,
                ..Default::default()
            };
            let index = HnswIndex::with_build_options(config, options).unwrap();
            let started = std::time::Instant::now();
            index.insert_batch(&rows).unwrap();
            let build = started.elapsed().as_secs_f64();

            let started = std::time::Instant::now();
            let top1 = own
                .iter()
                .filter(|(id, v)| {
                    let hits = index.search_with_ef(v, 1, 200).unwrap();
                    hits.first().is_some_and(|&(hit, _)| hit == *id)
                })
                .count();
            let top10: usize = queries
                .iter()
                .zip(&truth)
                .map(|(q, expected)| {
                    let hits = index.search_with_ef(q, 10, 200).unwrap();
                    hits.iter().filter(|(id, _)| expected.contains(id)).count()
                })
                .sum();
            let per_query_us = started.elapsed().as_secs_f64() * 1e6 / (own.len() + queries.len()) as f64;
            println!(
                "{label:<26} build {build:>8.1}s  top-1 {:>6.2}%  top-10 recall {:>6.2}%  {per_query_us:>7.0} us/query",
                100.0 * top1 as f64 / own.len() as f64,
                100.0 * top10 as f64 / (10 * queries.len()) as f64,
            );
        }
    }

    /// Item 5 measurement (this tree only): graph-only top-1 misses of each
    /// build option against hnsw_rs's defaults, so `BuildOptions::DEFAULT` can
    /// be checked against data. Run:
    /// `cargo test --release --lib vector::hnsw_index::tests::measure_build_options -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement (Sprinter 66aeb488f312): prints miss rates, asserts nothing"]
    fn measure_build_options() {
        const RUNS: u64 = 50;
        const N: usize = 400;
        let variants = [
            (
                "hnsw_rs defaults",
                BuildOptions {
                    keep_pruned: false,
                    seed_empty_batch: false,
                    shuffle_batch: false,
                },
            ),
            (
                "keep_pruned only",
                BuildOptions {
                    keep_pruned: true,
                    seed_empty_batch: false,
                    shuffle_batch: false,
                },
            ),
            (
                "seed + shuffle only",
                BuildOptions {
                    keep_pruned: false,
                    seed_empty_batch: true,
                    shuffle_batch: true,
                },
            ),
            ("BuildOptions::DEFAULT", BuildOptions::DEFAULT),
        ];
        let collinear: Vec<(u64, Vector)> = (0..N)
            .map(|i| {
                let v = i as f32 * 0.25;
                (i as u64 + 1, vec![v, v + 1.0, v * 0.5, 1.0])
            })
            .collect();
        println!(
            "\ngraph-only top-1 misses, {N} rows x 4 dims, M = 16, ef_construction = 200, ef = 200, {RUNS} builds"
        );
        for (label, options) in variants {
            for batch in [false, true] {
                for dataset in ["uniform", "collinear"] {
                    let (mut queries, mut misses) = (0_u64, 0_u64);
                    for run in 0..RUNS {
                        let rows = if dataset == "uniform" {
                            Rows::take(0xB0B0_0000 + run, N, 4)
                        } else {
                            collinear.clone()
                        };
                        let index = HnswIndex::with_build_options(config(4), options).unwrap();
                        if batch {
                            index.insert_batch(&rows).unwrap();
                        } else {
                            for (id, v) in &rows {
                                index.insert(*id, v).unwrap();
                            }
                        }
                        for (id, v) in &rows {
                            queries += 1;
                            let hits = index.search_with_ef(v, 1, 200).unwrap();
                            if hits.first().is_none_or(|&(hit, d)| hit != *id && d > 0.0) {
                                misses += 1;
                            }
                        }
                    }
                    let build = if batch { "batch" } else { "incremental" };
                    println!(
                        "{label:<22} {build:<12} {dataset:<10} misses {misses:>5} / {queries} ({:.4}%)",
                        100.0 * misses as f64 / queries as f64
                    );
                }
            }
        }
    }
}
