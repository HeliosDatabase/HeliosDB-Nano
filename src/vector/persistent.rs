//! Persistent PQ-HNSW vector index (feature `vector-persist`).
//!
//! Persistent PQ-HNSW vector index design. Implemented in phases:
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

use super::quantization::{Codebook, ProductQuantizer, ProductQuantizerConfig, QuantizedVector};
use super::{DistanceMetric, Vector};
use crate::storage::WriteGate;
use crate::{Error, Result};
use parking_lot::RwLock;
use rocksdb::{Direction, IteratorMode, WriteBatch, DB};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

/// Internal element identifier within an index.
pub type ElementId = u64;

/// On-disk format version for the persistent index keyspace.
pub const PERSIST_SCHEMA_VERSION: u32 = 1;

/// Rerank-vector storage precision — a memory/accuracy dial for the persisted full
/// vectors that stacks on top of PQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VectorPrecision {
    /// 32-bit float — exact, no compression (4 bytes/dim).
    F32,
    /// IEEE-754 half — ~2× smaller (2 bytes/dim), ~3 significant digits.
    F16,
    /// Per-vector symmetric int8 scalar quantization — ~4× smaller (1 byte/dim + scale).
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
    /// Latest logical row images ride their owning heap groups. Graph hooks
    /// never erase them; a complete replacement consumes the whole namespace.
    row_images: HashMap<u64, Option<Vector>>,
    /// A row image can precede its rerank record. Read-only repair must not
    /// fault a stale vector from RocksDB while serving its corrected graph.
    rerank_overrides: HashMap<ElementId, Vector>,
    rederived_from_rows: bool,
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
            row_images: HashMap::new(),
            rerank_overrides: HashMap::new(),
            rederived_from_rows: false,
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

    /// The vector to keep resident in RAM (PQ-inactive mode): exact for `F32`, otherwise
    /// round-tripped through the configured precision so in-session results match
    /// post-reopen (which loads the lossily-stored vector).
    fn resident_vector(&self, v: &Vector) -> Result<Vector> {
        match self.config.rerank_precision {
            VectorPrecision::F32 => Ok(v.clone()),
            _ => decode_vector(&self.config, &encode_vector(&self.config, v)?),
        }
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
/// The versioned authoritative overlay is part of the existing Vidx key class.
/// Values are sealed by the owning engine, independently of raw graph records.
pub(crate) fn row_image_key(index_id: u64, row_id: u64) -> Vec<u8> {
    format!("{}row_image:{row_id}", prefix(index_id)).into_bytes()
}

#[derive(Serialize, Deserialize)]
struct RowImageV1 {
    version: u8,
    vector: Option<Vector>,
}

pub(crate) fn encode_row_image(
    key_manager: Option<&crate::crypto::KeyManager>,
    vector: Option<&Vector>,
) -> Result<Vec<u8>> {
    let plain = ser(&RowImageV1 {
        version: 1,
        vector: vector.cloned(),
    })?;
    Ok(crate::storage::tde::seal(key_manager, &plain)?.into_owned())
}

fn decode_row_image(
    key_manager: Option<&crate::crypto::KeyManager>,
    key: &[u8],
    value: &[u8],
) -> Result<Option<Vector>> {
    let plain = crate::storage::tde::open_ref(key_manager, key, value)?;
    use bincode::Options;
    let image: RowImageV1 = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(plain.len() as u64)
        .reject_trailing_bytes()
        .deserialize(plain.as_ref())
        .map_err(|error| Error::storage(format!("persistent row-image payload: {error}")))?;
    if image.version != 1 {
        return Err(Error::storage(format!(
            "unsupported persistent row-image version {}",
            image.version
        )));
    }
    Ok(image.vector)
}

fn parse_row_image_suffix(suffix: &[u8]) -> Result<u64> {
    let digits = suffix
        .strip_prefix(b"row_image:")
        .ok_or_else(|| Error::storage("invalid persistent row-image key"))?;
    let text = std::str::from_utf8(digits).map_err(|_| Error::storage("invalid persistent row-image row id"))?;
    let row = text
        .parse::<u64>()
        .map_err(|_| Error::storage("invalid persistent row-image row id"))?;
    if text != row.to_string() {
        return Err(Error::storage("noncanonical persistent row-image row id"));
    }
    Ok(row)
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

/// Split a key of the persistent keyspace into its index id and the suffix
/// after `__vidx:{index_id}:` — the inverse of the key builders above, for a physical
/// standby that receives the keys rather than writing them (spec P1.2).
/// `None` for a key of any other shape.
#[must_use]
pub fn split_record_key(key: &[u8]) -> Option<(u64, &[u8])> {
    let rest = key.strip_prefix(b"__vidx:")?;
    let colon = rest.iter().position(|&byte| byte == b':')?;
    let index_id = std::str::from_utf8(rest.get(..colon)?).ok()?.parse().ok()?;
    Some((index_id, rest.get(colon + 1..)?))
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

/// Refuse a `meta` record of an on-disk format this build does not read.
/// Shared by [`PersistentVectorIndex::open`] and [`PersistentVectorIndex::apply_record`].
fn check_schema_version(meta: &PersistedMeta) -> Result<()> {
    if meta.schema_version != PERSIST_SCHEMA_VERSION {
        return Err(Error::storage(format!(
            "vector-persist: on-disk schema version {} unsupported (expected {})",
            meta.schema_version, PERSIST_SCHEMA_VERSION
        )));
    }
    Ok(())
}

/// Rebuild the quantizer from a persisted `pq` codebook under `config`'s PQ
/// parameters (or the dimension's default when none were recorded). Shared
/// by [`PersistentVectorIndex::open`] and [`PersistentVectorIndex::apply_record`].
fn quantizer_from_codebook(config: &PqHnswConfig, codebook_bytes: &[u8]) -> Result<ProductQuantizer> {
    let codebook: Codebook = de(codebook_bytes)?;
    let pq_cfg = match config.pq_config.clone() {
        Some(c) => c,
        None => ProductQuantizerConfig::default_for_dimension(config.dimension)
            .map_err(|e| Error::storage(format!("vector-persist: pq config: {e}")))?,
    };
    ProductQuantizer::new(pq_cfg, codebook).map_err(|e| Error::storage(format!("vector-persist: pq reconstruct: {e}")))
}

/// One record of an index's keyspace, applied to its in-memory mirror: the
/// per-key step of [`PersistentVectorIndex::open`] turned into a state
/// transition (spec P1.2; the suffix → structure table is on
/// [`PersistentVectorIndex::apply_record`]). `suffix` is the key after
/// `__vidx:{index_id}:`; `value` is its post-batch value, `None` when the key
/// was deleted.
///
/// A suffix outside that table is an error naming it: `open` skips a suffix
/// it does not know, but a follower that skipped one would claim to mirror
/// keys it never read. The order records are applied in matters only through
/// `config` and `pq` (how a `vec` is decoded, and whether it is kept); a
/// replicated batch arrives in key order, and `meta` < `pq` < `vec`.
fn apply_record_to(st: &mut IndexState, index_id: u64, suffix: &[u8], value: Option<&[u8]>) -> Result<()> {
    let text = std::str::from_utf8(suffix).map_err(|e| Error::storage(format!("vector-persist: non-utf8 key: {e}")))?;
    let mut parts = text.split(':');
    match parts.next() {
        Some("meta") => match value {
            Some(v) => {
                let meta: PersistedMeta = de(v)?;
                check_schema_version(&meta)?;
                st.config = meta.config;
                st.entry_point = meta.entry_point;
                st.next_element_id = meta.next_element_id;
                st.layer_count = meta.layer_count;
                st.element_count = meta.element_count;
            }
            None => {
                st.entry_point = None;
                st.next_element_id = 0;
                st.layer_count = 0;
                st.element_count = 0;
            }
        },
        Some("pq") => {
            st.pq = match value {
                Some(v) => Some(Arc::new(quantizer_from_codebook(&st.config, v)?)),
                None => None,
            };
        }
        Some("tomb") => {
            st.tombstones = match value {
                Some(v) => de::<Vec<ElementId>>(v)?.into_iter().collect(),
                None => HashSet::new(),
            };
        }
        Some("vec") => {
            let e = parse_id(parts.next())?;
            match value {
                Some(v) => {
                    if st.pq.is_none() {
                        let vector = decode_vector(&st.config, v)?;
                        st.vectors.insert(e, vector);
                    }
                }
                None => {
                    st.vectors.remove(&e);
                }
            }
        }
        Some("code") => {
            let e = parse_id(parts.next())?;
            match value {
                Some(v) => {
                    st.codes.insert(e, de::<QuantizedVector>(v)?);
                }
                None => {
                    st.codes.remove(&e);
                }
            }
        }
        Some("lvl") => {
            let e = parse_id(parts.next())?;
            match value {
                Some(v) => {
                    st.levels.insert(e, de::<u32>(v)?);
                }
                None => {
                    st.levels.remove(&e);
                }
            }
        }
        Some("adj") => {
            let layer = parse_id(parts.next())? as u32;
            let e = parse_id(parts.next())?;
            match value {
                Some(v) => {
                    st.adjacency.insert((layer, e), de::<Vec<ElementId>>(v)?);
                }
                None => {
                    st.adjacency.remove(&(layer, e));
                }
            }
        }
        Some("map") => {
            let e = parse_id(parts.next())?;
            match value {
                Some(v) => {
                    st.elem_to_row.insert(e, de::<u64>(v)?);
                }
                None => {
                    st.elem_to_row.remove(&e);
                }
            }
        }
        Some("rmap") => {
            let row = parse_id(parts.next())?;
            match value {
                Some(v) => {
                    st.row_to_elem.insert(row, de::<ElementId>(v)?);
                }
                None => {
                    st.row_to_elem.remove(&row);
                }
            }
        }
        _ => {
            return Err(Error::storage(format!(
                "vector-persist: index {index_id}: unknown record suffix '{text}'"
            )))
        }
    }
    Ok(())
}

/// Encode a rerank vector for storage at the configured precision. `F16` halves the
/// footprint (IEEE half), `I8` quarters it (per-vector symmetric scalar quantization).
fn encode_vector(cfg: &PqHnswConfig, v: &Vector) -> Result<Vec<u8>> {
    match cfg.rerank_precision {
        VectorPrecision::F32 => ser(v),
        VectorPrecision::F16 => {
            let half: Vec<u16> = v.iter().map(|&x| f32_to_f16(x)).collect();
            ser(&half)
        }
        VectorPrecision::I8 => {
            let max_abs = v.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
            let scale = if max_abs > 0.0 { max_abs / 127.0 } else { 1.0 };
            let codes: Vec<i8> = v
                .iter()
                .map(|&x| (x / scale).round().clamp(-127.0, 127.0) as i8)
                .collect();
            ser(&(scale, codes))
        }
    }
}
/// Decode a rerank vector stored at the configured precision.
fn decode_vector(cfg: &PqHnswConfig, b: &[u8]) -> Result<Vector> {
    match cfg.rerank_precision {
        VectorPrecision::F32 => de::<Vector>(b),
        VectorPrecision::F16 => {
            let half: Vec<u16> = de(b)?;
            Ok(half.iter().map(|&h| f16_to_f32(h)).collect())
        }
        VectorPrecision::I8 => {
            let (scale, codes): (f32, Vec<i8>) = de(b)?;
            Ok(codes.iter().map(|&c| f32::from(c) * scale).collect())
        }
    }
}

/// Convert `f32` to IEEE-754 half-precision bits (round to nearest even). Hand-rolled to
/// stay dependency-free; handles normals, subnormals, zero, overflow, and inf/nan.
fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let raw_exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;
    if raw_exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 }; // inf / nan
    }
    let exp = raw_exp - 127 + 15;
    if exp >= 0x1f {
        return sign | 0x7c00; // overflow → inf
    }
    if exp <= 0 {
        if exp < -10 {
            return sign; // underflow → ±0
        }
        let m = mant | 0x0080_0000; // restore implicit leading 1
        let shift = (14 - exp) as u32;
        let mut half = (m >> shift) as u16;
        let rem = m & ((1u32 << shift) - 1);
        let halfway = 1u32 << (shift - 1);
        if rem > halfway || (rem == halfway && (half & 1) == 1) {
            half += 1;
        }
        return sign | half;
    }
    let mut half = sign | ((exp as u16) << 10) | ((mant >> 13) as u16);
    let rem = mant & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && (half & 1) == 1) {
        half += 1; // round to nearest even; a carry into the exponent field is correct
    }
    half
}

