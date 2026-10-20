//! A physical standby's vector indexes (spec P1.2): followed in memory,
//! never written.
//!
//! # Why
//!
//! HNSW indexes live in memory. A standard one is a graph built from rows; a
//! persistent one (`vector-persist`) is an in-RAM copy of its
//! `__vidx:{index_id}:*` keyspace whose own mutators WRITE those keys. On a
//! physical standby both the rows and the keys arrive through the applier,
//! byte for byte, and nothing else may write: the engine's gate is closed and
//! refuses — and counts — every attempt. Without this hook a standby served
//! the vector indexes its hydration built, whatever the primary did after.
//!
//! # How
//!
//! `StorageEngine::open_physical_standby` opens the engine's
//! `VectorIndexManager` under `PersistentPolicy::ReplicaMemoryOnly`, so no
//! manager mutator reaches the gate (and persistent indexes are skipped by
//! the row hooks). [`StandbyVectorHook`], the applier's default
//! [`VectorHook`], then publishes per applied batch:
//!
//! | Applier event | Publication |
//! |---|---|
//! | a row changed (pre- and post-batch images) | `on_row_insert` / `on_row_update` / `on_row_delete`: the table's standard indexes, in memory |
//! | a `__vidx:` record | kept for the batch, per index id; at `on_batch_published` applied to the registered persistent index of that id under one hold of its lock (`apply_persistent_records`); records that do not apply (an unknown suffix, a value that does not decode) → the index is reopened from its keys; an id no registered index owns → nothing |
//! | a vector `meta:index:` put | registered exactly as strict hydration registers it (`Catalog::build_physical_vector_index`): `hnsw` from rows, `persistent_hnsw` by reopening its keys; a definition unchanged in what an index keys off is left alone; an unsupported family (`hnsw_pq`, or `persistent_hnsw` without the feature) is marked degraded — queries on it scan, nothing is downgraded |
//! | a `meta:index:` delete, a table dropped | the index, or the table's indexes, forgotten (memory only) |
//! | a table re-derived | its standard indexes rebuilt from rows (their row-by-row step was skipped); its definitions not registered yet are registered |
//! | a full re-derivation | every index rebuilt or reopened from the definitions; registrations without one forgotten |
//!
//! # Records that arrive before their index
//!
//! A persistent index created on a populated table reaches a standby as
//! many write groups: its `meta` record, one group per backfilled vector,
//! and only then its `meta:index:` definition. The records of an index id no
//! registered index owns are not kept: registration opens the keyspace from
//! the store (`PersistentVectorIndex::open`), and the applier applies every
//! batch before publishing it, so the store already holds every one of
//! those records at its post-batch value. Opening from the store is also the
//! only correct start for a standby whose base image was cut in the middle
//! of such a CREATE INDEX: replaying only the records seen since would miss
//! the ones the image already held.

use std::collections::BTreeMap;
#[cfg(feature = "vector-persist")]
use std::collections::BTreeSet;

use parking_lot::Mutex;
use tracing::{debug, warn};

use super::physical_applier::{durable_schema, VectorHook};
use crate::storage::{
    index_family, physical_vector_family, IndexFamily, PersistedIndexDefinition, StorageEngine, VectorIndexMetadata,
    VectorIndexType,
};
use crate::{Schema, Tuple};

/// What a [`StandbyVectorHook`] did since it was created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorHookStats {
    /// `__vidx:` records applied to a registered persistent index.
    pub records_applied: u64,
    /// `__vidx:` records of an index id no registered index owns: an index
    /// whose definition has not arrived yet (its registration opens the
    /// keys), orphaned keys, or — without `vector-persist` — any record.
    pub records_unowned: u64,
    /// Indexes registered from a definition this standby had not built.
    pub registrations: u64,
    /// Registered indexes built again: a table re-derived, a definition
    /// rewritten, a full re-derivation.
    pub rebuilds: u64,
    /// Persistent indexes reopened from their keys because a record of
    /// theirs did not apply. Zero on every ordinary path.
    pub reopens: u64,
    /// Definitions this standby could not build (marked degraded).
    pub degraded: u64,
}

/// The applier's default [`VectorHook`]: a physical standby's vector indexes
/// follow the primary in memory and never write. See the module
/// documentation.
#[derive(Default)]
pub struct StandbyVectorHook {
    /// The current batch's `__vidx:` records, per index id, in key order:
    /// `(suffix after "__vidx:{id}:", post-batch value)`. Drained by
    /// [`VectorHook::on_batch_published`].
    #[cfg(feature = "vector-persist")]
    batch: Mutex<BTreeMap<u64, Vec<(Vec<u8>, Option<Vec<u8>>)>>>,
    stats: Mutex<VectorHookStats>,
}

