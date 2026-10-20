//! A physical standby's applier: apply the primary's raw write batches
//! verbatim, then keep the serving engine's derived state right.
//!
//! # Why
//!
//! A physical standby's default column family is a byte-for-byte copy of its
//! primary's (`replication::physical`). A `StorageEngine` opened over it
//! (`StorageEngine::open_physical_standby`) does not answer queries from those
//! bytes alone, though: PRIMARY KEY lookups and `COUNT(*)` come from the
//! in-memory ART indexes, `AS OF` resolution from the snapshot map, new
//! snapshots' visibility from the MVCC timestamp counter, and a dozen caches
//! sit in front of the catalog. On a primary the write path maintains all of
//! them. On a standby nothing writes through the engine, so every applied
//! batch must be followed by a publication that brings the derived state in
//! line with the keys the batch touched — or the standby keeps answering from
//! the state it had when it opened.
//!
//! # How (per batch, one thread, under the engine's replay guard)
//!
//! 1. **Decode** the raw batch ([`decode_raw_batch`]: the same record walker
//!    that validates it) and classify every key (`storage::keyspace`). A range
//!    deletion is enumerated against the store before it is applied (at most
//!    [`RANGE_ENUMERATION_CAP`] keys; beyond that the table is re-derived, and
//!    a range that is not one table's rows defers a full re-derivation, which
//!    also forgets every snapshot left without a record).
//! 2. **Pre-read**, before anything changes: the old image of every touched
//!    row of an indexed table, decoded with the pre-batch schema (an index
//!    removal must name the key the row actually held), and the pre-batch
//!    schemas and constraint sets of touched tables. Then the census hold
//!    (below), and the row-cache / committed-write-watermark invalidation of
//!    every touched row — invalidated before the new bytes are visible.
//! 3. **Apply** verbatim through [`LocalWriter::apply_source`]: one fsynced
//!    write of the source bytes plus the node-local cursor. The applier never
//!    writes to RocksDB in any other way.
//! 4. **Publish**, in this order:
//!
//! | Key class | Publication |
//! |---|---|
//! | `Dict` | `forget_cached_dictionaries` — first, so the rows below decode against the new entries |
//! | `MetaTable` | schema/constraints/statistics/row caches of the table; then by schema change: dropped → `drop_table_indexes` + vector hook; created → registered and backfilled (step 5); pure column rename → `rename_column_in_indexes`; columns appended → nothing, or the new UNIQUE flags registered and backfilled; anything else → the table is re-derived; `pg_temp_*` → `note_temp_table_present` |
//! | `MetaIndex` delete | `drop_index` (memory only) + vector hook |
//! | `Data` | new image decoded with the post-batch schema (`decode_stored_row`, the scan's decode); `stage_replicated_row(old, new).commit()` per row, a key swapped between rows inside the batch resolved by removing every refused row's old keys before adding the new ones; vector hook; row-id counter raised to the row id; `publish_live_replay_visibility`; decode failure → the table is re-derived |
//! | `TableConstraints` | constraints cache; new UNIQUE / FK indexes (`register_unique_constraint_indexes`, `create_fk_index`) registered and backfilled; a constraint that disappeared → the table is re-derived |
//! | `MetaIndex` put | ART family: `register_index_definition` + backfill of that index; vector family: vector hook |
//! | `Snapshot` | `ingest_snapshot_record` + `raise_timestamp`; delete → `forget_snapshots` (the batch's deletions at once) + `clear_cache` |
//! | `VersionIdx` / `Version` / `Vmeta` | timestamp raised past `u64::MAX - rev` and past a COPY marker's timestamp; COPY markers ingested / forgotten; the table's cached `AS OF` answers invalidated |
//! | `Counter` | `raise_row_counter_to(decoded value)` |
//! | `VgcWatermark` | `set_gc_low_watermark` + `raise_timestamp` |
//! | `Branch` / `Git` | `reload_branch_manager` (retried on the next batch while a registry names metadata not yet arrived) |
//! | `Statistics`, `Columnar*`, branch rows | statistics / row caches of the table |
//! | `Vidx` | vector hook |
//! | catalog classes (`Meta*`, `TableConstraints`, `Trigger*`, `ViewMeta`, `Statistics`, `Compression`) | [`DerivedStateListener::on_catalog_change`]; schema generation bumped once |
//! | `Cas` | nothing to invalidate (content-addressed by hash); only retries a pending re-derivation |
//! | logical WAL, cursors, durable index snapshots, TDE / elision sentinels, `vgc:anchor`, legacy counters, MV deltas, sync change log | nothing: no derived state reads them on a standby |
//! | `Unknown` | WARN once per prefix; `derived_state_suspect` set (sticky) and a full re-derivation deferred to [`PhysicalApplier::run_pending_rebuilds`] |
//!
//! 5. **Re-derive** the tables the batch created and those it queued:
//!    drop the table's ART registrations, register them again from the durable
//!    records exactly as an open would (`register_index_definition`,
//!    `register_table_indexes`) and backfill them by scan
//!    (`backfill_table_indexes`). A re-derivation that fails (a row that does
//!    not decode yet) leaves the table PENDING; it is retried when a later
//!    batch carries a key that can fix decoding, or at the next idle poll.
//!
//! # Readers during publication
//!
//! Readers do not take the replay guard. Between step 3 and the end of step 5
//! a touched table's index can lag its rows by one batch (a fresh row not yet
//! in the PK index, `pk_index_len` one batch behind). The applier therefore
//! holds the engine's uncommitted-write census for every table it publishes
//! (for all tables when a batch touches index definitions or unknown keys),
//! from before the apply until the publication is done — the same gate a
//! transaction's eager index writes take — so every miss-based and cardinality
//! fast path declines to the planner, which reads row storage and is exact. A
//! PENDING table keeps its hold until its re-derivation succeeds. The index
//! never advertises a row the store lacks: rows are applied first.
//!
//! # Consistency
//!
//! One [`PhysicalBatch`] is one primary write group, so publishing per batch
//! exposes exactly the states a primary passes through; DDL that spans write
//! groups (a column-storage migration's rows then its schema; RENAME COLUMN's
//! schema and index definitions, then its constraint records) converges when
//! its last group is published. DROP TABLE deletes everything the table owns
//! in one group (spec P1.1b).
//!
//! What this module does not do: the SQL layer's registries and caches
//! (functions, triggers, plan caches) are the [`DerivedStateListener`]'s
//! (`EmbeddedDatabase`, spec P1.4); vector indexes are the [`VectorHook`]'s
//! (spec P1.2), a no-op by default.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use rocksdb::{Direction, IteratorMode, ReadOptions};
use tracing::{debug, warn};

use super::physical::{walk_raw_batch, LocalWriter, PhysicalBatch, PhysicalError};
use crate::sql::TableConstraints;
use crate::storage::keyspace::{classify, KeyClass};
use crate::storage::{index_family, ArtIndexType, IndexFamily, StorageEngine, UncommittedWriteHold};
use crate::{Schema, Tuple, Value};

pub use super::physical::RawOp;

/// Most keys one range deletion is enumerated for. A wider range is not
/// published key by key: its table is re-derived from a scan instead (or,
/// when the range is not one table's rows, the whole derived state).
pub const RANGE_ENUMERATION_CAP: usize = 10_000;

/// Longest key prefix an `Unknown` warning quotes, and the most distinct
/// prefixes warned about per applier (later ones are only counted).
const UNKNOWN_PREFIX_BYTES: usize = 32;
const UNKNOWN_PREFIXES_WARNED: usize = 256;

/// A batch the applier could not apply. Publication failures never surface
/// here: once the bytes are durable a failed publication is turned into a
/// re-derivation (or the suspect flag), because the receiver retrying the
/// batch would only see a retransmission.
#[derive(Debug, thiserror::Error)]
pub enum ApplierError {
    /// The batch was refused by the decoder or the writer: a gap, a foreign
    /// history, a malformed record, a poisoned writer.
    #[error(transparent)]
    Physical(#[from] PhysicalError),
    /// The engine could not be read or its derived state built. From
    /// `apply`, only before the batch was applied: nothing was written.
    #[error("physical applier: {0}")]
    Engine(#[from] crate::Error),
}

pub type Result<T> = std::result::Result<T, ApplierError>;

/// Decode one raw default-column-family write batch (12-byte header and its
/// records) into its sequence-consuming operations, in batch order. The
/// record checks are exactly those [`PhysicalBatch::decode`] applies to the
/// bytes (`validate_raw_batch`, the same walker), so a batch that validates
/// always decodes and vice versa.
pub fn decode_raw_batch(raw: &[u8]) -> Result<Vec<RawOp<'_>>> {
    let mut ops = Vec::new();
    walk_raw_batch(raw, |op| ops.push(op))?;
    Ok(ops)
}

/// What the applier tells the layer above the engine — the SQL layer's
/// registries and caches (function, procedure and trigger registries, plan,
/// parse and result caches), which `EmbeddedDatabase` owns (spec P1.4).
pub trait DerivedStateListener: Send + Sync {
    /// A catalog key of `class` was written (`deleted == false`) or deleted by
    /// an applied batch. Called after the engine's own publication.
    fn on_catalog_change(&self, class: KeyClass, key: &[u8], deleted: bool);

