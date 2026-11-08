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

/// Default HNSW `M` (max connections per node) when neither the `[vector]`
/// config section nor `CREATE INDEX ... WITH (m = ...)` specify one.
/// Mirrors `VectorConfig::default().hnsw_m`.
pub const DEFAULT_HNSW_M: usize = 16;
/// Default HNSW `ef_construction` companion to [`DEFAULT_HNSW_M`].
/// Mirrors `VectorConfig::default().hnsw_ef_construction`.
pub const DEFAULT_HNSW_EF_CONSTRUCTION: usize = 200;

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

/// Undo descriptor for eager HNSW maintenance inside an open transaction
/// (R5.V1). Mirrors the `ArtUndoOp` pattern in `lib.rs`: DML hooks mutate the
/// index at statement time and return these ops so a ROLLBACK can revert the
/// index to its pre-statement state via [`VectorIndexManager::apply_undo`].
#[derive(Debug, Clone)]
pub enum VectorIndexUndoOp {
    /// An INSERT added this entry — rollback removes it.
    RemoveInserted { index_name: String, row_id: u64 },
    /// A DELETE removed this entry — rollback re-inserts the old vector.
    RestoreDeleted {
        index_name: String,
        row_id: u64,
        vector: Vector,
    },
    /// An UPDATE replaced (or removed/added) this entry — rollback deletes the
    /// current entry and re-inserts the old vector when one existed.
    RestoreUpdated {
        index_name: String,
        row_id: u64,
        old_vector: Option<Vector>,
    },
}

/// Sprinter 746342a0bfbb: what a checkpoint needs to know about a standard
/// (in-memory HNSW) index before it dumps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardDumpState {
    /// The index holds no point. hnsw_rs cannot dump a graph without an entry
    /// point, and rebuilding it from its rows at open costs nothing.
    Empty,
    /// Unchanged since the dump its sidecar describes, or since the reload
    /// from that dump: the checkpoint keeps the dump.
    Unchanged,
    /// Changed since its last dump, or never dumped.
    Changed,
}

/// One standard index's graph dump, published under `sidecar.basename`, and
/// the sidecar describing it ([`VectorIndexManager::dump_standard_index`]).
#[derive(Debug, Clone)]
pub struct StandardIndexDump {
    pub sidecar: super::index_snapshot::VectorGraphSidecar,
    /// The index's mutation count the dump was taken at: hand it to
    /// [`VectorIndexManager::mark_standard_index_dumped`] once the sidecar is
    /// durable.
    pub generation: u64,
}

/// How a [`VectorIndexManager`] treats the RocksDB keyspace of its persistent
/// (`vector-persist`) indexes (spec P1.2).
///
/// A persistent index is an in-RAM copy of its `__vidx:{index_id}:*` keys
/// whose own mutators WRITE those keys. On a physical standby the keys are
/// the primary's, applied byte for byte by the standby's applier, and the
/// engine's write gate is closed: a mutator that wrote would be refused (and
/// counted), and one that changed memory alone would leave the copy disagreeing
/// with the keys it claims to mirror.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PersistentPolicy {
    /// Every mutator writes its keys through the engine's gate: a primary,
    /// and every engine that may write.
    #[default]
    Durable,
    /// A physical standby (`StorageEngine::open_physical_standby`): no mutator
    /// reaches the gate. Standard and quantized indexes are maintained in
    /// memory as always (`on_row_*` from the applier's row changes); persistent
    /// ones follow the replicated records (`apply_persistent_records`) and
    /// are skipped by `on_row_*`; `drop_index` forgets them without
    /// deleting keys (the primary's drop deletes them, and the deletes
    /// replicate); and every mutation that only a write could express —
    /// creating one, or an insert or delete aimed at one — is refused before
    /// it changes anything.
    ReplicaMemoryOnly,
}

/// One persistent table graph staged in its MV's existing row group.
/// Publishing consumes the write lease before any later registry operation.
#[cfg(feature = "vector-persist")]
struct PreparedTableVector {
    name: String,
    replacement: crate::vector::persistent::PreparedPersistentReplacement,
}

#[cfg(feature = "vector-persist")]
impl PreparedTableVector {
    fn publish(self) -> String {
        self.replacement.publish();
        self.name
    }
}

#[cfg(feature = "vector-persist")]
#[derive(Default)]
struct RegistryPlanning {
    busy: parking_lot::Mutex<bool>,
    ready: parking_lot::Condvar,
}

struct RegistryOperation {
    #[cfg(feature = "vector-persist")]
    planning: Arc<RegistryPlanning>,
}

#[cfg(feature = "vector-persist")]
impl RegistryPlanning {
    fn acquire(self: &Arc<Self>) -> RegistryOperation {
        let mut busy = self.busy.lock();
        if *busy {
            crate::runtime_offload::off_worker(|| {
                while *busy {
                    self.ready.wait(&mut busy);
                }
            });
        }
        *busy = true;
        drop(busy);
        RegistryOperation {
            planning: Arc::clone(self),
        }
    }
}

#[cfg(feature = "vector-persist")]
impl Drop for RegistryOperation {
    fn drop(&mut self) {
        *self.planning.busy.lock() = false;
        self.planning.ready.notify_all();
    }
}

/// Snapshot once, before the batch's FIRST purge lease. Later cascade targets
/// never need a manager registry lock while an earlier lease remains held.
#[derive(Default)]
pub(crate) struct PersistentRetirementHandles {
    #[cfg(feature = "vector-persist")]
    handles: HashMap<String, crate::vector::persistent::PersistentVectorIndex>,
    #[cfg(feature = "vector-persist")]
    _planning: Option<RegistryOperation>,
}

impl PersistentRetirementHandles {
    #[cfg(feature = "vector-persist")]
    fn target(&self, name: &str) -> Option<crate::vector::persistent::PersistentRetirement> {
        self.handles.get(name).and_then(|index| index.retirement_target())
    }
}

/// Staged physical graph removal. A native error is not proof of rejection:
/// only an unchanged complete prefix AND definition preserve the old handle.
struct PreparedPersistentPurge {
    db: Arc<rocksdb::DB>,
    prefix: Vec<u8>,
    before: [u8; 32],
    definition_key: Vec<u8>,
    definition: Option<Vec<u8>>,
    settled: bool,
    #[cfg(feature = "vector-persist")]
    target: Option<crate::vector::persistent::PersistentRetirement>,
}

impl PreparedPersistentPurge {
    fn retire(&mut self) {
        #[cfg(feature = "vector-persist")]
        if let Some(target) = self.target.take() {
            target.retire();
        }
        self.settled = true;
    }

    fn settle(mut self, success: bool) {
        if success {
            self.retire();
        }
        // Error or unwritten drop: Drop proves non-application, otherwise
        // retires before its lease releases. It never writes or compacts.
    }
}

impl Drop for PreparedPersistentPurge {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let unchanged = (|| -> Result<bool> {
            if self.db.get(&self.definition_key)? != self.definition {
                return Ok(false);
            }
            Ok(VectorIndexManager::index_image_digest(&self.db, &self.prefix, |_| {})? == self.before)
        })();
        match unchanged {
            Ok(true) => {}
            Ok(false) => self.retire(),
            Err(error) => {
                self.retire();
                tracing::error!("persistent purge could not verify actual state: {error}; old handle retired");
            }
        }
    }
}

const PUBLICATION_INFLIGHT: u64 = 0xffff;
const PUBLICATION_EXHAUSTED: u64 = 1 << 63;

fn advance_publication(epoch: &std::sync::atomic::AtomicU64, delta: u64) {
    use std::sync::atomic::Ordering;
    let _ = epoch.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        Some(
            if value & PUBLICATION_EXHAUSTED != 0
                || (delta == 1 && value & PUBLICATION_INFLIGHT == PUBLICATION_INFLIGHT)
            {
                PUBLICATION_EXHAUSTED
            } else {
                value
                    .checked_add(delta)
                    .filter(|next| next & PUBLICATION_EXHAUSTED == 0)
                    .unwrap_or(PUBLICATION_EXHAUSTED)
            },
        )
    });
}

pub(crate) struct VectorPublication<'a>(&'a std::sync::atomic::AtomicU64);

impl Drop for VectorPublication<'_> {
    fn drop(&mut self) {
        advance_publication(self.0, PUBLICATION_INFLIGHT);
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
    /// Number of live indexes. Lets the per-row DML hooks bail with a single
    /// relaxed atomic load in the overwhelmingly common no-vector-index case,
    /// instead of taking the `metadata` read lock on every INSERT/UPDATE/DELETE.
    index_count: std::sync::atomic::AtomicUsize,
    /// Read tickets distinguish a writer that starts AND finishes during ANN.
    publication_epoch: std::sync::atomic::AtomicU64,
    hook_faults: dashmap::DashMap<String, std::sync::atomic::AtomicU64>,
    #[cfg(feature = "vector-persist")]
    registry_planning: Arc<RegistryPlanning>,
    /// R4.2: true while the persisted HNSW snapshot markers still describe the
    /// in-memory graphs. First mutation after a checkpoint flips it and fires
    /// `snapshot_invalidation_hook` once (durable marker delete).
    snapshot_clean: std::sync::atomic::AtomicBool,
    /// Engine-wired callback that deletes the vector snapshot markers in RocksDB.
    snapshot_invalidation_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// R4.2: indexes whose rebuild/reload failed at open, with the reason.
    /// Surfaced instead of silently degrading to an empty/missing index
    /// (the ISSUE-08 incident class).
    degraded: RwLock<HashMap<String, String>>,
    /// Spec P1.2: [`PersistentPolicy::ReplicaMemoryOnly`] when set. One
    /// relaxed load per check, so a primary (`Durable`) pays nothing else.
    replica_memory_only: std::sync::atomic::AtomicBool,
    /// Installed once by an engine before managed persistent hydration. The
    /// standalone manager keeps its existing unencrypted default.
    persistent_key_manager: std::sync::OnceLock<Option<Arc<crate::crypto::KeyManager>>>,
}

