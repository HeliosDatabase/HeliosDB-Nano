//! Persistent PQ-HNSW vector index (feature `vector-persist`).
//!
//! See `PROPOSAL_PERSISTENT_PQ_HNSW.md` for the full design. Implemented in phases:
//!
//!   * **P1** — durable substrate: RocksDB key schema (`__vidx:<index_id>:…`),
//!     write-through persistence with atomic `WriteBatch`, crash recovery via
//!     [`PersistentVectorIndex::open`], coarse per-index `RwLock`.
//!   * **P2** — in-house HNSW graph (level assignment, layer search, neighbor
//!     heuristic, hierarchical search), following the published algorithm and the
//!     greedy-descent pattern in this crate's `in_descent` module.
//!   * **P3** — true online deletes with neighbor repair, plus bulk compaction.
//!   * **P4** — Product Quantization unified with the graph: PQ codes resident in
//!     RAM with full vectors on disk, ADC-based traversal, and a two-stage exact
//!     rerank. Enabled per index via [`PersistentVectorIndex::create_with_pq`];
//!     when disabled (the default), the index keeps full `f32` vectors in RAM and
//!     behaves exactly as P1–P3.
//!
//! Grounded in published research — HNSW (Malkov & Yashunin, arXiv:1603.09320) and
//! Product Quantization (Jégou et al., 2011) — and implemented independently against
//! this crate's own primitives (`rocksdb`, `bincode`, the `quantization` module, the
//! SIMD distance kernels). Level assignment uses the public-domain SplitMix64 mixer;
//! no external RNG and no third-party graph code are used. See the proposal's
//! IP-posture section.

#![allow(clippy::similar_names)]

use crate::{Error, Result};
use super::quantization::{Codebook, ProductQuantizer, ProductQuantizerConfig, QuantizedVector};
use super::{DistanceMetric, Vector};
use parking_lot::RwLock;
use rocksdb::{DB, Direction, IteratorMode, WriteBatch};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

/// Internal element identifier within an index.
pub type ElementId = u64;

/// On-disk format version for the persistent index keyspace.
pub const PERSIST_SCHEMA_VERSION: u32 = 1;

/// Rerank-vector storage precision.
///
/// Only [`VectorPrecision::F32`] is wired; `F16` / `I8` are accepted on the config
/// surface but rejected with a clear error until the multi-precision phase lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VectorPrecision {
    /// 32-bit float — exact rerank, no compression.
    F32,
    /// 16-bit float — reserved for a later phase.
    F16,
    /// 8-bit integer — reserved for a later phase.
    I8,
}

/// Configuration for a persistent PQ-HNSW index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PqHnswConfig {
    /// Vector dimension.
    pub dimension: usize,
    /// Distance metric.
    pub distance_metric: DistanceMetric,
    /// Max connections per element on upper layers (`M`).
    pub m: usize,
    /// Max connections per element on layer 0 (`M0`, conventionally `2*M`).
    pub m0: usize,
    /// Dynamic candidate-list size during construction (`ef_construction`).
    pub ef_construction: usize,
    /// Level multiplier used in random level assignment (`mL`).
    pub ml: f64,
    /// Storage precision for the exact-rerank vectors.
    pub rerank_precision: VectorPrecision,
    /// Whether Product Quantization is active (set by `create_with_pq`).
    pub pq_enabled: bool,
    /// Product Quantization parameters; `None` derives a default from the dimension.
    pub pq_config: Option<ProductQuantizerConfig>,
}

impl PqHnswConfig {
    /// Default configuration for a given dimension and metric (`M=16`, `efc=200`).
    #[must_use]
    pub fn new(dimension: usize, distance_metric: DistanceMetric) -> Self {
        let m = 16;
        Self {
            dimension,
            distance_metric,
            m,
            m0: m * 2,
            ef_construction: 200,
            ml: 1.0 / (m as f64).ln(),
            rerank_precision: VectorPrecision::F32,
            pq_enabled: false,
            pq_config: None,
        }
    }
}

/// Metadata persisted under the `…:meta` key — the part of the index state that is not
/// per-element. Loaded first on `open` so recovery can restore the entry point and
/// counters before faulting in element data.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedMeta {
    schema_version: u32,
    config: PqHnswConfig,
    entry_point: Option<ElementId>,
    next_element_id: ElementId,
    layer_count: usize,
    element_count: u64,
}

/// In-memory mirror of the index, guarded by the coarse per-index lock.
///
/// When PQ is active, `codes` is the resident representation and `vectors` is empty
/// (full vectors live on disk for rerank). When PQ is inactive, `vectors` holds the
/// full `f32` vectors and `codes` is empty.
struct IndexState {
    config: PqHnswConfig,
    entry_point: Option<ElementId>,
    next_element_id: ElementId,
    layer_count: usize,
    element_count: u64,
    vectors: HashMap<ElementId, Vector>,
    codes: HashMap<ElementId, QuantizedVector>,
    pq: Option<Arc<ProductQuantizer>>,
    levels: HashMap<ElementId, u32>,
    adjacency: HashMap<(u32, ElementId), Vec<ElementId>>,
    elem_to_row: HashMap<ElementId, u64>,
    row_to_elem: HashMap<u64, ElementId>,
    tombstones: HashSet<ElementId>,
}

impl IndexState {
    fn new(config: PqHnswConfig) -> Self {
        Self {
            config,
            entry_point: None,
            next_element_id: 0,
            layer_count: 0,
            element_count: 0,
            vectors: HashMap::new(),
            codes: HashMap::new(),
            pq: None,
            levels: HashMap::new(),
            adjacency: HashMap::new(),
            elem_to_row: HashMap::new(),
            row_to_elem: HashMap::new(),
            tombstones: HashSet::new(),
        }
    }

    fn meta_bytes(&self) -> Result<Vec<u8>> {
        let meta = PersistedMeta {
            schema_version: PERSIST_SCHEMA_VERSION,
            config: self.config.clone(),
            entry_point: self.entry_point,
            next_element_id: self.next_element_id,
            layer_count: self.layer_count,
            element_count: self.element_count,
        };
        ser(&meta)
    }

    /// Resolve an element's vector: cloned from RAM (PQ inactive) or decoded from its
    /// PQ code (PQ active). Used by the build heuristic and repair.
    fn element_vector(&self, id: ElementId) -> Option<Vec<f32>> {
        if let Some(v) = self.vectors.get(&id) {
            return Some(v.clone());
        }
        if let (Some(pq), Some(code)) = (self.pq.as_ref(), self.codes.get(&id)) {
            return pq.decode(code).ok();
        }
        None
    }