    /// The whole derived state was re-derived (after an unknown key): every
    /// registry and cache may be stale. Defaults to one catalog change of
    /// class `Unknown` with an empty key.
    fn on_full_rebuild(&self) {
        self.on_catalog_change(KeyClass::Unknown, &[], false);
    }
}

/// A listener for an engine nobody serves SQL from (tests, tools).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopDerivedStateListener;

impl DerivedStateListener for NoopDerivedStateListener {
    fn on_catalog_change(&self, _class: KeyClass, _key: &[u8], _deleted: bool) {}
}

/// Vector-index maintenance on a physical standby (spec P1.2).
///
/// HNSW indexes are in-memory structures; a persistent one is an in-RAM copy
/// of replicated `__vidx:` keys whose own mutators WRITE those keys, so a
/// standby must follow them memory-only. The applier calls these hooks at the
/// points where the ART indexes are published; every default does nothing.
pub trait VectorHook: Send + Sync {
    /// Whether rows of `table` must be decoded for [`Self::on_row`] even when
    /// the table has no ART index.
    fn wants_rows(&self, _engine: &StorageEngine, _table: &str) -> bool {
        false
    }

    /// One row of `table` changed: `old` decoded before the batch, `new`
    /// after it, both shaped to the post-batch `schema`.
    fn on_row(
        &self,
        _engine: &StorageEngine,
        _table: &str,
        _row_id: u64,
        _schema: &Schema,
        _old: Option<&Tuple>,
        _new: Option<&Tuple>,
    ) {
    }

    /// One `__vidx:` record changed; `value` is its post-batch value (`None`
    /// when deleted).
    fn on_vidx_record(&self, _engine: &StorageEngine, _key: &[u8], _value: Option<&[u8]>) {}

    /// A vector-family `meta:index:` definition was written or deleted.
    fn on_index_definition(&self, _engine: &StorageEngine, _index_name: &str, _deleted: bool) {}

    /// `table` was dropped.
    fn on_table_dropped(&self, _engine: &StorageEngine, _table: &str) {}

    /// `table`'s ART indexes were re-derived from a scan.
    fn on_table_rederived(&self, _engine: &StorageEngine, _table: &str) {}

    /// The whole derived state was re-derived.
    fn on_full_rebuild(&self, _engine: &StorageEngine) {}
}

/// The default [`VectorHook`]: vector indexes are not maintained (P1.2).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopVectorHook;

impl VectorHook for NoopVectorHook {}

/// What one [`PhysicalApplier::apply`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// The batch's last source sequence: the cursor after it.
    pub source_sequence: u64,
    /// Distinct `data:` keys published (0 for a retransmission).
    pub rows: usize,
    /// Tables whose derived state fell back to a re-derivation in this call.
    pub tables_rebuilt: Vec<String>,
}

/// Counters since the applier was created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplierStats {
    /// Batches applied (retransmissions excluded).
    pub batches: u64,
    /// Distinct `data:` keys published.
    pub rows: u64,
    /// Fallback re-derivations of one table: a row that did not decode, a
    /// schema change the row-by-row path cannot follow, a constraint dropped,
    /// a range too wide to enumerate. Zero on the ordinary DML/DDL paths.
    pub rebuilds: u64,
    /// Planned index registrations with a backfill: a table created, an
    /// index or constraint added.
    pub registrations: u64,
    /// Full re-derivations (after an unknown key, or on request).
    pub full_rebuilds: u64,
    /// Keys no keyspace entry classifies.
    pub unknown_keys: u64,
    /// Tables whose re-derivation failed and is pending (each holds the census).
    pub pending_tables: usize,
    /// An unknown key was applied: this binary cannot vouch for the derived
    /// state of the primary's keyspace. Sticky; surfaced by the posture (P1.3).
    pub derived_state_suspect: bool,
}

/// State carried between batches; only touched under the replay guard.
#[derive(Default)]
struct ApplierState {
    /// Tables whose derived state is known stale, each holding the census so
    /// readers take the scan paths until it is re-derived.
    pending: BTreeMap<String, UncommittedWriteHold>,
    full_rebuild_pending: bool,
    branch_reload_pending: bool,
    warned_prefixes: HashSet<Vec<u8>>,
}

/// Applies a physical standby's source batches and publishes their effect on
/// the serving engine's derived state. See the module documentation.
pub struct PhysicalApplier {
    engine: Arc<StorageEngine>,
    writer: Arc<LocalWriter>,
    listener: Arc<dyn DerivedStateListener>,
    vector: Arc<dyn VectorHook>,
    stats: Mutex<ApplierStats>,
    state: Mutex<ApplierState>,
}

impl PhysicalApplier {
    /// An applier for `engine`, which must have been opened with
    /// `StorageEngine::open_physical_standby` over `writer` — the one writer
    /// of its store. Anything else is refused: publishing into an engine that
    /// does not read the store being written would serve one store's indexes
    /// over another's rows.
    pub fn new(
        engine: Arc<StorageEngine>,
        writer: Arc<LocalWriter>,
        listener: Arc<dyn DerivedStateListener>,
    ) -> Result<Self> {
        match engine.physical_standby_writer() {
            Some(own) if Arc::ptr_eq(own, &writer) => {}
            Some(_) => {
                return Err(crate::Error::config(
                    "the physical applier was handed a different writer than the one its engine was opened over",
                )
                .into())
            }
            None => {
                return Err(crate::Error::config(
                    "a physical applier needs an engine opened with StorageEngine::open_physical_standby",
                )
                .into())
            }
        }
        Ok(Self {
            engine,
            writer,
            listener,
            vector: Arc::new(NoopVectorHook),
            stats: Mutex::new(ApplierStats::default()),
            state: Mutex::new(ApplierState::default()),
        })
    }

    /// Replace the default (no-op) [`VectorHook`].
    pub fn with_vector_hook(mut self, hook: Arc<dyn VectorHook>) -> Self {
        self.vector = hook;
        self
    }

    pub fn engine(&self) -> &Arc<StorageEngine> {
        &self.engine
    }

    pub fn writer(&self) -> &Arc<LocalWriter> {
        &self.writer
    }

    pub fn stats(&self) -> ApplierStats {
        self.stats.lock().clone()
    }

    /// Hydrate a freshly opened standby engine nobody serves SQL from (tests,
    /// tools): `StorageEngine::hydrate_physical`, the strict physical-image
    /// index rebuild (`Catalog::rebuild_all_indexes_for_physical_snapshot`:
    /// ART indexes, standard HNSW from rows, row counters in memory) and the
    /// lazy catalog validation that `EmbeddedDatabase::open_physical_snapshot`
    /// runs too. Call it once, before the first batch.
    ///
    /// Spec P1.4: a SERVING standby is hydrated by
    /// `EmbeddedDatabase::with_physical_standby`, which owns the hydration of
    /// its engine; do not call this on that engine. The engine refuses a
    /// second hydration whatever the store holds.
    pub fn hydrate(&self) -> Result<()> {
        self.engine.hydrate_physical()?;
        Ok(())
    }

    /// Decode, pre-read, apply verbatim, publish. See the module
    /// documentation for the steps and the publication table.
    ///
    /// `Err` means the batch was NOT applied (or the writer poisoned itself:
    /// then nothing more is). Once the bytes are durable, a publication that
    /// cannot be completed is a re-derivation or the suspect flag, never an
    /// error.
    pub fn apply(&self, batch: &PhysicalBatch) -> Result<Applied> {
        let _replay = self.engine.replay_guard();
        let mut state = self.state.lock();

        // An exact retransmission changes nothing, and anything else at or
        // below the cursor is refused by the writer; neither needs a plan.
        if batch.last_sequence() <= self.writer.cursor().source_sequence() {
            if self.writer.apply_source(batch)? {
                // Unreachable through `apply_source`'s own rules; if it ever
                // happens, the batch was applied without a publication.
                self.defer_full_rebuild(&mut state, "a batch at or below the cursor was applied");
            }
            return Ok(Applied {
                source_sequence: batch.last_sequence(),
                ..Applied::default()
            });
        }

        // 1. Decode and classify; enumerate range deletions while the keys
        //    they remove still exist.
        let mut touched = Touched::default();
        for op in decode_raw_batch(batch.data())? {
            match op {
                RawOp::Put { key, .. } => touched.note(key, true),
                RawOp::Delete { key } => touched.note(key, false),
                RawOp::DeleteRange { start, end } => self.enumerate_range(start, end, &mut touched)?,
            }
        }

        // 2. Pre-read, census hold, invalidate before visible.
        let mut work = self.pre_read(&touched, &state)?;
        let held = touched.held_tables();
        let every_table = touched.holds_every_table();
        // A batch that touches no table's rows or catalog (sequence state,
        // the logical WAL) leaves every index exact: no hold.
        let _hold =
            (every_table || !held.is_empty()).then(|| self.engine.write_census().hold_for_replay(held, every_table));
        for (table, rows) in &touched.data {
            self.engine.invalidate_write_watermark(table);
            for row_id in rows.keys() {
                self.engine.row_cache().invalidate(table, *row_id);
            }
        }

        // 3. Apply verbatim.
        if !self.writer.apply_source(batch)? {
            return Ok(Applied {
                source_sequence: batch.last_sequence(),
                ..Applied::default()
            });
        }
        if touched.touches_catalog() {
            // Existence and plan caches key off the schema generation; bump
            // it as soon as the new catalog bytes are visible.
            self.engine.bump_schema_generation();
        }

        // 4. Publish.
        self.publish(&touched, &mut work, &mut state);

        // 5. Re-derive what the batch created or queued.
        let mut attempt: BTreeSet<String> = work.register.iter().chain(&work.rederive).cloned().collect();
        if touched.fixes_decoding {
            attempt.extend(state.pending.keys().cloned());
        }
        let fallback: BTreeSet<String> = work.rederive.iter().chain(state.pending.keys()).cloned().collect();
        let tables_rebuilt = self.rederive_tables(attempt, &fallback, &mut state);

        let rows: usize = touched.data.values().map(BTreeMap::len).sum();
        {
            let mut stats = self.stats.lock();
            stats.batches += 1;
            stats.rows += rows as u64;
            stats.pending_tables = state.pending.len();
        }
        Ok(Applied {
            source_sequence: batch.last_sequence(),
            rows,
            tables_rebuilt,
        })
    }