impl VectorIndexManager {
    /// Create a new vector index manager
    pub fn new() -> Self {
        Self {
            indexes: Arc::new(RwLock::new(HashMap::new())),
            metadata: Arc::new(RwLock::new(HashMap::new())),
            records: Arc::new(RwLock::new(HashMap::new())),
            external_ids: Arc::new(RwLock::new(HashMap::new())),
            index_count: std::sync::atomic::AtomicUsize::new(0),
            publication_epoch: std::sync::atomic::AtomicU64::new(0),
            hook_faults: dashmap::DashMap::new(),
            #[cfg(feature = "vector-persist")]
            registry_planning: Arc::new(RegistryPlanning::default()),
            snapshot_clean: std::sync::atomic::AtomicBool::new(true),
            snapshot_invalidation_hook: RwLock::new(None),
            degraded: RwLock::new(HashMap::new()),
            replica_memory_only: std::sync::atomic::AtomicBool::new(false),
            persistent_key_manager: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn set_persistent_key_manager(&self, key_manager: Option<Arc<crate::crypto::KeyManager>>) {
        let _ = self.persistent_key_manager.set(key_manager);
    }

    #[cfg(feature = "vector-persist")]
    fn persistent_key_manager(&self) -> Option<Arc<crate::crypto::KeyManager>> {
        self.persistent_key_manager.get().and_then(Option::as_ref).cloned()
    }

    /// Stage authoritative latest row images and invalidate every vector dump
    /// in the owning heap batch. No hook or mutation is executed here.
    pub(crate) fn stage_row_images(
        &self,
        table: &str,
        row: u64,
        schema: &crate::Schema,
        tuple: Option<&crate::Tuple>,
        batch: &mut rocksdb::WriteBatch,
        ceiling: usize,
    ) -> Result<()> {
        #[cfg(not(feature = "vector-persist"))]
        let _ = (row, schema, tuple);
        if !self.has_any_index() {
            return Ok(());
        }
        // Preserve the existing global clean/dirty contract: the first
        // mutation invalidates every cache marker, now in the owning row group.
        if self.snapshot_clean.load(std::sync::atomic::Ordering::Acquire) {
            for name in self.metadata.read().keys() {
                batch.delete(super::index_snapshot::vec_snapshot_marker_key(name));
                if batch.size_in_bytes() > ceiling {
                    return Err(Error::write_group_too_large(batch.size_in_bytes(), ceiling));
                }
            }
        }
        let metadata: Vec<_> = self
            .metadata
            .read()
            .values()
            .filter(|meta| meta.table_name == table)
            .cloned()
            .collect();
        for meta in metadata {
            batch.delete(super::index_snapshot::vec_snapshot_marker_key(&meta.name));
            #[cfg(feature = "vector-persist")]
            if let VectorIndexType::Persistent(config) = &meta.index_type {
                let vector = tuple.and_then(|tuple| Self::tuple_vector_value(schema, tuple, &meta.column_name));
                if vector.is_some_and(|vector| vector.len() != config.dimension) {
                    return Err(Error::query_execution(format!(
                        "vector dimension mismatch for index '{}'",
                        meta.name
                    )));
                }
                let image =
                    crate::vector::persistent::encode_row_image(self.persistent_key_manager().as_deref(), vector)?;
                batch.put(
                    crate::vector::persistent::row_image_key(stable_index_id(&meta.name), row),
                    image,
                );
            }
            if batch.size_in_bytes() > ceiling {
                return Err(Error::write_group_too_large(batch.size_in_bytes(), ceiling));
            }
        }
        Ok(())
    }

    /// The original data group has already invalidated its snapshot markers.
    /// Clear only the RAM flag here; invoking its durable hook would add a group.
    pub(crate) fn row_group_applied(&self) {
        self.snapshot_clean.store(false, std::sync::atomic::Ordering::Release);
    }

    /// Complete committed row images, after the owning group and before its
    /// census/locks release. The backend re-reads current overlay truth, so an
    /// old callback cannot overwrite a later row with its captured tuple.
    pub(crate) fn publish_row_images(&self, table: &str, row: u64) -> Result<()> {
        #[cfg(feature = "vector-persist")]
        {
            let _publication = self.begin_publication();
            let targets: Vec<_> = {
                let indexes = self.indexes.read();
                let metadata = self.metadata.read();
                metadata
                    .values()
                    .filter(|meta| meta.table_name == table)
                    .filter_map(|meta| match indexes.get(&meta.name) {
                        Some(IndexStorage::Persistent(index)) => Some((meta.name.clone(), index.replacement_handle())),
                        _ => None,
                    })
                    .collect()
            };
            let mut first_error = None;
            for (name, index) in targets {
                if let Err(error) = index.apply_latest_row_image(row) {
                    self.note_hook_error(table, &name, row, &error);
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        #[cfg(not(feature = "vector-persist"))]
        let _ = (table, row);
        Ok(())
    }

    fn publish_committed_row_image(&self, table: &str, index: &str, row: u64) -> bool {
        #[cfg(feature = "vector-persist")]
        {
            let target = {
                let indexes = self.indexes.read();
                match indexes.get(index) {
                    Some(IndexStorage::Persistent(target)) => Some(target.replacement_handle()),
                    _ => None,
                }
            };
            if let Some(target) = target {
                return match target.apply_latest_row_image(row) {
                    Ok(found) => found,
                    Err(error) => {
                        self.note_hook_error(table, index, row, &error);
                        true
                    }
                };
            }
        }
        #[cfg(not(feature = "vector-persist"))]
        let _ = (table, index, row);
        false
    }

    fn persistent_names_on_table(&self, table: &str) -> Vec<String> {
        self.metadata
            .read()
            .values()
            .filter(|meta| meta.table_name == table)
            .filter(|meta| matches!(meta.index_type, VectorIndexType::Persistent(_)))
            .map(|meta| meta.name.clone())
            .collect()
    }

    /// Persistent graph writes are deferred to the transaction's final row
    /// group. Standard/quantized eager undo remains unchanged.
    pub(crate) fn on_row_insert_staged(
        &self,
        table: &str,
        row: u64,
        schema: &crate::Schema,
        tuple: &crate::Tuple,
    ) -> Vec<VectorIndexUndoOp> {
        if !self.has_any_index() {
            return Vec::new();
        }
        self.on_row_insert_with_policy(table, row, schema, tuple, &self.persistent_names_on_table(table), true)
    }

    pub(crate) fn on_row_delete_staged(
        &self,
        table: &str,
        row: u64,
        schema: Option<&crate::Schema>,
        tuple: Option<&crate::Tuple>,
    ) -> Vec<VectorIndexUndoOp> {
        if !self.has_any_index() {
            return Vec::new();
        }
        self.on_row_delete_with_policy(table, row, schema, tuple, &self.persistent_names_on_table(table), true)
    }

    pub(crate) fn on_row_update_staged(
        &self,
        table: &str,
        row: u64,
        schema: &crate::Schema,
        old: Option<&crate::Tuple>,
        new: &crate::Tuple,
    ) -> Vec<VectorIndexUndoOp> {
        if !self.has_any_index() {
            return Vec::new();
        }
        self.on_row_update_with_policy(
            table,
            row,
            schema,
            old,
            new,
            &self.persistent_names_on_table(table),
            true,
        )
    }

    // ---- Spec P1.2: the persistent-keyspace policy -------------------------

    /// Choose how persistent indexes treat their keyspace. The engine sets
    /// [`PersistentPolicy::ReplicaMemoryOnly`] when it opens a physical
    /// standby, before anything is registered; every other engine keeps the
    /// default [`PersistentPolicy::Durable`].
    pub fn set_persistent_policy(&self, policy: PersistentPolicy) {
        self.replica_memory_only.store(
            policy == PersistentPolicy::ReplicaMemoryOnly,
            std::sync::atomic::Ordering::Release,
        );
    }

    /// The policy in force (see [`PersistentPolicy`]).
    pub fn persistent_policy(&self) -> PersistentPolicy {
        if self.replica_memory_only() {
            PersistentPolicy::ReplicaMemoryOnly
        } else {
            PersistentPolicy::Durable
        }
    }

    #[inline]
    fn replica_memory_only(&self) -> bool {
        self.replica_memory_only.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Under `ReplicaMemoryOnly`, the refusal of a mutation of persistent
    /// index `name` that only a write could express. Checked before the
    /// mutation touches the index, so nothing changes and the gate is never
    /// asked.
    #[cfg(feature = "vector-persist")]
    fn refuse_replica_persistent_write(&self, name: &str) -> Result<()> {
        if self.replica_memory_only() {
            return Err(Error::storage(format!(
                "persistent vector index '{name}' follows its primary's replicated records on a physical standby; \
                 it is never written here"
            )));
        }
        Ok(())
    }

    /// Whether `index_name` is a persistent index that, under
    /// `ReplicaMemoryOnly`, follows its replicated records rather than the
    /// rows: the row hooks skip it. Always `false` on a primary.
    fn follows_replicated_records(&self, index_name: &str) -> bool {
        #[cfg(feature = "vector-persist")]
        if self.replica_memory_only() {
            return matches!(self.indexes.read().get(index_name), Some(IndexStorage::Persistent(_)));
        }
        false
    }

    /// Whether a physical standby must decode `table`'s changed rows to keep
    /// its vector indexes right: the table has an index the row hooks
    /// maintain (any but a persistent one under `ReplicaMemoryOnly`).
    pub fn maintains_rows_of(&self, table_name: &str) -> bool {
        self.indexes_on_table(table_name)
            .iter()
            .any(|(index_name, _)| !self.follows_replicated_records(index_name))
    }

    /// Relabel metadata once from ORIGINAL to final column names. Bulk mapping
    /// handles rename cycles without changing graph identity/config/records.
    /// Called only in ordinary publication after all owned write leases settle.
    pub(crate) fn relabel_table_columns(&self, table: &str, renames: &[(String, String)]) {
        let _registry_operation = self.registry_operation();
        let _publication = self.begin_publication();
        let mut metadata = self.metadata.write();
        for meta in metadata.values_mut().filter(|meta| meta.table_name == table) {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == &meta.column_name) {
                meta.column_name.clone_from(new);
            }
        }
    }

    /// Reconcile labels/retirement against actual durable definitions after a
    /// possibly-applied column DDL. Surviving graph identities/configs are kept;
    /// a genuinely missing definition retires RAM only. The caller proves row
    /// vector identity and validates schema/definitions under table ownership.
    pub(crate) fn reconcile_table_vector_definitions(
        &self,
        table: &str,
        definitions: &[(String, super::PersistedIndexDefinition)],
    ) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.begin_publication();
        for (name, definition) in definitions
            .iter()
            .filter(|(_, definition)| definition.table_name == table)
        {
            if super::catalog::index_family(definition.index_type.as_deref()).is_none() {
                return Err(Error::storage(format!(
                    "vector reconciliation found unknown backend for index '{name}'"
                )));
            }
        }
        let desired: HashMap<_, _> = definitions
            .iter()
            .filter(|(_, definition)| {
                definition.table_name == table
                    && super::catalog::index_family(definition.index_type.as_deref())
                        == Some(super::catalog::IndexFamily::Vector)
            })
            .map(|(name, definition)| (name.as_str(), definition))
            .collect();
        let live: Vec<_> = self
            .metadata
            .read()
            .values()
            .filter(|meta| meta.table_name == table)
            .cloned()
            .collect();
        for (name, _) in &desired {
            if !live.iter().any(|meta| meta.name == *name) {
                return Err(Error::storage(format!(
                    "vector index '{name}' needs actual-state rebuild"
                )));
            }
        }
        let mut removed = Vec::new();
        for meta in &live {
            let Some(definition) = desired.get(meta.name.as_str()) else {
                removed.push(meta.name.clone());
                continue;
            };
            let compatible = matches!(
                (&meta.index_type, definition.index_type.as_deref()),
                (VectorIndexType::Standard(_), Some("hnsw"))
                    | (VectorIndexType::Quantized(_), Some("hnsw_pq"))
                    | (VectorIndexType::Persistent(_), Some("persistent_hnsw"))
            );
            if !compatible {
                return Err(Error::storage(format!(
                    "vector index '{}' changed backend during column DDL",
                    meta.name
                )));
            }
        }
        #[cfg(feature = "vector-persist")]
        self.retire_persistent_handles(&removed, &_registry_operation);
        {
            let mut indexes = self.indexes.write();
            let mut metadata = self.metadata.write();
            for meta in live {
                if let Some(definition) = desired.get(meta.name.as_str()) {
                    if let Some(actual) = metadata.get_mut(&meta.name) {
                        actual.column_name.clone_from(&definition.column_name);
                    }
                } else {
                    if indexes.remove(&meta.name).is_some() {
                        self.index_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    metadata.remove(&meta.name);
                }
            }
        }
        for name in removed {
            self.records.write().remove(&name);
            self.external_ids.write().remove(&name);
            self.degraded.write().remove(&name);
        }
        Ok(())
    }

    #[cfg(feature = "vector-persist")]
    fn retire_persistent_handles(&self, names: &[String], _planning: &RegistryOperation) {
        // Gather ALL before the first lease and never reacquire a registry
        // until every target has retired and every lease has released.
        let handles: Vec<_> = {
            let indexes = self.indexes.read();
            names
                .iter()
                .filter_map(|name| match indexes.get(name) {
                    Some(IndexStorage::Persistent(index)) => Some(index.replacement_handle()),
                    _ => None,
                })
                .collect()
        };
        for index in handles {
            if let Some(target) = index.retirement_target() {
                target.retire();
            }
        }
    }

    /// The registered persistent index whose keyspace is
    /// `__vidx:{index_id}:` (`PersistentVectorIndex::index_id`), if any.
    #[cfg(feature = "vector-persist")]
    pub fn persistent_index_with_id(&self, index_id: u64) -> Option<String> {
        self.indexes.read().iter().find_map(|(name, storage)| match storage {
            IndexStorage::Persistent(index) if index.index_id() == index_id => Some(name.clone()),
            _ => None,
        })
    }

    /// Apply the replicated records one batch wrote to persistent index
    /// `name`'s keyspace — `(suffix after "__vidx:{id}:", post-batch value)`
    /// in key order — to its in-memory copy, under one hold of the index lock
    /// (`PersistentVectorIndex::apply_records`). Memory only: a physical
    /// standby's way of following the keys its applier wrote. An error leaves
    /// the copy partly updated; the caller replaces it from the keys
    /// ([`Self::reopen_persistent_index`]).
    #[cfg(feature = "vector-persist")]
    pub fn apply_persistent_records(&self, name: &str, records: &[(Vec<u8>, Option<Vec<u8>>)]) -> Result<()> {
        let _publication = self.begin_publication();
        let indexes = self.indexes.read();
        match indexes.get(name) {
            Some(IndexStorage::Persistent(index)) => {
                let target = index.replacement_handle();
                drop(indexes);
                target.apply_records(
                    records
                        .iter()
                        .map(|(suffix, value)| (suffix.as_slice(), value.as_deref())),
                )
            }
            Some(_) => Err(Error::storage(format!(
                "vector index '{name}' is not a persistent index; it has no replicated records"
            ))),
            None => Err(Error::query_execution(format!("Index '{}' not found", name))),
        }
    }

    // ---- R4.2: durable snapshot validity tracking --------------------------

    /// Wire the callback that durably deletes the persisted vector snapshot
    /// markers. Called once by the storage engine at open.
    pub fn set_snapshot_invalidation_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.snapshot_invalidation_hook.write() = Some(hook);
    }

    /// Re-arm the validity flag after a checkpoint wrote fresh markers.
    pub fn mark_snapshot_clean(&self) {
        self.snapshot_clean.store(true, std::sync::atomic::Ordering::Release);
    }

    /// True when no vector index mutation happened since the last checkpoint.
    pub fn snapshot_is_clean(&self) -> bool {
        self.snapshot_clean.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record a vector index mutation; the first one after a checkpoint
    /// durably invalidates the snapshot markers.
    #[inline]
    fn note_mutation(&self) -> VectorPublication<'_> {
        let publication = self.begin_publication();
        use std::sync::atomic::Ordering;
        if self.snapshot_clean.load(Ordering::Relaxed) && self.snapshot_clean.swap(false, Ordering::AcqRel) {
            let hook = self.snapshot_invalidation_hook.read().clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        publication
    }

    fn registry_operation(&self) -> RegistryOperation {
        #[cfg(feature = "vector-persist")]
        {
            self.registry_planning.acquire()
        }
        #[cfg(not(feature = "vector-persist"))]
        {
            RegistryOperation {}
        }
    }

    pub(crate) fn hook_error_generation(&self, table: &str) -> Option<u64> {
        let value = self
            .hook_faults
            .get(table)
            .map_or(0, |fault| fault.load(std::sync::atomic::Ordering::SeqCst));
        (value != u64::MAX).then_some(value)
    }

    fn note_hook_error(&self, table: &str, index: &str, row: u64, error: &Error) {
        let fault = self.hook_faults.entry(table.to_string()).or_default();
        let _ = fault.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |value| Some(value.saturating_add(1)),
        );
        tracing::error!("vector hook failed for table '{table}', index '{index}', row {row}: {error}");
    }

    fn note_undo_error(&self, index: &str, row: u64, error: &Error) {
        let owner = self.metadata.read().get(index).map(|meta| meta.table_name.clone());
        if let Some(owner) = owner {
            self.note_hook_error(&owner, index, row, error);
        } else {
            tracing::error!("vector undo failed for removed index '{index}', row {row}: {error}");
        }
    }

    pub(crate) fn begin_publication(&self) -> VectorPublication<'_> {
        advance_publication(&self.publication_epoch, 1);
        VectorPublication(&self.publication_epoch)
    }

    pub(crate) fn publication_ticket(&self) -> Option<u64> {
        let value = self.publication_epoch.load(std::sync::atomic::Ordering::SeqCst);
        (value & (PUBLICATION_INFLIGHT | PUBLICATION_EXHAUSTED) == 0).then_some(value)
    }

    pub(crate) fn publication_is_current(&self, ticket: u64) -> bool {
        self.publication_epoch.load(std::sync::atomic::Ordering::SeqCst) == ticket
    }

    /// Record an index as degraded (rebuild/reload failed at open). The index
    /// is *not* registered, so queries fall back to exact scans — but the
    /// failure is now queryable instead of silent.
    pub fn mark_degraded(&self, name: &str, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::warn!("vector index '{}' degraded: {}", name, reason);
        self.degraded.write().insert(name.to_string(), reason);
    }

    /// Clear a degraded flag (e.g. after a successful later rebuild).
    pub fn clear_degraded(&self, name: &str) {
        self.degraded.write().remove(name);
    }

    /// `(index_name, reason)` for every index that failed to rebuild/reload.
    pub fn degraded_indexes(&self) -> Vec<(String, String)> {
        self.degraded
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Create a new standard (non-quantized) vector index with the default
    /// HNSW construction parameters (`M = 16`, `ef_construction = 200`).
    pub fn create_index(
        &self,
        name: String,
        table_name: String,
        column_name: String,
        dimension: usize,
        distance_metric: DistanceMetric,
    ) -> Result<()> {
        self.create_index_with_params(
            name,
            table_name,
            column_name,
            dimension,
            distance_metric,
            DEFAULT_HNSW_M,
            DEFAULT_HNSW_EF_CONSTRUCTION,
        )
    }

    /// Create a new standard (non-quantized) vector index with explicit HNSW
    /// construction parameters — R5.V6: plumbed from the `[vector]` config
    /// section (`hnsw_m` / `hnsw_ef_construction`) and overridable per index
    /// via `CREATE INDEX ... WITH (m = ..., ef_construction = ...)`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_index_with_params(
        &self,
        name: String,
        table_name: String,
        column_name: String,
        dimension: usize,
        distance_metric: DistanceMetric,
        m: usize,
        ef_construction: usize,
    ) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.note_mutation();
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
            max_connections: m,
            ef_construction,
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
        drop(metadata);
        drop(indexes);
        self.records.write().entry(name.clone()).or_default();
        self.external_ids.write().entry(name).or_default();
        self.index_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    /// A later explicit CREATE may reclaim scratch left by its failed
    /// pre-definition build. Inspect the entire prefix, even if an earlier
    /// interrupted purge already removed metadata. No error callback calls this.
    #[cfg(feature = "vector-persist")]
    pub(crate) fn prepare_persistent_index_creation(
        &self,
        name: &str,
        table: &str,
        storage: &super::StorageEngine,
    ) -> Result<()> {
        self.refuse_replica_persistent_write(name)?;
        let planning = self.registry_operation();
        let id = stable_index_id(name);
        let prefix = format!("__vidx:{id}:").into_bytes();
        let db = storage.db();
        let mut options = rocksdb::ReadOptions::default();
        options.set_total_order_seek(true);
        options.set_iterate_lower_bound(prefix.clone());
        let mut upper = prefix.clone();
        if let Some(last) = upper.last_mut() {
            *last += 1;
        }
        options.set_iterate_upper_bound(upper);
        let mut iter = db.raw_iterator_opt(options);
        iter.seek(&prefix);
        if !iter.valid() {
            iter.status()?;
            return Ok(());
        }
        {
            let indexes = self.indexes.read();
            if indexes.contains_key(name)
                || indexes
                    .values()
                    .any(|index| matches!(index, IndexStorage::Persistent(index) if index.index_id() == id))
            {
                return Err(Error::storage("persistent scratch namespace still has a live owner"));
            }
        }
        let definitions = match storage.catalog().index_definitions_strict() {
            Ok(definitions) => definitions,
            Err(error) => {
                // Retirement takes planning itself: release it before a
                // targeted fence enters any manager registry or graph lease.
                drop(iter);
                drop(planning);
                return Err(storage.quarantine_generic_indexes(
                    table,
                    &format!("cannot read persistent CREATE namespace references: {error}"),
                ));
            }
        };
        // Any name hashing to this namespace is protected, irrespective of
        // backend tag. A future/unknown definition is never guessed away.
        if definitions.iter().any(|(recorded, _)| stable_index_id(recorded) == id) {
            return Err(Error::storage(
                "persistent scratch namespace still has an actual definition",
            ));
        }

        // Scratch has no reader authority, so bounded point-delete chunks are
        // safe across interruptions. The next explicit retry rechecks all
        // references and the whole remaining prefix before constructing a graph.
        let ceiling = storage.write_gate().write_group_ceiling();
        let chunk_limit = ceiling.min(1024 * 1024);
        let mut batch = rocksdb::WriteBatch::default();
        let header = batch.size_in_bytes();
        while iter.valid() {
            let key = iter
                .key()
                .ok_or_else(|| Error::storage("missing persistent scratch key"))?;
            if !key.starts_with(&prefix) {
                return Err(Error::storage("persistent scratch iterator escaped its namespace"));
            }
            // Measure the actual encoder's one-key cost; do not invent a
            // second WriteBatch format or drop keys that happen not to fit.
            let mut one = rocksdb::WriteBatch::default();
            one.delete(key);
            storage.write_gate().refuse_oversized(&one)?;
            let added = one.size_in_bytes().saturating_sub(header);
            if !batch.is_empty() && batch.size_in_bytes().saturating_add(added) > chunk_limit {
                storage.write_gate().write(&db, std::mem::take(&mut batch), None)?;
            }
            batch.delete(key);
            iter.next();
        }
        iter.status()?;
        if !batch.is_empty() {
            storage.write_gate().write(&db, batch, None)?;
        }
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
        gate: Arc<super::write_gate::WriteGate>,
    ) -> Result<()> {
        let _registry_operation = self.registry_operation();
        // Spec P1.2: creating one writes its `meta` key. A standby registers
        // the primary's from the replicated keys (`reopen_persistent_index`).
        self.refuse_replica_persistent_write(&name)?;
        let _publication = self.note_mutation();
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
            crate::vector::persistent::PersistentVectorIndex::create_with_pq(
                db,
                gate,
                index_id,
                config,
                training_vectors,
            )?
        } else {
            crate::vector::persistent::PersistentVectorIndex::create(db, gate, index_id, config)?
        }
        .with_key_manager(self.persistent_key_manager());

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
        drop(metadata);
        drop(indexes);
        self.records.write().entry(name.clone()).or_default();
        self.external_ids.write().entry(name).or_default();
        self.index_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        _gate: Arc<super::write_gate::WriteGate>,
    ) -> Result<()> {
        Err(Error::query_execution(
            "Persistent vector indexes require the 'vector-persist' feature".to_string(),
        ))
    }

    /// Create a new quantized vector index with the default HNSW
    /// construction parameters (`M = 16`, `ef_construction = 200`).
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
        self.create_quantized_index_with_params(
            name,
            table_name,
            column_name,
            dimension,
            distance_metric,
            pq_config,
            training_vectors,
            DEFAULT_HNSW_M,
            DEFAULT_HNSW_EF_CONSTRUCTION,
        )
    }

    /// Create a new quantized vector index with explicit HNSW construction
    /// parameters — R5.V6, same plumbing as [`Self::create_index_with_params`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_quantized_index_with_params(
        &self,
        name: String,
        table_name: String,
        column_name: String,
        dimension: usize,
        distance_metric: DistanceMetric,
        pq_config: ProductQuantizerConfig,
        training_vectors: &[Vector],
        m: usize,
        ef_construction: usize,
    ) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.note_mutation();
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();