    /// Build a query probe: an ADC distance table when PQ is active, otherwise an
    /// exact handle on the query vector.
    fn make_probe<'a>(&self, q: &'a [f32]) -> Probe<'a> {
        if let Some(pq) = self.pq.as_ref() {
            if let Ok(table) = pq.precompute_distance_table(&q.to_vec()) {
                return Probe::Adc(table);
            }
        }
        Probe::Exact(q)
    }

    /// Whether an element may be admitted to the result set: not tombstoned, and (if a
    /// filter is supplied) its row passes the predicate.
    fn admits(&self, id: ElementId, filter: Option<&dyn Fn(u64) -> bool>) -> bool {
        if self.tombstones.contains(&id) {
            return false;
        }
        match filter {
            None => true,
            Some(f) => self.elem_to_row.get(&id).is_some_and(|&r| f(r)),
        }
    }

    /// HNSW layer search (Algorithm 2): best-first exploration of `layer` from
    /// `entry`, returning up to `ef` closest *admitted* elements sorted ascending by
    /// distance. Distances are ADC (PQ active) or exact (PQ inactive).
    ///
    /// Non-admitted elements (tombstoned, or failing `filter`) are still **traversed**
    /// — pushed to the frontier so the graph stays connected — but never collected.
    /// This is predicate-during-traversal: it preserves top-k quality under a filter
    /// where naive post-filtering of an unfiltered top-k would return too few results.
    fn search_layer(
        &self,
        q: &[f32],
        entry: &[ElementId],
        ef: usize,
        layer: u32,
        filter: Option<&dyn Fn(u64) -> bool>,
    ) -> Vec<Cand> {
        let probe = self.make_probe(q);
        let mut visited: HashSet<ElementId> = HashSet::new();
        let mut frontier: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
        let mut best: BinaryHeap<Cand> = BinaryHeap::new();
        for &e in entry {
            if !visited.insert(e) {
                continue;
            }
            let d = probe.dist(self, e);
            frontier.push(Reverse(Cand { dist: d, id: e }));
            if self.admits(e, filter) {
                best.push(Cand { dist: d, id: e });
                if best.len() > ef {
                    best.pop();
                }
            }
        }
        while let Some(Reverse(c)) = frontier.pop() {
            if let Some(worst) = best.peek() {
                if c.dist > worst.dist && best.len() >= ef {
                    break;
                }
            }
            let Some(neighbors) = self.adjacency.get(&(layer, c.id)) else {
                continue;
            };
            for &n in neighbors {
                if !visited.insert(n) {
                    continue;
                }
                let d = probe.dist(self, n);
                let worst = best.peek().map_or(f32::INFINITY, |w| w.dist);
                if best.len() < ef || d < worst {
                    frontier.push(Reverse(Cand { dist: d, id: n }));
                    if self.admits(n, filter) {
                        best.push(Cand { dist: d, id: n });
                        if best.len() > ef {
                            best.pop();
                        }
                    }
                }
            }
        }
        best.into_sorted_vec()
    }

    /// Neighbor-selection heuristic (Algorithm 4, base variant): keep a diverse set —
    /// an element is kept only if it is closer to `base_vec` than to every
    /// already-selected neighbor. Candidate vectors are resolved via `element_vector`
    /// (decoded when PQ is active), so the diversity test is consistent within itself.
    fn select_neighbors(&self, base_vec: &[f32], candidate_ids: &[ElementId], m: usize) -> Vec<ElementId> {
        let metric = self.config.distance_metric;
        let mut cands: Vec<(f32, ElementId, Vec<f32>)> = Vec::with_capacity(candidate_ids.len());
        for &id in candidate_ids {
            if let Some(v) = self.element_vector(id) {
                let d = metric_dist(metric, base_vec, &v);
                cands.push((d, id, v));
            }
        }
        cands.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

        let mut result: Vec<(ElementId, Vec<f32>)> = Vec::with_capacity(m);
        for (d, id, v) in cands {
            if result.len() >= m {
                break;
            }
            let keep = result.iter().all(|(_, rv)| metric_dist(metric, &v, rv) >= d);
            if keep {
                result.push((id, v));
            }
        }
        result.into_iter().map(|(id, _)| id).collect()
    }
}

// ── Key schema ──────────────────────────────────────────────────────────────
// All keys live under `__vidx:<index_id>:` so an index's keyspace is a contiguous,
// independently-droppable range within the shared RocksDB instance.

fn prefix(index_id: u64) -> String {
    format!("__vidx:{index_id}:")
}
fn key_meta(p: &str) -> Vec<u8> {
    format!("{p}meta").into_bytes()
}
fn key_vec(p: &str, e: ElementId) -> Vec<u8> {
    format!("{p}vec:{e}").into_bytes()
}
fn key_code(p: &str, e: ElementId) -> Vec<u8> {
    format!("{p}code:{e}").into_bytes()
}
fn key_pq(p: &str) -> Vec<u8> {
    format!("{p}pq").into_bytes()
}
fn key_lvl(p: &str, e: ElementId) -> Vec<u8> {
    format!("{p}lvl:{e}").into_bytes()
}
fn key_adj(p: &str, layer: u32, e: ElementId) -> Vec<u8> {
    format!("{p}adj:{layer}:{e}").into_bytes()
}
fn key_map(p: &str, e: ElementId) -> Vec<u8> {
    format!("{p}map:{e}").into_bytes()
}
fn key_rmap(p: &str, row: u64) -> Vec<u8> {
    format!("{p}rmap:{row}").into_bytes()
}
fn key_tomb(p: &str) -> Vec<u8> {
    format!("{p}tomb").into_bytes()
}

// ── Encoding helpers ─────────────────────────────────────────────────────────

fn ser<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    bincode::serialize(v).map_err(|e| Error::storage(format!("vector-persist: serialize: {e}")))
}
fn de<T: DeserializeOwned>(b: &[u8]) -> Result<T> {
    bincode::deserialize(b).map_err(|e| Error::storage(format!("vector-persist: deserialize: {e}")))
}
fn map_db(e: rocksdb::Error) -> Error {
    Error::storage(format!("vector-persist: rocksdb: {e}"))
}
fn parse_id(s: Option<&str>) -> Result<u64> {
    s.and_then(|x| x.parse().ok())
        .ok_or_else(|| Error::storage("vector-persist: malformed key id"))
}

/// Encode a rerank vector for storage at the configured precision.
fn encode_vector(cfg: &PqHnswConfig, v: &Vector) -> Result<Vec<u8>> {
    match cfg.rerank_precision {
        VectorPrecision::F32 => ser(v),
        other => Err(Error::storage(format!(
            "vector-persist: rerank precision {other:?} is implemented in a later phase"
        ))),
    }
}
/// Decode a rerank vector stored at the configured precision.
fn decode_vector(cfg: &PqHnswConfig, b: &[u8]) -> Result<Vector> {
    match cfg.rerank_precision {
        VectorPrecision::F32 => de::<Vector>(b),
        other => Err(Error::storage(format!(
            "vector-persist: rerank precision {other:?} is implemented in a later phase"
        ))),
    }
}

/// Distance between two raw vectors under a metric (SIMD-backed kernels).
fn metric_dist(metric: DistanceMetric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        DistanceMetric::L2 => super::l2_distance(a, b),
        DistanceMetric::Cosine => super::cosine_distance(a, b),
        DistanceMetric::InnerProduct => super::inner_product_distance(a, b),
    }
}

/// A query distance probe — exact (to a raw query vector) or ADC (precomputed PQ
/// distance table).
enum Probe<'a> {
    Exact(&'a [f32]),
    Adc(Vec<Vec<f32>>),
}
impl Probe<'_> {
    fn dist(&self, st: &IndexState, id: ElementId) -> f32 {
        match self {
            Probe::Exact(q) => match st.vectors.get(&id) {
                Some(v) => metric_dist(st.config.distance_metric, q, v),
                None => f32::INFINITY,
            },
            Probe::Adc(table) => match (st.pq.as_ref(), st.codes.get(&id)) {
                (Some(pq), Some(code)) => {
                    pq.compute_distance_with_table(table, code).unwrap_or(f32::INFINITY)
                }
                _ => f32::INFINITY,
            },
        }
    }
}

// ── In-house HNSW graph ──────────────────────────────────────────────────────

/// Defensive cap on the (geometric) level distribution.
const MAX_LEVEL: u32 = 31;

/// A `(distance, element)` pair ordered ascending by distance (via `total_cmp`),
/// tie-broken by id. Used in the search heaps.
#[derive(Copy, Clone, Debug)]
struct Cand {
    dist: f32,
    id: ElementId,
}
impl PartialEq for Cand {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.dist.to_bits() == other.dist.to_bits()
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cand {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist.total_cmp(&other.dist).then(self.id.cmp(&other.id))
    }
}

/// SplitMix64 (public domain, Sebastiano Vigna) → uniform f64 in `[0, 1)`.
fn unit_f64_from(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    ((z >> 11) as f64) / ((1u64 << 53) as f64)
}

/// Assign a level with the standard `floor(-ln(U) * mL)` rule, seeded deterministically
/// from the element id so builds are reproducible.
fn random_level(seed: u64, ml: f64) -> u32 {
    let u = unit_f64_from(seed).max(f64::MIN_POSITIVE);
    let lvl = (-u.ln() * ml).floor();
    if lvl <= 0.0 {
        0
    } else {
        (lvl as u32).min(MAX_LEVEL)
    }
}