    /// Re-derive one table's ART indexes from the durable records and a scan
    /// (`rederive_table`), as a fallback would. A dropped table just loses
    /// its registrations.
    pub fn rebuild_table_derived_state(&self, table: &str) -> Result<()> {
        let _replay = self.engine.replay_guard();
        let mut state = self.state.lock();
        let tables = BTreeSet::from([table.to_string()]);
        self.rederive_tables(tables.clone(), &tables, &mut state);
        let pending = state.pending.contains_key(table);
        self.stats.lock().pending_tables = state.pending.len();
        if pending {
            return Err(crate::Error::storage(format!(
                "the derived state of '{table}' could not be re-derived; it stays pending and readers use scans"
            ))
            .into());
        }
        Ok(())
    }

    /// Re-derive the whole derived state: every table's ART indexes, the
    /// snapshot map and timestamp, the version-GC horizon, the branch
    /// registry, and every cache; then tell the listener and the vector hook.
    pub fn rebuild_all_derived_state(&self) -> Result<()> {
        let _replay = self.engine.replay_guard();
        let mut state = self.state.lock();
        let result = self.rebuild_all_locked(&mut state);
        self.stats.lock().pending_tables = state.pending.len();
        result.map_err(ApplierError::from)
    }

    /// Run what a batch deferred, for the receiver's idle poll: the full
    /// re-derivation an unknown key asked for, the pending tables, a branch
    /// registry that could not be loaded yet. Returns whether anything was
    /// pending.
    pub fn run_pending_rebuilds(&self) -> Result<bool> {
        let _replay = self.engine.replay_guard();
        let mut state = self.state.lock();
        if state.full_rebuild_pending {
            let result = self.rebuild_all_locked(&mut state);
            self.stats.lock().pending_tables = state.pending.len();
            result?;
            return Ok(true);
        }
        let mut anything = false;
        if state.branch_reload_pending {
            anything = true;
            self.reload_branches(&mut state);
        }
        if !state.pending.is_empty() {
            anything = true;
            let tables: BTreeSet<String> = state.pending.keys().cloned().collect();
            self.rederive_tables(tables.clone(), &tables, &mut state);
            self.stats.lock().pending_tables = state.pending.len();
        }
        Ok(anything)
    }

    // ---------------------------------------------------------------------
    // Steps 1-2
    // ---------------------------------------------------------------------

    /// Note every key a range deletion will remove, while they still exist.
    fn enumerate_range(&self, start: &[u8], end: &[u8], touched: &mut Touched) -> crate::Result<()> {
        let mut read = ReadOptions::default();
        read.set_total_order_seek(true);
        read.set_iterate_upper_bound(end.to_vec());
        let mut seen = 0_usize;
        for item in self
            .writer
            .db()
            .iterator_opt(IteratorMode::From(start, Direction::Forward), read)
        {
            let (key, _) = item?;
            if &*key >= end {
                break;
            }
            seen += 1;
            if seen > RANGE_ENUMERATION_CAP {
                match single_table_data_range(start, end) {
                    Some(table) => {
                        touched.overflow_tables.insert(table);
                    }
                    None => touched.overflow_unattributed = true,
                }
                return Ok(());
            }
            touched.note(&key, false);
        }
        Ok(())
    }

    /// Everything that must be read before the batch is applied.
    fn pre_read(&self, touched: &Touched, state: &ApplierState) -> Result<BatchWork> {
        let art = self.engine.art_indexes();
        let mut work = BatchWork::default();
        for table in &touched.overflow_tables {
            work.rederive.insert(table.clone());
        }
        for (table, rows) in &touched.data {
            if state.pending.contains_key(table) || work.rederive.contains(table) {
                continue;
            }
            let indexed = !art.list_table_indexes(table).is_empty() || self.vector.wants_rows(&self.engine, table);
            if !indexed {
                continue;
            }
            let pre_schema = match self.durable_schema(table) {
                Ok(schema) => schema,
                Err(e) => {
                    warn!(
                        "physical applier: the schema of '{}' does not decode ({}); re-deriving it",
                        table, e
                    );
                    work.rederive.insert(table.clone());
                    continue;
                }
            };
            let mut old = BTreeMap::new();
            let mut undecodable = false;
            for row in rows.values() {
                let image = match (self.engine.get(&row.key)?, &pre_schema) {
                    (None, _) => None,
                    (Some(bytes), Some(schema)) => {
                        match self.engine.decode_stored_row(table, schema, row.row_id, &bytes) {
                            Ok(tuple) => Some(tuple),
                            Err(e) => {
                                debug!(
                                    "physical applier: old '{}' row {} does not decode: {}",
                                    table, row.row_id, e
                                );
                                undecodable = true;
                                break;
                            }
                        }
                    }
                    // An indexed table with a row but no schema: nothing the
                    // row-by-row path can describe.
                    (Some(_), None) => {
                        undecodable = true;
                        break;
                    }
                };
                old.insert(row.row_id, image);
            }
            if undecodable {
                work.rederive.insert(table.clone());
                continue;
            }
            work.staged.insert(table.clone(), StagedTable { pre_schema, old });
        }
        for table in &touched.meta_tables {
            let pre = match self.durable_schema(table) {
                Ok(schema) => schema,
                Err(e) => {
                    warn!(
                        "physical applier: the schema of '{}' does not decode ({}); re-deriving it",
                        table, e
                    );
                    work.rederive.insert(table.clone());
                    None
                }
            };
            work.pre_schemas.insert(table.clone(), pre);
        }
        for table in &touched.constraints {
            let pre = match self.durable_constraints(table) {
                Ok(constraints) => constraints,
                Err(e) => {
                    warn!(
                        "physical applier: the constraints of '{}' do not decode ({}); re-deriving the table",
                        table, e
                    );
                    work.rederive.insert(table.clone());
                    None
                }
            };
            work.pre_constraints.insert(table.clone(), pre);
        }
        Ok(work)
    }

    // ---------------------------------------------------------------------
    // Step 4
    // ---------------------------------------------------------------------

    fn publish(&self, touched: &Touched, work: &mut BatchWork, state: &mut ApplierState) {
        if touched.dictionaries {
            // Before any row below is decoded against a dictionary.
            self.engine.forget_cached_dictionaries();
        }
        self.publish_meta_tables(touched, work);
        self.publish_index_deletes(touched, work, state);
        self.stage_rows(touched, work);
        self.publish_constraints(touched, work);
        self.publish_index_puts(work);
        self.publish_appended_unique_columns(work);
        self.publish_time_travel(touched, state);
        self.publish_counters(touched);
        if touched.branch_registry || state.branch_reload_pending {
            self.reload_branches(state);
        }
        self.publish_caches(touched);
        self.publish_vector_records(touched);
        self.publish_listener(touched);
        self.publish_unknown(touched, state);
    }

    fn publish_meta_tables(&self, touched: &Touched, work: &mut BatchWork) {
        let art = self.engine.art_indexes();
        for table in &touched.meta_tables {
            self.invalidate_table_caches(table);
            let post = match self.durable_schema(table) {
                Ok(post) => post,
                Err(e) => {
                    warn!(
                        "physical applier: the new schema of '{}' does not decode ({}); re-deriving it",
                        table, e
                    );
                    work.rederive.insert(table.clone());
                    continue;
                }
            };
            if post.is_some() && crate::sql::temp_tables::is_temp_key(table) {
                // Arms the session-private visibility filter, as
                // `Catalog::create_table` does before a temp table's first write.
                self.engine.note_temp_table_present();
            }
            let pre = work.pre_schemas.get(table).cloned().flatten();
            match (pre, post) {
                (_, None) => {
                    // DROP TABLE, or the old name of a RENAME: its indexes go
                    // with it, as `Catalog::drop_table` drops them.
                    if let Err(e) = art.drop_table_indexes(table) {
                        debug!("physical applier: dropping the indexes of '{}': {}", table, e);
                    }
                    self.vector.on_table_dropped(&self.engine, table);
                    self.engine.predicate_pushdown().remove_table(table);
                    work.dropped.insert(table.clone());
                }
                (None, Some(_)) => {
                    work.register.insert(table.clone());
                }
                (Some(pre), Some(post)) => match classify_schema_change(&pre, &post) {
                    SchemaChange::Unchanged | SchemaChange::Appended { unique_added: false } => {}
                    SchemaChange::Renamed { old, new } => {
                        // What `alter_table_rename_column` does to the
                        // primary's live indexes: entries are keyed by
                        // column NAME, so they follow the rename.
                        art.rename_column_in_indexes(table, &old, &new);
                    }
                    SchemaChange::Appended { unique_added: true } => {
                        work.appended_unique.insert(table.clone(), post);
                    }
                    SchemaChange::Rederive => {
                        work.rederive.insert(table.clone());
                    }
                },
            }
        }
    }

    /// `meta:index:` deletes, before rows are staged: a dropped index is not
    /// maintained for this batch's rows. Puts are kept for after staging.
    fn publish_index_deletes(&self, touched: &Touched, work: &mut BatchWork, state: &mut ApplierState) {
        let art = self.engine.art_indexes();
        for name in &touched.index_definitions {
            match self.engine.get(&format!("meta:index:{name}").into_bytes()) {
                Ok(None) => {
                    if art.index_exists(name) {
                        if let Err(e) = art.drop_index(name) {
                            debug!("physical applier: dropping index '{}': {}", name, e);
                        }
                    }
                    self.vector.on_index_definition(&self.engine, name, true);
                }
                Ok(Some(_)) => {
                    work.index_puts.insert(name.clone());
                }
                Err(e) => {
                    warn!("physical applier: reading index definition '{}' failed: {}", name, e);
                    self.defer_full_rebuild(state, "an index definition could not be read back");
                }
            }
        }
    }