impl StandbyVectorHook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> VectorHookStats {
        self.stats.lock().clone()
    }

    /// Build `name` from `definition`, replacing whatever is registered under
    /// it. Returns whether an index is registered afterwards.
    fn build(&self, engine: &StorageEngine, name: &str, definition: &PersistedIndexDefinition) -> bool {
        let manager = engine.vector_indexes();
        if manager.index_exists(name) {
            if let Err(e) = manager.drop_index(name) {
                debug!("physical standby: forgetting vector index '{}': {}", name, e);
            }
        }
        if !physical_vector_family(definition.index_type.as_deref()) {
            // What strict hydration refuses; the open would not have built it.
            manager.mark_degraded(
                name,
                format!(
                    "a physical standby does not build {:?} vector indexes (no downgrade); queries on it scan",
                    definition.index_type
                ),
            );
            self.stats.lock().degraded += 1;
            return false;
        }
        let schema = match durable_schema(engine, &definition.table_name) {
            Ok(Some(schema)) => schema,
            // The table arrives in a later batch: its registration
            // (`on_table_rederived`) builds the index then.
            Ok(None) => return false,
            Err(e) => {
                manager.mark_degraded(name, format!("the schema of '{}': {e}", definition.table_name));
                self.stats.lock().degraded += 1;
                return false;
            }
        };
        match engine.catalog().build_physical_vector_index(name, definition, &schema) {
            Ok(_) => {
                manager.clear_degraded(name);
                true
            }
            Err(e) => {
                manager.mark_degraded(name, e.to_string());
                self.stats.lock().degraded += 1;
                false
            }
        }
    }

    /// [`Self::build`], counted as a registration or a rebuild.
    fn build_counted(&self, engine: &StorageEngine, name: &str, definition: &PersistedIndexDefinition) {
        let registered = engine.vector_indexes().index_exists(name);
        if self.build(engine, name, definition) {
            let mut stats = self.stats.lock();
            if registered {
                stats.rebuilds += 1;
            } else {
                stats.registrations += 1;
            }
        }
    }

    fn forget(engine: &StorageEngine, name: &str) {
        let manager = engine.vector_indexes();
        if manager.index_exists(name) {
            if let Err(e) = manager.drop_index(name) {
                debug!("physical standby: forgetting vector index '{}': {}", name, e);
            }
        }
    }
}

/// Every vector-family index definition, by name.
fn vector_definitions(engine: &StorageEngine) -> crate::Result<BTreeMap<String, PersistedIndexDefinition>> {
    Ok(engine
        .catalog()
        .list_index_definitions()?
        .into_iter()
        .filter(|(_, definition)| index_family(definition.index_type.as_deref()) == Some(IndexFamily::Vector))
        .collect())
}

/// Whether the registered `index` is what `definition` describes, in
/// everything an index keys off: table, column and structure. A definition
/// is rewritten in place by RENAME COLUMN / RENAME TABLE, and an unchanged
/// rewrite must not cost a rebuild.
fn describes(index: &VectorIndexMetadata, definition: &PersistedIndexDefinition) -> bool {
    let tag = match index.index_type {
        VectorIndexType::Standard(_) => "hnsw",
        VectorIndexType::Quantized(_) => "hnsw_pq",
        VectorIndexType::Persistent(_) => "persistent_hnsw",
    };
    index.table_name == definition.table_name
        && index.column_name == definition.column_name
        && definition.index_type.as_deref() == Some(tag)
}

impl VectorHook for StandbyVectorHook {
    fn wants_rows(&self, engine: &StorageEngine, table: &str) -> bool {
        engine.vector_indexes().maintains_rows_of(table)
    }

    fn on_row(
        &self,
        engine: &StorageEngine,
        table: &str,
        row_id: u64,
        schema: &Schema,
        old: Option<&Tuple>,
        new: Option<&Tuple>,
    ) {
        // The DML hooks a primary's write path calls, minus the undo log (a
        // replicated row change is committed). Persistent indexes are skipped
        // inside them under `ReplicaMemoryOnly`.
        let manager = engine.vector_indexes();
        match (old, new) {
            (None, Some(new)) => drop(manager.on_row_insert(table, row_id, schema, new)),
            (Some(old), Some(new)) => drop(manager.on_row_update(table, row_id, schema, Some(old), new)),
            (Some(old), None) => drop(manager.on_row_delete(table, row_id, Some(schema), Some(old))),
            (None, None) => {}
        }
    }

    #[cfg_attr(not(feature = "vector-persist"), allow(unused_variables))]
    fn on_vidx_record(&self, _engine: &StorageEngine, key: &[u8], value: Option<&[u8]>) {
        #[cfg(feature = "vector-persist")]
        if let Some((index_id, suffix)) = crate::vector::persistent::split_record_key(key) {
            self.batch
                .lock()
                .entry(index_id)
                .or_default()
                .push((suffix.to_vec(), value.map(<[u8]>::to_vec)));
            return;
        }
        // No index can own it: a key of no index id, or no persistent index
        // compiled in (a definition of one is marked degraded).
        self.stats.lock().records_unowned += 1;
    }

