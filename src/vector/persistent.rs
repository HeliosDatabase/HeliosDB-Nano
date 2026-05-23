//! Persistent PQ-HNSW vector index — Phase 1: persistence + crash-recovery scaffolding.
//!
//! See `PROPOSAL_PERSISTENT_PQ_HNSW.md` for the full design. This phase implements only the
//! durable storage substrate that later phases populate:
//!
//!   * the RocksDB key schema (`__vidx:<index_id>:…`) for index metadata, rerank vectors,
//!     per-layer adjacency, id mappings, and the tombstone set;
//!   * write-through persistence with atomic multi-key writes (`WriteBatch`);
//!   * crash recovery via [`PersistentVectorIndex::open`] — reload from RocksDB, no rebuild;
//!   * a single coarse per-index lock (`RwLock`) for structural correctness.
//!
//! The HNSW graph construction (level assignment, neighbor-selection heuristic, search) and
//! the PQ/ADC + exact-rerank query path arrive in later phases. This module deliberately
//! ships the substrate they will write through, behind the opt-in `vector-persist` feature,
//! so the default vector path (`hnsw_index`, `quantized_hnsw`) is unchanged.
//!
//! The persistence layer is grounded in the published HNSW and Product-Quantization
//! literature and implemented independently against this crate's own storage primitives
//! (`rocksdb`, `bincode`, `parking_lot`); see the proposal's IP-posture section.

#![allow(clippy::similar_names)]

use crate::{Error, Result};
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
/// Only [`VectorPrecision::F32`] is wired in this phase; `F16` / `I8` are accepted on the
/// config surface but rejected with a clear error until the multi-precision phase lands.
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Whether Product Quantization codes are stored (wired in a later phase).
    pub pq_enabled: bool,
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
        }
    }
}

/// Metadata persisted under the `…:meta` key — the part of the index state that is not
/// per-element. Loaded first on `open` so recovery can restore the entry point and counters
/// before faulting in element data.
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
struct IndexState {
    config: PqHnswConfig,
    entry_point: Option<ElementId>,
    next_element_id: ElementId,
    layer_count: usize,
    element_count: u64,
    vectors: HashMap<ElementId, Vector>,
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

/// A durable, crash-recoverable vector index backed by RocksDB.
///
/// Phase 1 provides the persistence substrate: write-through mutation of the index's
/// metadata, vectors, adjacency, id mappings, and tombstones, plus full reconstruction on
/// [`open`](Self::open). Structural mutations are serialized by a single coarse `RwLock`.
pub struct PersistentVectorIndex {
    db: Arc<DB>,
    index_id: u64,
    state: Arc<RwLock<IndexState>>,
}

impl PersistentVectorIndex {
    /// Create a new, empty index and persist its metadata. Errors if one already exists.
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