    /// Bring every staged table's existing indexes from the pre-batch to the
    /// post-batch row images.
    fn stage_rows(&self, touched: &Touched, work: &mut BatchWork) {
        let staged = std::mem::take(&mut work.staged);
        for (table, staged_table) in staged {
            if work.rederive.contains(&table) || work.dropped.contains(&table) || work.register.contains(&table) {
                continue;
            }
            let Some(rows) = touched.data.get(&table) else {
                continue;
            };
            let post_schema = match self.durable_schema(&table) {
                Ok(Some(schema)) => schema,
                // The table went away in an earlier step of this batch.
                Ok(None) => continue,
                Err(e) => {
                    warn!(
                        "physical applier: the schema of '{}' does not decode ({}); re-deriving it",
                        table, e
                    );
                    work.rederive.insert(table);
                    continue;
                }
            };
            // A schema change that moved or retyped an indexed column was
            // routed to a re-derivation by the meta-table step; what is left
            // (a rename, appended columns) encodes the same keys under the
            // post-batch schema once the old image is padded to its width.
            let schema_changed = staged_table.pre_schema.as_ref() != Some(&post_schema);
            if schema_changed && !touched.meta_tables.contains(&table) {
                work.rederive.insert(table);
                continue;
            }
            let mut changes: Vec<RowChange> = Vec::with_capacity(rows.len());
            let mut undecodable = false;
            for row in rows.values() {
                let new = match self.engine.get(&row.key) {
                    Ok(None) => None,
                    Ok(Some(bytes)) => match self.engine.decode_stored_row(&table, &post_schema, row.row_id, &bytes) {
                        Ok(tuple) => Some(tuple),
                        Err(e) => {
                            debug!(
                                "physical applier: new '{}' row {} does not decode: {}",
                                table, row.row_id, e
                            );
                            undecodable = true;
                            break;
                        }
                    },
                    Err(e) => {
                        warn!("physical applier: reading '{}' row {} failed: {}", table, row.row_id, e);
                        undecodable = true;
                        break;
                    }
                };
                let mut old = staged_table.old.get(&row.row_id).cloned().flatten();
                if schema_changed {
                    if let Some(tuple) = old.as_mut() {
                        if tuple.values.len() < post_schema.columns.len() {
                            tuple.values.resize(post_schema.columns.len(), Value::Null);
                        }
                    }
                }
                changes.push(RowChange {
                    row_id: row.row_id,
                    old,
                    new,
                });
            }
            if undecodable || !self.stage_changes(&table, &post_schema, &changes) {
                work.rederive.insert(table);
                continue;
            }
            for change in &changes {
                self.vector.on_row(
                    &self.engine,
                    &table,
                    change.row_id,
                    &post_schema,
                    change.old.as_ref(),
                    change.new.as_ref(),
                );
            }
        }
    }

    /// Stage one table's row changes into its ART indexes. `false` when they
    /// cannot be brought in line row by row; the caller re-derives the table.
    ///
    /// Each row goes from its old to its new keys in one
    /// `stage_replicated_row`, which refuses a key another row still owns. A
    /// batch is a whole write group, so two of its rows can trade a UNIQUE
    /// key (or a key can move to a row staged later): no single row's step is
    /// valid while the other still holds the key. For the refused rows the
    /// old keys are therefore all removed first and the new ones added after,
    /// which is valid for any permutation inside the batch.
    fn stage_changes(&self, table: &str, schema: &Schema, changes: &[RowChange]) -> bool {
        let art = self.engine.art_indexes();
        let mut refused: Vec<&RowChange> = Vec::new();
        for change in changes {
            match art.stage_replicated_row(table, change.row_id, schema, change.old.as_ref(), change.new.as_ref()) {
                Ok(staged) => staged.commit(),
                Err(_) => refused.push(change),
            }
        }
        if refused.is_empty() {
            return true;
        }
        for change in &refused {
            match art.stage_replicated_row(table, change.row_id, schema, change.old.as_ref(), None) {
                Ok(staged) => staged.commit(),
                Err(e) => {
                    debug!(
                        "physical applier: '{}' row {} keeps an old index key ({}); re-deriving the table",
                        table, change.row_id, e
                    );
                    return false;
                }
            }
        }
        for change in &refused {
            match art.stage_replicated_row(table, change.row_id, schema, None, change.new.as_ref()) {
                Ok(staged) => staged.commit(),
                Err(e) => {
                    debug!(
                        "physical applier: '{}' row {} cannot take its new index key ({}); re-deriving the table",
                        table, change.row_id, e
                    );
                    return false;
                }
            }
        }
        true
    }

    /// The constraints cache of every touched table, then the indexes of the
    /// constraints the batch added.
    fn publish_constraints(&self, touched: &Touched, work: &mut BatchWork) {
        let catalog = self.engine.catalog();
        let art = self.engine.art_indexes();
        for table in &touched.constraints {
            self.engine.invalidate_table_constraints_cache(table);
            let post = match self.durable_constraints(table) {
                Ok(post) => post,
                Err(e) => {
                    warn!(
                        "physical applier: the new constraints of '{}' do not decode ({}); re-deriving the table",
                        table, e
                    );
                    work.rederive.insert(table.clone());
                    continue;
                }
            };
            if let Some(post) = &post {
                self.engine.cache_table_constraints(table, post.clone());
            }
            if work.rederive.contains(table) || work.register.contains(table) || work.dropped.contains(table) {
                // Re-derived from the durable records, or gone.
                continue;
            }
            let pre = work.pre_constraints.get(table).and_then(Option::as_ref);
            if constraints_were_removed(pre, post.as_ref()) {
                work.rederive.insert(table.clone());
                continue;
            }
            let Some(post) = post else {
                continue;
            };
            let schema = match self.durable_schema(table) {
                Ok(Some(schema)) => schema,
                // Constraints of a table not (yet) here: registered with it.
                Ok(None) => continue,
                Err(_) => {
                    work.rederive.insert(table.clone());
                    continue;
                }
            };
            let before = table_index_names(&self.engine, table);
            if let Err(e) = catalog.register_unique_constraint_indexes(table, &post) {
                warn!(
                    "physical applier: a UNIQUE constraint of '{}' did not register ({}); re-deriving",
                    table, e
                );
                work.rederive.insert(table.clone());
                continue;
            }
            for fk in post.foreign_keys.iter().filter(|fk| fk.table_name == *table) {
                if let Err(e) = art.create_fk_index(
                    &fk.table_name,
                    &fk.columns,
                    &fk.references_table,
                    &fk.references_columns,
                    Some(&fk.name),
                ) {
                    debug!("physical applier: FK index '{}' already registered: {}", fk.name, e);
                }
            }
            let added: Vec<String> = table_index_names(&self.engine, table)
                .difference(&before)
                .cloned()
                .collect();
            self.backfill_new_indexes(table, &schema, &added, work);
        }
    }

    /// Register the ART indexes of `meta:index:` definitions the batch wrote
    /// and backfill each from a scan of its table.
    fn publish_index_puts(&self, work: &mut BatchWork) {
        let catalog = self.engine.catalog();
        let art = self.engine.art_indexes();
        let mut by_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let names: Vec<String> = work.index_puts.iter().cloned().collect();
        for name in names {
            let definition = match catalog.get_index_definition(&name) {
                Ok(Some(definition)) => definition,
                Ok(None) => {
                    warn!(
                        "physical applier: index definition '{}' does not decode; it is not registered",
                        name
                    );
                    continue;
                }
                Err(e) => {
                    warn!("physical applier: reading index definition '{}' failed: {}", name, e);
                    continue;
                }
            };
            match index_family(definition.index_type.as_deref()) {
                Some(IndexFamily::Art) => {
                    let table = definition.table_name.clone();
                    if work.rederive.contains(&table)
                        || work.register.contains(&table)
                        || work.dropped.contains(&table)
                        || art.index_exists(&name)
                    {
                        // Re-derived from the durable records, gone, or this
                        // definition is already registered.
                        continue;
                    }
                    if !matches!(self.durable_schema(&table), Ok(Some(_))) {
                        // As the open skips a definition whose table is not
                        // there: the table's own registration (it is created
                        // in a later batch) picks the definition up.
                        continue;
                    }
                    let before = all_index_owners(&self.engine);
                    catalog.register_index_definition(&name, &definition);
                    let after = all_index_owners(&self.engine);
                    // A recorded name evicts a constraint mint squatting it
                    // (`register_persisted_art_index`); an open re-mints it,
                    // so the evicted table is re-derived.
                    for (evicted, owner) in &before {
                        if !after.contains_key(evicted) {
                            work.rederive.insert(owner.clone());
                        }
                    }
                    if art.index_exists(&name) {
                        by_table.entry(table).or_default().push(name);
                    } else {
                        warn!(
                            "physical applier: index '{}' on '{}' did not register; queries full-scan, as on the \
                             primary after a restart",
                            name, definition.table_name
                        );
                    }
                }
                Some(IndexFamily::Vector) => self.vector.on_index_definition(&self.engine, &name, false),
                Some(IndexFamily::DdlOnly) => {}
                None => warn!(
                    "physical applier: index '{}' has the unknown index type {:?}; nothing registered",
                    name, definition.index_type
                ),
            }
        }
        for (table, names) in by_table {
            if work.rederive.contains(&table) {
                continue;
            }
            match self.durable_schema(&table) {
                Ok(Some(schema)) => self.backfill_new_indexes(&table, &schema, &names, work),
                Ok(None) => {}
                Err(_) => {
                    work.rederive.insert(table);
                }
            }
        }
    }