    #[cfg_attr(not(feature = "vector-persist"), allow(unused_variables))]
    fn on_batch_published(&self, engine: &StorageEngine) {
        #[cfg(feature = "vector-persist")]
        {
            let batch = std::mem::take(&mut *self.batch.lock());
            let manager = engine.vector_indexes();
            let mut reopen = BTreeSet::new();
            for (index_id, records) in batch {
                let Some(name) = manager.persistent_index_with_id(index_id) else {
                    // See "Records that arrive before their index".
                    self.stats.lock().records_unowned += records.len() as u64;
                    continue;
                };
                match manager.apply_persistent_records(&name, &records) {
                    Ok(()) => self.stats.lock().records_applied += records.len() as u64,
                    Err(e) => {
                        warn!(
                            "physical standby: persistent vector index '{}' could not follow a replicated record \
                             ({}); reopening it from its keys",
                            name, e
                        );
                        reopen.insert(name);
                    }
                }
            }
            // The keys are the primary's as of this batch, so `open` over
            // them is always a valid replacement for a copy that could not
            // follow them.
            for name in reopen {
                self.stats.lock().reopens += 1;
                match engine.catalog().get_index_definition(&name) {
                    Ok(Some(definition)) => {
                        self.build(engine, &name, &definition);
                    }
                    Ok(None) => Self::forget(engine, &name),
                    Err(e) => {
                        warn!(
                            "physical standby: reading the definition of vector index '{}' failed ({}); forgetting it",
                            name, e
                        );
                        Self::forget(engine, &name);
                        manager.mark_degraded(&name, format!("its definition could not be read: {e}"));
                        self.stats.lock().degraded += 1;
                    }
                }
            }
        }
    }

    fn on_index_definition(&self, engine: &StorageEngine, index_name: &str, deleted: bool) {
        let manager = engine.vector_indexes();
        if deleted {
            // Also called for ART definitions; nothing is registered here
            // under their names.
            Self::forget(engine, index_name);
            manager.clear_degraded(index_name);
            return;
        }
        let definition = match engine.catalog().get_index_definition(index_name) {
            Ok(Some(definition)) => definition,
            Ok(None) => return,
            Err(e) => {
                warn!(
                    "physical standby: reading the definition of vector index '{}' failed: {}",
                    index_name, e
                );
                return;
            }
        };
        if manager
            .get_metadata(index_name)
            .is_ok_and(|current| describes(&current, &definition))
        {
            return;
        }
        self.build_counted(engine, index_name, &definition);
    }

    fn on_table_dropped(&self, engine: &StorageEngine, table: &str) {
        for (name, _) in engine.vector_indexes().indexes_on_table(table) {
            Self::forget(engine, &name);
        }
    }

    fn on_table_rederived(&self, engine: &StorageEngine, table: &str) {
        let definitions = match vector_definitions(engine) {
            Ok(definitions) => definitions,
            Err(e) => {
                warn!(
                    "physical standby: listing index definitions to rebuild the vector indexes of '{}' failed: {}",
                    table, e
                );
                return;
            }
        };
        let manager = engine.vector_indexes();
        for (name, definition) in definitions.iter().filter(|(_, d)| d.table_name == table) {
            // A persistent index follows its keys, not the rows: registered
            // under this definition, it is current.
            let current = manager.get_metadata(name).ok();
            if current
                .as_ref()
                .is_some_and(|m| describes(m, definition) && matches!(m.index_type, VectorIndexType::Persistent(_)))
            {
                continue;
            }
            self.build_counted(engine, name, definition);
        }
    }

    fn on_full_rebuild(&self, engine: &StorageEngine) {
        let definitions = match vector_definitions(engine) {
            Ok(definitions) => definitions,
            Err(e) => {
                warn!(
                    "physical standby: listing index definitions to rebuild the vector indexes failed: {}",
                    e
                );
                return;
            }
        };
        let manager = engine.vector_indexes();
        for registered in manager.list_all_metadata() {
            if !definitions.contains_key(&registered.name) {
                Self::forget(engine, &registered.name);
            }
        }
        // A full re-derivation is the rare fallback (an unknown key, a range
        // deletion too wide to name): every index is built again rather than
        // trusted, including the standard ones `on_table_rederived` rebuilt a
        // moment earlier.
        for (name, definition) in &definitions {
            self.build_counted(engine, name, definition);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::HnswConfig;

    fn definition(table: &str, column: &str, tag: &str) -> PersistedIndexDefinition {
        PersistedIndexDefinition {
            table_name: table.into(),
            column_name: column.into(),
            index_type: Some(tag.into()),
            options: Vec::new(),
        }
    }

    #[test]
    fn a_definition_describes_an_index_by_table_column_and_structure() {
        let standard = VectorIndexMetadata {
            name: "i".into(),
            table_name: "docs".into(),
            column_name: "emb".into(),
            index_type: VectorIndexType::Standard(HnswConfig::default()),
        };
        assert!(describes(&standard, &definition("docs", "emb", "hnsw")));
        assert!(
            !describes(&standard, &definition("docs", "emb2", "hnsw")),
            "a renamed column"
        );
        assert!(
            !describes(&standard, &definition("papers", "emb", "hnsw")),
            "a renamed table"
        );
        assert!(
            !describes(&standard, &definition("docs", "emb", "persistent_hnsw")),
            "another structure"
        );
    }
}