/// In-memory HNSW insert shared by `insert` (write-through) and `compact` (bulk
/// rebuild). Mutates `st` only — performs no persistence — and returns the new element
/// id plus the adjacency keys it touched. When PQ is active, the element is stored as a
/// code; otherwise as a full vector. Caller validates the dimension and persists.
fn graph_insert(
    st: &mut IndexState,
    row_id: u64,
    vector: &Vector,
) -> Result<(ElementId, Vec<(u32, ElementId)>)> {
    let elem = st.next_element_id;
    let level = random_level(elem, st.config.ml);
    let m = st.config.m;
    let m0 = st.config.m0;
    let efc = st.config.ef_construction;

    // Store the resident representation.
    let pq = st.pq.clone();
    if let Some(pq) = pq.as_ref() {
        let code = pq
            .encode(vector)
            .map_err(|e| Error::storage(format!("vector-persist: pq encode: {e}")))?;
        st.codes.insert(elem, code);
    } else {
        st.vectors.insert(elem, vector.clone());
    }
    st.elem_to_row.insert(elem, row_id);
    st.row_to_elem.insert(row_id, elem);
    st.levels.insert(elem, level);
    st.element_count += 1;
    st.next_element_id = elem + 1;

    let mut touched: Vec<(u32, ElementId)> = Vec::new();

    match st.entry_point {
        None => {
            st.entry_point = Some(elem);
            if (level as usize) + 1 > st.layer_count {
                st.layer_count = level as usize + 1;
            }
        }
        Some(ep0) => {
            let top = (st.layer_count.saturating_sub(1)) as u32;
            let mut ep = ep0;
            let mut ep_dist = st.make_probe(vector).dist(st, ep);

            // Greedy descent through the layers above the new element's level.
            let mut layer = top;
            while layer > level {
                let res = st.search_layer(vector, &[ep], 1, layer, None);
                if let Some(c) = res.first() {
                    if c.dist < ep_dist {
                        ep = c.id;
                        ep_dist = c.dist;
                    }
                }
                layer -= 1;
            }

            // Connect from min(level, top) down to layer 0.
            let start = level.min(top);
            let mut entry = ep;
            let mut l = start as i64;
            while l >= 0 {
                let layer_u = l as u32;
                let m_l = if layer_u == 0 { m0 } else { m };
                let candidates = st.search_layer(vector, &[entry], efc, layer_u, None);
                let cand_ids: Vec<ElementId> = candidates.iter().map(|c| c.id).collect();
                let selected = st.select_neighbors(vector, &cand_ids, m_l);

                st.adjacency.insert((layer_u, elem), selected.clone());
                touched.push((layer_u, elem));

                for &nb in &selected {
                    let mut nb_list =
                        st.adjacency.get(&(layer_u, nb)).cloned().unwrap_or_default();
                    if !nb_list.contains(&elem) {
                        nb_list.push(elem);
                    }
                    let cap = if layer_u == 0 { m0 } else { m };
                    if nb_list.len() > cap {
                        if let Some(nb_vec) = st.element_vector(nb) {
                            nb_list = st.select_neighbors(&nb_vec, &nb_list, cap);
                        } else {
                            nb_list.truncate(cap);
                        }
                    }
                    st.adjacency.insert((layer_u, nb), nb_list);
                    touched.push((layer_u, nb));
                }

                if let Some(c) = candidates.first() {
                    entry = c.id;
                }
                l -= 1;
            }

            if (level as usize) + 1 > st.layer_count {
                st.layer_count = level as usize + 1;
            }
            if level > top {
                st.entry_point = Some(elem);
            }
        }
    }

    Ok((elem, touched))
}

/// A durable, crash-recoverable vector index backed by RocksDB.
pub struct PersistentVectorIndex {
    db: Arc<DB>,
    index_id: u64,
    state: Arc<RwLock<IndexState>>,
}

impl PersistentVectorIndex {
    /// Create a new, empty index (PQ inactive — full vectors resident in RAM).
    pub fn create(db: Arc<DB>, index_id: u64, config: PqHnswConfig) -> Result<Self> {
        if config.rerank_precision != VectorPrecision::F32 {
            return Err(Error::storage(format!(
                "vector-persist: rerank precision {:?} is implemented in a later phase",
                config.rerank_precision
            )));
        }
        let p = prefix(index_id);
        if db.get(key_meta(&p)).map_err(map_db)?.is_some() {
            return Err(Error::storage(format!(
                "vector-persist: index {index_id} already exists"
            )));
        }
        let state = IndexState::new(config);
        db.put(key_meta(&p), state.meta_bytes()?).map_err(map_db)?;
        Ok(Self {
            db,
            index_id,
            state: Arc::new(RwLock::new(state)),
        })
    }

    /// Create a PQ-backed index: train a Product Quantizer on `training` vectors,
    /// persist the codebook, and store PQ codes in RAM with full vectors on disk
    /// (faulted in only for the two-stage rerank). PQ is L2-only.
    pub fn create_with_pq(
        db: Arc<DB>,
        index_id: u64,
        mut config: PqHnswConfig,
        training: &[Vector],
    ) -> Result<Self> {
        if config.rerank_precision != VectorPrecision::F32 {
            return Err(Error::storage("vector-persist: PQ requires F32 rerank precision"));
        }
        if config.distance_metric != DistanceMetric::L2 {
            return Err(Error::storage("vector-persist: PQ is supported only with the L2 metric"));
        }
        if training.is_empty() {
            return Err(Error::storage("vector-persist: PQ training set is empty"));
        }
        let p = prefix(index_id);
        if db.get(key_meta(&p)).map_err(map_db)?.is_some() {
            return Err(Error::storage(format!(
                "vector-persist: index {index_id} already exists"
            )));
        }

        let mut pq_cfg = match config.pq_config.clone() {
            Some(c) => c,
            None => ProductQuantizerConfig::default_for_dimension(config.dimension)
                .map_err(|e| Error::storage(format!("vector-persist: pq config: {e}")))?,
        };
        // Never reject on sample count — clamp the requirement to what we were given.
        pq_cfg.min_training_samples = pq_cfg.min_training_samples.min(training.len().max(1));
        let pq = ProductQuantizer::train(pq_cfg.clone(), training)
            .map_err(|e| Error::storage(format!("vector-persist: pq train: {e}")))?;

        config.pq_enabled = true;
        config.pq_config = Some(pq_cfg);
        let mut state = IndexState::new(config);
        state.pq = Some(Arc::new(pq));

        let mut wb = WriteBatch::default();
        wb.put(key_meta(&p), state.meta_bytes()?);
        if let Some(pq) = state.pq.as_ref() {
            wb.put(key_pq(&p), ser(&*pq.codebook())?);
        }
        db.write(wb).map_err(map_db)?;

        Ok(Self {
            db,
            index_id,
            state: Arc::new(RwLock::new(state)),
        })
    }

    /// Open (recover) an existing index from RocksDB. Reconstructs the PQ codebook if
    /// the index was created with PQ. Errors if it does not exist or the on-disk schema
    /// version is unknown.
    pub fn open(db: Arc<DB>, index_id: u64) -> Result<Self> {
        let p = prefix(index_id);
        let meta_bytes = db
            .get(key_meta(&p))
            .map_err(map_db)?
            .ok_or_else(|| Error::storage(format!("vector-persist: index {index_id} not found")))?;
        let meta: PersistedMeta = de(&meta_bytes)?;
        if meta.schema_version != PERSIST_SCHEMA_VERSION {
            return Err(Error::storage(format!(
                "vector-persist: on-disk schema version {} unsupported (expected {})",
                meta.schema_version, PERSIST_SCHEMA_VERSION
            )));
        }

        let mut st = IndexState::new(meta.config.clone());
        st.entry_point = meta.entry_point;
        st.next_element_id = meta.next_element_id;
        st.layer_count = meta.layer_count;
        st.element_count = meta.element_count;

        // Reconstruct the quantizer if a codebook was persisted.
        let pq_active = if let Some(cb_bytes) = db.get(key_pq(&p)).map_err(map_db)? {
            let codebook: Codebook = de(&cb_bytes)?;
            let pq_cfg = match meta.config.pq_config.clone() {
                Some(c) => c,
                None => ProductQuantizerConfig::default_for_dimension(meta.config.dimension)
                    .map_err(|e| Error::storage(format!("vector-persist: pq config: {e}")))?,
            };
            let pq = ProductQuantizer::new(pq_cfg, codebook)
                .map_err(|e| Error::storage(format!("vector-persist: pq reconstruct: {e}")))?;
            st.pq = Some(Arc::new(pq));
            true
        } else {
            false
        };

        // Scan the contiguous keyspace for this index and rebuild the in-memory mirror.
        let pb = p.as_bytes();
        let iter = db.iterator(IteratorMode::From(pb, Direction::Forward));
        for item in iter {
            let (k, v) = item.map_err(map_db)?;
            if !k.starts_with(pb) {
                break;
            }
            let suffix = std::str::from_utf8(&k[pb.len()..])
                .map_err(|e| Error::storage(format!("vector-persist: non-utf8 key: {e}")))?;
            let mut parts = suffix.split(':');
            match parts.next() {
                Some("meta") | Some("pq") => {}
                Some("tomb") => {
                    let set: Vec<ElementId> = de(&v)?;
                    st.tombstones = set.into_iter().collect();
                }
                Some("vec") => {
                    // Full vectors stay on disk when PQ is active (rerank only).
                    if !pq_active {
                        let e = parse_id(parts.next())?;
                        st.vectors.insert(e, decode_vector(&meta.config, &v)?);
                    }
                }
                Some("code") => {
                    let e = parse_id(parts.next())?;
                    st.codes.insert(e, de::<QuantizedVector>(&v)?);
                }
                Some("lvl") => {
                    let e = parse_id(parts.next())?;
                    st.levels.insert(e, de::<u32>(&v)?);
                }
                Some("adj") => {
                    let layer = parse_id(parts.next())? as u32;
                    let e = parse_id(parts.next())?;
                    st.adjacency.insert((layer, e), de::<Vec<ElementId>>(&v)?);
                }
                Some("map") => {
                    let e = parse_id(parts.next())?;
                    st.elem_to_row.insert(e, de::<u64>(&v)?);
                }
                Some("rmap") => {
                    let row = parse_id(parts.next())?;
                    st.row_to_elem.insert(row, de::<ElementId>(&v)?);
                }
                _ => {}
            }
        }

        Ok(Self {
            db,
            index_id,
            state: Arc::new(RwLock::new(st)),
        })
    }