    /// Columns appended with a UNIQUE flag: register their constraint indexes
    /// as an open would and backfill them.
    fn publish_appended_unique_columns(&self, work: &mut BatchWork) {
        let appended = std::mem::take(&mut work.appended_unique);
        for (table, schema) in appended {
            if work.rederive.contains(&table) {
                continue;
            }
            let before = table_index_names(&self.engine, &table);
            self.engine.catalog().register_table_indexes(&table, &schema);
            let added: Vec<String> = table_index_names(&self.engine, &table)
                .difference(&before)
                .cloned()
                .collect();
            self.backfill_new_indexes(&table, &schema, &added, work);
        }
    }

    /// Fill freshly registered (empty) indexes of `table` from one scan. A
    /// new PRIMARY KEY on a table that already has rows, or a backfill that
    /// fails, re-derives the table instead.
    fn backfill_new_indexes(&self, table: &str, schema: &Schema, names: &[String], work: &mut BatchWork) {
        if names.is_empty() {
            return;
        }
        let art = self.engine.art_indexes();
        let tuples = match self.engine.scan_table_with_schema(table, schema) {
            Ok(tuples) => tuples,
            Err(e) => {
                debug!(
                    "physical applier: scanning '{}' to backfill failed ({}); re-deriving it",
                    table, e
                );
                work.rederive.insert(table.to_string());
                return;
            }
        };
        for name in names {
            let filled = match art.index_kind_and_table(name) {
                Some((ArtIndexType::Manual, _)) => art.backfill_manual_index(name, schema, &tuples).map(drop),
                Some((ArtIndexType::Unique, _)) => art.backfill_unique_index(name, schema, &tuples).map(drop),
                Some((ArtIndexType::ForeignKey, _)) => art.backfill_fk_index(name, schema, &tuples).map(drop),
                Some((ArtIndexType::PrimaryKey, _)) => {
                    work.rederive.insert(table.to_string());
                    return;
                }
                None => continue,
            };
            if let Err(e) = filled {
                debug!(
                    "physical applier: backfilling '{}' on '{}' failed ({}); re-deriving",
                    name, table, e
                );
                work.rederive.insert(table.to_string());
                return;
            }
        }
        self.stats.lock().registrations += 1;
    }

    fn publish_time_travel(&self, touched: &Touched, state: &mut ApplierState) {
        let snapshots = self.engine.snapshot_manager();
        let db = self.writer.db();
        let mut forgot_any = false;
        let mut forgotten = Vec::new();
        for key in &touched.snapshots {
            // `snapshot:` records are stored unsealed (`persist_snapshot_metadata`),
            // and recovery reads them raw; so does this.
            match db.get(key) {
                Ok(Some(value)) => match snapshots.ingest_snapshot_record(key, &value) {
                    Ok(timestamp) => self.engine.raise_timestamp(timestamp),
                    Err(e) => warn!(
                        "physical applier: snapshot record {} does not decode ({}); AS OF cannot resolve it",
                        String::from_utf8_lossy(key),
                        e
                    ),
                },
                Ok(None) => {
                    forgotten.extend(snapshot_timestamp(key));
                    forgot_any = true;
                }
                Err(e) => {
                    warn!("physical applier: reading a snapshot record failed: {}", e);
                    self.defer_full_rebuild(state, "a snapshot record could not be read back");
                }
            }
        }
        if forgot_any {
            // One call for the batch: a GC batch deletes hundreds of records,
            // and the transaction ids they leave unmapped are re-pointed in a
            // single pass over the survivors.
            snapshots.forget_snapshots(&forgotten);
            snapshots.clear_cache();
        }
        if let Some(timestamp) = touched.max_version_ts {
            self.engine.raise_timestamp(timestamp);
        }
        for key in &touched.copy_markers {
            match db.get(key) {
                Ok(Some(value)) => {
                    snapshots.ingest_copy_marker(key, &value);
                    // A marker's value is its COPY's commit timestamp
                    // (`CopyMarkers::parse_marker`): raised past like any
                    // other replicated commit.
                    if let Some(bytes) = value.get(0..8).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
                        self.engine.raise_timestamp(u64::from_be_bytes(bytes));
                    }
                }
                Ok(None) => {
                    snapshots.forget_copy_marker(key);
                }
                Err(e) => {
                    warn!("physical applier: reading a COPY marker failed: {}", e);
                    self.defer_full_rebuild(state, "a COPY marker could not be read back");
                }
            }
        }
        for table in touched.version_tables.iter().chain(touched.data.keys()) {
            snapshots.invalidate_cached_versions(table);
        }
        if touched.vgc_watermark {
            match db.get(crate::storage::version_gc::LOW_WATERMARK_KEY) {
                Ok(Some(raw)) => {
                    // The pattern `VersionGc::new` recovers it with.
                    if let Some(bytes) = raw.get(0..8).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
                        let watermark = u64::from_be_bytes(bytes);
                        snapshots.set_gc_low_watermark(watermark);
                        self.engine.raise_timestamp(watermark);
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("physical applier: reading the version-GC watermark failed: {}", e),
            }
        }
    }

    fn publish_counters(&self, touched: &Touched) {
        for table in &touched.counters {
            // `counter:` values are sealed (`put_internal`); `get` opens them.
            match self.engine.get(&format!("counter:{table}").into_bytes()) {
                Ok(Some(bytes)) => match bincode::deserialize::<u64>(&bytes) {
                    Ok(value) => self.engine.raise_row_counter_to(table, value),
                    Err(e) => warn!(
                        "physical applier: the row counter of '{}' does not decode: {}",
                        table, e
                    ),
                },
                // DROP TABLE deleted it; the in-memory counter only moves forward.
                Ok(None) => {}
                Err(e) => warn!("physical applier: reading the row counter of '{}' failed: {}", table, e),
            }
        }
        // A row id the primary handed out is used, whatever its durable
        // counter says yet (the fast path persists it every 64 rows) — the
        // rule the logical applier follows (`live_replay_counter`).
        for (table, rows) in &touched.data {
            if let Some(highest) = rows.values().filter(|row| row.put).map(|row| row.row_id).max() {
                self.engine.raise_row_counter_to(table, highest);
            }
        }
    }

    fn reload_branches(&self, state: &mut ApplierState) {
        match self.engine.reload_branch_manager() {
            Ok(()) => state.branch_reload_pending = false,
            Err(e) => {
                // A registry can name a branch whose metadata is written by
                // a later write group; retried on the next batch.
                debug!("physical applier: branch registry not loadable yet ({}); retrying", e);
                state.branch_reload_pending = true;
            }
        }
    }

    /// Row and statistics caches of tables whose rows changed outside `data:`
    /// (columnar sidecars, branch overlays) or whose statistics were rewritten,
    /// and the row-cache visibility of every published row.
    fn publish_caches(&self, touched: &Touched) {
        for table in &touched.statistics_tables {
            self.invalidate_statistics(table);
        }
        for table in touched.column_storage_tables.iter().chain(&touched.branch_tables) {
            self.engine.row_cache().invalidate_table(table);
            self.invalidate_statistics(table);
        }
        for (table, rows) in &touched.data {
            for row_id in rows.keys() {
                self.engine.publish_live_replay_visibility(table, *row_id);
            }
            // Bloom filters and zone maps summarise rows and are never
            // maintained incrementally (TRUNCATE / DROP purge them on a
            // primary); dropping them only loses pruning.
            if self.engine.predicate_pushdown().has_structures_for_table(table) {
                self.engine.predicate_pushdown().remove_table(table);
            }
        }
    }

    fn publish_vector_records(&self, touched: &Touched) {
        let db = self.writer.db();
        for key in &touched.vidx {
            match db.get(key) {
                Ok(value) => self.vector.on_vidx_record(&self.engine, key, value.as_deref()),
                Err(e) => warn!("physical applier: reading a vector-index record failed: {}", e),
            }
        }
    }

    fn publish_listener(&self, touched: &Touched) {
        let db = self.writer.db();
        for (key, class) in &touched.catalog {
            let deleted = !matches!(db.get(key), Ok(Some(_)));
            self.listener.on_catalog_change(*class, key, deleted);
        }
    }

    fn publish_unknown(&self, touched: &Touched, state: &mut ApplierState) {
        if touched.unknown.is_empty() && !touched.overflow_unattributed {
            return;
        }
        for key in &touched.unknown {
            let prefix = unknown_prefix(key);
            if state.warned_prefixes.len() < UNKNOWN_PREFIXES_WARNED && state.warned_prefixes.insert(prefix.clone()) {
                warn!(
                    "physical applier: applied a key no keyspace entry classifies (prefix {:?}); this binary \
                     cannot vouch for the derived state it feeds, which is marked suspect and re-derived at the \
                     next idle poll",
                    String::from_utf8_lossy(&prefix)
                );
            }
        }
        self.stats.lock().unknown_keys += touched.unknown.len() as u64;
        let reason = if touched.overflow_unattributed {
            "a range deletion too wide to enumerate"
        } else {
            "an unclassified key"
        };
        self.defer_full_rebuild(state, reason);
    }

    /// Mark the derived state suspect and schedule a full re-derivation for
    /// the next idle poll ([`Self::run_pending_rebuilds`]).
    fn defer_full_rebuild(&self, state: &mut ApplierState, reason: &str) {
        if !state.full_rebuild_pending {
            warn!("physical applier: full re-derivation scheduled: {}", reason);
        }
        state.full_rebuild_pending = true;
        self.stats.lock().derived_state_suspect = true;
    }

    // ---------------------------------------------------------------------
    // Step 5 and the fallbacks
    // ---------------------------------------------------------------------