        // Check if index already exists
        if indexes.contains_key(&name) {
            return Err(Error::query_execution(format!("Index '{}' already exists", name)));
        }

        // Create Quantized HNSW configuration
        let config = QuantizedHnswConfig {
            max_connections: m,
            ef_construction,
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
        drop(metadata);
        drop(indexes);
        self.records.write().entry(name.clone()).or_default();
        self.external_ids.write().entry(name).or_default();
        self.index_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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
        let _publication = self.note_mutation();
        let indexes = self.indexes.read();
        if let Some(index) = indexes.get(index_name) {
            match index {
                IndexStorage::Standard(idx) => idx.insert(row_id, vector)?,
                IndexStorage::Quantized(idx) => idx.insert(row_id, vector)?,
                #[cfg(feature = "vector-persist")]
                IndexStorage::Persistent(idx) => {
                    self.refuse_replica_persistent_write(index_name)?;
                    let target = idx.replacement_handle();
                    drop(indexes);
                    target.insert(row_id, vector)?;
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
        let _publication = self.note_mutation();
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
                self.refuse_replica_persistent_write(index_name)?;
                let target = idx.replacement_handle();
                drop(indexes);
                for (row_id, vector) in batch {
                    target.insert(*row_id, vector)?;
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
        let _publication = self.note_mutation();
        let indexes = self.indexes.read();
        match indexes.get(index_name) {
            Some(IndexStorage::Standard(idx)) => {
                idx.delete(row_id)?;
                drop(indexes);
            }
            Some(IndexStorage::Quantized(idx)) => {
                idx.delete(row_id)?;
                drop(indexes);
            }
            #[cfg(feature = "vector-persist")]
            Some(IndexStorage::Persistent(idx)) => {
                self.refuse_replica_persistent_write(index_name)?;
                let target = idx.replacement_handle();
                drop(indexes);
                target.remove(row_id)?;
            }
            None => return Err(Error::query_execution(format!("Index '{}' not found", index_name))),
        }
        // Namespace fetches hold external_ids before records and may inspect
        // the registry. Hold neither registry nor a payload map across the next.
        self.forget_row_record(index_name, row_id);

        Ok(())
    }

    /// Hash exact raw index bytes under total-order bounds. Framed lengths prevent
    /// ambiguous key/value concatenations. This also supplies a non-application
    /// witness for a token discarded after an uncertain native write result.
    pub(crate) fn index_image_digest(
        db: &rocksdb::DB,
        prefix: &[u8],
        mut visit: impl FnMut(&[u8]),
    ) -> Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let mut options = rocksdb::ReadOptions::default();
        options.set_total_order_seek(true);
        options.set_iterate_lower_bound(prefix.to_vec());
        let mut upper = prefix.to_vec();
        if let Some(last) = upper.last_mut() {
            *last += 1;
        }
        options.set_iterate_upper_bound(upper);
        for entry in db.iterator_opt(
            rocksdb::IteratorMode::From(prefix, rocksdb::Direction::Forward),
            options,
        ) {
            let (key, value) = entry.map_err(Error::from)?;
            if !key.starts_with(prefix) {
                break;
            }
            hash.update((key.len() as u64).to_le_bytes());
            hash.update(&key);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(&value);
            visit(&key);
        }
        Ok(hash.finalize().into())
    }

    pub(crate) fn snapshot_persistent_retirement_handles(&self) -> PersistentRetirementHandles {
        #[cfg(not(feature = "vector-persist"))]
        {
            PersistentRetirementHandles::default()
        }
        #[cfg(feature = "vector-persist")]
        {
            let planning = self.registry_operation();
            if !self.has_any_index() {
                return PersistentRetirementHandles {
                    handles: HashMap::new(),
                    _planning: Some(planning),
                };
            }
            let indexes = self.indexes.read();
            PersistentRetirementHandles {
                _planning: Some(planning),
                handles: indexes
                    .iter()
                    .filter_map(|(name, index)| match index {
                        IndexStorage::Persistent(index) => Some((name.clone(), index.replacement_handle())),
                        _ => None,
                    })
                    .collect(),
            }
        }
    }

    /// The existing encoded namespace hashes the whole accepted index name:
    /// a literal ':' in a quoted name cannot extend another index's prefix.
    pub(crate) fn stage_persistent_index_purge(
        &self,
        name: &str,
        db: Arc<rocksdb::DB>,
        ddl: &mut super::DdlBatch<'_>,
    ) -> Result<()> {
        if !ddl.begin_persistent_retirement(name) {
            return Ok(());
        }
        #[cfg(feature = "vector-persist")]
        let target = ddl.persistent_retirement_handles()?.target(name);
        let prefix = format!("__vidx:{}:", stable_index_id(name)).into_bytes();
        let definition_key = format!("meta:index:{name}").into_bytes();
        let definition = db.get(&definition_key)?;
        let before = Self::index_image_digest(&db, &prefix, |key| ddl.delete(key))?;
        let purge = PreparedPersistentPurge {
            db,
            prefix,
            before,
            definition_key,
            definition,
            settled: false,
            #[cfg(feature = "vector-persist")]
            target,
        };
        ddl.after_write_before_publication(move |success| purge.settle(success));
        Ok(())
    }

    fn forget_row_record(&self, index_name: &str, row_id: u64) {
        let removed = self
            .records
            .write()
            .get_mut(index_name)
            .and_then(|records| records.remove(&row_id));
        if let Some(record) = removed {
            if let Some(ids) = self.external_ids.write().get_mut(index_name) {
                ids.remove(&VectorExternalKey::new(&record.id, record.namespace.as_deref()));
            }
        }
    }

    pub(crate) fn forget_replaced_row_records(&self, indexes: &[String], rows: &[crate::Tuple]) {
        for name in indexes {
            for row in rows {
                if let Some(id) = row.row_id {
                    self.forget_row_record(name, id);
                }
            }
        }
    }

    /// Stage persistent graphs from the replacement's logical rows, without
    /// writing or publishing any index state. Release registry guards BEFORE
    /// acquiring the first owned index lease, and acquire leases by name.
    pub(crate) fn stage_persistent_table_replacement<'e>(
        &'e self,
        table: &str,
        schema: &crate::Schema,
        rows: &[(u64, crate::Tuple)],
        ddl: &mut super::DdlBatch<'e>,
    ) -> Result<Vec<String>> {
        #[cfg(not(feature = "vector-persist"))]
        {
            let _ = (table, schema, rows, ddl);
            Ok(Vec::new())
        }
        #[cfg(feature = "vector-persist")]
        {
            // Avoid topology planning entirely for ordinary, non-persistent
            // populations. Recheck after taking planning before any lease.
            if !self
                .metadata
                .read()
                .values()
                .any(|meta| meta.table_name == table && matches!(meta.index_type, VectorIndexType::Persistent(_)))
            {
                return Ok(Vec::new());
            }
            let planning = self.registry_operation();
            let mut targets = {
                let indexes = self.indexes.read();
                let metadata = self.metadata.read();
                let mut targets = Vec::new();
                for meta in metadata.values().filter(|meta| meta.table_name == table) {
                    if !matches!(meta.index_type, VectorIndexType::Persistent(_)) {
                        continue;
                    }
                    let Some(IndexStorage::Persistent(index)) = indexes.get(&meta.name) else {
                        return Err(Error::storage(format!(
                            "persistent MV index '{}' has no persistent backend",
                            meta.name
                        )));
                    };
                    targets.push((meta.name.clone(), meta.column_name.clone(), index.replacement_handle()));
                }
                targets
            };
            targets.sort_by(|left, right| left.0.cmp(&right.0));
            let mut prepared = Vec::with_capacity(targets.len());
            for (name, column, index) in targets {
                let position = schema.get_column_index(&column).ok_or_else(|| {
                    Error::storage(format!("persistent MV index '{name}' references a missing column"))
                })?;
                let mut vectors = Vec::new();
                for (row, tuple) in rows {
                    match tuple.values.get(position) {
                        Some(crate::Value::Null) => {}
                        Some(crate::Value::Vector(vector)) => vectors.push((*row, vector.clone())),
                        _ => {
                            return Err(Error::storage(format!(
                                "persistent MV index '{name}' received a non-vector value"
                            )))
                        }
                    }
                }
                let replacement = index.stage_replacement(&vectors, ddl.batch_mut())?;
                prepared.push(PreparedTableVector { name, replacement });
            }
            let names = prepared.iter().map(|index| index.name.clone()).collect();
            // ALL graph leases and topology planning settle before any normal
            // DDL callback or ACK. Crash injection remains before this phase.
            ddl.after_write_before_publication(move |success| {
                if success {
                    let _publication = self.begin_publication();
                    for index in prepared {
                        index.publish();
                    }
                } else {
                    drop(prepared);
                }
                drop(planning);
            });
            Ok(names)
        }
    }

    /// Retire an uncertain table's in-memory handles without changing its
    /// durable HNSW records or snapshot markers. Only failed reconciliation
    /// uses this; reopen determines which definitions actually survived.
    pub(crate) fn retire_table_indexes(&self, table: &str, reason: &str) {
        let _registry_operation = self.registry_operation();
        let _publication = self.begin_publication();
        #[cfg(feature = "vector-persist")]
        {
            let names: Vec<_> = self
                .metadata
                .read()
                .values()
                .filter(|meta| meta.table_name == table)
                .map(|meta| meta.name.clone())
                .collect();
            self.retire_persistent_handles(&names, &_registry_operation);
        }
        let names = {
            let mut indexes = self.indexes.write();
            let mut metadata = self.metadata.write();
            let names: Vec<_> = metadata
                .values()
                .filter(|meta| meta.table_name == table)
                .map(|meta| meta.name.clone())
                .collect();
            for name in &names {
                if indexes.remove(name).is_some() {
                    self.index_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                metadata.remove(name);
            }
            names
        };
        // Public namespace fetches hold external_ids before records and may
        // inspect the index registry while holding them. Never keep a registry
        // or one payload-map guard while acquiring another payload-map guard.
        for name in names {
            self.records.write().remove(&name);
            self.external_ids.write().remove(&name);
            self.degraded.write().insert(name, reason.to_string());
        }
    }

    /// Forget only an unreferenced failed CREATE's registration. The caller
    /// proved that its definition is absent under the table's exclusive lock.
    /// Other live (including standalone) registrations are preserved.
    pub(crate) fn retire_unwritten_index(&self, name: &str, table: &str) -> Result<()> {
        let _planning = self.registry_operation();
        let _publication = self.begin_publication();
        let owner = self.metadata.read().get(name).map(|meta| meta.table_name.clone());
        match owner {
            Some(owner) if owner == table => {}
            Some(_) => return Err(Error::storage("failed CREATE graph changed table ownership")),
            None if !self.indexes.read().contains_key(name) => return Ok(()),
            None => return Err(Error::storage("failed CREATE graph has no ownership metadata")),
        }
        #[cfg(feature = "vector-persist")]
        self.retire_persistent_handles(&[name.to_string()], &_planning);
        {
            let mut indexes = self.indexes.write();
            let mut metadata = self.metadata.write();
            if indexes.remove(name).is_some() {
                self.index_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            metadata.remove(name);
        }
        self.records.write().remove(name);
        self.external_ids.write().remove(name);
        self.degraded.write().remove(name);
        Ok(())
    }

    /// Actual persistent write-lease contention, for deterministic publication tests.
    #[cfg(all(feature = "internal-tests", feature = "vector-persist"))]
    pub fn debug_persistent_write_waiters(&self, name: &str) -> Option<usize> {
        match self.indexes.read().get(name) {
            Some(IndexStorage::Persistent(index)) => Some(index.write_waiters()),
            _ => None,
        }
    }

    /// Drop an index
    pub fn drop_index(&self, name: &str) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.note_mutation();
        // A proven native refusal keeps the live registration and graph.
        // Clone the persistent target, release the registry, then acquire its
        // lease. Topology planning keeps the incarnation stable until detach.
        #[cfg(feature = "vector-persist")]
        if !self.replica_memory_only() {
            let persistent = {
                let indexes = self.indexes.read();
                match indexes.get(name) {
                    Some(IndexStorage::Persistent(index)) => Some(index.replacement_handle()),
                    _ => None,
                }
            };
            if let Some(index) = persistent {
                index.drop_storage()?;
            }
        }
        let _index = {
            let mut indexes = self.indexes.write();
            let mut metadata = self.metadata.write();
            let Some(index) = indexes.remove(name) else {
                return Err(Error::query_execution(format!("Index '{}' does not exist", name)));
            };
            metadata.remove(name);
            self.index_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            index
        };
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

    /// Cheap gate for the per-row DML hooks: one relaxed atomic load.
    /// `false` means no vector index exists in the process, so DML on any
    /// table can skip vector maintenance entirely.
    #[inline]
    pub fn has_any_index(&self) -> bool {
        self.index_count.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// `(index_name, column_name)` pairs for every vector index on `table_name`.
    /// Returns an empty (non-allocating) Vec when the table has none.
    pub fn indexes_on_table(&self, table_name: &str) -> Vec<(String, String)> {
        if !self.has_any_index() {
            return Vec::new();
        }
        let metadata = self.metadata.read();
        let mut out = Vec::new();
        for meta in metadata.values() {
            if meta.table_name == table_name {
                out.push((meta.name.clone(), meta.column_name.clone()));
            }
        }
        out
    }

    /// True when `table_name` has at least one vector index. One atomic load
    /// in the common case; a short metadata scan otherwise.
    pub fn table_has_indexes(&self, table_name: &str) -> bool {
        if !self.has_any_index() {
            return false;
        }
        let metadata = self.metadata.read();
        metadata.values().any(|meta| meta.table_name == table_name)
    }

    /// Physical (tombstone-inclusive) entry count for an index. Used by the
    /// kNN fast path to bound its tombstone over-fetch (R5.V5): the graph can
    /// never return more candidates than this.
    pub fn index_physical_size(&self, name: &str) -> Option<usize> {
        let indexes = self.indexes.read();
        indexes.get(name).map(|index| match index {
            IndexStorage::Standard(idx) => idx.len(),
            IndexStorage::Quantized(idx) => idx.len(),
            #[cfg(feature = "vector-persist")]
            IndexStorage::Persistent(idx) => idx.len(),
        })
    }

    /// Live (tombstone-excluded) entry count for an index. The kNN fast
    /// path uses this to fall back to the EXACT scan path for small indexes:
    /// hnsw_rs graphs at tiny sizes can be weakly connected (especially when
    /// bulk-built) and may miss live nodes regardless of ef_search, so
    /// approximate search is only engaged at scales where its recall is
    /// statistically sound.
    pub fn index_live_size(&self, name: &str) -> Option<usize> {
        let indexes = self.indexes.read();
        indexes.get(name).map(|index| match index {
            IndexStorage::Standard(idx) => idx.live_len(),
            IndexStorage::Quantized(idx) => idx.len(),
            #[cfg(feature = "vector-persist")]
            IndexStorage::Persistent(idx) => idx.len(),
        })
    }

    // ---- R5.V1: SQL DML maintenance hooks --------------------------------
    //
    // These mirror the ART hooks (`ArtIndexManager::on_insert/on_delete/
    // on_update`): they are called eagerly at statement time from every DML
    // path that maintains ART, and return undo descriptors so transactional
    // callers can push them onto the same per-transaction undo log the ART
    // entries use. Autocommit callers simply drop the returned Vec.
    //
    // Maintenance is deliberately non-fatal: failures (e.g. dimension
    // mismatch) are logged at debug level and never abort the DML, matching
    // the established ART hook behaviour.

    /// Extract the vector value for `column_name` from a tuple, if present
    /// and non-NULL.
    fn tuple_vector_value<'t>(
        schema: &crate::Schema,
        tuple: &'t crate::Tuple,
        column_name: &str,
    ) -> Option<&'t Vector> {
        let idx = schema.get_column_index(column_name)?;
        match tuple.values.get(idx) {
            Some(crate::Value::Vector(v)) => Some(v),
            _ => None,
        }
    }

    /// Row inserted: add its vector value (if any) to every vector index on
    /// the table.
    pub fn on_row_insert(
        &self,
        table_name: &str,
        row_id: u64,
        schema: &crate::Schema,
        tuple: &crate::Tuple,
    ) -> Vec<VectorIndexUndoOp> {
        self.on_row_insert_excluding(table_name, row_id, schema, tuple, &[])
    }

    pub(crate) fn on_row_insert_excluding(
        &self,
        table_name: &str,
        row_id: u64,
        schema: &crate::Schema,
        tuple: &crate::Tuple,
        excluded: &[String],
    ) -> Vec<VectorIndexUndoOp> {
        self.on_row_insert_with_policy(table_name, row_id, schema, tuple, excluded, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_row_insert_with_policy(
        &self,
        table_name: &str,
        row_id: u64,
        schema: &crate::Schema,
        tuple: &crate::Tuple,
        excluded: &[String],
        ram_only: bool,
    ) -> Vec<VectorIndexUndoOp> {
        if !self.has_any_index() {
            return Vec::new();
        }
        let _publication = self.begin_publication();
        let mut undo = Vec::new();
        for (index_name, column_name) in self.indexes_on_table(table_name) {
            if excluded.contains(&index_name) {
                continue;
            }
            if self.follows_replicated_records(&index_name) {
                continue; // spec P1.2: its replicated records carry the change
            }
            if self.publish_committed_row_image(table_name, &index_name, row_id) {
                continue;
            }
            let Some(vector) = Self::tuple_vector_value(schema, tuple, &column_name) else {
                continue; // NULL / missing vector: nothing to index
            };
            match if ram_only {
                self.insert_vector_ram_only(&index_name, row_id, vector)
            } else {
                self.insert_vector(&index_name, row_id, vector)
            } {
                Ok(()) => undo.push(VectorIndexUndoOp::RemoveInserted { index_name, row_id }),
                Err(e) => {
                    self.note_hook_error(table_name, &index_name, row_id, &e);
                }
            }
        }
        undo
    }

    /// Row deleted: remove its entry from every vector index on the table.
    /// `old_tuple` (when available) lets the undo op restore the exact old
    /// vector on rollback; without it the delete still happens but cannot be
    /// undone (only autocommit paths call it that way).
    pub fn on_row_delete(
        &self,
        table_name: &str,
        row_id: u64,
        schema: Option<&crate::Schema>,
        old_tuple: Option<&crate::Tuple>,
    ) -> Vec<VectorIndexUndoOp> {
        self.on_row_delete_excluding(table_name, row_id, schema, old_tuple, &[])
    }

    pub(crate) fn on_row_delete_excluding(
        &self,
        table_name: &str,
        row_id: u64,
        schema: Option<&crate::Schema>,
        old_tuple: Option<&crate::Tuple>,
        excluded: &[String],
    ) -> Vec<VectorIndexUndoOp> {
        self.on_row_delete_with_policy(table_name, row_id, schema, old_tuple, excluded, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_row_delete_with_policy(
        &self,
        table_name: &str,
        row_id: u64,
        schema: Option<&crate::Schema>,
        old_tuple: Option<&crate::Tuple>,
        excluded: &[String],
        ram_only: bool,
    ) -> Vec<VectorIndexUndoOp> {
        if !self.has_any_index() {
            return Vec::new();
        }
        let _publication = self.begin_publication();
        let mut undo = Vec::new();
        for (index_name, column_name) in self.indexes_on_table(table_name) {
            if excluded.contains(&index_name) {
                continue;
            }
            if self.follows_replicated_records(&index_name) {
                continue; // spec P1.2: its replicated records carry the change
            }
            if self.publish_committed_row_image(table_name, &index_name, row_id) {
                continue;
            }
            let old_vector = match (schema, old_tuple) {
                (Some(schema), Some(tuple)) => Self::tuple_vector_value(schema, tuple, &column_name).cloned(),
                _ => None,
            };
            // Tolerate missing entries (row's vector was NULL, or the row
            // predates the index in ways backfill didn't cover).
            match if ram_only {
                self.delete_vector_ram_only(&index_name, row_id)
            } else {
                self.delete_vector(&index_name, row_id)
            } {
                Ok(()) => {
                    if let Some(vector) = old_vector {
                        undo.push(VectorIndexUndoOp::RestoreDeleted {
                            index_name,
                            row_id,
                            vector,
                        });
                    }
                }
                Err(error) => self.note_hook_error(table_name, &index_name, row_id, &error),
            }
        }
        undo
    }

    /// Row updated: if the indexed vector column changed, replace the entry
    /// (HNSW has no in-place update: delete tombstones the old entry, insert
    /// adds the new one). Unchanged vector columns are skipped entirely.
    /// `old_tuple == None` means the caller couldn't supply the previous row
    /// (engine fast paths that proved no *ART* index change); in that case the
    /// entry is unconditionally replaced.
    pub fn on_row_update(
        &self,
        table_name: &str,
        row_id: u64,
        schema: &crate::Schema,
        old_tuple: Option<&crate::Tuple>,
        new_tuple: &crate::Tuple,
    ) -> Vec<VectorIndexUndoOp> {
        self.on_row_update_excluding(table_name, row_id, schema, old_tuple, new_tuple, &[])
    }

    pub(crate) fn on_row_update_excluding(
        &self,
        table_name: &str,
        row_id: u64,
        schema: &crate::Schema,
        old_tuple: Option<&crate::Tuple>,
        new_tuple: &crate::Tuple,
        excluded: &[String],
    ) -> Vec<VectorIndexUndoOp> {
        self.on_row_update_with_policy(table_name, row_id, schema, old_tuple, new_tuple, excluded, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_row_update_with_policy(
        &self,
        table_name: &str,
        row_id: u64,
        schema: &crate::Schema,
        old_tuple: Option<&crate::Tuple>,
        new_tuple: &crate::Tuple,
        excluded: &[String],
        ram_only: bool,
    ) -> Vec<VectorIndexUndoOp> {
        if !self.has_any_index() {
            return Vec::new();
        }
        let _publication = self.begin_publication();
        let mut undo = Vec::new();
        for (index_name, column_name) in self.indexes_on_table(table_name) {
            if excluded.contains(&index_name) {
                continue;
            }
            if self.follows_replicated_records(&index_name) {
                continue; // spec P1.2: its replicated records carry the change
            }
            if self.publish_committed_row_image(table_name, &index_name, row_id) {
                continue;
            }
            let old_vector = old_tuple.and_then(|t| Self::tuple_vector_value(schema, t, &column_name));
            let new_vector = Self::tuple_vector_value(schema, new_tuple, &column_name);

            if old_tuple.is_some() {
                if old_vector == new_vector {
                    continue; // vector column untouched — common OLTP update
                }
            } else if new_vector.is_none() {
                // Old state unknown and the new value is NULL/absent: drop any
                // existing entry so the index never serves a stale vector.
                if let Err(error) = if ram_only {
                    self.delete_vector_ram_only(&index_name, row_id)
                } else {
                    self.delete_vector(&index_name, row_id)
                } {
                    self.note_hook_error(table_name, &index_name, row_id, &error);
                }
                continue;
            }

            let had_old_entry = match if ram_only {
                self.delete_vector_ram_only(&index_name, row_id)
            } else {
                self.delete_vector(&index_name, row_id)
            } {
                Ok(()) => true,
                Err(error) => {
                    self.note_hook_error(table_name, &index_name, row_id, &error);
                    false
                }
            };
            let inserted = match new_vector {
                Some(vector) => match if ram_only {
                    self.insert_vector_ram_only(&index_name, row_id, vector)
                } else {
                    self.insert_vector(&index_name, row_id, vector)
                } {
                    Ok(()) => true,
                    Err(e) => {
                        self.note_hook_error(table_name, &index_name, row_id, &e);
                        false
                    }
                },
                None => false,
            };
            if had_old_entry || inserted {
                undo.push(VectorIndexUndoOp::RestoreUpdated {
                    index_name,
                    row_id,
                    old_vector: if old_tuple.is_some() { old_vector.cloned() } else { None },
                });
            }
        }
        undo
    }

    // Explicit RAM-only paths are separate from normal mutators: even a
    // Standard mutation's note_mutation hook can write snapshot invalidation.
    fn insert_vector_ram_only(&self, name: &str, row: u64, vector: &Vector) -> Result<()> {
        let indexes = self.indexes.read();
        match indexes.get(name) {
            Some(IndexStorage::Standard(index)) => index.insert(row, vector),
            Some(IndexStorage::Quantized(index)) => index.insert(row, vector),
            #[cfg(feature = "vector-persist")]
            Some(IndexStorage::Persistent(index)) => {
                let target = index.replacement_handle();
                drop(indexes);
                target.reload_actual_ram()
            }
            None => Err(Error::query_execution(format!("Index '{name}' not found"))),
        }
    }

    fn delete_vector_ram_only(&self, name: &str, row: u64) -> Result<()> {
        let indexes = self.indexes.read();
        match indexes.get(name) {
            Some(IndexStorage::Standard(index)) => {
                index.delete_if_present(row);
            }
            Some(IndexStorage::Quantized(index)) => {
                index.delete_if_present(row);
            }
            #[cfg(feature = "vector-persist")]
            Some(IndexStorage::Persistent(index)) => {
                let target = index.replacement_handle();
                drop(indexes);
                target.reload_actual_ram()?;
                self.forget_row_record(name, row);
                return Ok(());
            }
            None => return Err(Error::query_execution(format!("Index '{name}' not found"))),
        }
        drop(indexes);
        self.forget_row_record(name, row);
        Ok(())
    }

    /// Reconcile MAIN's changed row IDs from actual logical rows, with no
    /// durable snapshot invalidation or graph repair. Caller retains the
    /// write census/table ownership and quarantines the table on error.
    /// Persistent indexes reload committed truth ONCE each, after all registry
    /// reads and with no other persistent lease held.
    pub(crate) fn reconcile_rows_ram_only(
        &self,
        table: &str,
        schema: &crate::Schema,
        changes: &[(u64, Option<crate::Tuple>)],
    ) -> Result<()> {
        if !self.has_any_index() {
            return Ok(());
        }
        let _planning = self.registry_operation();
        let _publication = self.begin_publication();
        let targets: Vec<_> = self
            .metadata
            .read()
            .values()
            .filter(|meta| meta.table_name == table)
            .cloned()
            .collect();
        for meta in &targets {
            if schema.get_column_index(&meta.column_name).is_none() {
                let error = Error::storage(format!(
                    "vector reconciliation index '{}' references missing column '{}'",
                    meta.name, meta.column_name
                ));
                self.note_hook_error(table, &meta.name, 0, &error);
                return Err(error);
            }
        }
        #[cfg(feature = "vector-persist")]
        let mut persistent = Vec::new();
        {
            let indexes = self.indexes.read();
            for meta in &targets {
                match (indexes.get(&meta.name), &meta.index_type) {
                    (Some(IndexStorage::Standard(_)), VectorIndexType::Standard(_))
                    | (Some(IndexStorage::Quantized(_)), VectorIndexType::Quantized(_)) => {}
                    #[cfg(feature = "vector-persist")]
                    (Some(IndexStorage::Persistent(index)), VectorIndexType::Persistent(_)) => {
                        persistent.push((meta.name.clone(), index.replacement_handle()));
                    }
                    _ => {
                        let error = Error::storage(format!(
                            "vector reconciliation index '{}' has no matching backend",
                            meta.name
                        ));
                        self.note_hook_error(table, &meta.name, 0, &error);
                        return Err(error);
                    }
                }
            }
        }
        for meta in &targets {
            if matches!(meta.index_type, VectorIndexType::Persistent(_)) {
                continue;
            }
            for (row, actual) in changes {
                let repaired = (|| -> Result<()> {
                    self.delete_vector_ram_only(&meta.name, *row)?;
                    if let Some(vector) = actual
                        .as_ref()
                        .and_then(|tuple| Self::tuple_vector_value(schema, tuple, &meta.column_name))
                    {
                        self.insert_vector_ram_only(&meta.name, *row, vector)?;
                    }
                    Ok(())
                })();
                if let Err(error) = repaired {
                    self.note_hook_error(table, &meta.name, *row, &error);
                    return Err(error);
                }
            }
        }
        #[cfg(feature = "vector-persist")]
        {
            for (name, index) in &persistent {
                if let Err(error) = index.reload_actual_ram() {
                    self.note_hook_error(table, name, 0, &error);
                    return Err(error);
                }
            }
            // Every graph lease has released before payload bookkeeping.
            for (name, _) in &persistent {
                for (row, _) in changes {
                    self.forget_row_record(name, *row);
                }
            }
        }
        Ok(())
    }

    /// Embedded transaction journals undo derived RAM only. Persistent eager
    /// journals are normally absent; if supplied, reload committed truth rather
    /// than persisting an old image over a possibly-applied commit.
    pub(crate) fn apply_undo_ram_only(&self, op: &VectorIndexUndoOp) {
        let _planning = self.registry_operation();
        let _publication = self.begin_publication();
        let (name, row, restored, remove) = match op {
            VectorIndexUndoOp::RemoveInserted { index_name, row_id } => (index_name, *row_id, None, true),
            VectorIndexUndoOp::RestoreDeleted {
                index_name,
                row_id,
                vector,
            } => (index_name, *row_id, Some(vector), false),
            VectorIndexUndoOp::RestoreUpdated {
                index_name,
                row_id,
                old_vector,
            } => (index_name, *row_id, old_vector.as_ref(), true),
        };
        if remove {
            if let Err(error) = self.delete_vector_ram_only(name, row) {
                self.note_undo_error(name, row, &error);
                return;
            }
        }
        if let Some(vector) = restored {
            if let Err(error) = self.insert_vector_ram_only(name, row, vector) {
                self.note_undo_error(name, row, &error);
            }
        }
    }

    /// Revert one eager maintenance op (transaction ROLLBACK). Backend failures
    /// advance the owner's fault generation, so readers/callers cannot treat
    /// the derived graph as successfully published merely because rows match.
    pub fn apply_undo(&self, op: &VectorIndexUndoOp) {
        let _publication = self.begin_publication();
        match op {
            VectorIndexUndoOp::RemoveInserted { index_name, row_id } => {
                if let Err(e) = self.delete_vector(index_name, *row_id) {
                    self.note_undo_error(index_name, *row_id, &e);
                }
            }
            VectorIndexUndoOp::RestoreDeleted {
                index_name,
                row_id,
                vector,
            } => {
                if let Err(e) = self.insert_vector(index_name, *row_id, vector) {
                    self.note_undo_error(index_name, *row_id, &e);
                }
            }
            VectorIndexUndoOp::RestoreUpdated {
                index_name,
                row_id,
                old_vector,
            } => {
                if let Err(error) = self.delete_vector(index_name, *row_id) {
                    self.note_undo_error(index_name, *row_id, &error);
                }
                if let Some(vector) = old_vector {
                    if let Err(e) = self.insert_vector(index_name, *row_id, vector) {
                        self.note_undo_error(index_name, *row_id, &e);
                    }
                }
            }
        }
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

    /// Load index from bytes.
    /// Legacy PQ dumps did not encode deleted membership; rebuild those from
    /// authoritative rows (see [`QuantizedHnswIndex::from_bytes`]).
    pub fn load_index(&self, bytes: &[u8]) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.begin_publication();
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
        self.index_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    /// Get statistics for a specific index
    pub fn get_index_stats(&self, name: &str) -> Result<VectorIndexStats> {
        let indexes = self.indexes.read();
        let metadata = self.metadata.read();

        if let (Some(index), Some(meta)) = (indexes.get(name), metadata.get(name)) {
            let (num_vectors, dimensions, quantization, memory_bytes) = match index {
                IndexStorage::Standard(idx) => {
                    // Live count (tombstones excluded): deletes hide entries
                    // from search, so stats must not keep counting them.
                    // Memory is still estimated from the physical entry count
                    // because hnsw_rs never frees tombstoned vectors.
                    let num_vectors = idx.live_len();
                    let physical = idx.len();
                    let dimensions = match &meta.index_type {
                        VectorIndexType::Standard(config) => config.dimension,
                        _ => 0,
                    } as i32;
                    let memory_bytes = (physical as i64) * (dimensions as i64) * 4 + 1024;
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

    // ---- R4.2: standard HNSW graph dump / reload ---------------------------

    /// Sprinter 746342a0bfbb: the dump state of standard index `name`; `None`
    /// when it is not a registered standard index.
    pub fn standard_index_dump_state(&self, name: &str) -> Option<StandardDumpState> {
        match self.indexes.read().get(name) {
            Some(IndexStorage::Standard(index)) if index.is_empty() => Some(StandardDumpState::Empty),
            Some(IndexStorage::Standard(index)) if index.is_unchanged_since_dump() => {
                Some(StandardDumpState::Unchanged)
            }
            Some(IndexStorage::Standard(_)) => Some(StandardDumpState::Changed),
            _ => None,
        }
    }

    /// Record that the dump taken at `generation` now durably describes
    /// standard index `name` (see [`crate::vector::GraphIndex::mark_dumped`]).
    pub fn mark_standard_index_dumped(&self, name: &str, generation: u64) {
        if let Some(IndexStorage::Standard(index)) = self.indexes.read().get(name) {
            index.mark_dumped(generation);
        }
    }

    /// Whether `sidecar` describes standard index `name` as it is registered
    /// now: same name, table, column and construction parameters. A checkpoint
    /// keeps an unchanged index's dump only when its sidecar still matches.
    pub fn standard_index_matches_sidecar(
        &self,
        name: &str,
        sidecar: &super::index_snapshot::VectorGraphSidecar,
    ) -> bool {
        let metadata = self.metadata.read();
        let Some(meta) = metadata.get(name) else {
            return false;
        };
        let VectorIndexType::Standard(config) = &meta.index_type else {
            return false;
        };
        sidecar.index_name == name
            && sidecar.table_name == meta.table_name
            && sidecar.column_name == meta.column_name
            && sidecar.dimension == config.dimension
            && sidecar.metric == metric_name(config.distance_metric)
            && sidecar.m == config.max_connections
            && sidecar.ef_construction == config.ef_construction
    }

    /// Dump a standard (non-quantized, non-persistent) HNSW index's graph into
    /// `dir` and return it with the sidecar describing it. Returns `Ok(None)`
    /// for non-Standard backends (quantized/persistent have their own paths).
    ///
    /// Sprinter 746342a0bfbb: the dump goes under a FRESH basename derived from
    /// the index's stable id (`index_snapshot::graph_dump_basename`), never
    /// over the files the current sidecar references, and is fsynced and
    /// renamed into place before this returns. Switching the sidecar to it and
    /// removing the previous dump is the caller's
    /// (`StorageEngine::persist_index_snapshots`).
    pub fn dump_standard_index(&self, name: &str, dir: &std::path::Path) -> Result<Option<StandardIndexDump>> {
        use super::index_snapshot as snap;
        let indexes = self.indexes.read();
        let metadata = self.metadata.read();
        let (Some(index), Some(meta)) = (indexes.get(name), metadata.get(name)) else {
            return Err(Error::query_execution(format!("Index '{}' not found", name)));
        };
        let IndexStorage::Standard(index) = index else {
            return Ok(None);
        };
        let VectorIndexType::Standard(config) = &meta.index_type else {
            return Ok(None);
        };

        let basename = fresh_graph_dump_basename(name, dir);
        let dump = index.dump_graph(dir, &basename)?;
        let digest = |suffix: &str| {
            let path = dir.join(format!("{basename}{suffix}"));
            snap::dump_file_digest(&path)
                .map_err(|e| Error::storage(format!("HNSW graph dump {} digest: {e}", path.display())))
        };
        let (graph_sha256, data_sha256) = match (digest(snap::HNSW_GRAPH_SUFFIX), digest(snap::HNSW_DATA_SUFFIX)) {
            (Ok(graph), Ok(data)) => (graph, data),
            (Err(e), _) | (_, Err(e)) => {
                // Nothing references the new files yet: do not leave them behind.
                if let Err(remove) = snap::remove_graph_dump(dir, &basename) {
                    tracing::warn!("HNSW graph dump '{}': could not remove it: {}", basename, remove);
                }
                return Err(e);
            }
        };

        Ok(Some(StandardIndexDump {
            sidecar: snap::VectorGraphSidecar {
                format_version: snap::INDEX_SNAPSHOT_FORMAT_VERSION,
                index_name: name.to_string(),
                table_name: meta.table_name.clone(),
                column_name: meta.column_name.clone(),
                dimension: config.dimension,
                metric: metric_name(config.distance_metric).to_string(),
                m: config.max_connections,
                ef_construction: config.ef_construction,
                basename,
                id_mapping: dump.id_mapping,
                reverse_mapping: dump.reverse_mapping,
                graph_sha256,
                data_sha256,
            },
            generation: dump.generation,
        }))
    }

    /// Reconstruct a standard HNSW index from a graph dump + sidecar, instead
    /// of rebuilding it from a table scan. Errors surface to the caller, which
    /// falls back to the scan rebuild (and never leaves an empty index behind).
    pub fn reload_standard_index(
        &self,
        sidecar: &super::index_snapshot::VectorGraphSidecar,
        dir: &std::path::Path,
    ) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.begin_publication();
        if sidecar.format_version != super::index_snapshot::INDEX_SNAPSHOT_FORMAT_VERSION {
            return Err(Error::storage(format!(
                "vector snapshot format {} != supported {}",
                sidecar.format_version,
                super::index_snapshot::INDEX_SNAPSHOT_FORMAT_VERSION
            )));
        }
        // Spec P2.2: only the dump this sidecar describes is loaded. A dump
        // file that is missing or differs (another checkpoint's, a partial
        // copy) is an error here, and the caller rebuilds the index from rows.
        for (suffix, expected) in [
            (super::index_snapshot::HNSW_GRAPH_SUFFIX, sidecar.graph_sha256),
            (super::index_snapshot::HNSW_DATA_SUFFIX, sidecar.data_sha256),
        ] {
            let path = dir.join(format!("{}{suffix}", sidecar.basename));
            let actual = super::index_snapshot::dump_file_digest(&path).map_err(|e| {
                Error::storage(format!(
                    "HNSW graph dump {} is missing or unreadable ({e})",
                    path.display()
                ))
            })?;
            if actual != expected {
                return Err(Error::storage(format!(
                    "HNSW graph dump {} does not match the digest its vecsnap sidecar recorded",
                    path.display()
                )));
            }
        }
        let distance_metric = metric_from_name(&sidecar.metric)?;
        let config = HnswConfig {
            dimension: sidecar.dimension,
            distance_metric,
            max_connections: sidecar.m,
            ef_construction: sidecar.ef_construction,
            ef_search_base: 200,
            dynamic_ef_search: true,
            ef_search_min: 50,
            ef_search_max: 500,
        };

        // Reload the graph BEFORE registering anything: a failure here must
        // leave the manager exactly as it was (no empty index registered).
        let index = MultiMetricHnswIndex::reload_from_dump(
            config.clone(),
            dir,
            &sidecar.basename,
            sidecar.id_mapping.clone(),
            &sidecar.reverse_mapping,
        )?;

        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();
        if indexes.contains_key(&sidecar.index_name) {
            return Err(Error::query_execution(format!(
                "Index '{}' already exists",
                sidecar.index_name
            )));
        }
        let meta = VectorIndexMetadata {
            name: sidecar.index_name.clone(),
            table_name: sidecar.table_name.clone(),
            column_name: sidecar.column_name.clone(),
            index_type: VectorIndexType::Standard(config),
        };
        indexes.insert(sidecar.index_name.clone(), IndexStorage::Standard(index));
        metadata.insert(sidecar.index_name.clone(), meta);
        drop(metadata);
        drop(indexes);
        self.records.write().entry(sidecar.index_name.clone()).or_default();
        self.external_ids.write().entry(sidecar.index_name.clone()).or_default();
        self.index_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.clear_degraded(&sidecar.index_name);
        Ok(())
    }

    /// R4.2 / R5.V2: reopen a RocksDB-backed persistent vector index in place
    /// instead of downgrading it to a freshly rebuilt in-memory Standard index.
    #[cfg(feature = "vector-persist")]
    pub fn reopen_persistent_index(
        &self,
        name: &str,
        table_name: &str,
        column_name: &str,
        config: PersistentVectorConfig,
        db: Arc<rocksdb::DB>,
        gate: Arc<super::write_gate::WriteGate>,
    ) -> Result<()> {
        let _registry_operation = self.registry_operation();
        let _publication = self.begin_publication();
        let index_id = stable_index_id(name);
        let index = crate::vector::persistent::PersistentVectorIndex::open_with_key_manager(
            db,
            gate,
            index_id,
            self.persistent_key_manager(),
        )?;

        // Older catalog definitions carry only layout hints, not the backend's
        // persisted PQ/precision choices. Validate the schema-facing contract,
        // then publish the configuration actually decoded from durable records.
        let durable = index.config();
        if durable.dimension != config.dimension || durable.distance_metric != config.distance_metric {
            return Err(Error::storage(format!(
                "persistent index '{name}' catalog layout (dimension {}, metric {:?}) does not match durable graph \
                 (dimension {}, metric {:?}); refusing to replace its persistent backend",
                config.dimension, config.distance_metric, durable.dimension, durable.distance_metric
            )));
        }
        let actual = PersistentVectorConfig {
            dimension: durable.dimension,
            distance_metric: durable.distance_metric,
            pq_enabled: durable.pq_enabled,
            rerank_precision: persistent_precision_name(durable.rerank_precision).to_string(),
        };
        let mut indexes = self.indexes.write();
        let mut metadata = self.metadata.write();
        if indexes.contains_key(name) {
            return Err(Error::query_execution(format!("Index '{}' already exists", name)));
        }
        let meta = VectorIndexMetadata {
            name: name.to_string(),
            table_name: table_name.to_string(),
            column_name: column_name.to_string(),
            index_type: VectorIndexType::Persistent(actual),
        };
        indexes.insert(name.to_string(), IndexStorage::Persistent(index));
        metadata.insert(name.to_string(), meta);
        drop(metadata);
        drop(indexes);
        self.records.write().entry(name.to_string()).or_default();
        self.external_ids.write().entry(name.to_string()).or_default();
        self.index_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.clear_degraded(name);
        Ok(())
    }
}

/// Sprinter 746342a0bfbb: a dump basename for `index_name` that no file in
/// `dir` has: the index's stable id and a nanosecond generation. It replaces
/// the old basename, the index name with every non-alphanumeric mapped to `_`,
/// under which `a.b` and `a_b` shared one dump.
fn fresh_graph_dump_basename(index_name: &str, dir: &std::path::Path) -> String {
    use super::index_snapshot as snap;
    let id = stable_index_id(index_name);
    let mut generation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    for _ in 0..64 {
        let basename = snap::graph_dump_basename(id, generation);
        if !dir.join(format!("{basename}{}", snap::HNSW_GRAPH_SUFFIX)).exists()
            && !dir.join(format!("{basename}{}", snap::HNSW_DATA_SUFFIX)).exists()
        {
            return basename;
        }
        generation = generation.wrapping_add(1);
    }
    snap::graph_dump_basename(id, generation)
}

fn metric_name(metric: DistanceMetric) -> &'static str {
    match metric {
        DistanceMetric::L2 => "l2",
        DistanceMetric::Cosine => "cosine",
        DistanceMetric::InnerProduct => "ip",
    }
}

fn metric_from_name(name: &str) -> Result<DistanceMetric> {
    match name {
        "l2" => Ok(DistanceMetric::L2),
        "cosine" => Ok(DistanceMetric::Cosine),
        "ip" => Ok(DistanceMetric::InnerProduct),
        other => Err(Error::storage(format!("unknown vector metric '{other}' in snapshot"))),
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

/// Exact distance between two raw vectors, dispatched through the in-house
/// SIMD kernels (AVX2+FMA when the CPU supports them, scalar fallback
/// otherwise) — R5.V6; this was the last hand-rolled scalar distance loop.
/// Semantics match the kernels' scalar fallbacks exactly: L2 takes the
/// square root, cosine returns 1.0 when either norm is zero, inner product
/// returns the negated dot product. The dimension-mismatch guard stays here
/// because the kernels assert equal lengths while exact scans historically
/// rank mismatched records last via +INF.
fn exact_distance(metric: DistanceMetric, a: &Vector, b: &Vector) -> f32 {
    if a.len() != b.len() {
        return f32::INFINITY;
    }
    match metric {
        DistanceMetric::L2 => crate::vector::l2_distance(a, b),
        DistanceMetric::Cosine => crate::vector::cosine_distance(a, b),
        DistanceMetric::InnerProduct => crate::vector::inner_product_distance(a, b),
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
    fn staged_standard_hooks_and_undo_defer_snapshot_invalidation_to_the_row_group() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let manager = VectorIndexManager::new();
        for (index, table) in [("t_vec", "t"), ("u_vec", "u")] {
            manager
                .create_index(index.into(), table.into(), "embedding".into(), 2, DistanceMetric::L2)
                .unwrap();
        }
        let schema = crate::Schema::new(vec![crate::Column::new("embedding", crate::DataType::Vector(2))]);
        let old = crate::Tuple::new(vec![crate::Value::Vector(vec![1.0, 0.0])]);
        let new = crate::Tuple::new(vec![crate::Value::Vector(vec![0.0, 1.0])]);
        manager.on_row_insert("t", 1, &schema, &old);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        manager.set_snapshot_invalidation_hook(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        manager.mark_snapshot_clean();
        let undo = manager.on_row_update_staged("t", 1, &schema, Some(&old), &new);
        assert_eq!(manager.search("t_vec", &vec![0.0, 1.0], 1).unwrap()[0].0, 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no durable snapshot hook before COMMIT"
        );
        assert!(
            manager.snapshot_is_clean(),
            "staging must not consume the global clean flag"
        );
        let mut batch = rocksdb::WriteBatch::default();
        manager
            .stage_row_images("t", 1, &schema, Some(&new), &mut batch, usize::MAX)
            .unwrap();
        struct Deleted(std::collections::BTreeSet<Vec<u8>>);
        impl rocksdb::WriteBatchIterator for Deleted {
            fn put(&mut self, _: Box<[u8]>, _: Box<[u8]>) {
                panic!("Standard snapshot staging has no value writes");
            }
            fn delete(&mut self, key: Box<[u8]>) {
                self.0.insert(key.into_vec());
            }
        }
        let mut deleted = Deleted(std::collections::BTreeSet::new());
        batch.iterate(&mut deleted);
        assert_eq!(
            deleted.0,
            [
                super::super::index_snapshot::vec_snapshot_marker_key("t_vec"),
                super::super::index_snapshot::vec_snapshot_marker_key("u_vec"),
            ]
            .into_iter()
            .collect(),
            "all dump validity markers ride the owning group"
        );
        for action in undo.iter().rev() {
            manager.apply_undo_ram_only(action);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "ROLLBACK is RAM only");
        assert!(manager.snapshot_is_clean());
        assert_eq!(manager.search("t_vec", &vec![1.0, 0.0], 1).unwrap()[0].1, 0.0);
        // Only an applied owning group consumes the flag. A subsequent raw
        // vector API still has its original explicit snapshot-hook contract.
        manager.row_group_applied();
        assert!(!manager.snapshot_is_clean());
        manager.mark_snapshot_clean();
        manager.insert_vector("u_vec", 2, &vec![0.0, 1.0]).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn publication_ticket_tracks_overlapping_mutations_and_exhaustion() {
        let manager = VectorIndexManager::new();
        let before = manager.publication_ticket().unwrap();
        let first = manager.begin_publication();
        let second = manager.begin_publication();
        assert!(manager.publication_ticket().is_none());
        drop(first);
        assert!(manager.publication_ticket().is_none());
        drop(second);
        assert!(!manager.publication_is_current(before));
        assert!(manager.publication_ticket().is_some());
        manager.publication_epoch.store(
            PUBLICATION_EXHAUSTED - (PUBLICATION_INFLIGHT + 1),
            std::sync::atomic::Ordering::SeqCst,
        );
        drop(manager.begin_publication());
        assert!(
            manager.publication_ticket().is_none(),
            "generation never wraps into an accepted old ticket"
        );
        drop(manager.begin_publication());
        assert!(manager.publication_ticket().is_none());
    }

    #[test]
    fn hook_faults_are_scoped_to_the_actual_table_and_exhaustion_fails_closed() {
        let manager = VectorIndexManager::new();
        manager
            .create_index(
                "bad_idx".into(),
                "bad".into(),
                "embedding".into(),
                2,
                DistanceMetric::L2,
            )
            .unwrap();
        let schema = crate::Schema::new(vec![crate::Column::new("embedding", crate::DataType::Vector(2))]);
        let wrong = crate::Tuple::new(vec![crate::Value::Vector(vec![1.0, 0.0, 0.0])]);
        assert!(manager.on_row_insert("bad", 1, &schema, &wrong).is_empty());
        assert_eq!(manager.hook_error_generation("bad"), Some(1));
        assert_eq!(manager.hook_error_generation("healthy"), Some(0));
        manager
            .hook_faults
            .get("bad")
            .unwrap()
            .store(u64::MAX - 1, std::sync::atomic::Ordering::SeqCst);
        manager.on_row_insert("bad", 1, &schema, &wrong);
        assert_eq!(manager.hook_error_generation("bad"), None);
        manager.on_row_insert("bad", 1, &schema, &wrong);
        assert_eq!(manager.hook_error_generation("bad"), None);
        assert_eq!(manager.hook_error_generation("healthy"), Some(0));
    }

    #[test]
    fn vector_column_relabel_uses_original_mapping_without_changing_graph_identity() {
        let manager = VectorIndexManager::new();
        for (name, column) in [("first", "a"), ("second", "b")] {
            manager
                .create_index(name.into(), "t".into(), column.into(), 2, DistanceMetric::L2)
                .unwrap();
            manager.insert_vector(name, 7, &vec![1.0, 0.0]).unwrap();
        }
        let before = manager.search("first", &vec![1.0, 0.0], 1).unwrap();
        manager.relabel_table_columns("t", &[("a".into(), "b".into()), ("b".into(), "a".into())]);
        assert_eq!(manager.get_metadata("first").unwrap().column_name, "b");
        assert_eq!(manager.get_metadata("second").unwrap().column_name, "a");
        assert_eq!(manager.search("first", &vec![1.0, 0.0], 1).unwrap(), before);
    }

    #[test]
    fn public_quantized_save_load_preserves_deleted_and_replaced_membership() {
        let manager = VectorIndexManager::new();
        let training: Vec<_> = (0..16).map(|row| vec![row as f32, 0.0]).collect();
        manager
            .create_quantized_index_with_params(
                "pq_saved".into(),
                "t".into(),
                "v".into(),
                2,
                DistanceMetric::L2,
                ProductQuantizerConfig {
                    num_subquantizers: 1,
                    num_centroids: 2,
                    dimension: 2,
                    training_iterations: 2,
                    min_training_samples: 16,
                },
                &training,
                12,
                48,
            )
            .unwrap();
        manager.insert_vector("pq_saved", 1, &vec![0.0, 0.0]).unwrap();
        manager.insert_vector("pq_saved", 2, &vec![15.0, 0.0]).unwrap();
        manager.insert_vector("pq_saved", 3, &vec![9.0, 0.0]).unwrap();
        manager.insert_vector("pq_saved", 2, &vec![1.0, 0.0]).unwrap();
        manager.delete_vector("pq_saved", 1).unwrap();
        manager.delete_vector("pq_saved", 3).unwrap();
        let expected = manager.search("pq_saved", &vec![0.0, 0.0], 10).unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].0, 2);
        let saved = manager.save_index("pq_saved").unwrap();
        let loaded = VectorIndexManager::new();
        loaded.load_index(&saved).unwrap();
        assert_eq!(loaded.index_live_size("pq_saved"), Some(1));
        assert_eq!(loaded.search("pq_saved", &vec![0.0, 0.0], 10).unwrap(), expected);
        assert_eq!(
            bincode::serialize(&loaded.get_metadata("pq_saved").unwrap()).unwrap(),
            bincode::serialize(&manager.get_metadata("pq_saved").unwrap()).unwrap(),
            "public roundtrip preserves table/column/index/config identity"
        );
        loaded.insert_vector("pq_saved", 4, &vec![8.0, 0.0]).unwrap();
        loaded.delete_vector("pq_saved", 2).unwrap();
        let again = VectorIndexManager::new();
        again.load_index(&loaded.save_index("pq_saved").unwrap()).unwrap();
        assert_eq!(again.index_live_size("pq_saved"), Some(1));
        assert_eq!(again.search("pq_saved", &vec![0.0, 0.0], 10).unwrap()[0].0, 4);
        again.delete_vector("pq_saved", 4).unwrap();
        let empty = VectorIndexManager::new();
        empty.load_index(&again.save_index("pq_saved").unwrap()).unwrap();
        assert_eq!(empty.index_live_size("pq_saved"), Some(0));
        assert!(empty.search("pq_saved", &vec![0.0, 0.0], 10).unwrap().is_empty());
    }

    #[test]
    fn ram_only_reconciliation_and_journal_undo_never_invalidate_durable_snapshots() {
        let manager = VectorIndexManager::new();
        manager
            .create_index("standard".into(), "t".into(), "v".into(), 2, DistanceMetric::L2)
            .unwrap();
        let training: Vec<_> = (0..16).map(|row| vec![row as f32, 0.0]).collect();
        manager
            .create_quantized_index(
                "quantized".into(),
                "t".into(),
                "v".into(),
                2,
                DistanceMetric::L2,
                ProductQuantizerConfig {
                    num_subquantizers: 1,
                    num_centroids: 2,
                    dimension: 2,
                    training_iterations: 2,
                    min_training_samples: 4,
                },
                &training,
            )
            .unwrap();
        for name in ["standard", "quantized"] {
            manager.insert_vector(name, 1, &vec![9.0, 0.0]).unwrap();
        }
        let invalidations = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = Arc::clone(&invalidations);
        manager.set_snapshot_invalidation_hook(Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        manager.mark_snapshot_clean();
        let schema = crate::Schema::new(vec![crate::Column::new("v", crate::DataType::Vector(2))]);
        manager
            .reconcile_rows_ram_only(
                "t",
                &schema,
                &[
                    (1, None),
                    (2, Some(crate::Tuple::new(vec![crate::Value::Vector(vec![0.0, 0.0])]))),
                ],
            )
            .unwrap();
        for name in ["standard", "quantized"] {
            assert_eq!(manager.index_live_size(name), Some(1));
            assert_eq!(manager.search(name, &vec![0.0, 0.0], 1).unwrap()[0].0, 2);
            manager.apply_undo_ram_only(&VectorIndexUndoOp::RemoveInserted {
                index_name: name.into(),
                row_id: 2,
            });
            assert_eq!(manager.index_live_size(name), Some(0));
            manager.apply_undo_ram_only(&VectorIndexUndoOp::RestoreDeleted {
                index_name: name.into(),
                row_id: 1,
                vector: vec![9.0, 0.0],
            });
            assert_eq!(manager.search(name, &vec![9.0, 0.0], 1).unwrap()[0].0, 1);
            manager.apply_undo_ram_only(&VectorIndexUndoOp::RestoreUpdated {
                index_name: name.into(),
                row_id: 1,
                old_vector: Some(vec![8.0, 0.0]),
            });
            assert_eq!(manager.index_live_size(name), Some(1));
        }
        assert_eq!(invalidations.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(manager.snapshot_is_clean());
        assert_eq!(manager.hook_error_generation("t"), Some(0));
    }

    #[cfg(feature = "vector-persist")]
    #[test]
    fn persistent_reopen_metadata_follows_durable_precision_and_pq_and_refuses_wrong_layout() {
        use crate::vector::persistent::VectorPrecision;
        let mut cells = 0;
        for precision in [VectorPrecision::F32, VectorPrecision::F16, VectorPrecision::I8] {
            for pq in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let db = Arc::new(rocksdb::DB::open_default(root.path()).unwrap());
                let gate = super::super::WriteGate::open();
                let creator = VectorIndexManager::new();
                let training: Vec<_> = (0..32).map(|row| vec![row as f32, 0.0, 1.0, 0.0]).collect();
                creator
                    .create_persistent_index(
                        "reopen_config".into(),
                        "t".into(),
                        "v".into(),
                        4,
                        DistanceMetric::L2,
                        pq.then_some(ProductQuantizerConfig {
                            num_subquantizers: 2,
                            num_centroids: 4,
                            dimension: 4,
                            training_iterations: 2,
                            min_training_samples: 32,
                        }),
                        Some(precision),
                        &training,
                        Arc::clone(&db),
                        Arc::clone(&gate),
                    )
                    .unwrap();
                creator
                    .insert_vector("reopen_config", 7, &vec![1.0, 0.0, 1.0, 0.0])
                    .unwrap();
                let before = db.latest_sequence_number();
                let prefix = format!("__vidx:{}:", stable_index_id("reopen_config"));
                let bytes = VectorIndexManager::index_image_digest(&db, prefix.as_bytes(), |_| {}).unwrap();
                let legacy_hint = PersistentVectorConfig {
                    dimension: 4,
                    distance_metric: DistanceMetric::L2,
                    pq_enabled: !pq,
                    rerank_precision: if precision == VectorPrecision::F32 {
                        "f16"
                    } else {
                        "f32"
                    }
                    .into(),
                };
                let readonly_gate = super::super::WriteGate::closed("read-only reopen control");
                let reopened = VectorIndexManager::new();
                reopened
                    .reopen_persistent_index(
                        "reopen_config",
                        "t",
                        "v",
                        legacy_hint.clone(),
                        Arc::clone(&db),
                        Arc::clone(&readonly_gate),
                    )
                    .unwrap();
                let VectorIndexType::Persistent(actual) = reopened.get_metadata("reopen_config").unwrap().index_type
                else {
                    panic!("reopen must retain persistent backend")
                };
                assert_eq!(actual.dimension, 4);
                assert_eq!(actual.distance_metric, DistanceMetric::L2);
                assert_eq!(actual.pq_enabled, pq);
                assert_eq!(actual.rerank_precision, persistent_precision_name(precision));
                assert_eq!(
                    reopened.search("reopen_config", &vec![1.0, 0.0, 1.0, 0.0], 1).unwrap()[0].0,
                    7
                );
                for (dimension, metric) in [(3, DistanceMetric::L2), (4, DistanceMetric::Cosine)] {
                    let wrong = VectorIndexManager::new();
                    let error = wrong
                        .reopen_persistent_index(
                            "reopen_config",
                            "t",
                            "v",
                            PersistentVectorConfig {
                                dimension,
                                distance_metric: metric,
                                ..legacy_hint.clone()
                            },
                            Arc::clone(&db),
                            Arc::clone(&readonly_gate),
                        )
                        .unwrap_err();
                    assert!(error.to_string().contains("does not match durable graph"), "{error}");
                    assert!(!wrong.index_exists("reopen_config"));
                    assert!(wrong.get_metadata("reopen_config").is_err());
                }
                assert_eq!(db.latest_sequence_number(), before);
                assert_eq!(
                    VectorIndexManager::index_image_digest(&db, prefix.as_bytes(), |_| {}).unwrap(),
                    bytes
                );
                assert_eq!(readonly_gate.refusals(), 0);
                cells += 1;
            }
        }
        assert_eq!(cells, 6);
        eprintln!("persistent-reopen-config-receipt precision_pq_cells={cells}");
    }

    #[cfg(feature = "vector-persist")]
    #[test]
    fn ram_only_persistent_reconciliation_reloads_actual_bytes_once_without_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(rocksdb::DB::open_default(dir.path()).unwrap());
        let gate = super::super::WriteGate::open();
        let manager = VectorIndexManager::new();
        manager
            .create_persistent_index(
                "persistent".into(),
                "t".into(),
                "v".into(),
                2,
                DistanceMetric::L2,
                None,
                None,
                &[],
                Arc::clone(&db),
                Arc::clone(&gate),
            )
            .unwrap();
        manager.insert_vector("persistent", 1, &vec![9.0, 0.0]).unwrap();
        // Simulate a native group/applier changing durable graph bytes before
        // the manager publishes them; this independent backend is not exposed.
        let durable = crate::vector::persistent::PersistentVectorIndex::open(
            Arc::clone(&db),
            Arc::clone(&gate),
            stable_index_id("persistent"),
        )
        .unwrap();
        durable.remove(1).unwrap();
        durable.insert(2, &vec![0.0, 0.0]).unwrap();
        assert_eq!(manager.search("persistent", &vec![0.0, 0.0], 1).unwrap()[0].0, 1);
        let sequence = db.latest_sequence_number();
        let prefix = format!("__vidx:{}:", stable_index_id("persistent"));
        let before = VectorIndexManager::index_image_digest(&db, prefix.as_bytes(), |_| {}).unwrap();
        let invalidations = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = Arc::clone(&invalidations);
        manager.set_snapshot_invalidation_hook(Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        manager.mark_snapshot_clean();
        manager.set_persistent_policy(PersistentPolicy::ReplicaMemoryOnly);
        let schema = crate::Schema::new(vec![crate::Column::new("v", crate::DataType::Vector(2))]);
        manager
            .reconcile_rows_ram_only(
                "t",
                &schema,
                &[
                    (1, None),
                    (2, Some(crate::Tuple::new(vec![crate::Value::Vector(vec![0.0, 0.0])]))),
                ],
            )
            .unwrap();
        assert_eq!(manager.search("persistent", &vec![0.0, 0.0], 1).unwrap()[0].0, 2);
        assert_eq!(manager.index_live_size("persistent"), Some(1));
        assert_eq!(db.latest_sequence_number(), sequence);
        assert_eq!(
            VectorIndexManager::index_image_digest(&db, prefix.as_bytes(), |_| {}).unwrap(),
            before
        );
        assert_eq!(invalidations.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            gate.refusals(),
            0,
            "RAM reconciliation never asks even a replica's gate to write"
        );
    }

    #[test]
    fn uncertain_retirement_forgets_only_target_memory_without_running_durable_invalidation() {
        let manager = VectorIndexManager::new();
        for (name, table) in [("target_idx", "target"), ("other_idx", "other")] {
            manager
                .create_index(name.into(), table.into(), "embedding".into(), 2, DistanceMetric::L2)
                .unwrap();
        }
        let invalidations = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = Arc::clone(&invalidations);
        manager.set_snapshot_invalidation_hook(Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        manager.mark_snapshot_clean();
        manager.retire_table_indexes("target", "uncertain population");
        assert_eq!(invalidations.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(!manager.index_exists("target_idx"));
        assert!(!manager.table_has_indexes("target"));
        assert!(manager.index_exists("other_idx"));
        assert!(manager.table_has_indexes("other"));
        assert!(manager.has_any_index());
        assert!(manager.degraded_indexes().iter().any(|(name, _)| name == "target_idx"));
        manager.retire_table_indexes("target", "again");
        assert!(
            manager.has_any_index(),
            "idempotent retirement cannot underflow the registry count"
        );
    }

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

        // Insert vectors. The query's nearest neighbor (id 1) goes LAST — the
        // first-inserted point can be layer-0-isolated by hnsw_rs's unseeded
        // level draw and drop out of the k=2 result (~4/2000 runs); see
        // test_hnsw_basic in vector::hnsw_index for the full mechanism.
        manager.insert_vector("test_idx", 2, &vec![0.0, 1.0, 0.0]).unwrap();
        manager.insert_vector("test_idx", 3, &vec![0.0, 0.0, 1.0]).unwrap();
        manager.insert_vector("test_idx", 1, &vec![1.0, 0.0, 0.0]).unwrap();

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

    /// Sprinter 746342a0bfbb item 4: the dump basename was the index name with
    /// every non-alphanumeric mapped to `_`, so `docs.emb` and `docs_emb` wrote
    /// the same two files and the first sidecar described the second's graph.
    /// Each index now gets a basename from its stable id, and both reload.
    #[test]
    fn index_names_that_sanitize_alike_get_their_own_graph_dumps() {
        use crate::storage::index_snapshot as snap;
        let dir = tempfile::tempdir().unwrap();
        let manager = VectorIndexManager::new();
        for (name, offset) in [("docs.emb", 0.0_f32), ("docs_emb", 100.0)] {
            manager
                .create_index(
                    name.to_string(),
                    "docs".to_string(),
                    "emb".to_string(),
                    3,
                    DistanceMetric::L2,
                )
                .unwrap();
            for i in 0..20_u64 {
                manager
                    .insert_vector(name, i + 1, &vec![offset + i as f32, 1.0, 0.5])
                    .unwrap();
            }
        }
        let a = manager.dump_standard_index("docs.emb", dir.path()).unwrap().unwrap();
        let b = manager.dump_standard_index("docs_emb", dir.path()).unwrap().unwrap();
        assert_ne!(a.sidecar.basename, b.sidecar.basename);
        for (name, dump) in [("docs.emb", &a), ("docs_emb", &b)] {
            let prefix = format!("{:016x}-", stable_index_id(name));
            assert!(dump.sidecar.basename.starts_with(&prefix), "{}", dump.sidecar.basename);
            for (suffix, recorded) in [
                (snap::HNSW_GRAPH_SUFFIX, dump.sidecar.graph_sha256),
                (snap::HNSW_DATA_SUFFIX, dump.sidecar.data_sha256),
            ] {
                let path = dir.path().join(format!("{}{suffix}", dump.sidecar.basename));
                assert_eq!(
                    snap::dump_file_digest(&path).unwrap(),
                    recorded,
                    "{name}: its sidecar describes its own file"
                );
            }
        }

        let reopened = VectorIndexManager::new();
        reopened.reload_standard_index(&a.sidecar, dir.path()).unwrap();
        reopened.reload_standard_index(&b.sidecar, dir.path()).unwrap();
        assert_eq!(
            reopened.search("docs.emb", &vec![5.0, 1.0, 0.5], 1).unwrap(),
            vec![(6, 0.0)]
        );
        assert_eq!(
            reopened.search("docs_emb", &vec![105.0, 1.0, 0.5], 1).unwrap(),
            vec![(6, 0.0)]
        );
    }

    /// Sprinter 746342a0bfbb: the per-index dump state a checkpoint reads.
    #[test]
    fn the_standard_dump_state_follows_dumps_and_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let manager = VectorIndexManager::new();
        manager
            .create_index(
                "st_idx".to_string(),
                "st".to_string(),
                "emb".to_string(),
                3,
                DistanceMetric::L2,
            )
            .unwrap();
        assert_eq!(
            manager.standard_index_dump_state("st_idx"),
            Some(StandardDumpState::Empty)
        );
        assert_eq!(manager.standard_index_dump_state("missing"), None);

        manager.insert_vector("st_idx", 1, &vec![1.0, 0.0, 0.0]).unwrap();
        assert_eq!(
            manager.standard_index_dump_state("st_idx"),
            Some(StandardDumpState::Changed)
        );
        let dump = manager.dump_standard_index("st_idx", dir.path()).unwrap().unwrap();
        assert!(manager.standard_index_matches_sidecar("st_idx", &dump.sidecar));
        manager.mark_standard_index_dumped("st_idx", dump.generation);
        assert_eq!(
            manager.standard_index_dump_state("st_idx"),
            Some(StandardDumpState::Unchanged)
        );

        manager.delete_vector("st_idx", 1).unwrap();
        assert_eq!(
            manager.standard_index_dump_state("st_idx"),
            Some(StandardDumpState::Changed)
        );

        let mut other = dump.sidecar.clone();
        other.table_name = "renamed".to_string();
        assert!(!manager.standard_index_matches_sidecar("st_idx", &other));
    }
}