    /// Whether an index with the given id exists in the store.
    pub fn exists(db: &DB, index_id: u64) -> Result<bool> {
        Ok(db.get(key_meta(&prefix(index_id))).map_err(map_db)?.is_some())
    }

    /// Delete every key belonging to an index.
    pub fn drop_index(db: &DB, index_id: u64) -> Result<()> {
        let p = prefix(index_id);
        let pb = p.as_bytes();
        let mut wb = WriteBatch::default();
        let iter = db.iterator(IteratorMode::From(pb, Direction::Forward));
        for item in iter {
            let (k, _) = item.map_err(map_db)?;
            if !k.starts_with(pb) {
                break;
            }
            wb.delete(k);
        }
        db.write(wb).map_err(map_db)?;
        Ok(())
    }

    // ── Low-level write-through mutators (substrate; used directly in P1 tests) ──

    /// Store an element's rerank vector, row mapping, and level (no graph edges).
    pub fn put_vector(
        &self,
        elem_id: ElementId,
        row_id: u64,
        vector: &Vector,
        level: u32,
    ) -> Result<()> {
        let mut st = self.state.write();
        if vector.len() != st.config.dimension {
            return Err(Error::query_execution(format!(
                "vector dimension mismatch: expected {}, got {}",
                st.config.dimension,
                vector.len()
            )));
        }
        let p = prefix(self.index_id);
        let is_new = !st.vectors.contains_key(&elem_id);

        let mut wb = WriteBatch::default();
        wb.put(key_vec(&p, elem_id), encode_vector(&st.config, vector)?);
        wb.put(key_map(&p, elem_id), ser(&row_id)?);
        wb.put(key_rmap(&p, row_id), ser(&elem_id)?);
        wb.put(key_lvl(&p, elem_id), ser(&level)?);

        st.vectors.insert(elem_id, vector.clone());
        st.elem_to_row.insert(elem_id, row_id);
        st.row_to_elem.insert(row_id, elem_id);
        st.levels.insert(elem_id, level);
        if is_new {
            st.element_count += 1;
        }
        st.next_element_id = st.next_element_id.max(elem_id + 1);
        if (level as usize) + 1 > st.layer_count {
            st.layer_count = level as usize + 1;
        }

        wb.put(key_meta(&p), st.meta_bytes()?);
        self.db.write(wb).map_err(map_db)?;
        Ok(())
    }

    /// Store an element's neighbor list at a given layer.
    pub fn put_adjacency(
        &self,
        layer: u32,
        elem_id: ElementId,
        neighbors: Vec<ElementId>,
    ) -> Result<()> {
        let mut st = self.state.write();
        let p = prefix(self.index_id);
        let mut wb = WriteBatch::default();
        wb.put(key_adj(&p, layer, elem_id), ser(&neighbors)?);
        st.adjacency.insert((layer, elem_id), neighbors);
        if (layer as usize) + 1 > st.layer_count {
            st.layer_count = layer as usize + 1;
            wb.put(key_meta(&p), st.meta_bytes()?);
        }
        self.db.write(wb).map_err(map_db)?;
        Ok(())
    }

    /// Set (or clear) the graph entry point.
    pub fn set_entry_point(&self, ep: Option<ElementId>) -> Result<()> {
        let mut st = self.state.write();
        st.entry_point = ep;
        self.db
            .put(key_meta(&prefix(self.index_id)), st.meta_bytes()?)
            .map_err(map_db)?;
        Ok(())
    }

    /// Mark an element as soft-deleted (excluded from search; reclaimed by `compact`).
    pub fn mark_tombstone(&self, elem_id: ElementId) -> Result<()> {
        let mut st = self.state.write();
        st.tombstones.insert(elem_id);
        let set: Vec<ElementId> = st.tombstones.iter().copied().collect();
        self.db
            .put(key_tomb(&prefix(self.index_id)), ser(&set)?)
            .map_err(map_db)?;
        Ok(())
    }

    // ── Graph build / search / delete ────────────────────────────────────────

    /// Insert a vector into the graph. Persists the full vector (for rerank) plus, when
    /// PQ is active, its code; the new element's keys + every touched adjacency list +
    /// metadata are flushed atomically in one batch. Returns the assigned element id.
    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<ElementId> {
        let mut st = self.state.write();
        if vector.len() != st.config.dimension {
            return Err(Error::query_execution(format!(
                "vector dimension mismatch: expected {}, got {}",
                st.config.dimension,
                vector.len()
            )));
        }
        let (elem, mut touched) = graph_insert(&mut st, row_id, vector)?;

        let p = prefix(self.index_id);
        let level = st.levels.get(&elem).copied().unwrap_or(0);
        let mut wb = WriteBatch::default();
        wb.put(key_vec(&p, elem), encode_vector(&st.config, vector)?);
        wb.put(key_map(&p, elem), ser(&row_id)?);
        wb.put(key_rmap(&p, row_id), ser(&elem)?);
        wb.put(key_lvl(&p, elem), ser(&level)?);
        if let Some(code) = st.codes.get(&elem) {
            wb.put(key_code(&p, elem), ser(code)?);
        }
        touched.sort_unstable();
        touched.dedup();
        for (layer_u, e) in touched {
            let nbrs = st.adjacency.get(&(layer_u, e)).cloned().unwrap_or_default();
            wb.put(key_adj(&p, layer_u, e), ser(&nbrs)?);
        }
        wb.put(key_meta(&p), st.meta_bytes()?);
        self.db.write(wb).map_err(map_db)?;
        Ok(elem)
    }

    /// Search for the `k` nearest neighbors of `query` with candidate-list size `ef`
    /// (clamped up to at least `k`). When PQ is active, traversal uses ADC and the top
    /// candidates are re-ranked with exact distances from the on-disk full vectors.
    /// Returns `(row_id, distance)` sorted ascending; tombstoned elements are excluded.
    pub fn search(&self, query: &Vector, k: usize, ef: usize) -> Result<Vec<(u64, f32)>> {
        self.search_inner(query, k, ef, None)
    }

    /// Like [`search`](Self::search), but only returns neighbors whose row id passes
    /// `filter`. The predicate is applied *during* graph traversal (not after), so a
    /// full `k` of matching neighbors is returned even for selective filters — where
    /// post-filtering an unfiltered top-k would fall short. Compose with a widened `ef`
    /// for very selective predicates.
    pub fn search_filtered(
        &self,
        query: &Vector,
        k: usize,
        ef: usize,
        filter: impl Fn(u64) -> bool,
    ) -> Result<Vec<(u64, f32)>> {
        let f: &dyn Fn(u64) -> bool = &filter;
        self.search_inner(query, k, ef, Some(f))
    }