    /// Re-derive `tables` (and any table a durable index name evicts a mint
    /// from on the way); a table in `fallback` counts as a rebuild, any other
    /// as a registration. A failure leaves the table pending, holding the
    /// census. Returns the fallback tables re-derived.
    fn rederive_tables(
        &self,
        tables: BTreeSet<String>,
        fallback: &BTreeSet<String>,
        state: &mut ApplierState,
    ) -> Vec<String> {
        let mut queue: Vec<String> = tables.into_iter().rev().collect();
        let mut done: BTreeMap<String, usize> = BTreeMap::new();
        let mut rebuilt = Vec::new();
        while let Some(table) = queue.pop() {
            let attempts = done.entry(table.clone()).or_insert(0);
            // An evicted mint is re-minted onto a free key and cannot evict
            // a durable name in turn, so a table needs at most a second pass.
            if *attempts >= 2 {
                continue;
            }
            *attempts += 1;
            match self.rederive_table(&table) {
                Ok(evicted) => {
                    state.pending.remove(&table);
                    if fallback.contains(&table) {
                        self.stats.lock().rebuilds += 1;
                        if !rebuilt.contains(&table) {
                            rebuilt.push(table.clone());
                        }
                    } else {
                        self.stats.lock().registrations += 1;
                    }
                    queue.extend(evicted);
                }
                Err(e) => {
                    if !state.pending.contains_key(&table) {
                        warn!(
                            "physical applier: re-deriving '{}' failed ({}); its readers use scans until it succeeds",
                            table, e
                        );
                        let hold = self.engine.write_census().hold_for_replay(vec![table.clone()], false);
                        state.pending.insert(table, hold);
                    }
                }
            }
        }
        rebuilt
    }

    /// Drop `table`'s ART registrations and register and backfill them again
    /// from the durable records, as an open would (pass 1 and pass 2 of
    /// `Catalog::rebuild_all_indexes`, for one table). Returns the tables whose
    /// constraint mint a durable index name evicted on the way.
    fn rederive_table(&self, table: &str) -> crate::Result<Vec<String>> {
        let art = self.engine.art_indexes();
        let catalog = self.engine.catalog();
        let _hold = self
            .engine
            .write_census()
            .hold_for_replay(vec![table.to_string()], false);
        let before = all_index_owners(&self.engine);
        if let Err(e) = art.drop_table_indexes(table) {
            debug!("physical applier: dropping the indexes of '{}': {}", table, e);
        }
        self.invalidate_table_caches(table);
        self.engine.predicate_pushdown().remove_table(table);
        self.engine.snapshot_manager().invalidate_cached_versions(table);
        self.engine.bump_schema_generation();
        let Some(schema) = self.durable_schema(table)? else {
            self.vector.on_table_dropped(&self.engine, table);
            return Ok(Vec::new());
        };
        for (name, definition) in catalog.list_index_definitions()? {
            if definition.table_name == table {
                catalog.register_index_definition(&name, &definition);
            }
        }
        catalog.register_table_indexes(table, &schema);
        let backfill = catalog.backfill_table_indexes(table, &schema)?;
        if let Some(highest) = backfill.max_row_id {
            self.engine.raise_row_counter_to(table, highest);
        }
        self.vector.on_table_rederived(&self.engine, table);
        let after = all_index_owners(&self.engine);
        let evicted: BTreeSet<String> = before
            .into_iter()
            .filter(|(name, owner)| owner != table && !after.contains_key(name))
            .map(|(_, owner)| owner)
            .collect();
        Ok(evicted.into_iter().collect())
    }

    fn rebuild_all_locked(&self, state: &mut ApplierState) -> crate::Result<()> {
        let engine = &self.engine;
        let art = engine.art_indexes();
        let _hold = engine.write_census().hold_for_replay(Vec::new(), true);
        let tables: BTreeSet<String> = engine.catalog().list_tables_including_temp()?.into_iter().collect();
        // Registrations whose table is gone.
        let orphaned: BTreeSet<String> = art
            .list_indexes()
            .into_iter()
            .map(|(_, table, _, _)| table)
            .filter(|table| !tables.contains(table))
            .collect();
        for table in &orphaned {
            if let Err(e) = art.drop_table_indexes(table) {
                debug!("physical applier: dropping the indexes of '{}': {}", table, e);
            }
            self.invalidate_table_caches(table);
        }
        // Every table, and every pending one (a pending table that has since
        // been dropped is settled by losing its registrations).
        let mut targets = tables;
        targets.extend(state.pending.keys().cloned());
        self.rederive_tables(targets.clone(), &targets, state);

        // The snapshot map as a fresh open builds it from the records, and
        // everything raised from it. Every record is ingested, and every
        // snapshot left without one is forgotten: those a range deletion too
        // wide to enumerate removed (the batch could not name them), and the
        // memory-only snapshots of read-only COMMITs, which a restart loses
        // as well. The counters only move forward.
        let snapshots = engine.snapshot_manager();
        let mut durable = HashSet::new();
        let mut read = ReadOptions::default();
        read.set_total_order_seek(true);
        for item in self
            .writer
            .db()
            .iterator_opt(IteratorMode::From(b"snapshot:", Direction::Forward), read)
        {
            let (key, value) = item?;
            if !key.starts_with(b"snapshot:") {
                break;
            }
            match snapshots.ingest_snapshot_record(&key, &value) {
                Ok(timestamp) => {
                    durable.insert(timestamp);
                    engine.raise_timestamp(timestamp);
                }
                Err(e) => warn!(
                    "physical applier: snapshot record {} does not decode: {}",
                    String::from_utf8_lossy(&key),
                    e
                ),
            }
        }
        let unrecorded: Vec<u64> = snapshots
            .list_snapshots()?
            .into_iter()
            .map(|snapshot| snapshot.timestamp)
            .filter(|timestamp| !durable.contains(timestamp))
            .collect();
        snapshots.forget_snapshots(&unrecorded);
        if let Some(raw) = self.writer.db().get(crate::storage::version_gc::LOW_WATERMARK_KEY)? {
            if let Some(bytes) = raw.get(0..8).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
                let watermark = u64::from_be_bytes(bytes);
                snapshots.set_gc_low_watermark(watermark);
                engine.raise_timestamp(watermark);
            }
        }
        snapshots.clear_cache();
        engine.forget_cached_dictionaries();
        if let Err(e) = engine.statistics_cache().invalidate_all() {
            debug!("physical applier: clearing the statistics cache: {}", e);
        }
        engine.row_cache().clear();
        engine.clear_schema_cache();
        engine.clear_referencing_fk_cache();
        engine.bump_schema_generation();
        self.reload_branches(state);
        self.vector.on_full_rebuild(engine);
        self.listener.on_full_rebuild();
        state.full_rebuild_pending = false;
        self.stats.lock().full_rebuilds += 1;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    /// `table`'s durable schema, read past the schema cache and the
    /// session-private visibility rule (`Catalog::get_table_schema` applies
    /// both; the applier has no session and must see every table), with the
    /// same B31 `source_table_name` stamp. `None` when the table does not
    /// exist.
    fn durable_schema(&self, table: &str) -> crate::Result<Option<Schema>> {
        let Some(bytes) = self.engine.get(&format!("meta:table:{table}").into_bytes())? else {
            return Ok(None);
        };
        let mut schema: Schema = bincode::deserialize(&bytes)
            .map_err(|e| crate::Error::storage(format!("the schema of '{table}' does not decode: {e}")))?;
        for column in &mut schema.columns {
            if column.source_table_name.is_none() {
                column.source_table_name = Some(table.to_string());
            }
        }
        Ok(Some(schema))
    }

    /// `table`'s durable constraint set, read past the constraints cache.
    fn durable_constraints(&self, table: &str) -> crate::Result<Option<TableConstraints>> {
        let Some(bytes) = self.engine.get(&format!("table_constraints:{table}").into_bytes())? else {
            return Ok(None);
        };
        bincode::deserialize(&bytes)
            .map(Some)
            .map_err(|e| crate::Error::storage(format!("the constraints of '{table}' do not decode: {e}")))
    }

    fn invalidate_table_caches(&self, table: &str) {
        self.engine.invalidate_schema_cache(table);
        self.engine.invalidate_table_constraints_cache(table);
        self.engine.row_cache().invalidate_table(table);
        self.invalidate_statistics(table);
    }

    fn invalidate_statistics(&self, table: &str) {
        if let Err(e) = self.engine.statistics_cache().invalidate(table) {
            debug!("physical applier: invalidating the statistics of '{}': {}", table, e);
        }
    }
}

/// One row a batch changed: its image before the batch (decoded with the
/// pre-batch schema) and after it.
struct RowChange {
    row_id: u64,
    old: Option<Tuple>,
    new: Option<Tuple>,
}

/// A `data:` key a batch touched; `put` when any record of the batch wrote it.
struct RowKey {
    row_id: u64,
    key: Vec<u8>,
    put: bool,
}

/// A table whose rows are staged row by row, with what was read before the
/// batch.
struct StagedTable {
    pre_schema: Option<Schema>,
    old: BTreeMap<u64, Option<Tuple>>,
}

/// One batch's publication plan, filled by the pre-read and the steps.
#[derive(Default)]
struct BatchWork {
    staged: BTreeMap<String, StagedTable>,
    pre_schemas: BTreeMap<String, Option<Schema>>,
    pre_constraints: BTreeMap<String, Option<TableConstraints>>,
    index_puts: BTreeSet<String>,
    appended_unique: BTreeMap<String, Schema>,
    /// Tables the batch created: registered and backfilled in step 5.
    register: BTreeSet<String>,
    /// Tables whose derived state falls back to a re-derivation in step 5.
    rederive: BTreeSet<String>,
    dropped: BTreeSet<String>,
}

