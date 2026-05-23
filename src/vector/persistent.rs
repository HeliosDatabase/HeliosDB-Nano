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
use std::collections::{HashMap, HashSet};
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
}