    fn search_inner(
        &self,
        query: &Vector,
        k: usize,
        ef: usize,
        filter: Option<&dyn Fn(u64) -> bool>,
    ) -> Result<Vec<(u64, f32)>> {
        let st = self.state.read();
        if query.len() != st.config.dimension {
            return Err(Error::query_execution(format!(
                "query dimension mismatch: expected {}, got {}",
                st.config.dimension,
                query.len()
            )));
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let Some(ep0) = st.entry_point else {
            return Ok(Vec::new());
        };

        // Navigate the upper layers unfiltered (greedy descent), then collect (with the
        // filter applied) at layer 0.
        let top = (st.layer_count.saturating_sub(1)) as u32;
        let mut ep = ep0;
        let mut ep_dist = st.make_probe(query).dist(&st, ep);
        let mut layer = top;
        while layer > 0 {
            let res = st.search_layer(query, &[ep], 1, layer, None);
            if let Some(c) = res.first() {
                if c.dist < ep_dist {
                    ep = c.id;
                    ep_dist = c.dist;
                }
            }
            layer -= 1;
        }

        let ef_eff = ef.max(k);
        let found = st.search_layer(query, &[ep], ef_eff, 0, filter);

        if st.pq.is_some() {
            // Two-stage exact rerank of the (already filtered) candidate set, scored
            // against the on-disk full vectors.
            let p = prefix(self.index_id);
            let metric = st.config.distance_metric;
            let mut reranked: Vec<(u64, f32)> = Vec::with_capacity(found.len());
            for c in &found {
                let Some(&row) = st.elem_to_row.get(&c.id) else {
                    continue;
                };
                let dist = match self.db.get(key_vec(&p, c.id)).map_err(map_db)? {
                    Some(bytes) => metric_dist(metric, query, &decode_vector(&st.config, &bytes)?),
                    None => c.dist,
                };
                reranked.push((row, dist));
            }
            reranked.sort_by(|a, b| a.1.total_cmp(&b.1));
            reranked.truncate(k);
            Ok(reranked)
        } else {
            let out: Vec<(u64, f32)> = found
                .into_iter()
                .filter_map(|c| st.elem_to_row.get(&c.id).map(|&r| (r, c.dist)))
                .take(k)
                .collect();
            Ok(out)
        }
    }

    /// Remove a row from the graph: delete the element and repair every node that
    /// referenced it by re-selecting connections from the candidate pool left by the
    /// hole. Promotes the entry point if it was removed. Persisted atomically. Returns
    /// whether the row existed. No stale edges remain — recall stays stable under churn.
    pub fn remove(&self, row_id: u64) -> Result<bool> {
        let mut st = self.state.write();
        let Some(elem) = st.row_to_elem.get(&row_id).copied() else {
            return Ok(false);
        };
        let level = st.levels.get(&elem).copied().unwrap_or(0);
        let p = prefix(self.index_id);
        let mut wb = WriteBatch::default();
        let mut touched: Vec<(u32, ElementId)> = Vec::new();

        for layer in 0..=level {
            let x_nbrs: Vec<ElementId> =
                st.adjacency.get(&(layer, elem)).cloned().unwrap_or_default();
            let cap = if layer == 0 { st.config.m0 } else { st.config.m };

            let referrers: Vec<ElementId> = st
                .adjacency
                .iter()
                .filter(|(k, v)| k.0 == layer && v.contains(&elem))
                .map(|(k, _)| k.1)
                .collect();

            for nb in referrers {
                if nb == elem {
                    continue;
                }
                let Some(nb_vec) = st.element_vector(nb) else {
                    continue;
                };
                let mut pool: HashSet<ElementId> = HashSet::new();
                for &c in &x_nbrs {
                    if c != elem && c != nb {
                        pool.insert(c);
                    }
                }
                if let Some(cur) = st.adjacency.get(&(layer, nb)) {
                    for &c in cur {
                        if c != elem && c != nb {
                            pool.insert(c);
                        }
                    }
                }
                let pool_ids: Vec<ElementId> = pool
                    .into_iter()
                    .filter(|c| !st.tombstones.contains(c))
                    .collect();
                let new_list = st.select_neighbors(&nb_vec, &pool_ids, cap);
                st.adjacency.insert((layer, nb), new_list);
                touched.push((layer, nb));
            }

            st.adjacency.remove(&(layer, elem));
            wb.delete(key_adj(&p, layer, elem));
        }

        // Drop the element's own data (both resident forms + on-disk keys).
        st.vectors.remove(&elem);
        st.codes.remove(&elem);
        st.levels.remove(&elem);
        st.elem_to_row.remove(&elem);
        st.row_to_elem.remove(&row_id);
        let was_tomb = st.tombstones.remove(&elem);
        st.element_count = st.element_count.saturating_sub(1);
        wb.delete(key_vec(&p, elem));
        wb.delete(key_code(&p, elem));
        wb.delete(key_lvl(&p, elem));
        wb.delete(key_map(&p, elem));
        wb.delete(key_rmap(&p, row_id));

        if st.entry_point == Some(elem) {
            st.entry_point = st
                .levels
                .iter()
                .filter(|(id, _)| !st.tombstones.contains(*id))
                .max_by_key(|(_, lvl)| **lvl)
                .map(|(id, _)| *id);
        }
        st.layer_count = st
            .levels
            .iter()
            .filter(|(id, _)| !st.tombstones.contains(*id))
            .map(|(_, lvl)| *lvl as usize + 1)
            .max()
            .unwrap_or(0);

        touched.sort_unstable();
        touched.dedup();
        for (layer, e) in touched {
            if let Some(nbrs) = st.adjacency.get(&(layer, e)) {
                wb.put(key_adj(&p, layer, e), ser(nbrs)?);
            }
        }
        if was_tomb {
            let set: Vec<ElementId> = st.tombstones.iter().copied().collect();
            wb.put(key_tomb(&p), ser(&set)?);
        }
        wb.put(key_meta(&p), st.meta_bytes()?);
        self.db.write(wb).map_err(map_db)?;
        Ok(true)
    }

    /// Rebuild the graph from scratch over the surviving (non-tombstoned) elements,
    /// reclaiming space and clearing tombstones; persisted atomically. Preserves the PQ
    /// codebook (when active), reloading full vectors from disk to re-encode.
    pub fn compact(&self) -> Result<()> {
        let mut st = self.state.write();
        let p = prefix(self.index_id);

        // Gather surviving (row, full-vector) pairs.
        let elem_ids: Vec<ElementId> = if st.pq.is_some() {
            st.codes.keys().copied().collect()
        } else {
            st.vectors.keys().copied().collect()
        };
        let mut survivors: Vec<(u64, Vector)> = Vec::with_capacity(elem_ids.len());
        for elem in elem_ids {
            if st.tombstones.contains(&elem) {
                continue;
            }
            let Some(&row) = st.elem_to_row.get(&elem) else {
                continue;
            };
            let full = if let Some(v) = st.vectors.get(&elem) {
                v.clone()
            } else {
                match self.db.get(key_vec(&p, elem)).map_err(map_db)? {
                    Some(bytes) => decode_vector(&st.config, &bytes)?,
                    None => continue,
                }
            };
            survivors.push((row, full));
        }
        survivors.sort_by_key(|(row, _)| *row);

        // Reset the graph, preserving config + PQ codebook, then re-insert survivors.
        let config = st.config.clone();
        let pq = st.pq.clone();
        let mut fresh = IndexState::new(config);
        fresh.pq = pq;
        *st = fresh;
        for (row, v) in &survivors {
            let _ = graph_insert(&mut st, *row, v)?;
        }

        // Full rewrite of the keyspace from the rebuilt state + the survivor vectors.
        let mut wb = WriteBatch::default();
        let pb = p.as_bytes();
        let iter = self.db.iterator(IteratorMode::From(pb, Direction::Forward));
        for item in iter {
            let (k, _) = item.map_err(map_db)?;
            if !k.starts_with(pb) {
                break;
            }
            wb.delete(k);
        }
        wb.put(key_meta(&p), st.meta_bytes()?);
        if let Some(pq) = st.pq.as_ref() {
            wb.put(key_pq(&p), ser(&*pq.codebook())?);
        }
        for (&elem, &row) in &st.elem_to_row {
            wb.put(key_map(&p, elem), ser(&row)?);
            wb.put(key_rmap(&p, row), ser(&elem)?);
            if let Some(&lvl) = st.levels.get(&elem) {
                wb.put(key_lvl(&p, elem), ser(&lvl)?);
            }
            if let Some(code) = st.codes.get(&elem) {
                wb.put(key_code(&p, elem), ser(code)?);
            }
            if let Some(v) = st.vectors.get(&elem) {
                wb.put(key_vec(&p, elem), encode_vector(&st.config, v)?);
            }
        }
        if st.pq.is_some() {
            // PQ mode keeps full vectors only on disk — write them from the survivors.
            for (row, v) in &survivors {
                if let Some(&elem) = st.row_to_elem.get(row) {
                    wb.put(key_vec(&p, elem), encode_vector(&st.config, v)?);
                }
            }
        }
        for (&(layer, elem), nbrs) in &st.adjacency {
            wb.put(key_adj(&p, layer, elem), ser(nbrs)?);
        }
        self.db.write(wb).map_err(map_db)?;
        Ok(())
    }

    // ── Read accessors ───────────────────────────────────────────────────────

    /// The index id.
    #[must_use]
    pub fn index_id(&self) -> u64 {
        self.index_id
    }
    /// A copy of the index configuration.
    #[must_use]
    pub fn config(&self) -> PqHnswConfig {
        self.state.read().config.clone()
    }
    /// Whether Product Quantization is active for this index.
    #[must_use]
    pub fn pq_active(&self) -> bool {
        self.state.read().pq.is_some()
    }
    /// Number of stored elements.
    #[must_use]
    pub fn len(&self) -> usize {
        let st = self.state.read();
        if st.pq.is_some() {
            st.codes.len()
        } else {
            st.vectors.len()
        }
    }
    /// Whether the index has no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Estimated resident (RAM) bytes for the vector representation — PQ codes when
    /// active, full `f32` vectors otherwise. Used to demonstrate the PQ memory win.
    #[must_use]
    pub fn ram_vector_bytes(&self) -> usize {
        let st = self.state.read();
        if st.pq.is_some() {
            st.codes.values().map(|c| c.codes.len()).sum()
        } else {
            st.vectors.values().map(|v| v.len() * std::mem::size_of::<f32>()).sum()
        }
    }
    /// The current graph entry point, if any.
    #[must_use]
    pub fn entry_point(&self) -> Option<ElementId> {
        self.state.read().entry_point
    }
    /// Number of layers (max element level + 1).
    #[must_use]
    pub fn layer_count(&self) -> usize {
        self.state.read().layer_count
    }
    /// The next element id to assign.
    #[must_use]
    pub fn next_element_id(&self) -> ElementId {
        self.state.read().next_element_id
    }
    /// A copy of an element's resident vector (decoded from its PQ code when active).
    #[must_use]
    pub fn vector(&self, elem_id: ElementId) -> Option<Vector> {
        self.state.read().element_vector(elem_id)
    }
    /// An element's assigned level.
    #[must_use]
    pub fn level(&self, elem_id: ElementId) -> Option<u32> {
        self.state.read().levels.get(&elem_id).copied()
    }
    /// An element's neighbor list at a layer.
    #[must_use]
    pub fn neighbors(&self, layer: u32, elem_id: ElementId) -> Option<Vec<ElementId>> {
        self.state.read().adjacency.get(&(layer, elem_id)).cloned()
    }
    /// The external row id mapped to an element.
    #[must_use]
    pub fn row_of(&self, elem_id: ElementId) -> Option<u64> {
        self.state.read().elem_to_row.get(&elem_id).copied()
    }
    /// The element id mapped to an external row.
    #[must_use]
    pub fn elem_of(&self, row_id: u64) -> Option<ElementId> {
        self.state.read().row_to_elem.get(&row_id).copied()
    }
    /// Whether an element is tombstoned.
    #[must_use]
    pub fn is_tombstoned(&self, elem_id: ElementId) -> bool {
        self.state.read().tombstones.contains(&elem_id)
    }
    /// The sorted set of tombstoned element ids.
    #[must_use]
    pub fn tombstones(&self) -> Vec<ElementId> {
        let mut v: Vec<ElementId> = self.state.read().tombstones.iter().copied().collect();
        v.sort_unstable();
        v
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, Arc<DB>) {
        let dir = TempDir::new().unwrap();
        let db = Arc::new(DB::open_default(dir.path()).unwrap());
        (dir, db)
    }

    fn sample_config() -> PqHnswConfig {
        PqHnswConfig::new(3, DistanceMetric::L2)
    }

    /// Deterministic pseudo-random vector (no RNG dependency) for graph tests.
    fn rand_vec(seed: u64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|j| unit_f64_from(seed.wrapping_mul(0x0100_0193).wrapping_add(j as u64 + 1)) as f32)
            .collect()
    }