/// Every key a batch touched, sorted by what publishing it involves. Keys are
/// deduplicated: publication reads each one's post-batch value, which is the
/// net effect of all of the batch's records on it.
#[derive(Default)]
struct Touched {
    data: BTreeMap<String, BTreeMap<u64, RowKey>>,
    meta_tables: BTreeSet<String>,
    index_definitions: BTreeSet<String>,
    constraints: BTreeSet<String>,
    counters: BTreeSet<String>,
    snapshots: BTreeSet<Vec<u8>>,
    version_tables: BTreeSet<String>,
    max_version_ts: Option<u64>,
    copy_markers: BTreeSet<Vec<u8>>,
    vidx: BTreeSet<Vec<u8>>,
    branch_tables: BTreeSet<String>,
    branch_registry: bool,
    dictionaries: bool,
    column_storage_tables: BTreeSet<String>,
    statistics_tables: BTreeSet<String>,
    vgc_watermark: bool,
    /// Catalog keys for the listener.
    catalog: BTreeMap<Vec<u8>, KeyClass>,
    unknown: Vec<Vec<u8>>,
    /// Tables a range deletion too wide to enumerate removed rows from.
    overflow_tables: BTreeSet<String>,
    /// A range deletion too wide to enumerate that is not one table's rows.
    overflow_unattributed: bool,
    /// The batch carried a key that can make a pending table re-derivable
    /// (a dictionary, a CAS payload, a columnar sidecar, a schema, an index
    /// definition, a constraint set).
    fixes_decoding: bool,
}

impl Touched {
    /// Record one key a record of the batch wrote (`put`) or deleted.
    fn note(&mut self, key: &[u8], put: bool) {
        let classified = classify(key);
        let class = classified.class;
        match class {
            KeyClass::Data => match (classified.table, row_id_of(key)) {
                (Some(table), Some(row_id)) => {
                    let row = self
                        .data
                        .entry(table.to_string())
                        .or_default()
                        .entry(row_id)
                        .or_insert_with(|| RowKey {
                            row_id,
                            key: key.to_vec(),
                            put: false,
                        });
                    row.put |= put;
                }
                // A `data:` key no table can be named for: the engine itself
                // could not attribute it either.
                _ => self.unknown.push(key.to_vec()),
            },
            KeyClass::Version | KeyClass::Vmeta => {
                if let Some(table) = classified.table {
                    self.version_tables.insert(table.to_string());
                }
                if class == KeyClass::Vmeta {
                    self.copy_markers.insert(key.to_vec());
                }
            }
            KeyClass::VersionIdx => {
                if let Some(table) = classified.table {
                    self.version_tables.insert(table.to_string());
                }
                if put {
                    if let Some(timestamp) = version_index_timestamp(key) {
                        self.max_version_ts = Some(self.max_version_ts.map_or(timestamp, |m| m.max(timestamp)));
                    }
                }
            }
            KeyClass::Snapshot => {
                self.snapshots.insert(key.to_vec());
            }
            KeyClass::MetaTable => {
                self.catalog.insert(key.to_vec(), class);
                self.fixes_decoding = true;
                match suffix(key, b"meta:table:") {
                    Some(table) => {
                        self.meta_tables.insert(table);
                    }
                    None => self.unknown.push(key.to_vec()),
                }
            }
            KeyClass::MetaIndex => {
                self.catalog.insert(key.to_vec(), class);
                self.fixes_decoding = true;
                match suffix(key, b"meta:index:") {
                    Some(name) => {
                        self.index_definitions.insert(name);
                    }
                    None => self.unknown.push(key.to_vec()),
                }
            }
            KeyClass::TableConstraints => {
                self.catalog.insert(key.to_vec(), class);
                // `constraint:` keys (no caller today) are the listener's only.
                if let Some(table) = suffix(key, b"table_constraints:") {
                    self.fixes_decoding = true;
                    self.constraints.insert(table);
                }
            }
            KeyClass::Statistics => {
                self.catalog.insert(key.to_vec(), class);
                if let Some(table) = suffix(key, b"statistics:table:") {
                    self.statistics_tables.insert(table);
                }
            }
            KeyClass::MetaSchema
            | KeyClass::MetaEnumType
            | KeyClass::MetaIdentity
            | KeyClass::MetaPartition
            | KeyClass::MetaSequence
            | KeyClass::MetaSeqState
            | KeyClass::MetaMv
            | KeyClass::MetaRole
            | KeyClass::MetaAcl
            | KeyClass::MetaFunction
            | KeyClass::MetaProcedure
            | KeyClass::MetaColStorage
            | KeyClass::Trigger
            | KeyClass::TriggerRowMut
            | KeyClass::Compression
            | KeyClass::ViewMeta => {
                self.catalog.insert(key.to_vec(), class);
            }
            KeyClass::Counter => {
                if let Some(table) = classified.table {
                    self.counters.insert(table.to_string());
                }
            }
            KeyClass::Dict => {
                self.dictionaries = true;
                self.fixes_decoding = true;
            }
            KeyClass::Columnar
            | KeyClass::ColumnarZ
            | KeyClass::ColumnarP
            | KeyClass::ColumnarPm
            | KeyClass::ColumnarZm => {
                self.fixes_decoding = true;
                if let Some(table) = second_segment(key) {
                    self.column_storage_tables.insert(table);
                }
            }
            // Content-addressed by hash and never rewritten: the rows that
            // reference a payload change through `data:`.
            KeyClass::Cas => self.fixes_decoding = true,
            KeyClass::Branch | KeyClass::Git => self.branch_registry = true,
            KeyClass::BranchData | KeyClass::BranchDel | KeyClass::BranchVersion => {
                if let Some(table) = classified.table {
                    self.branch_tables.insert(table.to_string());
                }
            }
            KeyClass::VgcWatermark => self.vgc_watermark = true,
            KeyClass::Vidx => {
                self.vidx.insert(key.to_vec());
            }
            // Read by nothing on a standby: the logical WAL and the
            // receivers' cursors, durable index snapshots (only an open loads
            // them, and a physical open does not), the TDE and elision
            // sentinels (checked at open), GC anchors, the legacy counter key,
            // MV delta bookkeeping (refresh never runs on a standby), the
            // legacy snapshot maps, and the sync change log.
            KeyClass::WalEntries
            | KeyClass::WalLastLsn
            | KeyClass::WalCheckpoint
            | KeyClass::WalPrunedThrough
            | KeyClass::ReplAppliedLsn
            | KeyClass::PhysicalCursor
            | KeyClass::IdxSnap
            | KeyClass::IdxSnapMarker
            | KeyClass::VecSnap
            | KeyClass::VecSnapMarker
            | KeyClass::MetaTdeKeyCheck
            | KeyClass::ElideSentinel
            | KeyClass::VgcAnchor
            | KeyClass::LegacyCounter
            | KeyClass::MetaDelta
            | KeyClass::Delta
            | KeyClass::SnapshotMap
            | KeyClass::ChangeLog
            | KeyClass::ChangeIndex
            | KeyClass::ChangeMeta
            | KeyClass::SyncAppliedLsn => {}
            KeyClass::Unknown => self.unknown.push(key.to_vec()),
        }
    }

    /// The tables whose indexes this batch's publication may leave behind
    /// their rows for a moment.
    fn held_tables(&self) -> Vec<String> {
        let mut tables: BTreeSet<&String> = self.data.keys().collect();
        tables.extend(&self.meta_tables);
        tables.extend(&self.constraints);
        tables.extend(&self.column_storage_tables);
        tables.extend(&self.overflow_tables);
        tables.into_iter().cloned().collect()
    }

    /// Index definitions name a table only through their value, and unknown
    /// keys none at all: hold every table.
    fn holds_every_table(&self) -> bool {
        !self.index_definitions.is_empty() || !self.unknown.is_empty() || self.overflow_unattributed
    }

    fn touches_catalog(&self) -> bool {
        !self.catalog.is_empty()
    }
}

/// How a table's schema changed across a batch, as far as its ART indexes
/// are concerned (they key off column names, positions, types, key roles and
/// the storage mode the values are decoded through).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SchemaChange {
    /// Nothing index-relevant (nullability, defaults).
    Unchanged,
    /// Exactly one column renamed, nothing else.
    Renamed { old: String, new: String },
    /// Columns appended at the end; `unique_added` when one carries a UNIQUE flag.
    Appended { unique_added: bool },
    /// Anything else: re-derive the table.
    Rederive,
}

fn classify_schema_change(pre: &Schema, post: &Schema) -> SchemaChange {
    let same_key_role = |a: &crate::Column, b: &crate::Column| {
        a.data_type == b.data_type
            && a.primary_key == b.primary_key
            && a.unique == b.unique
            && a.storage_mode == b.storage_mode
    };
    let same = |a: &crate::Column, b: &crate::Column| a.name == b.name && same_key_role(a, b);
    if pre.columns.len() == post.columns.len() {
        let mut differing = pre.columns.iter().zip(&post.columns).filter(|&(a, b)| !same(a, b));
        return match (differing.next(), differing.next()) {
            (None, _) => SchemaChange::Unchanged,
            (Some((a, b)), None) if a.name != b.name && same_key_role(a, b) => SchemaChange::Renamed {
                old: a.name.clone(),
                new: b.name.clone(),
            },
            _ => SchemaChange::Rederive,
        };
    }
    if post.columns.len() > pre.columns.len() && pre.columns.iter().zip(&post.columns).all(|(a, b)| same(a, b)) {
        let added = post.columns.iter().skip(pre.columns.len());
        let mut unique_added = false;
        for column in added {
            if column.primary_key {
                return SchemaChange::Rederive;
            }
            unique_added |= column.unique;
        }
        return SchemaChange::Appended { unique_added };
    }
    SchemaChange::Rederive
}

/// Whether a UNIQUE (non-PK) or FOREIGN KEY constraint of `pre` is gone from
/// `post`, by name. Adding is published in place; removing re-derives (the
/// index a removed constraint owned must go, and only a re-derivation knows
/// which tree is whose).
fn constraints_were_removed(pre: Option<&TableConstraints>, post: Option<&TableConstraints>) -> bool {
    let Some(pre) = pre else {
        return false;
    };
    let unique = |c: &TableConstraints| -> BTreeSet<String> {
        c.unique_constraints
            .iter()
            .filter(|u| !u.is_primary_key)
            .map(|u| u.name.clone())
            .collect()
    };
    let foreign =
        |c: &TableConstraints| -> BTreeSet<String> { c.foreign_keys.iter().map(|f| f.name.clone()).collect() };
    match post {
        None => !unique(pre).is_empty() || !foreign(pre).is_empty(),
        Some(post) => !unique(pre).is_subset(&unique(post)) || !foreign(pre).is_subset(&foreign(post)),
    }
}