    /// Open (recover) an existing index from RocksDB. Errors if it does not exist or the
    /// on-disk schema version is unknown.
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
                Some("meta") => {} // already loaded
                Some("tomb") => {
                    let set: Vec<ElementId> = de(&v)?;
                    st.tombstones = set.into_iter().collect();
                }
                Some("vec") => {
                    let e = parse_id(parts.next())?;
                    st.vectors.insert(e, decode_vector(&meta.config, &v)?);
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
                _ => {} // unknown sub-key — ignore for forward compatibility
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

    // ── Write-through mutators (coarse-locked) ───────────────────────────────

    /// Store an element's rerank vector, row mapping, and level. Atomic across all keys.
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

    /// Mark an element as soft-deleted (full graph repair lands in a later phase).
    pub fn mark_tombstone(&self, elem_id: ElementId) -> Result<()> {
        let mut st = self.state.write();
        st.tombstones.insert(elem_id);
        let set: Vec<ElementId> = st.tombstones.iter().copied().collect();
        self.db
            .put(key_tomb(&prefix(self.index_id)), ser(&set)?)
            .map_err(map_db)?;
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
    /// Number of stored elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.read().vectors.len()
    }
    /// Whether the index has no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    /// A copy of an element's rerank vector.
    #[must_use]
    pub fn vector(&self, elem_id: ElementId) -> Option<Vector> {
        self.state.read().vectors.get(&elem_id).cloned()
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

// ── Phase 2: in-house HNSW graph (build + search) ────────────────────────────
//
// A self-contained Hierarchical Navigable Small World graph implemented directly
// against the persistent substrate above, following the published algorithm
// (Malkov & Yashunin, arXiv:1603.09320) and the layer-search / greedy-descent
// pattern already established in this crate's `in_descent` module. Distances use
// the crate's SIMD-backed metric kernels (`super::l2_distance`, …). No third-party
// graph code is used; level assignment uses the public-domain SplitMix64 mixer so
// the build needs no external RNG dependency.

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

/// Assign a level with the standard `floor(-ln(U) * mL)` rule, seeded
/// deterministically from the element id so builds are reproducible.
fn random_level(seed: u64, ml: f64) -> u32 {
    let u = unit_f64_from(seed).max(f64::MIN_POSITIVE);
    let lvl = (-u.ln() * ml).floor();
    if lvl <= 0.0 {
        0
    } else {
        (lvl as u32).min(MAX_LEVEL)
    }
}

/// In-memory HNSW insert shared by `insert` (write-through) and `compact`
/// (bulk rebuild). Mutates `st` only — performs no persistence — and returns the
/// new element id plus the adjacency keys it touched (for the caller to flush).
/// Caller is responsible for validating the vector dimension.
fn graph_insert(
    st: &mut IndexState,
    row_id: u64,
    vector: &Vector,
) -> (ElementId, Vec<(u32, ElementId)>) {
    let elem = st.next_element_id;
    let level = random_level(elem, st.config.ml);
    let m = st.config.m;
    let m0 = st.config.m0;
    let efc = st.config.ef_construction;

    st.vectors.insert(elem, vector.clone());
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
            let mut ep_dist = st.dist(vector, ep);

            // Greedy descent through the layers above the new element's level.
            let mut layer = top;
            while layer > level {
                let res = st.search_layer(vector, &[ep], 1, layer);
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
                let candidates = st.search_layer(vector, &[entry], efc, layer_u);
                let selected = st.select_neighbors(vector, candidates.clone(), m_l);

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
                        if let Some(nb_vec) = st.vectors.get(&nb).cloned() {
                            let cands: Vec<Cand> = nb_list
                                .iter()
                                .map(|&x| Cand { dist: st.dist(&nb_vec, x), id: x })
                                .collect();
                            nb_list = st.select_neighbors(&nb_vec, cands, cap);
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

    (elem, touched)
}

impl IndexState {
    /// Distance from a query vector to a stored element under the index metric.
    fn dist(&self, q: &[f32], id: ElementId) -> f32 {
        match self.vectors.get(&id) {
            Some(v) => match self.config.distance_metric {
                DistanceMetric::L2 => super::l2_distance(q, v),
                DistanceMetric::Cosine => super::cosine_distance(q, v),
                DistanceMetric::InnerProduct => super::inner_product_distance(q, v),
            },
            None => f32::INFINITY,
        }
    }

    /// HNSW layer search (Algorithm 2): best-first exploration of `layer` from
    /// `entry`, returning up to `ef` closest elements sorted ascending by distance.
    /// Tombstoned elements are traversed for connectivity but never collected.
    fn search_layer(&self, q: &[f32], entry: &[ElementId], ef: usize, layer: u32) -> Vec<Cand> {
        let mut visited: HashSet<ElementId> = HashSet::new();
        let mut frontier: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
        let mut best: BinaryHeap<Cand> = BinaryHeap::new();
        for &e in entry {
            if !visited.insert(e) {
                continue;
            }
            let d = self.dist(q, e);
            frontier.push(Reverse(Cand { dist: d, id: e }));
            if !self.tombstones.contains(&e) {
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
                let d = self.dist(q, n);
                let worst = best.peek().map_or(f32::INFINITY, |w| w.dist);
                if best.len() < ef || d < worst {
                    frontier.push(Reverse(Cand { dist: d, id: n }));
                    if !self.tombstones.contains(&n) {
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

    /// Neighbor-selection heuristic (Algorithm 4, base variant): keep a diverse
    /// set — an element is kept only if it is closer to `base` than to every
    /// already-selected neighbor. `candidates` carry their distance to `base`.
    fn select_neighbors(&self, base: &[f32], mut candidates: Vec<Cand>, m: usize) -> Vec<ElementId> {
        let _ = base; // base distances are precomputed into `candidates`
        candidates.sort_unstable();
        let mut result: Vec<ElementId> = Vec::with_capacity(m);
        for cand in candidates {
            if result.len() >= m {
                break;
            }
            let Some(cand_vec) = self.vectors.get(&cand.id) else {
                continue;
            };
            let keep = result.iter().all(|&r| self.dist(cand_vec, r) >= cand.dist);
            if keep {
                result.push(cand.id);
            }
        }
        result
    }
}

impl PersistentVectorIndex {
    /// Insert a vector into the graph: assign a fresh element id, choose a level,
    /// connect it to its nearest neighbors at each layer, and persist every touched
    /// key atomically with the vector. Returns the assigned element id.
    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<ElementId> {
        let mut st = self.state.write();
        if vector.len() != st.config.dimension {
            return Err(Error::query_execution(format!(
                "vector dimension mismatch: expected {}, got {}",
                st.config.dimension,
                vector.len()
            )));
        }
        let (elem, mut touched) = graph_insert(&mut st, row_id, vector);

        // Flush the new element's keys + every touched adjacency list + metadata,
        // atomically in one batch.
        let p = prefix(self.index_id);
        let level = st.levels.get(&elem).copied().unwrap_or(0);
        let mut wb = WriteBatch::default();
        wb.put(key_vec(&p, elem), encode_vector(&st.config, vector)?);
        wb.put(key_map(&p, elem), ser(&row_id)?);
        wb.put(key_rmap(&p, row_id), ser(&elem)?);
        wb.put(key_lvl(&p, elem), ser(&level)?);
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

    /// Search the graph for the `k` nearest neighbors of `query`, exploring with a
    /// candidate-list size of `ef` (clamped up to at least `k`). Returns
    /// `(row_id, distance)` sorted ascending by distance; tombstoned elements are
    /// excluded.
    pub fn search(&self, query: &Vector, k: usize, ef: usize) -> Result<Vec<(u64, f32)>> {
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

        let top = (st.layer_count.saturating_sub(1)) as u32;
        let mut ep = ep0;
        let mut ep_dist = st.dist(query, ep);
        let mut layer = top;
        while layer > 0 {
            let res = st.search_layer(query, &[ep], 1, layer);
            if let Some(c) = res.first() {
                if c.dist < ep_dist {
                    ep = c.id;
                    ep_dist = c.dist;
                }
            }
            layer -= 1;
        }

        let ef_eff = ef.max(k);
        let found = st.search_layer(query, &[ep], ef_eff, 0);
        let out: Vec<(u64, f32)> = found
            .into_iter()
            .filter_map(|c| st.elem_to_row.get(&c.id).map(|&r| (r, c.dist)))
            .take(k)
            .collect();
        Ok(out)
    }

    /// Remove a row from the graph: delete the element and repair every node that
    /// referenced it by re-selecting that node's connections from the candidate pool
    /// left by the hole (the deleted node's other neighbors plus the referrer's own).
    /// Promotes the entry point if it was removed. Persisted atomically. Returns
    /// whether the row existed. This keeps recall stable under churn — unlike a
    /// tombstone-only delete, no stale edges are left behind.
    pub fn remove(&self, row_id: u64) -> Result<bool> {
        let mut st = self.state.write();
        let Some(elem) = st.row_to_elem.get(&row_id).copied() else {
            return Ok(false);
        };
        let level = st.levels.get(&elem).copied().unwrap_or(0);
        let p = prefix(self.index_id);
        let mut wb = WriteBatch::default();
        let mut touched: Vec<(u32, ElementId)> = Vec::new();

        // Edges only exist at layers <= the element's level.
        for layer in 0..=level {
            let x_nbrs: Vec<ElementId> =
                st.adjacency.get(&(layer, elem)).cloned().unwrap_or_default();
            let cap = if layer == 0 { st.config.m0 } else { st.config.m };

            // Every node that still points at `elem` at this layer must be repaired
            // (HNSW edges can be asymmetric, so scan rather than trust `x_nbrs`).
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
                let Some(nb_vec) = st.vectors.get(&nb).cloned() else {
                    continue;
                };
                // Candidate pool: the hole's other neighbors + the referrer's current
                // neighbors, minus the deleted node and self, restricted to live nodes.
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
                let cands: Vec<Cand> = pool
                    .into_iter()
                    .filter(|c| st.vectors.contains_key(c) && !st.tombstones.contains(c))
                    .map(|c| Cand { dist: st.dist(&nb_vec, c), id: c })
                    .collect();
                let new_list = st.select_neighbors(&nb_vec, cands, cap);
                st.adjacency.insert((layer, nb), new_list);
                touched.push((layer, nb));
            }

            st.adjacency.remove(&(layer, elem));
            wb.delete(key_adj(&p, layer, elem));
        }

        // Drop the element's own data.
        st.vectors.remove(&elem);
        st.levels.remove(&elem);
        st.elem_to_row.remove(&elem);
        st.row_to_elem.remove(&row_id);
        let was_tomb = st.tombstones.remove(&elem);
        st.element_count = st.element_count.saturating_sub(1);
        wb.delete(key_vec(&p, elem));
        wb.delete(key_lvl(&p, elem));
        wb.delete(key_map(&p, elem));
        wb.delete(key_rmap(&p, row_id));

        // Promote the entry point and recompute the layer count over survivors.
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
    /// reclaiming space and clearing tombstones. A maintenance safety net for
    /// heavy-churn or soft-delete workloads; persisted atomically.
    pub fn compact(&self) -> Result<()> {
        let mut st = self.state.write();
        let mut survivors: Vec<(u64, Vector)> = st
            .vectors
            .iter()
            .filter(|(elem, _)| !st.tombstones.contains(*elem))
            .filter_map(|(elem, v)| st.elem_to_row.get(elem).map(|&row| (row, v.clone())))
            .collect();
        survivors.sort_by_key(|(row, _)| *row);

        let config = st.config.clone();
        *st = IndexState::new(config);
        for (row, v) in &survivors {
            let _ = graph_insert(&mut st, *row, v);
        }
        self.persist_full(&st)?;
        Ok(())
    }

    /// Wipe the index's keyspace and re-persist the full in-memory state in one
    /// atomic batch. Used by `compact`.
    fn persist_full(&self, st: &IndexState) -> Result<()> {
        let p = prefix(self.index_id);
        let pb = p.as_bytes();
        let mut wb = WriteBatch::default();
        let iter = self.db.iterator(IteratorMode::From(pb, Direction::Forward));
        for item in iter {
            let (k, _) = item.map_err(map_db)?;
            if !k.starts_with(pb) {
                break;
            }
            wb.delete(k);
        }
        wb.put(key_meta(&p), st.meta_bytes()?);
        for (&elem, vec) in &st.vectors {
            wb.put(key_vec(&p, elem), encode_vector(&st.config, vec)?);
            if let Some(&row) = st.elem_to_row.get(&elem) {
                wb.put(key_map(&p, elem), ser(&row)?);
                wb.put(key_rmap(&p, row), ser(&elem)?);
            }
            if let Some(&lvl) = st.levels.get(&elem) {
                wb.put(key_lvl(&p, elem), ser(&lvl)?);
            }
        }
        for (&(layer, elem), nbrs) in &st.adjacency {
            wb.put(key_adj(&p, layer, elem), ser(nbrs)?);
        }
        if !st.tombstones.is_empty() {
            let set: Vec<ElementId> = st.tombstones.iter().copied().collect();
            wb.put(key_tomb(&p), ser(&set)?);
        }
        self.db.write(wb).map_err(map_db)?;
        Ok(())
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
        assert_eq!(reopened.config(), sample_config());
        assert_eq!(reopened.len(), 3);
        assert_eq!(reopened.entry_point(), Some(0));
        assert_eq!(reopened.layer_count(), 3); // max level 2 ⇒ 3 layers
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
            // Simulate a crash: drop the index handle and close the DB with no explicit
            // flush/close of the index itself.
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
        // Guards the prefix-iteration boundary (index 1 vs index 2 share the DB).
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
        assert!(idx.put_vector(0, 1, &vec![1.0, 2.0], 0).is_err()); // dim 2 ≠ 3
    }

    #[test]
    fn test_unsupported_precision_is_err() {
        let (_dir, db) = test_db();
        let mut cfg = sample_config();
        cfg.rerank_precision = VectorPrecision::F16;
        assert!(PersistentVectorIndex::create(db.clone(), 1, cfg).is_err());
    }

    // ── Phase 2: HNSW graph ──────────────────────────────────────────────────

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
        assert_eq!(got[0].0, 2, "nearest to e_3 is element 2");
    }

    #[test]
    fn test_recall_vs_bruteforce_l2() {
        // Parity gate: in-house HNSW recall@k vs exact ground truth.
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
            assert_eq!(got.len(), k, "expected {k} results");
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

        // Reopen from disk and confirm the recovered graph yields identical results.
        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, 1).unwrap();
        let after = idx.search(&q, 5, 64).unwrap();
        assert_eq!(before, after, "search must be identical after crash-recovery reopen");
    }

    // ── Phase 3: online deletes + compaction ─────────────────────────────────

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
        let q = rand_vec(7, dim); // exactly row 7's vector
        let before = idx.search(&q, 5, 64).unwrap();
        assert!(before.iter().any(|(r, _)| *r == 7), "row 7 should be a top hit");

        assert!(idx.remove(7).unwrap());
        assert!(!idx.remove(7).unwrap(), "second remove is a no-op");
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
        // Interleaved delete/insert rounds — the pattern that collapses a
        // tombstone-only index's recall.
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
        assert_eq!(idx.len(), live.len(), "element count must track live set");

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
        // Row i maps to element i in a fresh index.
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
}