    fn l2(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt()
    }

    fn brute_topk(data: &[Vec<f32>], q: &[f32], k: usize) -> Vec<u64> {
        let mut all: Vec<(usize, f32)> =
            data.iter().enumerate().map(|(i, v)| (i, l2(q, v))).collect();
        all.sort_by(|a, b| a.1.total_cmp(&b.1));
        all.iter().take(k).map(|(i, _)| *i as u64).collect()
    }

    fn populate(idx: &PersistentVectorIndex) {
        idx.put_vector(0, 100, &vec![1.0, 0.0, 0.0], 2).unwrap();
        idx.put_vector(1, 101, &vec![0.0, 1.0, 0.0], 0).unwrap();
        idx.put_vector(2, 102, &vec![0.0, 0.0, 1.0], 1).unwrap();
        idx.put_adjacency(0, 0, vec![1, 2]).unwrap();
        idx.put_adjacency(0, 1, vec![0]).unwrap();
        idx.put_adjacency(1, 0, vec![2]).unwrap();
        idx.set_entry_point(Some(0)).unwrap();
        idx.mark_tombstone(1).unwrap();
    }

    // ── P1: persistence substrate ────────────────────────────────────────────

    #[test]
    fn test_create_then_open_roundtrip() {
        let (_dir, db) = test_db();
        {
            let idx = PersistentVectorIndex::create(db.clone(), 7, sample_config()).unwrap();
            populate(&idx);
            assert_eq!(idx.len(), 3);
            assert_eq!(idx.entry_point(), Some(0));
        }
        let reopened = PersistentVectorIndex::open(db.clone(), 7).unwrap();
        let cfg = reopened.config();
        assert_eq!(cfg.dimension, 3);
        assert_eq!(cfg.distance_metric, DistanceMetric::L2);
        assert_eq!(cfg.m, 16);
        assert!(!cfg.pq_enabled);
        assert_eq!(reopened.len(), 3);
        assert_eq!(reopened.entry_point(), Some(0));
        assert_eq!(reopened.layer_count(), 3);
        assert_eq!(reopened.next_element_id(), 3);
        assert_eq!(reopened.vector(0), Some(vec![1.0, 0.0, 0.0]));
        assert_eq!(reopened.level(2), Some(1));
        assert_eq!(reopened.neighbors(0, 0), Some(vec![1, 2]));
        assert_eq!(reopened.neighbors(1, 0), Some(vec![2]));
        assert_eq!(reopened.row_of(0), Some(100));
        assert_eq!(reopened.elem_of(102), Some(2));
        assert!(reopened.is_tombstoned(1));
        assert!(!reopened.is_tombstoned(0));
        assert_eq!(reopened.tombstones(), vec![1]);
    }