/// The names of `table`'s ART indexes.
fn table_index_names(engine: &StorageEngine, table: &str) -> BTreeSet<String> {
    engine
        .art_indexes()
        .list_table_indexes(table)
        .into_iter()
        .map(|(name, _, _)| name)
        .collect()
}

/// Every ART index name and its table.
fn all_index_owners(engine: &StorageEngine) -> BTreeMap<String, String> {
    engine
        .art_indexes()
        .list_indexes()
        .into_iter()
        .map(|(name, table, _, _)| (name, table))
        .collect()
}

/// The row id of a well-formed `data:{table}:{row_id}` key (`classify` has
/// already checked the shape).
fn row_id_of(key: &[u8]) -> Option<u64> {
    std::str::from_utf8(key).ok()?.rsplit_once(':')?.1.parse().ok()
}

/// The commit timestamp a `v_idx:{table}:{row_id}:{reverse_ts:020}` key
/// encodes (`u64::MAX - reverse_ts`, `create_reverse_timestamp_index`).
fn version_index_timestamp(key: &[u8]) -> Option<u64> {
    let reverse: u64 = std::str::from_utf8(key).ok()?.rsplit_once(':')?.1.parse().ok()?;
    Some(u64::MAX - reverse)
}

/// The timestamp a `snapshot:{ts}` key names.
fn snapshot_timestamp(key: &[u8]) -> Option<u64> {
    std::str::from_utf8(key.strip_prefix(b"snapshot:")?).ok()?.parse().ok()
}

/// The UTF-8 rest of `key` after `prefix`.
fn suffix(key: &[u8], prefix: &[u8]) -> Option<String> {
    let rest = key.strip_prefix(prefix)?;
    std::str::from_utf8(rest).ok().map(str::to_string)
}

/// The table of a columnar sidecar key (`col:{table}:{column}:{batch}` and
/// its siblings): the segment after the prefix, split at ':' as
/// `extract_table_from_key` splits `data:` keys.
fn second_segment(key: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(key).ok()?;
    let table = text.split(':').nth(1)?;
    (!table.is_empty()).then(|| table.to_string())
}

/// The table a range deletion `[start, end)` removes rows from, when it lies
/// within one table's `data:{table}:` rows (`;` is the byte after `:`).
fn single_table_data_range(start: &[u8], end: &[u8]) -> Option<String> {
    let rest = start.strip_prefix(b"data:")?;
    let colon = rest.iter().position(|&byte| byte == b':')?;
    let table = std::str::from_utf8(rest.get(..colon)?).ok()?;
    let upper = format!("data:{table};").into_bytes();
    (end <= upper.as_slice()).then(|| table.to_string())
}

/// The part of an unclassified key a warning quotes: up to and including its
/// first ':' (its family, if it has one), at most [`UNKNOWN_PREFIX_BYTES`].
fn unknown_prefix(key: &[u8]) -> Vec<u8> {
    let end = key
        .iter()
        .position(|&byte| byte == b':')
        .map_or(key.len(), |colon| colon + 1)
        .min(UNKNOWN_PREFIX_BYTES);
    key.get(..end).unwrap_or(key).to_vec()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{Column, ColumnStorageMode, DataType};
    use rocksdb::WriteBatch;

    #[test]
    fn decode_returns_every_record_in_batch_order_and_refuses_what_validation_refuses() {
        let mut batch = WriteBatch::default();
        batch.put(b"a", b"1");
        batch.delete(b"b");
        batch.delete_range(&b"c"[..], &b"e"[..]);
        batch.put(b"a", b"2");
        let ops = decode_raw_batch(batch.data()).unwrap();
        assert_eq!(
            ops,
            vec![
                RawOp::Put { key: b"a", value: b"1" },
                RawOp::Delete { key: b"b" },
                RawOp::DeleteRange { start: b"c", end: b"e" },
                RawOp::Put { key: b"a", value: b"2" },
            ]
        );
        let mut merge = WriteBatch::default();
        merge.merge(b"a", b"1");
        assert!(decode_raw_batch(merge.data()).is_err());
        let valid = batch.data();
        for length in 0..valid.len() {
            assert!(decode_raw_batch(&valid[..length]).is_err(), "prefix {length}");
        }
    }

    fn column(name: &str, data_type: DataType) -> Column {
        Column::new(name, data_type)
    }

    #[test]
    fn schema_changes_are_classified_by_what_the_indexes_key_off() {
        let mut id = column("id", DataType::Int4);
        id.primary_key = true;
        let pre = Schema::new(vec![id.clone(), column("name", DataType::Text)]);

        let mut nullability = pre.clone();
        nullability.columns[1].nullable = false;
        assert_eq!(classify_schema_change(&pre, &nullability), SchemaChange::Unchanged);

        let mut renamed = pre.clone();
        renamed.columns[1].name = "label".into();
        assert_eq!(
            classify_schema_change(&pre, &renamed),
            SchemaChange::Renamed {
                old: "name".into(),
                new: "label".into()
            }
        );

        let mut appended = pre.clone();
        appended.columns.push(column("tag", DataType::Text));
        assert_eq!(
            classify_schema_change(&pre, &appended),
            SchemaChange::Appended { unique_added: false }
        );
        let mut unique = column("code", DataType::Text);
        unique.unique = true;
        appended.columns.push(unique);
        assert_eq!(
            classify_schema_change(&pre, &appended),
            SchemaChange::Appended { unique_added: true }
        );

        let mut retyped = pre.clone();
        retyped.columns[1].data_type = DataType::Int8;
        assert_eq!(classify_schema_change(&pre, &retyped), SchemaChange::Rederive);
        let mut restored = pre.clone();
        restored.columns[1].storage_mode = ColumnStorageMode::Dictionary;
        assert_eq!(classify_schema_change(&pre, &restored), SchemaChange::Rederive);
        let dropped = Schema::new(vec![id.clone()]);
        assert_eq!(classify_schema_change(&pre, &dropped), SchemaChange::Rederive);
        let mut two_renames = pre.clone();
        two_renames.columns[0].name = "key".into();
        two_renames.columns[1].name = "label".into();
        assert_eq!(classify_schema_change(&pre, &two_renames), SchemaChange::Rederive);
        let mut new_pk = pre;
        new_pk.columns.push({
            let mut c = column("k2", DataType::Int4);
            c.primary_key = true;
            c
        });
        let base = Schema::new(new_pk.columns[..2].to_vec());
        assert_eq!(classify_schema_change(&base, &new_pk), SchemaChange::Rederive);
    }

    #[test]
    fn key_helpers_read_the_engines_formats() {
        assert_eq!(row_id_of(b"data:users:42"), Some(42));
        assert_eq!(
            version_index_timestamp(format!("v_idx:users:1:{:020}", u64::MAX - 1700).as_bytes()),
            Some(1700)
        );
        assert_eq!(snapshot_timestamp(b"snapshot:1700"), Some(1700));
        assert_eq!(snapshot_timestamp(b"snapshot:x"), None);
        assert_eq!(second_segment(b"colz:users:tag:0").as_deref(), Some("users"));
        assert_eq!(second_segment(b"col:users:tag:0").as_deref(), Some("users"));
        assert_eq!(suffix(b"meta:table:users", b"meta:table:").as_deref(), Some("users"));
        assert_eq!(
            unknown_prefix(b"wholly-unclassified-key-without-a-colon-in-it").len(),
            UNKNOWN_PREFIX_BYTES
        );
        // Fixtures stay classified families (`tests/keyspace_classification.rs`
        // scans test code too); the rule is the same for any key.
        assert_eq!(unknown_prefix(b"data:users:42"), b"data:".to_vec());
    }

    #[test]
    fn a_range_is_one_tables_only_inside_its_row_prefix() {
        assert_eq!(single_table_data_range(b"data:t:", b"data:t;").as_deref(), Some("t"));
        assert_eq!(single_table_data_range(b"data:t:5", b"data:t:9").as_deref(), Some("t"));
        assert_eq!(single_table_data_range(b"data:t:", b"data:u:"), None);
        assert_eq!(single_table_data_range(b"meta:table:t", b"meta:table:u"), None);
    }

    #[test]
    fn notes_route_each_class_and_deduplicate_rows() {
        let mut touched = Touched::default();
        touched.note(b"data:users:1", true);
        touched.note(b"data:users:1", false);
        touched.note(b"data:users:2", false);
        touched.note(b"meta:table:users", true);
        touched.note(b"counter:users", true);
        touched.note(b"snapshot:7", true);
        touched.note(format!("v_idx:users:1:{:020}", u64::MAX - 7).as_bytes(), true);
        touched.note(b"dict:users:status", true);
        touched.note(b"wal:last_lsn", true);
        let rows = &touched.data["users"];
        assert_eq!(rows.len(), 2);
        assert!(rows[&1].put);
        assert!(!rows[&2].put);
        assert!(touched.meta_tables.contains("users"));
        assert!(touched.counters.contains("users"));
        assert_eq!(touched.snapshots.len(), 1);
        assert_eq!(touched.max_version_ts, Some(7));
        assert!(touched.dictionaries && touched.fixes_decoding);
        assert!(touched.unknown.is_empty());
        assert!(touched.touches_catalog());
        assert!(!touched.holds_every_table());
        assert_eq!(touched.held_tables(), vec!["users".to_string()]);
    }
}