/// Convert IEEE-754 half-precision bits to `f32`.
fn f16_to_f32(h: u16) -> f32 {
    let sign = (u32::from(h) & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = u32::from(h & 0x03ff);
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // subnormal → normalize
            let mut e: i32 = -1;
            let mut m = mant;
            loop {
                e += 1;
                m <<= 1;
                if m & 0x0400 != 0 {
                    break;
                }
            }
            sign | (((127 - 15 - e) as u32) << 23) | ((m & 0x03ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13) // inf / nan
    } else {
        sign | ((u32::from(exp) + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
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
                (Some(pq), Some(code)) => pq.compute_distance_with_table(table, code).unwrap_or(f32::INFINITY),
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
fn graph_insert(st: &mut IndexState, row_id: u64, vector: &Vector) -> Result<(ElementId, Vec<(u32, ElementId)>)> {
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
        let resident = st.resident_vector(vector)?;
        st.vectors.insert(elem, resident);
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
                    let mut nb_list = st.adjacency.get(&(layer_u, nb)).cloned().unwrap_or_default();
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

/// Read-only hydration from actual graph records plus latest row-group images.
/// A fully caught-up graph is reused. Only an unfinished graph publication builds
/// a replacement in RAM; no gate or durable cursor is touched.
fn read_state_from_db(db: &DB, index_id: u64, key_manager: Option<&crate::crypto::KeyManager>) -> Result<IndexState> {
    let p = prefix(index_id);
    let meta_bytes = db
        .get(key_meta(&p))
        .map_err(map_db)?
        .ok_or_else(|| Error::storage(format!("vector-persist: index {index_id} not found")))?;
    let meta: PersistedMeta = de(&meta_bytes)?;
    check_schema_version(&meta)?;

    let mut st = IndexState::new(meta.config.clone());
    st.entry_point = meta.entry_point;
    st.next_element_id = meta.next_element_id;
    st.layer_count = meta.layer_count;
    st.element_count = meta.element_count;

    // Reconstruct the quantizer if a codebook was persisted.
    let pq_active = if let Some(cb_bytes) = db.get(key_pq(&p)).map_err(map_db)? {
        st.pq = Some(Arc::new(quantizer_from_codebook(&meta.config, &cb_bytes)?));
        true
    } else {
        false
    };

    if st.config.pq_enabled != st.pq.is_some() {
        return Err(Error::storage("persistent graph PQ metadata/codebook mismatch"));
    }

    // Bound both sides: the engine has a prefix extractor and range
    // tombstones must not hide records outside one extractor block.
    let pb = p.as_bytes();
    let mut options = rocksdb::ReadOptions::default();
    options.set_total_order_seek(true);
    options.set_iterate_lower_bound(pb.to_vec());
    let mut upper = pb.to_vec();
    if let Some(last) = upper.last_mut() {
        *last += 1;
    }
    options.set_iterate_upper_bound(upper);
    let iter = db.iterator_opt(IteratorMode::From(pb, Direction::Forward), options);
    let mut full_vectors = HashMap::new();
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
                let e = parse_id(parts.next())?;
                let vector = decode_vector(&meta.config, &v)?;
                if !pq_active {
                    st.vectors.insert(e, vector.clone());
                }
                full_vectors.insert(e, v.to_vec());
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
            Some("row_image") => {
                let row = parse_row_image_suffix(suffix.as_bytes())?;
                st.row_images.insert(row, decode_row_image(key_manager, &k, &v)?);
            }
            Some("rmap") => {
                let row = parse_id(parts.next())?;
                st.row_to_elem.insert(row, de::<ElementId>(&v)?);
            }
            _ => {}
        }
    }

    reconcile_row_images(&mut st, &full_vectors)?;
    Ok(st)
}

fn row_image_matches(st: &IndexState, row: u64, image: &Option<Vector>, full: Option<&[u8]>) -> Result<bool> {
    let Some(vector) = image else {
        return Ok(!st.row_to_elem.contains_key(&row));
    };
    if vector.len() != st.config.dimension {
        return Err(Error::storage("persistent row image has the wrong vector dimension"));
    }
    let Some(element) = st.row_to_elem.get(&row) else {
        return Ok(false);
    };
    if st.elem_to_row.get(element) != Some(&row) || st.tombstones.contains(element) {
        return Ok(false);
    }
    let Some(full) = full else {
        return Ok(false);
    };
    if encode_vector(&st.config, vector)? != full {
        return Ok(false);
    }
    if let Some(pq) = &st.pq {
        let expected = pq
            .encode(vector)
            .map_err(|error| Error::storage(format!("persistent row image PQ: {error}")))?;
        return Ok(st
            .codes
            .get(element)
            .is_some_and(|actual| actual.codes == expected.codes));
    }
    Ok(true)
}

fn reconcile_row_images(st: &mut IndexState, full_vectors: &HashMap<ElementId, Vec<u8>>) -> Result<()> {
    if st.row_images.is_empty() {
        return Ok(());
    }
    let mut mismatch = false;
    for (&row, image) in &st.row_images {
        let full = st
            .row_to_elem
            .get(&row)
            .and_then(|element| full_vectors.get(element))
            .map(Vec::as_slice);
        mismatch |= !row_image_matches(st, row, image, full)?;
    }
    if !mismatch {
        return Ok(());
    }
    // Keep IDs for unaffected rows. Re-numbering from zero would allow a later
    // incremental hook to overwrite another row's still-durable vec key.
    let max_durable = full_vectors
        .keys()
        .chain(st.elem_to_row.keys())
        .chain(st.levels.keys())
        .copied()
        .max();
    let mut next = st.next_element_id;
    if let Some(max) = max_durable {
        next = next.max(
            max.checked_add(1)
                .ok_or_else(|| Error::storage("persistent element ids exhausted"))?,
        );
    }
    let mut rows: std::collections::BTreeMap<u64, (u64, Vector)> = std::collections::BTreeMap::new();
    for (&row, &element) in &st.row_to_elem {
        if st.tombstones.contains(&element) {
            continue;
        }
        match full_vectors.get(&element) {
            Some(vector) => {
                rows.insert(row, (element, decode_vector(&st.config, vector)?));
            }
            None if st.row_images.contains_key(&row) => {}
            None => {
                return Err(Error::storage(
                    "persistent graph has an unrecoverable missing row vector",
                ))
            }
        }
    }
    for (&row, image) in &st.row_images {
        match image {
            None => {
                rows.remove(&row);
            }
            Some(vector) => {
                if vector.len() != st.config.dimension {
                    return Err(Error::storage("persistent row image dimension mismatch"));
                }
                if let Some((_, value)) = rows.get_mut(&row) {
                    *value = vector.clone();
                } else {
                    let element = next;
                    next = next
                        .checked_add(1)
                        .ok_or_else(|| Error::storage("persistent element ids exhausted"))?;
                    rows.insert(row, (element, vector.clone()));
                }
            }
        }
    }
    let mut fresh = IndexState::new(st.config.clone());
    fresh.pq = st.pq.clone();
    let mut ordered: Vec<_> = rows
        .into_iter()
        .map(|(row, (element, vector))| (element, row, vector))
        .collect();
    ordered.sort_by_key(|(element, _, _)| *element);
    for (element, row, vector) in ordered {
        fresh.next_element_id = element;
        let (actual, _) = graph_insert(&mut fresh, row, &vector)?;
        if actual != element {
            return Err(Error::storage("persistent repair changed an element id"));
        }
        if fresh.pq.is_some() {
            let matches_disk = full_vectors.get(&element) == Some(&encode_vector(&fresh.config, &vector)?);
            if !matches_disk {
                fresh.rerank_overrides.insert(
                    element,
                    decode_vector(&fresh.config, &encode_vector(&fresh.config, &vector)?)?,
                );
            }
        }
    }
    fresh.next_element_id = fresh.next_element_id.max(next);
    fresh.row_images = std::mem::take(&mut st.row_images);
    fresh.rederived_from_rows = true;
    *st = fresh;
    Ok(())
}

/// An owned lease serializes a prepared replacement with ordinary mutators
/// without holding an index-state or manager-registry lock across WriteGate.
#[derive(Default)]
struct WriteLease {
    busy: parking_lot::Mutex<bool>,
    ready: parking_lot::Condvar,
    retired: std::sync::atomic::AtomicBool,
    #[cfg(feature = "internal-tests")]
    waiters: std::sync::atomic::AtomicUsize,
}

impl WriteLease {
    fn acquire(self: &Arc<Self>) -> Result<HeldWriteLease> {
        self.acquire_or_retired()
            .ok_or_else(|| Error::storage("persistent vector index was retired"))
    }

    fn acquire_or_retired(self: &Arc<Self>) -> Option<HeldWriteLease> {
        let mut busy = self.busy.lock();
        if *busy {
            #[cfg(feature = "internal-tests")]
            self.waiters.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::runtime_offload::off_worker(|| {
                while *busy {
                    self.ready.wait(&mut busy);
                }
            });
            #[cfg(feature = "internal-tests")]
            self.waiters.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        if self.retired.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        *busy = true;
        drop(busy);
        Some(HeldWriteLease(Arc::clone(self)))
    }
}

struct HeldWriteLease(Arc<WriteLease>);

impl Drop for HeldWriteLease {
    fn drop(&mut self) {
        *self.0.busy.lock() = false;
        self.0.ready.notify_all();
    }
}

/// A full graph staged in another owner's data group. Dropping it leaves the
/// live graph unchanged; publish only after that exact group succeeds.
pub(crate) struct PreparedPersistentReplacement {
    db: Arc<DB>,
    prefix: Vec<u8>,
    before: [u8; 32],
    target: Arc<RwLock<IndexState>>,
    fresh: Option<IndexState>,
    write: HeldWriteLease,
}

fn retire_state(target: &RwLock<IndexState>, write: &HeldWriteLease) {
    write.0.retired.store(true, std::sync::atomic::Ordering::Release);
    let mut current = target.write();
    let mut empty = IndexState::new(current.config.clone());
    empty.pq = current.pq.clone();
    *current = empty;
}

impl PreparedPersistentReplacement {
    pub(crate) fn publish(mut self) {
        // No manager registry lookup: a concurrent index drop may be waiting
        // for our lease while holding that registry's write lock.
        if let Some(fresh) = self.fresh.take() {
            *self.target.write() = fresh;
        }
    }
}

impl Drop for PreparedPersistentReplacement {
    fn drop(&mut self) {
        if self.fresh.is_none() {
            return;
        } // explicitly published
        match crate::storage::VectorIndexManager::index_image_digest(&self.db, &self.prefix, |_| {}) {
            Ok(actual) if actual == self.before => {} // known unchanged
            Ok(_) => retire_state(&self.target, &self.write),
            Err(error) => {
                retire_state(&self.target, &self.write);
                tracing::error!("persistent replacement could not verify actual state: {error}; old handle retired");
            }
        }
        // The lease releases only after actual changed state has retired every
        // old handle; a waiting direct mutator cannot resurrect old records.
    }
}

/// A purge's owned retirement target, settled before registry teardown.
pub(crate) struct PersistentRetirement {
    target: Arc<RwLock<IndexState>>,
    write: HeldWriteLease,
}

impl PersistentRetirement {
    pub(crate) fn retire(self) {
        retire_state(&self.target, &self.write);
    }
}

/// A durable, crash-recoverable vector index backed by RocksDB.
pub struct PersistentVectorIndex {
    /// Written only through `gate`.
    db: Arc<DB>,
    /// The write gate of the engine that owns `db`.
    gate: Arc<WriteGate>,
    index_id: u64,
    state: Arc<RwLock<IndexState>>,
    writes: Arc<WriteLease>,
    key_manager: Option<Arc<crate::crypto::KeyManager>>,
}

impl PersistentVectorIndex {
    #[cfg(feature = "internal-tests")]
    pub(crate) fn write_waiters(&self) -> usize {
        self.writes.waiters.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Create a new, empty index (PQ inactive — full vectors resident in RAM, persisted
    /// at the configured rerank precision: F32 / F16 / I8).
    pub fn create(db: Arc<DB>, gate: Arc<WriteGate>, index_id: u64, config: PqHnswConfig) -> Result<Self> {
        let p = prefix(index_id);
        if db.get(key_meta(&p)).map_err(map_db)?.is_some() {
            return Err(Error::storage(format!(
                "vector-persist: index {index_id} already exists"
            )));
        }
        let state = IndexState::new(config);
        gate.put(&db, key_meta(&p), state.meta_bytes()?, None)?;
        Ok(Self {
            db,
            gate,
            index_id,
            state: Arc::new(RwLock::new(state)),
            writes: Arc::new(WriteLease::default()),
            key_manager: None,
        })
    }

    /// Create a PQ-backed index: train a Product Quantizer on `training` vectors,
    /// persist the codebook, and store PQ codes in RAM with full vectors on disk
    /// (faulted in only for the two-stage rerank). PQ is L2-only.
    pub fn create_with_pq(
        db: Arc<DB>,
        gate: Arc<WriteGate>,
        index_id: u64,
        mut config: PqHnswConfig,
        training: &[Vector],
    ) -> Result<Self> {
        if config.distance_metric != DistanceMetric::L2 {
            return Err(Error::storage(
                "vector-persist: PQ is supported only with the L2 metric",
            ));
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
        gate.write(&db, wb, None)?;

        Ok(Self {
            db,
            gate,
            index_id,
            state: Arc::new(RwLock::new(state)),
            writes: Arc::new(WriteLease::default()),
            key_manager: None,
        })
    }

    /// Open (recover) an existing index from RocksDB. Reconstructs the PQ codebook if
    /// the index was created with PQ. Errors if it does not exist or the on-disk schema
    /// version is unknown.
    pub fn open(db: Arc<DB>, gate: Arc<WriteGate>, index_id: u64) -> Result<Self> {
        Self::open_with_key_manager(db, gate, index_id, None)
    }

    pub(crate) fn open_with_key_manager(
        db: Arc<DB>,
        gate: Arc<WriteGate>,
        index_id: u64,
        key_manager: Option<Arc<crate::crypto::KeyManager>>,
    ) -> Result<Self> {
        let st = read_state_from_db(&db, index_id, key_manager.as_deref())?;
        Ok(Self {
            db,
            gate,
            index_id,
            state: Arc::new(RwLock::new(st)),
            writes: Arc::new(WriteLease::default()),
            key_manager,
        })
    }

    pub(crate) fn with_key_manager(mut self, key_manager: Option<Arc<crate::crypto::KeyManager>>) -> Self {
        self.key_manager = key_manager;
        self
    }

    /// Shared with mutator error settlement: read actual durable truth without
    /// performing a repair write or trusting the attempted in-memory mutation.
    fn reload_actual_state(&self) -> Result<IndexState> {
        read_state_from_db(&self.db, self.index_id, self.key_manager.as_deref())
    }

    /// Apply one replicated record of this index's keyspace to the in-memory
    /// mirror (spec P1.2), on a physical standby: there the `__vidx:` keys are
    /// the primary's, written byte for byte by the standby's applier, and this
    /// index only follows them. `suffix` is the key after `__vidx:{index_id}:`,
    /// `value` the record's post-batch value (`None`: deleted). Writes nothing
    /// and never asks the gate.
    ///
    /// Each suffix updates the structure [`Self::open`]'s per-key loading
    /// fills from it, so that a mirror maintained record by record equals the
    /// one `open` builds from the same keys:
    ///
    /// | suffix | put | delete |
    /// |---|---|---|
    /// | `meta` | `config`, `entry_point`, `next_element_id`, `layer_count`, `element_count` (schema version checked, as `open` does) | the counters of an empty index (`drop_index` deletes the whole keyspace) |
    /// | `pq` | the quantizer, rebuilt under the current `config` | cleared |
    /// | `tomb` | `tombstones` replaced | cleared |
    /// | `vec:{e}` | `vectors[e]`, decoded at the configured precision — skipped while PQ is active (full vectors stay on disk, as in `open`) | removed |
    /// | `code:{e}` | `codes[e]` | removed |
    /// | `lvl:{e}` | `levels[e]` | removed |
    /// | `adj:{layer}:{e}` | `adjacency[(layer, e)]` | removed |
    /// | `map:{e}` | `elem_to_row[e]` | removed |
    /// | `rmap:{row}` | `row_to_elem[row]` | removed |
    ///
    /// Errors: a suffix `open` does not load (named in the error), a malformed
    /// element id, a value that does not decode, an unsupported `meta` schema
    /// version. The mirror may then be partly updated; the caller replaces the
    /// index with [`Self::open`] over the same keys, which is always valid
    /// because the keys are already the primary's.
    pub fn apply_record(&self, suffix: &[u8], value: Option<&[u8]>) -> Result<()> {
        self.apply_records(std::iter::once((suffix, value)))
    }

    /// [`Self::apply_record`] for every record one replicated batch wrote to
    /// this keyspace, under ONE hold of the index lock. The primary's own
    /// mutators (`insert`, `remove`, `compact`) change the mirror under one
    /// lock hold per write group; applied record by record, a concurrent
    /// search could meet adjacency naming an element whose vector or row
    /// mapping is not in yet — or, across a `compact`, an element id already
    /// reassigned to another row. Stops at the first error (see
    /// [`Self::apply_record`]).
    pub fn apply_records<'r>(&self, records: impl IntoIterator<Item = (&'r [u8], Option<&'r [u8]>)>) -> Result<()> {
        let _write = self.writes.acquire()?;
        let mut st = self.state.write();
        let was_rederived = st.rederived_from_rows;
        let mut affected = HashSet::new();
        for (suffix, value) in records {
            if suffix.starts_with(b"row_image:") {
                let row = parse_row_image_suffix(suffix)?;
                match value {
                    Some(value) => {
                        let image =
                            decode_row_image(self.key_manager.as_deref(), &row_image_key(self.index_id, row), value)?;
                        st.row_images.insert(row, image);
                    }
                    None => {
                        st.row_images.remove(&row);
                    }
                }
                affected.insert(row);
            } else {
                if let Some(digits) = suffix.strip_prefix(b"rmap:") {
                    let row = std::str::from_utf8(digits)
                        .ok()
                        .and_then(|digits| digits.parse::<u64>().ok())
                        .ok_or_else(|| Error::storage("invalid persistent row mapping id"))?;
                    affected.insert(row);
                }
                apply_record_to(&mut st, self.index_id, suffix, value)?;
            }
        }
        let mut mismatch = was_rederived;
        for row in affected {
            if let Some(image) = st.row_images.get(&row) {
                let full = match st.row_to_elem.get(&row) {
                    Some(element) => self.db.get(key_vec(&prefix(self.index_id), *element)).map_err(map_db)?,
                    None => None,
                };
                mismatch |= !row_image_matches(&st, row, image, full.as_deref())?;
            }
        }
        if mismatch {
            // Only unfinished publication takes the full read-only repair path.
            // Normal caught-up replay validates its touched rows incrementally.
            *st = self.reload_actual_state()?;
        } else if st.row_images.is_empty() {
            st.rerank_overrides.clear();
            st.rederived_from_rows = false;
        }
        Ok(())
    }

    /// Row hooks consume the latest committed overlay, never their caller's
    /// possibly older tuple. None means a legacy/raw index without an overlay.
    pub(crate) fn latest_row_image(&self, row: u64) -> Result<Option<Option<Vector>>> {
        let key = row_image_key(self.index_id, row);
        self.db
            .get(&key)
            .map_err(map_db)?
            .map(|value| decode_row_image(self.key_manager.as_deref(), &key, &value))
            .transpose()
    }

    pub(crate) fn apply_latest_row_image(&self, row: u64) -> Result<bool> {
        let _write = self.writes.acquire()?;
        let Some(image) = self.latest_row_image(row)? else {
            return Ok(false);
        };
        {
            let mut current = self.state.write();
            let full = match current.row_to_elem.get(&row) {
                Some(element) => self.db.get(key_vec(&prefix(self.index_id), *element)).map_err(map_db)?,
                None => None,
            };
            let matches = row_image_matches(&current, row, &image, full.as_deref())?;
            current.row_images.insert(row, image.clone());
            if matches {
                return Ok(true);
            }
        }
        // Original mutators serialize each native graph group under their owned
        // lease and reload actual state on error. The authoritative row image
        // remains in its owning heap group throughout remove/insert.
        self.remove_holding_lease(row)?;
        if let Some(vector) = &image {
            self.insert_holding_lease(row, vector)?;
        }
        if self.latest_row_image(row)? != Some(image) {
            *self.state.write() = self.reload_actual_state()?;
            return Err(Error::transaction(
                "serialization failure: persistent row image changed during graph publication",
            ));
        }
        Ok(true)
    }

    pub(crate) fn retirement_target(&self) -> Option<PersistentRetirement> {
        Some(PersistentRetirement {
            target: Arc::clone(&self.state),
            write: self.writes.acquire_or_retired()?,
        })
    }

    /// Replace only RAM with the actual committed graph/row-image truth.
    /// Rollback/error reconciliation must never repair durable records here.
    pub(crate) fn reload_actual_ram(&self) -> Result<()> {
        let write = self.writes.acquire()?;
        match self.reload_actual_state() {
            Ok(actual) => {
                *self.state.write() = actual;
                Ok(())
            }
            Err(error) => {
                retire_state(&self.state, &write);
                Err(error)
            }
        }
    }

    /// Share the existing per-index state and write lease for preparation after
    /// the manager registry guard has been released. No second cache is opened.
    pub(crate) fn replacement_handle(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            gate: Arc::clone(&self.gate),
            index_id: self.index_id,
            state: Arc::clone(&self.state),
            writes: Arc::clone(&self.writes),
            key_manager: self.key_manager.clone(),
        }
    }

    /// Prepare a complete replacement from current logical table rows, stage
    /// every old-key delete and fresh record into the caller's group, and keep
    /// the existing live state untouched until explicit publication. Preserves
    /// construction config, quantizer/codebook and rerank precision.
    pub(crate) fn stage_replacement(
        &self,
        rows: &[(u64, Vector)],
        batch: &mut WriteBatch,
    ) -> Result<PreparedPersistentReplacement> {
        let write = self.writes.acquire()?;
        let mut fresh = {
            let current = self.state.read();
            if current.config.pq_enabled != current.pq.is_some() {
                return Err(Error::storage(
                    "persistent replacement has inconsistent PQ metadata/codebook",
                ));
            }
            let mut fresh = IndexState::new(current.config.clone());
            fresh.pq = current.pq.clone();
            fresh
        };
        let mut seen = HashSet::new();
        for (row, vector) in rows {
            if vector.len() != fresh.config.dimension {
                return Err(Error::query_execution(format!(
                    "vector dimension mismatch: expected {}, got {}",
                    fresh.config.dimension,
                    vector.len()
                )));
            }
            if !seen.insert(*row) {
                return Err(Error::storage("persistent replacement contains a duplicate row id"));
            }
            graph_insert(&mut fresh, *row, vector)?;
        }
        let p = prefix(self.index_id);
        let before =
            crate::storage::VectorIndexManager::index_image_digest(&self.db, p.as_bytes(), |key| batch.delete(key))?;
        batch.put(key_meta(&p), fresh.meta_bytes()?);
        if let Some(pq) = fresh.pq.as_ref() {
            batch.put(key_pq(&p), ser(&*pq.codebook())?);
        }
        for (&element, &row) in &fresh.elem_to_row {
            batch.put(key_map(&p, element), ser(&row)?);
            batch.put(key_rmap(&p, row), ser(&element)?);
            if let Some(level) = fresh.levels.get(&element) {
                batch.put(key_lvl(&p, element), ser(level)?);
            }
            if let Some(code) = fresh.codes.get(&element) {
                batch.put(key_code(&p, element), ser(code)?);
            }
        }
        // Full vectors must ride this group even under PQ: rerank faults them
        // from these keys, so a fresh code can never name an old vector image.
        for (row, vector) in rows {
            let element = fresh
                .row_to_elem
                .get(row)
                .ok_or_else(|| Error::storage("persistent replacement omitted a row mapping"))?;
            batch.put(key_vec(&p, *element), encode_vector(&fresh.config, vector)?);
        }
        for (&(layer, element), neighbours) in &fresh.adjacency {
            batch.put(key_adj(&p, layer, element), ser(neighbours)?);
        }
        Ok(PreparedPersistentReplacement {
            db: Arc::clone(&self.db),
            prefix: p.into_bytes(),
            before,
            target: Arc::clone(&self.state),
            fresh: Some(fresh),
            write,
        })
    }

    /// Whether an index with the given id exists in the store.
    pub fn exists(db: &DB, index_id: u64) -> Result<bool> {
        Ok(db.get(key_meta(&prefix(index_id))).map_err(map_db)?.is_some())
    }

    /// Delete every key belonging to an index, through the gate of the engine
    /// that owns `db`.
    pub fn drop_index(db: &DB, gate: &WriteGate, index_id: u64) -> Result<()> {
        let p = prefix(index_id);
        let mut wb = WriteBatch::default();
        crate::storage::VectorIndexManager::index_image_digest(db, p.as_bytes(), |key| wb.delete(key))?;
        gate.write(db, wb, None)?;
        Ok(())
    }

    /// Drop this index's persisted RocksDB keyspace.
    pub fn drop_storage(&self) -> Result<()> {
        let Some(write) = self.writes.acquire_or_retired() else {
            return Ok(());
        };
        let p = prefix(self.index_id);
        let mut batch = WriteBatch::default();
        let before =
            crate::storage::VectorIndexManager::index_image_digest(&self.db, p.as_bytes(), |key| batch.delete(key))?;
        let outcome = self.gate.write(&self.db, batch, None);
        let retire = if outcome.is_ok() {
            true
        } else {
            match crate::storage::VectorIndexManager::index_image_digest(&self.db, p.as_bytes(), |_| {}) {
                Ok(actual) => actual != before,
                Err(error) => {
                    tracing::error!("persistent drop could not verify actual state: {error}; old handle retired");
                    true
                }
            }
        };
        if retire {
            retire_state(&self.state, &write);
        }
        outcome
    }

    /// Store an element's rerank vector, row mapping, and level (no graph edges).
    pub fn put_vector(&self, elem_id: ElementId, row_id: u64, vector: &Vector, level: u32) -> Result<()> {
        let _write = self.writes.acquire()?;
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

        let resident = st.resident_vector(vector)?;
        st.vectors.insert(elem_id, resident);
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
        self.apply_prepared_batch(&mut st, wb)?;
        Ok(())
    }

    /// Native errors may have applied. Reconstruct actual graph/row-image
    /// truth before releasing the write lease, so neither a cap refusal nor an
    /// ambiguous durability error leaves a successfully queryable partial RAM
    /// mutation. The parser is shared with open/physical hydration.
    fn apply_prepared_batch(&self, state: &mut IndexState, batch: WriteBatch) -> Result<()> {
        match self.gate.write(&self.db, batch, None) {
            Ok(()) => Ok(()),
            Err(error) => {
                match self.reload_actual_state() {
                    Ok(actual) => *state = actual,
                    Err(reload) => {
                        self.writes.retired.store(true, std::sync::atomic::Ordering::Release);
                        let mut empty = IndexState::new(state.config.clone());
                        empty.pq = state.pq.clone();
                        *state = empty;
                        tracing::error!("persistent mutation could not reload actual state: {reload}; handle retired");
                    }
                }
                Err(error)
            }
        }
    }

    /// Store an element's neighbor list at a given layer.
    pub fn put_adjacency(&self, layer: u32, elem_id: ElementId, neighbors: Vec<ElementId>) -> Result<()> {
        let _write = self.writes.acquire()?;
        let mut st = self.state.write();
        let p = prefix(self.index_id);
        let mut wb = WriteBatch::default();
        wb.put(key_adj(&p, layer, elem_id), ser(&neighbors)?);
        st.adjacency.insert((layer, elem_id), neighbors);
        if (layer as usize) + 1 > st.layer_count {
            st.layer_count = layer as usize + 1;
            wb.put(key_meta(&p), st.meta_bytes()?);
        }
        self.apply_prepared_batch(&mut st, wb)?;
        Ok(())
    }

    /// Set (or clear) the graph entry point.
    pub fn set_entry_point(&self, ep: Option<ElementId>) -> Result<()> {
        let _write = self.writes.acquire()?;
        let mut st = self.state.write();
        st.entry_point = ep;
        let mut batch = WriteBatch::default();
        batch.put(key_meta(&prefix(self.index_id)), st.meta_bytes()?);
        self.apply_prepared_batch(&mut st, batch)?;
        Ok(())
    }

    /// Mark an element as soft-deleted (excluded from search; reclaimed by `compact`).
    pub fn mark_tombstone(&self, elem_id: ElementId) -> Result<()> {
        let _write = self.writes.acquire()?;
        let mut st = self.state.write();
        st.tombstones.insert(elem_id);
        let set: Vec<ElementId> = st.tombstones.iter().copied().collect();
        let mut batch = WriteBatch::default();
        batch.put(key_tomb(&prefix(self.index_id)), ser(&set)?);
        self.apply_prepared_batch(&mut st, batch)?;
        Ok(())
    }

    // ── Graph build / search / delete ────────────────────────────────────────

    /// Insert a vector into the graph. Persists the full vector (for rerank) plus, when
    /// PQ is active, its code; the new element's keys + every touched adjacency list +
    /// metadata are flushed atomically in one batch. Returns the assigned element id.
    pub fn insert(&self, row_id: u64, vector: &Vector) -> Result<ElementId> {
        let _write = self.writes.acquire()?;
        self.insert_holding_lease(row_id, vector)
    }

    fn insert_holding_lease(&self, row_id: u64, vector: &Vector) -> Result<ElementId> {
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
        self.apply_prepared_batch(&mut st, wb)?;
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
        if self.writes.retired.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::storage("persistent vector index was retired"));
        }

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
                let dist = if let Some(vector) = st.rerank_overrides.get(&c.id) {
                    metric_dist(metric, query, vector)
                } else {
                    match self.db.get(key_vec(&p, c.id)).map_err(map_db)? {
                        Some(bytes) => metric_dist(metric, query, &decode_vector(&st.config, &bytes)?),
                        None => c.dist,
                    }
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
        let _write = self.writes.acquire()?;
        self.remove_holding_lease(row_id)
    }

    fn remove_holding_lease(&self, row_id: u64) -> Result<bool> {
        let mut st = self.state.write();
        let Some(elem) = st.row_to_elem.get(&row_id).copied() else {
            return Ok(false);
        };
        let level = st.levels.get(&elem).copied().unwrap_or(0);
        let p = prefix(self.index_id);
        let mut wb = WriteBatch::default();
        let mut touched: Vec<(u32, ElementId)> = Vec::new();

        for layer in 0..=level {
            let x_nbrs: Vec<ElementId> = st.adjacency.get(&(layer, elem)).cloned().unwrap_or_default();
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
                let pool_ids: Vec<ElementId> = pool.into_iter().filter(|c| !st.tombstones.contains(c)).collect();
                let new_list = st.select_neighbors(&nb_vec, &pool_ids, cap);
                st.adjacency.insert((layer, nb), new_list);
                touched.push((layer, nb));
            }

            st.adjacency.remove(&(layer, elem));
            wb.delete(key_adj(&p, layer, elem));
        }

        // Drop the element's own data (both resident forms + on-disk keys).
        st.vectors.remove(&elem);
        st.rerank_overrides.remove(&elem);
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
        self.apply_prepared_batch(&mut st, wb)?;
        Ok(true)
    }

    /// Rebuild the graph from scratch over the surviving (non-tombstoned) elements,
    /// reclaiming space and clearing tombstones; persisted atomically. Preserves the PQ
    /// codebook (when active), reloading full vectors from disk to re-encode.
    pub fn compact(&self) -> Result<()> {
        let _write = self.writes.acquire()?;
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
            let full = if let Some(v) = st.row_images.get(&row).and_then(Option::as_ref) {
                v.clone()
            } else if let Some(v) = st.rerank_overrides.get(&elem) {
                v.clone()
            } else if let Some(v) = st.vectors.get(&elem) {
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
        self.apply_prepared_batch(&mut st, wb)?;
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

    fn raw_index_image(db: &DB, id: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
        let prefix = prefix(id).into_bytes();
        let mut options = rocksdb::ReadOptions::default();
        options.set_total_order_seek(true);
        db.iterator_opt(IteratorMode::From(&prefix, Direction::Forward), options)
            .map(|item| item.unwrap())
            .take_while(|(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| (key.to_vec(), value.to_vec()))
            .collect()
    }

    #[test]
    fn row_image_keys_and_payloads_fail_closed_without_repair_writes() {
        for suffix in [
            "row_image:1:extra",
            "row_image:01",
            "row_image:+1",
            "row_image:",
            "row_image:18446744073709551616",
        ] {
            let (_dir, db) = test_db();
            let index = PersistentVectorIndex::create(Arc::clone(&db), WriteGate::open(), 91, sample_config()).unwrap();
            index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();
            let value = encode_row_image(None, Some(&vec![0.0, 1.0, 0.0])).unwrap();
            db.put(row_image_key(91, 1), &value).unwrap();
            db.put(format!("{}{suffix}", prefix(91)), &value).unwrap();
            let before = raw_index_image(&db, 91);
            let sequence = db.latest_sequence_number();
            assert!(
                PersistentVectorIndex::open(Arc::clone(&db), WriteGate::closed("read-only"), 91).is_err(),
                "{suffix}"
            );
            assert!(index.apply_record(suffix.as_bytes(), Some(&value)).is_err(), "{suffix}");
            assert_eq!(db.latest_sequence_number(), sequence);
            assert_eq!(raw_index_image(&db, 91), before);
        }
        let key = row_image_key(91, 1);
        let mut trailing = encode_row_image(None, Some(&vec![0.0, 1.0, 0.0])).unwrap();
        trailing.push(0);
        assert!(decode_row_image(None, &key, &trailing).is_err());
        let unsupported = ser(&RowImageV1 {
            version: 2,
            vector: None,
        })
        .unwrap();
        assert!(decode_row_image(None, &key, &unsupported)
            .unwrap_err()
            .to_string()
            .contains("version 2"));
    }

    #[cfg(feature = "encryption")]
    #[test]
    fn row_images_are_sealed_and_refuse_a_different_key() {
        let key = crate::crypto::KeyManager::generate_random();
        let other = crate::crypto::KeyManager::generate_random();
        let row_key = row_image_key(17, 1);
        let vector = vec![17.125, -89.25, 0.0625];
        let plain = ser(&RowImageV1 {
            version: 1,
            vector: Some(vector.clone()),
        })
        .unwrap();
        let sealed = encode_row_image(Some(&key), Some(&vector)).unwrap();
        assert_ne!(sealed, plain);
        assert!(!sealed.windows(plain.len()).any(|bytes| bytes == plain));
        assert_eq!(decode_row_image(Some(&key), &row_key, &sealed).unwrap(), Some(vector));
        assert!(decode_row_image(Some(&other), &row_key, &sealed).is_err());
        assert!(decode_row_image(None, &row_key, &sealed).is_err());
        let tombstone = encode_row_image(Some(&key), None).unwrap();
        assert_eq!(decode_row_image(Some(&key), &row_key, &tombstone).unwrap(), None);
    }

    #[test]
    fn unfinished_row_images_preserve_other_element_ids_and_late_hooks_take_latest_truth() {
        let (_dir, db) = test_db();
        let index = PersistentVectorIndex::create(Arc::clone(&db), WriteGate::open(), 92, sample_config()).unwrap();
        for row in 1..=3 {
            index.insert(row, &vec![row as f32, 0.0, 0.0]).unwrap();
        }
        let other_id = index.elem_of(3).unwrap();
        let other_key = key_vec(&prefix(92), other_id);
        let other_value = db.get(&other_key).unwrap().unwrap();
        db.put(
            row_image_key(92, 1),
            encode_row_image(None, Some(&vec![9.0, 0.0, 0.0])).unwrap(),
        )
        .unwrap();
        db.put(row_image_key(92, 2), encode_row_image(None, None).unwrap())
            .unwrap();
        db.put(
            row_image_key(92, 4),
            encode_row_image(None, Some(&vec![4.0, 0.0, 0.0])).unwrap(),
        )
        .unwrap();
        // An interrupted incremental publication removed an old mapping but
        // never inserted its replacement. Other vector IDs remain durable.
        index.remove(1).unwrap();
        let before = raw_index_image(&db, 92);
        let sequence = db.latest_sequence_number();
        let recovered = PersistentVectorIndex::open(Arc::clone(&db), WriteGate::open(), 92).unwrap();
        assert_eq!(recovered.len(), 3);
        assert_eq!(recovered.elem_of(3), Some(other_id));
        assert!(recovered.elem_of(1).unwrap() > other_id);
        assert!(recovered.elem_of(4).unwrap() > other_id);
        assert_eq!(recovered.elem_of(2), None);
        assert_eq!(recovered.search(&vec![9.0, 0.0, 0.0], 1, 32).unwrap()[0].0, 1);
        assert_eq!(raw_index_image(&db, 92), before, "recovery is RAM only");
        assert_eq!(db.latest_sequence_number(), sequence);
        // Two successive owning row groups may beat the first delayed hook.
        // That hook must read the second durable image, never its old caller.
        db.put(
            row_image_key(92, 1),
            encode_row_image(None, Some(&vec![10.0, 0.0, 0.0])).unwrap(),
        )
        .unwrap();
        assert!(recovered.apply_latest_row_image(1).unwrap());
        assert_eq!(db.get(&other_key).unwrap().unwrap(), other_value);
        let reopened = PersistentVectorIndex::open(Arc::clone(&db), WriteGate::closed("read-only"), 92).unwrap();
        assert_eq!(reopened.search(&vec![10.0, 0.0, 0.0], 1, 32).unwrap()[0].0, 1);
        assert_eq!(reopened.elem_of(3), Some(other_id));
        assert!(reopened.elem_of(2).is_none());
    }

    #[test]
    fn row_image_recovery_and_apply_keep_pq_precision_and_bytes() {
        for pq in [false, true] {
            for precision in [VectorPrecision::F32, VectorPrecision::F16, VectorPrecision::I8] {
                let (_dir, db) = test_db();
                let mut config = PqHnswConfig::new(4, DistanceMetric::L2);
                config.rerank_precision = precision;
                config.pq_config = pq.then_some(ProductQuantizerConfig {
                    num_subquantizers: 2,
                    num_centroids: 4,
                    dimension: 4,
                    training_iterations: 2,
                    min_training_samples: 8,
                });
                let training: Vec<_> = (0..32).map(|row| rand_vec(row, 4)).collect();
                let index = if pq {
                    PersistentVectorIndex::create_with_pq(Arc::clone(&db), WriteGate::open(), 93, config, &training)
                        .unwrap()
                } else {
                    PersistentVectorIndex::create(Arc::clone(&db), WriteGate::open(), 93, config).unwrap()
                };
                index.insert(1, &training[0]).unwrap();
                index.insert(2, &training[1]).unwrap();
                let config = ser(&index.config()).unwrap();
                let codebook = db.get(key_pq(&prefix(93))).unwrap();
                let value = encode_row_image(None, Some(&vec![7.25, -2.5, 1.125, 3.0])).unwrap();
                db.put(row_image_key(93, 1), &value).unwrap();
                let tombstone = encode_row_image(None, None).unwrap();
                db.put(row_image_key(93, 2), &tombstone).unwrap();
                let image = raw_index_image(&db, 93);
                let sequence = db.latest_sequence_number();
                index
                    .apply_records([
                        (b"row_image:1".as_slice(), Some(value.as_slice())),
                        (b"row_image:2".as_slice(), Some(tombstone.as_slice())),
                    ])
                    .unwrap();
                let readonly =
                    PersistentVectorIndex::open(Arc::clone(&db), WriteGate::closed("read-only"), 93).unwrap();
                let query = vec![7.25, -2.5, 1.125, 3.0];
                assert_eq!(
                    index.search(&query, 8, 64).unwrap(),
                    readonly.search(&query, 8, 64).unwrap()
                );
                assert_eq!(readonly.len(), 1);
                assert_eq!(readonly.elem_of(2), None);
                assert_eq!(ser(&readonly.config()).unwrap(), config);
                assert_eq!(readonly.pq_active(), pq);
                assert_eq!(db.get(key_pq(&prefix(93))).unwrap(), codebook);
                assert_eq!(raw_index_image(&db, 93), image);
                assert_eq!(db.latest_sequence_number(), sequence);
            }
        }
    }

    #[test]
    fn full_replacement_is_staged_without_publication_and_preserves_pq_and_precision() {
        for pq in [false, true] {
            for precision in [VectorPrecision::F32, VectorPrecision::F16, VectorPrecision::I8] {
                let (_dir, db) = test_db();
                let gate = WriteGate::open();
                let mut config = PqHnswConfig::new(4, DistanceMetric::L2);
                config.m = 4;
                config.m0 = 8;
                config.ef_construction = 32;
                config.rerank_precision = precision;
                config.pq_config = pq.then_some(ProductQuantizerConfig {
                    num_subquantizers: 2,
                    num_centroids: 4,
                    dimension: 4,
                    training_iterations: 2,
                    min_training_samples: 8,
                });
                let training: Vec<_> = (0..32).map(|row| rand_vec(row, 4)).collect();
                let index = if pq {
                    PersistentVectorIndex::create_with_pq(Arc::clone(&db), Arc::clone(&gate), 77, config, &training)
                        .unwrap()
                } else {
                    PersistentVectorIndex::create(Arc::clone(&db), Arc::clone(&gate), 77, config).unwrap()
                };
                index.insert(1, &training[0]).unwrap();
                let before_config = ser(&index.config()).unwrap();
                let before_codebook = db.get(key_pq(&prefix(77))).unwrap();
                let before_sequence = db.latest_sequence_number();
                let rows: Vec<_> = (100..108).map(|row| (row, rand_vec(row, 4))).collect();
                let mut batch = WriteBatch::default();
                let prepared = index.stage_replacement(&rows, &mut batch).unwrap();
                assert_eq!(
                    db.latest_sequence_number(),
                    before_sequence,
                    "preparation writes nothing"
                );
                assert!(index.elem_of(1).is_some(), "old live state remains until publication");
                assert!(index.elem_of(100).is_none());
                assert_eq!(index.len(), 1);
                gate.write(&db, batch, None).unwrap();
                prepared.publish();
                assert_eq!(index.len(), rows.len());
                assert!(index.elem_of(1).is_none());
                assert_eq!(ser(&index.config()).unwrap(), before_config);
                assert_eq!(
                    db.get(key_pq(&prefix(77))).unwrap(),
                    before_codebook,
                    "codebook is preserved"
                );
                assert_eq!(index.pq_active(), pq);
                let answer = index.search(&rows[2].1, 8, 64).unwrap();
                assert_eq!(answer.len(), rows.len());
                assert!(answer.iter().all(|(row, _)| *row >= 100));
                let current_sequence = db.latest_sequence_number();
                let readonly =
                    PersistentVectorIndex::open(Arc::clone(&db), WriteGate::closed("read-only vector"), 77).unwrap();
                assert_eq!(readonly.search(&rows[2].1, 8, 64).unwrap(), answer);
                assert_eq!(ser(&readonly.config()).unwrap(), before_config);
                assert_eq!(
                    db.latest_sequence_number(),
                    current_sequence,
                    "read-only reconstruction changes no bytes"
                );
            }
        }
    }

    #[test]
    fn rejected_replacement_preserves_live_handle_and_applied_discard_prevents_resurrection() {
        let (_dir, db) = test_db();
        let gate = WriteGate::open();
        let index = PersistentVectorIndex::create(Arc::clone(&db), Arc::clone(&gate), 88, sample_config()).unwrap();
        index.insert(1, &vec![1.0, 0.0, 0.0]).unwrap();
        let rows = vec![(10, vec![0.0, 1.0, 0.0]), (11, vec![0.0, 0.0, 1.0])];
        let before = db.latest_sequence_number();
        let mut batch = WriteBatch::default();
        let prepared = index.stage_replacement(&rows, &mut batch).unwrap();
        gate.debug_set_write_group_ceiling(Some(64));
        assert!(gate.write(&db, batch, None).is_err());
        drop(prepared);
        gate.debug_set_write_group_ceiling(None);
        assert_eq!(db.latest_sequence_number(), before);
        assert_eq!(index.search(&vec![1.0, 0.0, 0.0], 1, 16).unwrap()[0].0, 1);
        index.insert(2, &vec![0.0, 0.0, 1.0]).unwrap();
        let mut batch = WriteBatch::default();
        let prepared = index.stage_replacement(&rows, &mut batch).unwrap();
        gate.write(&db, batch, None).unwrap();
        let applied = db.latest_sequence_number();
        drop(prepared); // models an applied native error before normal publication
        assert!(index
            .insert(99, &vec![1.0, 1.0, 1.0])
            .unwrap_err()
            .to_string()
            .contains("retired"));
        assert!(index.search(&vec![0.0, 1.0, 0.0], 1, 16).is_err());
        assert_eq!(
            db.latest_sequence_number(),
            applied,
            "retirement never writes an old graph over actual bytes"
        );
        let reopened = PersistentVectorIndex::open(Arc::clone(&db), WriteGate::closed("read-only vector"), 88).unwrap();
        assert_eq!(reopened.len(), 2);
        assert!(reopened.elem_of(10).is_some());
        assert!(reopened.elem_of(11).is_some());
        assert!(reopened.elem_of(99).is_none());
        assert_eq!(db.latest_sequence_number(), applied);
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
        let mut all: Vec<(usize, f32)> = data.iter().enumerate().map(|(i, v)| (i, l2(q, v))).collect();
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
            let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 7, sample_config()).unwrap();
            populate(&idx);
            assert_eq!(idx.len(), 3);
            assert_eq!(idx.entry_point(), Some(0));
        }
        let reopened = PersistentVectorIndex::open(db.clone(), WriteGate::open(), 7).unwrap();
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
            let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 1, sample_config()).unwrap();
            populate(&idx);
        }
        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, WriteGate::open(), 1).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.entry_point(), Some(0));
        assert_eq!(idx.vector(2), Some(vec![0.0, 0.0, 1.0]));
        assert_eq!(idx.neighbors(0, 0), Some(vec![1, 2]));
        assert!(idx.is_tombstoned(1));
    }

    #[test]
    fn test_open_missing_is_err() {
        let (_dir, db) = test_db();
        assert!(PersistentVectorIndex::open(db.clone(), WriteGate::open(), 999).is_err());
        assert!(!PersistentVectorIndex::exists(&db, 999).unwrap());
    }

    #[test]
    fn test_create_duplicate_is_err() {
        let (_dir, db) = test_db();
        let _idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 5, sample_config()).unwrap();
        assert!(PersistentVectorIndex::create(db.clone(), WriteGate::open(), 5, sample_config()).is_err());
        assert!(PersistentVectorIndex::exists(&db, 5).unwrap());
    }

    #[test]
    fn test_drop_index_removes_keys() {
        let (_dir, db) = test_db();
        let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 3, sample_config()).unwrap();
        populate(&idx);
        drop(idx);
        PersistentVectorIndex::drop_index(&db, &WriteGate::open(), 3).unwrap();
        assert!(!PersistentVectorIndex::exists(&db, 3).unwrap());
        assert!(PersistentVectorIndex::open(db.clone(), WriteGate::open(), 3).is_err());
    }

    #[test]
    fn test_two_indexes_isolated() {
        let (_dir, db) = test_db();
        let a = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 1, sample_config()).unwrap();
        let b = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 2, sample_config()).unwrap();
        a.put_vector(0, 10, &vec![1.0, 2.0, 3.0], 0).unwrap();
        b.put_vector(0, 20, &vec![4.0, 5.0, 6.0], 0).unwrap();
        let a2 = PersistentVectorIndex::open(db.clone(), WriteGate::open(), 1).unwrap();
        let b2 = PersistentVectorIndex::open(db.clone(), WriteGate::open(), 2).unwrap();
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
        let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 1, sample_config()).unwrap();
        assert!(idx.put_vector(0, 1, &vec![1.0, 2.0], 0).is_err());
    }

    #[test]
    fn test_exact_f16_index_roundtrips() {
        // f16 rerank precision is supported (P6); an exact-mode f16 index is searchable.
        let (_dir, db) = test_db();
        let mut cfg = PqHnswConfig::new(8, DistanceMetric::L2);
        cfg.rerank_precision = VectorPrecision::F16;
        let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 1, cfg).unwrap();
        for i in 0..50u64 {
            idx.insert(i, &rand_vec(i, 8)).unwrap();
        }
        let got = idx.search(&rand_vec(3, 8), 5, 64).unwrap();
        assert_eq!(got.len(), 5);
        assert!(
            got.iter().any(|(r, _)| *r == 3),
            "exact match should rank near top under f16"
        );
    }

    // ── P2: HNSW graph ───────────────────────────────────────────────────────

    #[test]
    fn test_graph_search_finds_self() {
        let (_dir, db) = test_db();
        let cfg = PqHnswConfig::new(4, DistanceMetric::L2);
        let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 1, cfg).unwrap();
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
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
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
                WriteGate::open(),
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
        let idx = PersistentVectorIndex::open(db, WriteGate::open(), 1).unwrap();
        let after = idx.search(&q, 5, 64).unwrap();
        assert_eq!(before, after, "search must be identical after crash-recovery reopen");
    }

    // ── P3: online deletes + compaction ──────────────────────────────────────

    #[test]
    fn test_remove_excludes_and_keeps_searchable() {
        let (_dir, db) = test_db();
        let dim = 8;
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
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
                WriteGate::open(),
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
        let idx = PersistentVectorIndex::open(db, WriteGate::open(), 1).unwrap();
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
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
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
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
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
            PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, pq_config_dim64(), &data).unwrap();
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
            let idx = PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, pq_config_dim64(), &data)
                .unwrap();
            for (i, v) in data.iter().enumerate() {
                idx.insert(i as u64, v).unwrap();
            }
            idx.search(&q, 10, 100).unwrap()
        };

        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, WriteGate::open(), 1).unwrap();
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
            PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, pq_config_dim64(), &data).unwrap();
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
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
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
            PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, pq_config_dim64(), &data).unwrap();
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
        assert!(
            recall >= 0.70,
            "PQ filtered recall@{k} = {recall:.3} (expected >= 0.70)"
        );
    }

    #[test]
    fn test_filtered_beats_postfilter_completeness() {
        // The motivating case: a selective filter where post-filtering an unfiltered
        // top-k loses results that predicate-during-traversal retains.
        let (_dir, db) = test_db();
        let (dim, n) = (16usize, 1000usize);
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
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
        let idx = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
        .unwrap();
        for i in 0..50u64 {
            idx.insert(i, &rand_vec(i, dim)).unwrap();
        }
        let got = idx.search_filtered(&rand_vec(9, dim), 10, 64, |_row| false).unwrap();
        assert!(got.is_empty(), "a filter matching nothing yields no results");
    }

    // ── P6: multi-precision rerank vectors ───────────────────────────────────

    #[test]
    fn test_f16_roundtrip_accuracy() {
        for &x in &[
            0.0f32, 1.0, -1.0, 0.5, -0.25, 3.141_59, -2.718_28, 100.0, 0.001, 1000.0, -42.5,
        ] {
            let back = f16_to_f32(f32_to_f16(x));
            let tol = x.abs() * 1e-2 + 1e-3;
            assert!((back - x).abs() <= tol, "f16 roundtrip {x} -> {back}");
        }
        // Exactly representable values are exact.
        assert_eq!(f16_to_f32(f32_to_f16(0.0)), 0.0);
        assert_eq!(f16_to_f32(f32_to_f16(1.0)), 1.0);
        assert_eq!(f16_to_f32(f32_to_f16(0.5)), 0.5);
    }

    #[test]
    fn test_precision_encode_sizes() {
        let v: Vec<f32> = (0..64).map(|i| (i as f32) * 0.013 - 0.4).collect();
        let mk = |p| {
            let mut c = PqHnswConfig::new(64, DistanceMetric::L2);
            c.rerank_precision = p;
            c
        };
        let s32 = encode_vector(&mk(VectorPrecision::F32), &v).unwrap().len();
        let s16 = encode_vector(&mk(VectorPrecision::F16), &v).unwrap().len();
        let s8 = encode_vector(&mk(VectorPrecision::I8), &v).unwrap().len();
        assert!(s16 < s32, "f16 {s16} should be smaller than f32 {s32}");
        assert!(s8 < s16, "i8 {s8} should be smaller than f16 {s16}");

        let d16 = decode_vector(
            &mk(VectorPrecision::F16),
            &encode_vector(&mk(VectorPrecision::F16), &v).unwrap(),
        )
        .unwrap();
        let d8 = decode_vector(
            &mk(VectorPrecision::I8),
            &encode_vector(&mk(VectorPrecision::I8), &v).unwrap(),
        )
        .unwrap();
        assert_eq!(d16.len(), 64);
        assert_eq!(d8.len(), 64);
        for i in 0..64 {
            assert!((d16[i] - v[i]).abs() < 0.01, "f16 dim {i}");
            assert!((d8[i] - v[i]).abs() < 0.02, "i8 dim {i}");
        }
    }

    #[test]
    fn test_pq_f16_and_i8_rerank_recall() {
        for (prec, thresh) in [(VectorPrecision::F16, 0.70f64), (VectorPrecision::I8, 0.60f64)] {
            let (_dir, db) = test_db();
            let (dim, n, k, ef) = (64usize, 600usize, 10usize, 100usize);
            let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
            let mut cfg = pq_config_dim64();
            cfg.rerank_precision = prec;
            let idx = PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, cfg, &data).unwrap();
            for (i, v) in data.iter().enumerate() {
                idx.insert(i as u64, v).unwrap();
            }
            let mut hits = 0usize;
            let queries = 40u64;
            for qi in 0..queries {
                let q = rand_vec(8_000_000 + qi, dim);
                let truth: HashSet<u64> = brute_topk(&data, &q, k).into_iter().collect();
                let got = idx.search(&q, k, ef).unwrap();
                hits += got.iter().filter(|(r, _)| truth.contains(r)).count();
            }
            let recall = hits as f64 / (queries as usize * k) as f64;
            assert!(
                recall >= thresh,
                "PQ {prec:?} rerank recall@{k} = {recall:.3} (>= {thresh})"
            );
        }
    }

    #[test]
    fn test_i8_exact_index_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let dim = 16;
        let q = rand_vec(50, dim);

        let before = {
            let db = Arc::new(DB::open_default(&path).unwrap());
            let mut cfg = PqHnswConfig::new(dim, DistanceMetric::L2);
            cfg.rerank_precision = VectorPrecision::I8;
            let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 1, cfg).unwrap();
            for i in 0..100u64 {
                idx.insert(i, &rand_vec(i, dim)).unwrap();
            }
            idx.search(&q, 5, 64).unwrap()
        };

        let db = Arc::new(DB::open_default(&path).unwrap());
        let idx = PersistentVectorIndex::open(db, WriteGate::open(), 1).unwrap();
        let after = idx.search(&q, 5, 64).unwrap();
        assert_eq!(before, after, "i8 exact-mode search must match across reopen");
    }

    #[test]
    fn test_pq_default_config_recall_is_safe() {
        // Regression: create_with_pq with the DEFAULT pq_config (None) must not collapse
        // recall. The old default_for_dimension heuristic picked far too few sub-quantizers
        // (dim 128 -> 2, i.e. 64 dims/sub-vector), measuring ~0.14 recall@10; the recall-safe
        // default (~dim/4) keeps it on par with exact. All other PQ tests pass an explicit
        // config, so this exercises the default path that shipped the footgun.
        let (_dir, db) = test_db();
        let (dim, n, k, ef) = (128usize, 1500usize, 10usize, 100usize);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        let cfg = PqHnswConfig::new(dim, DistanceMetric::L2); // pq_config = None on purpose
        let idx = PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, cfg, &data).unwrap();
        assert!(
            idx.config().pq_config.is_some(),
            "create_with_pq should fill in a config"
        );
        for (i, v) in data.iter().enumerate() {
            idx.insert(i as u64, v).unwrap();
        }
        let mut hits = 0usize;
        let queries = 50u64;
        for qi in 0..queries {
            let q = rand_vec(9_900_000 + qi, dim);
            let truth: HashSet<u64> = brute_topk(&data, &q, k).into_iter().collect();
            hits += idx
                .search(&q, k, ef)
                .unwrap()
                .iter()
                .filter(|(r, _)| truth.contains(r))
                .count();
        }
        let recall = hits as f64 / (queries as usize * k) as f64;
        assert!(
            recall >= 0.75,
            "default-config PQ recall@{k} = {recall:.3} (expected >= 0.75; old heuristic gave ~0.14)"
        );
    }

    // ── Spec P1.2: a physical standby's follower ─────────────────────────────

    /// Every key of index `index_id`'s keyspace with its value.
    fn keyspace(db: &DB, index_id: u64) -> std::collections::BTreeMap<Vec<u8>, Vec<u8>> {
        let p = prefix(index_id);
        db.iterator(IteratorMode::From(p.as_bytes(), Direction::Forward))
            .map(|item| item.unwrap())
            .take_while(|(k, _)| k.starts_with(p.as_bytes()))
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect()
    }

    /// Feed `follower` what changed between two keyspace states, as a
    /// standby's applier delivers a batch: each changed key once, in key
    /// order, with its post-batch value.
    fn replay(
        follower: &PersistentVectorIndex,
        before: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
        after: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    ) {
        let p = prefix(follower.index_id());
        let keys: std::collections::BTreeSet<&Vec<u8>> = before.keys().chain(after.keys()).collect();
        let records: Vec<(Vec<u8>, Option<Vec<u8>>)> = keys
            .into_iter()
            .filter(|key| before.get(*key) != after.get(*key))
            .map(|key| (key.get(p.len()..).unwrap().to_vec(), after.get(key).cloned()))
            .collect();
        follower
            .apply_records(records.iter().map(|(s, v)| (s.as_slice(), v.as_deref())))
            .unwrap();
    }

    /// Everything a search and the accessors expose, for an equality check.
    fn observed(idx: &PersistentVectorIndex, dim: usize) -> Vec<String> {
        let mut out = vec![format!(
            "ep {:?} layers {} next {} len {} tomb {:?}",
            idx.entry_point(),
            idx.layer_count(),
            idx.next_element_id(),
            idx.len(),
            idx.tombstones()
        )];
        for qi in 0..12u64 {
            out.push(format!(
                "{:?}",
                idx.search(&rand_vec(4_000_000 + qi, dim), 7, 48).unwrap()
            ));
        }
        out
    }

    #[test]
    fn test_apply_records_follows_a_keyspace_it_never_writes() {
        let (_dir, db) = test_db();
        let dim = 8;
        let leader = PersistentVectorIndex::create(
            db.clone(),
            WriteGate::open(),
            1,
            PqHnswConfig::new(dim, DistanceMetric::L2),
        )
        .unwrap();
        // The follower shares the store, as a standby's engine shares its
        // applier's, behind a closed gate it must never ask.
        let gate = WriteGate::closed("follower");
        let follower = PersistentVectorIndex::open(db.clone(), Arc::clone(&gate), 1).unwrap();
        let mut before = keyspace(&db, 1);
        // One leader write group, then the follower fed its keys and compared.
        let mut follow = || {
            let after = keyspace(&db, 1);
            replay(&follower, &before, &after);
            assert_eq!(observed(&follower, dim), observed(&leader, dim));
            before = after;
        };
        for i in 0..80u64 {
            leader.insert(i, &rand_vec(i, dim)).unwrap();
            follow();
        }
        for row in [3u64, 40, 41, 79, 0] {
            assert!(leader.remove(row).unwrap());
            follow();
        }
        leader.mark_tombstone(7).unwrap();
        follow();
        // `compact` rewrites the whole keyspace under new element ids.
        leader.compact().unwrap();
        follow();
        for i in 100..110u64 {
            leader.insert(i, &rand_vec(i, dim)).unwrap();
            follow();
        }
        assert_eq!(gate.refusals(), 0, "the follower asked the gate to write");
        let reopened = PersistentVectorIndex::open(db.clone(), WriteGate::open(), 1).unwrap();
        assert_eq!(observed(&follower, dim), observed(&reopened, dim));

        // Dropping the keyspace leaves an empty mirror.
        let before = keyspace(&db, 1);
        PersistentVectorIndex::drop_index(&db, &WriteGate::open(), 1).unwrap();
        replay(&follower, &before, &keyspace(&db, 1));
        assert!(follower.is_empty());
        assert_eq!(follower.entry_point(), None);
        assert!(follower.search(&rand_vec(1, dim), 3, 16).unwrap().is_empty());
        assert_eq!(gate.refusals(), 0);
    }

    #[test]
    fn test_apply_records_follows_a_pq_keyspace() {
        let (_dir, db) = test_db();
        // `train_codebook` needs max(num_centroids, 10 * num_subquantizers)
        // = max(64, 160) samples for `pq_config_dim64`; 240 leaves margin.
        let (dim, n) = (64usize, 240usize);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        let leader =
            PersistentVectorIndex::create_with_pq(db.clone(), WriteGate::open(), 1, pq_config_dim64(), &data).unwrap();
        let follower = PersistentVectorIndex::open(db.clone(), WriteGate::closed("follower"), 1).unwrap();
        assert!(follower.pq_active());
        let mut before = keyspace(&db, 1);
        for (i, v) in data.iter().enumerate() {
            leader.insert(i as u64, v).unwrap();
            let after = keyspace(&db, 1);
            replay(&follower, &before, &after);
            before = after;
        }
        assert!(leader.remove(17).unwrap());
        replay(&follower, &before, &keyspace(&db, 1));
        assert_eq!(
            follower.ram_vector_bytes(),
            leader.ram_vector_bytes(),
            "codes, not vectors"
        );
        assert_eq!(observed(&follower, dim), observed(&leader, dim));
        // A re-delivered codebook rebuilds the same quantizer.
        let codebook = db.get(key_pq(&prefix(1))).unwrap().unwrap();
        follower.apply_record(b"pq", Some(codebook.as_slice())).unwrap();
        assert_eq!(observed(&follower, dim), observed(&leader, dim));
    }

    #[test]
    fn test_apply_record_names_what_it_cannot_mirror() {
        let (_dir, db) = test_db();
        let idx = PersistentVectorIndex::create(db.clone(), WriteGate::open(), 9, sample_config()).unwrap();
        // Suffixes are built at run time: `tests/keyspace_classification.rs`
        // requires every key-shaped literal under `src/` to be a real family.
        let suffix = |kind: &str, rest: &str| format!("{}:{}", kind, rest).into_bytes();
        let unknown = idx
            .apply_record(&suffix("bogus", "1"), Some(&b"x"[..]))
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("bogus:1"), "the error names the suffix: {unknown}");
        assert!(idx
            .apply_record(&suffix("vec", "not-a-number"), Some(&b"x"[..]))
            .is_err());
        assert!(
            idx.apply_record(&suffix("lvl", "1"), Some(&b""[..])).is_err(),
            "a value that does not decode"
        );
        let adjacency = suffix("adj", "0:3");
        assert_eq!(split_record_key(b"__vidx:9:adj:0:3"), Some((9, adjacency.as_slice())));
        assert_eq!(split_record_key(b"__vidx:x:meta"), None);
        assert_eq!(split_record_key(b"data:t:1"), None);
    }

    // ── P7: measurement (ignored; run with `-- --ignored --nocapture`) ───────

    #[test]
    #[ignore = "benchmark; run explicitly to print recall/latency/memory numbers"]
    fn bench_persistent_pq_summary() {
        use std::time::Instant;
        let (dim, n, k, ef, queries) = (128usize, 2000usize, 10usize, 100usize, 200u64);
        let data: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64, dim)).collect();
        let qs: Vec<Vec<f32>> = (0..queries).map(|qi| rand_vec(9_000_000 + qi, dim)).collect();
        let truth: Vec<HashSet<u64>> = qs
            .iter()
            .map(|q| brute_topk(&data, q, k).into_iter().collect())
            .collect();

        let pq_cfg = || {
            let mut c = PqHnswConfig::new(dim, DistanceMetric::L2);
            c.pq_config = Some(ProductQuantizerConfig {
                num_subquantizers: 32,
                num_centroids: 256,
                dimension: dim,
                training_iterations: 15,
                min_training_samples: 256,
            });
            c
        };

        let run = |label: &str, idx: &PersistentVectorIndex| {
            let t = Instant::now();
            for (i, v) in data.iter().enumerate() {
                idx.insert(i as u64, v).unwrap();
            }
            let build_ms = t.elapsed().as_secs_f64() * 1e3;
            let t = Instant::now();
            let mut hits = 0usize;
            for (qi, q) in qs.iter().enumerate() {
                let got = idx.search(q, k, ef).unwrap();
                hits += got.iter().filter(|(r, _)| truth[qi].contains(r)).count();
            }
            let q_us = t.elapsed().as_secs_f64() * 1e6 / queries as f64;
            let recall = hits as f64 / (queries as usize * k) as f64;
            eprintln!(
                "[{label}] build {build_ms:6.0}ms  query {q_us:6.0}us  recall@{k} {recall:.3}  ram {:5}KB",
                idx.ram_vector_bytes() / 1024
            );
        };

        eprintln!("dim={dim} n={n} k={k} ef={ef} queries={queries}");
        let (_d1, db1) = test_db();
        run(
            "exact f32   ",
            &PersistentVectorIndex::create(
                db1.clone(),
                WriteGate::open(),
                1,
                PqHnswConfig::new(dim, DistanceMetric::L2),
            )
            .unwrap(),
        );
        let (_d2, db2) = test_db();
        run(
            "pq f32-rerank",
            &PersistentVectorIndex::create_with_pq(db2.clone(), WriteGate::open(), 1, pq_cfg(), &data).unwrap(),
        );
        let (_d3, db3) = test_db();
        let mut ci8 = pq_cfg();
        ci8.rerank_precision = VectorPrecision::I8;
        run(
            "pq i8-rerank ",
            &PersistentVectorIndex::create_with_pq(db3.clone(), WriteGate::open(), 1, ci8, &data).unwrap(),
        );
    }
}