    #[test]
    fn test_crash_recovery_reopen_db() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        {
            let db = Arc::new(DB::open_default(&path).unwrap());
            let idx = PersistentVectorIndex::create(db.clone(), 1, sample_config()).unwrap();
            populate(&idx);
        }
        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, 1).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.entry_point(), Some(0));
        assert_eq!(idx.vector(2), Some(vec![0.0, 0.0, 1.0]));
        assert_eq!(idx.neighbors(0, 0), Some(vec![1, 2]));
        assert!(idx.is_tombstoned(1));
    }

    #[test]
    fn test_open_missing_is_err() {
        let (_dir, db) = test_db();
        assert!(PersistentVectorIndex::open(db.clone(), 999).is_err());
        assert!(!PersistentVectorIndex::exists(&db, 999).unwrap());
    }

    #[test]
    fn test_create_duplicate_is_err() {
        let (_dir, db) = test_db();
        let _idx = PersistentVectorIndex::create(db.clone(), 5, sample_config()).unwrap();
        assert!(PersistentVectorIndex::create(db.clone(), 5, sample_config()).is_err());
        assert!(PersistentVectorIndex::exists(&db, 5).unwrap());
    }

    #[test]
    fn test_drop_index_removes_keys() {
        let (_dir, db) = test_db();
        let idx = PersistentVectorIndex::create(db.clone(), 3, sample_config()).unwrap();
        populate(&idx);
        drop(idx);
        PersistentVectorIndex::drop_index(&db, 3).unwrap();
        assert!(!PersistentVectorIndex::exists(&db, 3).unwrap());
        assert!(PersistentVectorIndex::open(db.clone(), 3).is_err());
    }

    #[test]
    fn test_two_indexes_isolated() {
        let (_dir, db) = test_db();
        let a = PersistentVectorIndex::create(db.clone(), 1, sample_config()).unwrap();
        let b = PersistentVectorIndex::create(db.clone(), 2, sample_config()).unwrap();
        a.put_vector(0, 10, &vec![1.0, 2.0, 3.0], 0).unwrap();
        b.put_vector(0, 20, &vec![4.0, 5.0, 6.0], 0).unwrap();
        let a2 = PersistentVectorIndex::open(db.clone(), 1).unwrap();
        let b2 = PersistentVectorIndex::open(db.clone(), 2).unwrap();
        assert_eq!(a2.len(), 1);
        assert_eq!(b2.len(), 1);
        assert_eq!(a2.vector(0), Some(vec![1.0, 2.0, 3.0]));
        assert_eq!(b2.vector(0), Some(vec![4.0, 5.0, 6.0]));
        assert_eq!(a2.row_of(0), Some(10));
        assert_eq!(b2.row_of(0), Some(20));
    }

    #[test]
    fn test_dimension_mismatch_is_err() {
        let (_dir, db) = test_db();
        let idx = PersistentVectorIndex::create(db.clone(), 1, sample_config()).unwrap();
        assert!(idx.put_vector(0, 1, &vec![1.0, 2.0], 0).is_err());
    }

    #[test]
    fn test_unsupported_precision_is_err() {
        let (_dir, db) = test_db();
        let mut cfg = sample_config();
        cfg.rerank_precision = VectorPrecision::F16;
        assert!(PersistentVectorIndex::create(db.clone(), 1, cfg).is_err());
    }

    // ── P2: HNSW graph ───────────────────────────────────────────────────────

    #[test]
    fn test_graph_search_finds_self() {
        let (_dir, db) = test_db();
        let cfg = PqHnswConfig::new(4, DistanceMetric::L2);
        let idx = PersistentVectorIndex::create(db.clone(), 1, cfg).unwrap();
        let vecs = [
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0],
            vec![0.5, 0.5, 0.0, 0.0],
        ];
        for (i, v) in vecs.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        let got = idx.search(&vec![0.0, 0.0, 1.0, 0.0], 1, 32).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 2);
    }

    #[test]
    fn test_recall_vs_bruteforce_l2() {
        let (_dir, db) = test_db();
        let (dim, n, k, ef, queries) = (16usize, 1000usize, 10usize, 100usize, 100u64);
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        for (i, v) in data.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        assert_eq!(idx.len(), n);

        let mut hits = 0usize;
        for qi in 0..queries {
            let q = rand_vec(1_000_000 + qi, dim);
            let truth: HashSet<u64> = brute_topk(&data, &q, k).into_iter().collect();
            let got = idx.search(&q, k, ef).unwrap();
            assert_eq!(got.len(), k);
            hits += got.iter().filter(|(row, _)| truth.contains(row)).count();
        }
        let recall = hits as f64 / (queries as usize * k) as f64;
        assert!(recall >= 0.90, "recall@{k} = {recall:.3} (expected >= 0.90)");
    }

    #[test]
    fn test_graph_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let dim = 8;
        let n = 200u64;
        let q = rand_vec(99_999, dim);

        let before = {
            let db = Arc::new(DB::open_default(&path).unwrap());
            let idx = PersistentVectorIndex::create(
                db.clone(),
                1,
                PqHnswConfig::new(dim, DistanceMetric::L2),
            )
            .unwrap();
            for i in 0..n {
                idx.insert(i, &rand_vec(i, dim)).unwrap();
            }
            let res = idx.search(&q, 5, 64).unwrap();
            assert_eq!(res.len(), 5);
            res
        };

        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, 1).unwrap();
        let after = idx.search(&q, 5, 64).unwrap();
        assert_eq!(before, after, "search must be identical after crash-recovery reopen");
    }

    // ── P3: online deletes + compaction ──────────────────────────────────────

    #[test]
    fn test_remove_excludes_and_keeps_searchable() {
        let (_dir, db) = test_db();
        let dim = 8;
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();
        for i in 0..60u64 {
            idx.insert(i, &rand_vec(i, dim)).unwrap();
        }
        let q = rand_vec(7, dim);
        let before = idx.search(&q, 5, 64).unwrap();
        assert!(before.iter().any(|(r, _)| *r == 7));

        assert!(idx.remove(7).unwrap());
        assert!(!idx.remove(7).unwrap());
        assert_eq!(idx.len(), 59);

        let after = idx.search(&q, 5, 64).unwrap();
        assert!(!after.iter().any(|(r, _)| *r == 7), "removed row must not be returned");
        assert_eq!(after.len(), 5, "graph still returns k after repair");
        for (r, _) in &after {
            assert!(idx.elem_of(*r).is_some(), "no stale rows in results");
        }
    }

    #[test]
    fn test_remove_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let dim = 8;
        let q = rand_vec(99, dim);
        let removed = [3u64, 17, 42, 88];

        let before = {
            let db = Arc::new(DB::open_default(&path).unwrap());
            let idx = PersistentVectorIndex::create(
                db.clone(),
                1,
                PqHnswConfig::new(dim, DistanceMetric::L2),
            )
            .unwrap();
            for i in 0..100u64 {
                idx.insert(i, &rand_vec(i, dim)).unwrap();
            }
            for &r in &removed {
                assert!(idx.remove(r).unwrap());
            }
            idx.search(&q, 8, 64).unwrap()
        };

        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, 1).unwrap();
        assert_eq!(idx.len(), 96);
        let after = idx.search(&q, 8, 64).unwrap();
        assert_eq!(before, after, "deletions must survive reopen");
        for r in removed {
            assert!(idx.elem_of(r).is_none(), "removed row {r} must be absent after reopen");
        }
    }

    #[test]
    fn test_delete_churn_recall_stable() {
        use std::collections::BTreeMap;
        let (_dir, db) = test_db();
        let (dim, k, ef) = (12usize, 10usize, 100usize);
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();

        let mut live: BTreeMap<u64, Vec<f32>> = BTreeMap::new();
        let mut next = 0u64;
        for _ in 0..300 {
            let v = rand_vec(next, dim);
            idx.insert(next, &v).unwrap();
            live.insert(next, v);
            next += 1;
        }
        for _round in 0..4 {
            let del: Vec<u64> = live.keys().copied().take(60).collect();
            for r in del {
                assert!(idx.remove(r).unwrap());
                live.remove(&r);
            }
            for _ in 0..60 {
                let v = rand_vec(next, dim);
                idx.insert(next, &v).unwrap();
                live.insert(next, v);
                next += 1;
            }
        }
        assert_eq!(idx.len(), live.len());

        let data: Vec<(u64, Vec<f32>)> = live.iter().map(|(r, v)| (*r, v.clone())).collect();
        let mut hits = 0usize;
        let queries = 50u64;
        for qi in 0..queries {
            let q = rand_vec(3_000_000 + qi, dim);
            let mut all: Vec<(u64, f32)> = data.iter().map(|(r, v)| (*r, l2(&q, v))).collect();
            all.sort_by(|a, b| a.1.total_cmp(&b.1));
            let truth: HashSet<u64> = all.iter().take(k).map(|(r, _)| *r).collect();
            let got = idx.search(&q, k, ef).unwrap();
            for (r, _) in &got {
                assert!(live.contains_key(r), "stale row {r} returned after churn");
            }
            hits += got.iter().filter(|(r, _)| truth.contains(r)).count();
        }
        let recall = hits as f64 / (queries as usize * k) as f64;
        assert!(recall >= 0.80, "post-churn recall@{k} = {recall:.3} (expected >= 0.80)");
    }

    #[test]
    fn test_compact_drops_tombstoned_and_clears() {
        let (_dir, db) = test_db();
        let dim = 8;
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();
        for i in 0..100u64 {
            assert_eq!(idx.insert(i, &rand_vec(i, dim)).unwrap(), i);
        }
        for i in 0..20u64 {
            idx.mark_tombstone(i).unwrap();
        }
        assert_eq!(idx.tombstones().len(), 20);

        let q = rand_vec(123, dim);
        for (r, _) in idx.search(&q, 10, 64).unwrap() {
            assert!(r >= 20, "soft-deleted row {r} should be excluded from search");
        }

        idx.compact().unwrap();
        assert_eq!(idx.len(), 80, "compaction drops tombstoned elements");
        assert!(idx.tombstones().is_empty(), "compaction clears tombstones");
        for i in 0..20u64 {
            assert!(idx.elem_of(i).is_none(), "tombstoned row {i} fully gone after compact");
        }
        for (r, _) in idx.search(&q, 10, 64).unwrap() {
            assert!(r >= 20, "no dropped rows after compact");
        }
    }

    // ── P4: PQ unified with the graph ────────────────────────────────────────

    /// PQ config giving ~16× compression at dim 64 (16 sub-quantizers, 1-byte codes).
    fn pq_config_dim64() -> PqHnswConfig {
        let mut cfg = PqHnswConfig::new(64, DistanceMetric::L2);
        cfg.pq_config = Some(ProductQuantizerConfig {
            num_subquantizers: 16,
            num_centroids: 64,
            dimension: 64,
            training_iterations: 10,
            min_training_samples: 200,
        });
        cfg
    }

    #[test]
    fn test_pq_memory_and_recall() {
        let (_dir, db) = test_db();
        let (dim, n, k, ef) = (64usize, 600usize, 10usize, 100usize);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();

        let idx =
            PersistentVectorIndex::create_with_pq(db.clone(), 1, pq_config_dim64(), &data).unwrap();
        assert!(idx.pq_active());
        for (i, v) in data.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        assert_eq!(idx.len(), n);

        // Memory gate: PQ codes resident in RAM use >= 8x less than full f32 vectors.
        let pq_ram = idx.ram_vector_bytes();
        let full_ram = n * dim * std::mem::size_of::<f32>();
        assert!(
            pq_ram * 8 <= full_ram,
            "PQ RAM {pq_ram} vs full {full_ram} — expected >= 8x reduction"
        );

        // Recall after two-stage rerank.
        let mut hits = 0usize;
        let queries = 60u64;
        for qi in 0..queries {
            let q = rand_vec(5_000_000 + qi, dim);
            let truth: HashSet<u64> = brute_topk(&data, &q, k).into_iter().collect();
            let got = idx.search(&q, k, ef).unwrap();
            assert_eq!(got.len(), k);
            hits += got.iter().filter(|(r, _)| truth.contains(r)).count();
        }
        let recall = hits as f64 / (queries as usize * k) as f64;
        assert!(recall >= 0.75, "PQ recall@{k} = {recall:.3} (expected >= 0.75)");
    }

    #[test]
    fn test_pq_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let (dim, n) = (64usize, 400usize);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        let q = rand_vec(7_777, dim);

        let before = {
            let db = Arc::new(DB::open_default(&path).unwrap());
            let idx =
                PersistentVectorIndex::create_with_pq(db.clone(), 1, pq_config_dim64(), &data)
                    .unwrap();
            for (i, v) in data.iter().enumerate() {
                idx.insert(i as u64, v).unwrap();
            }
            idx.search(&q, 10, 100).unwrap()
        };

        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, 1).unwrap();
        assert!(idx.pq_active(), "PQ must be reconstructed on reopen");
        assert_eq!(idx.len(), n);
        let after = idx.search(&q, 10, 100).unwrap();
        assert_eq!(before, after, "PQ search must be identical after reopen");
    }

    #[test]
    fn test_pq_remove() {
        let (_dir, db) = test_db();
        let (dim, n) = (64usize, 300usize);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        let idx =
            PersistentVectorIndex::create_with_pq(db.clone(), 1, pq_config_dim64(), &data).unwrap();
        for (i, v) in data.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        let q = rand_vec(5, dim);
        assert!(idx.remove(5).unwrap());
        assert_eq!(idx.len(), n - 1);
        let got = idx.search(&q, 10, 100).unwrap();
        assert!(!got.iter().any(|(r, _)| *r == 5), "removed row absent under PQ");
        assert_eq!(got.len(), 10);
    }

    // ── P5: filtered KNN (predicate-during-traversal) ────────────────────────

    #[test]
    fn test_filtered_knn_correctness() {
        let (_dir, db) = test_db();
        let (dim, n, k, ef) = (16usize, 1000usize, 10usize, 200usize);
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        for (i, v) in data.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        let pass = |row: u64| row % 5 == 0; // 20% selectivity

        let mut hits = 0usize;
        let queries = 50u64;
        for qi in 0..queries {
            let q = rand_vec(6_000_000 + qi, dim);
            let mut all: Vec<(u64, f32)> = data
                .iter()
                .enumerate()
                .filter(|(i, _)| pass(*i as u64))
                .map(|(i, v)| (i as u64, l2(&q, v)))
                .collect();
            all.sort_by(|a, b| a.1.total_cmp(&b.1));
            let truth: HashSet<u64> = all.iter().take(k).map(|(r, _)| *r).collect();

            let got = idx.search_filtered(&q, k, ef, pass).unwrap();
            assert_eq!(got.len(), k, "filtered search must return k matching results");
            for (r, _) in &got {
                assert!(pass(*r), "result {r} must pass the filter");
            }
            hits += got.iter().filter(|(r, _)| truth.contains(r)).count();
        }
        let recall = hits as f64 / (queries as usize * k) as f64;
        assert!(recall >= 0.90, "filtered recall@{k} = {recall:.3} (expected >= 0.90)");
    }

    #[test]
    fn test_filtered_knn_pq() {
        let (_dir, db) = test_db();
        let (dim, n, k, ef) = (64usize, 600usize, 10usize, 200usize);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        let idx =
            PersistentVectorIndex::create_with_pq(db.clone(), 1, pq_config_dim64(), &data).unwrap();
        for (i, v) in data.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        let pass = |row: u64| row % 4 == 0; // 25% selectivity

        let mut hits = 0usize;
        let queries = 40u64;
        for qi in 0..queries {
            let q = rand_vec(7_000_000 + qi, dim);
            let mut all: Vec<(u64, f32)> = data
                .iter()
                .enumerate()
                .filter(|(i, _)| pass(*i as u64))
                .map(|(i, v)| (i as u64, l2(&q, v)))
                .collect();
            all.sort_by(|a, b| a.1.total_cmp(&b.1));
            let truth: HashSet<u64> = all.iter().take(k).map(|(r, _)| *r).collect();

            let got = idx.search_filtered(&q, k, ef, pass).unwrap();
            assert_eq!(got.len(), k);
            for (r, _) in &got {
                assert!(pass(*r), "PQ filtered result {r} must pass the filter");
            }
            hits += got.iter().filter(|(r, _)| truth.contains(r)).count();
        }
        let recall = hits as f64 / (queries as usize * k) as f64;
        assert!(recall >= 0.70, "PQ filtered recall@{k} = {recall:.3} (expected >= 0.70)");
    }

    #[test]
    fn test_filtered_beats_postfilter_completeness() {
        // The motivating case: a selective filter where post-filtering an unfiltered
        // top-k loses results that predicate-during-traversal retains.
        let (_dir, db) = test_db();
        let (dim, n) = (16usize, 1000usize);
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();
        for i in 0..n as u64 {
            idx.insert(i, &rand_vec(i, dim)).unwrap();
        }
        let pass = |row: u64| row % 50 == 0; // ~2% selectivity
        let q = rand_vec(123, dim);
        let (k, ef) = (5usize, 200usize);

        let filtered = idx.search_filtered(&q, k, ef, pass).unwrap();
        assert_eq!(filtered.len(), k, "filter-during-traversal returns a full k");
        for (r, _) in &filtered {
            assert!(pass(*r));
        }

        let unfiltered = idx.search(&q, k, ef).unwrap();
        let post: Vec<_> = unfiltered.into_iter().filter(|(r, _)| pass(*r)).collect();
        assert!(
            post.len() < filtered.len(),
            "post-filtering top-k ({} matches) loses results kept by filtered search ({})",
            post.len(),
            filtered.len()
        );
    }

    #[test]
    fn test_filtered_no_matches_returns_empty() {
        let (_dir, db) = test_db();
        let dim = 8;
        let idx =
            PersistentVectorIndex::create(db.clone(), 1, PqHnswConfig::new(dim, DistanceMetric::L2))
                .unwrap();
        for i in 0..50u64 {
            idx.insert(i, &rand_vec(i, dim)).unwrap();
        }
        let got = idx.search_filtered(&rand_vec(9, dim), 10, 64, |_row| false).unwrap();
        assert!(got.is_empty(), "a filter matching nothing yields no results");
    }
}
