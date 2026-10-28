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
//! # How (per run, one thread, under the engine's replay guard)
//!
//! A run is one batch ([`PhysicalApplier::apply_with`]) or the contiguous
//! batches of a receiver's poll ([`PhysicalApplier::apply_run`], spec P4.1d:
//! at most `physical_wire::APPLY_CHUNK_BYTES` of them, or one larger batch).
//! The steps below run once per run, not once per batch — the guard, the
//! decode-and-classify pass, the pre-read, the publication — except the
//! writes, which stay one per batch.
//!
//! 1. **Decode** every raw batch of the run ([`decode_raw_batch`]: the same
//!    record walker that validates it) and classify every key
//!    (`storage::keyspace`). A range deletion is enumerated against the store
//!    before the run's first write (at most [`RANGE_ENUMERATION_CAP`] keys;
//!    beyond that the table is re-derived, and a range that is not one
//!    table's rows defers a full re-derivation, which also forgets every
//!    snapshot left without a record); a key an earlier batch of the run
//!    writes into a later one's range is noted by its own record.
//! 2. **Pre-read**, before anything changes: the old image of every touched
//!    row of an indexed table, decoded with the pre-run schema (an index
//!    removal must name the key the row actually held), and the pre-run
//!    schemas and constraint sets of touched tables. A row several batches
//!    of the run touch is read once, as it was before the first of them.
//!    Then the census hold (below), and the row-cache /
//!    committed-write-watermark invalidation of every touched row —
//!    invalidated before the new bytes are visible.
//! 3. **Apply** verbatim through [`LocalWriter::apply_source_with`]: per
//!    batch, in order, one write of the source bytes plus the node-local
//!    cursor (fsynced, or — from a receiver's poll — deferred to one fsync
//!    per poll). The applier never writes to RocksDB in any other way.
//! 4. **Publish**, once for the run, from each key's pre-run to its post-run
//!    value, in this order:
//!
//! | Key class | Publication |
//! |---|---|
//! | `Dict` | `forget_cached_dictionaries` — first, so the rows below decode against the new entries |
//! | `MetaTable` | schema/constraints/statistics/row caches of the table; then by schema change: dropped → `drop_table_indexes` + vector hook; created → registered and backfilled (step 5); pure column rename → `rename_column_in_indexes`; columns appended → nothing, or the new UNIQUE flags registered and backfilled; anything else, or a schema more than one batch of the run wrote → the table is re-derived; `pg_temp_*` → `note_temp_table_present` |
//! | `MetaIndex` delete | `drop_index` (memory only) + vector hook |
//! | `Data` | new image decoded with the post-run schema (`decode_stored_row`, the scan's decode); `stage_replicated_row(old, new).commit()` per row, a key swapped between rows inside the run resolved by removing every refused row's old keys before adding the new ones; vector hook; row-id counter raised to the row id; `publish_live_replay_visibility`; decode failure → the table is re-derived |
//! | `TableConstraints` | constraints cache; new UNIQUE / FK indexes (`register_unique_constraint_indexes`, `create_fk_index`) registered and backfilled; a constraint that disappeared, or a set more than one batch of the run wrote → the table is re-derived |
//! | `MetaIndex` put | ART family: `register_index_definition` + backfill of that index (a definition more than one batch of the run wrote: its registration is dropped and the table it names re-derived, which registers it); vector family: vector hook |
//! | `Snapshot` | `ingest_snapshot_record` + `raise_timestamp`; delete → `forget_snapshots` (the run's deletions at once) + `clear_cache` |
//! | `VersionIdx` / `Version` / `Vmeta` | timestamp raised past `u64::MAX - rev` and past a COPY marker's timestamp; COPY markers ingested / forgotten; the table's cached `AS OF` answers invalidated |
//! | `Counter` | `raise_row_counter_to(decoded value)` |
//! | `VgcWatermark` | `set_gc_low_watermark` + `raise_timestamp` |
//! | `Branch` / `Git` | `reload_branch_manager` (retried on the next batch while a registry names metadata not yet arrived) |
//! | `Statistics`, `Columnar*`, branch rows | statistics / row caches of the table |
//! | `Vidx` | vector hook (`on_vidx_record` per key, then `on_batch_published` once per run) |
//! | catalog classes (`Meta*`, `TableConstraints`, `Trigger*`, `ViewMeta`, `Statistics`, `Compression`) | [`DerivedStateListener::on_catalog_change`]; schema generation bumped once |
//! | `Cas` | nothing to invalidate (content-addressed by hash); only retries a pending re-derivation |
//! | logical WAL, cursors, durable index snapshots, TDE / elision sentinels, `vgc:anchor`, legacy counters, MV deltas, sync change log | nothing: no derived state reads them on a standby |
//! | `Unknown` | WARN once per prefix; `derived_state_suspect` set (sticky) and a full re-derivation deferred to [`PhysicalApplier::run_pending_rebuilds`] |
//!
//! 5. **Re-derive** the tables the run created and those it queued:
//!    drop the table's ART registrations, register them again from the durable
//!    records exactly as an open would (`register_index_definition`,
//!    `register_table_indexes`) and backfill them by scan
//!    (`backfill_table_indexes`). A re-derivation that fails (a row that does
//!    not decode yet) leaves the table PENDING; it is retried when a later
//!    batch carries a key that can fix decoding, or at the next idle poll.
//!
//! # Readers during publication
//!
//! Readers do not take the replay guard. Between the run's first write and
//! the end of step 5 a touched table's index can lag its rows by one run (a
//! fresh row not yet in the PK index, `pk_index_len` one run behind). The
//! applier therefore holds the engine's uncommitted-write census for every
//! table it publishes (for all tables when a run touches index definitions
//! or unknown keys), from before the first write until the publication is
//! done — the same gate a transaction's eager index writes take — so every
//! miss-based and cardinality fast path declines to the planner, which reads
//! row storage and is exact. A PENDING table keeps its hold until its
//! re-derivation succeeds. The index never advertises a row the store lacks:
//! rows are applied first.
//!
//! # Consistency
//!
//! One [`PhysicalBatch`] is one primary write group, applied whole, so the
//! store only ever holds states a primary passes through; a run is published
//! at the state after its last group. Between two runs a reader sees exactly
//! a state the primary itself could have crashed into — every applied group
//! complete — and publication lags the store by at most one run (a poll: at
//! most `physical_wire::APPLY_CHUNK_BYTES` of groups, or one larger group)
//! instead of one group. DDL that spans write groups (a column-storage
//! migration's rows then its schema; RENAME COLUMN's schema and index
//! definitions, then its constraint records) converges when its last group is
//! published. DROP TABLE deletes everything the table owns in one group (spec
//! P1.1b). A run's publication compares each key's value before the run with
//! its value after it; a table schema, constraint set or index definition
//! more than one group of the run wrote — where that net change could hide a
//! DROP and a CREATE of the same name — has its table re-derived instead. A run whose write fails after some
//! of its groups were written keeps them (complete, durable groups of the
//! primary) and re-derives the tables it touched instead of publishing.
//!
//! What this module does not do: the SQL layer's registries and caches
//! (functions, triggers, plan caches) are the [`DerivedStateListener`]'s
//! (`EmbeddedDatabase`, spec P1.4); vector indexes are the [`VectorHook`]'s
//! (spec P1.2), by default `physical_vector::StandbyVectorHook`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use rocksdb::{Direction, IteratorMode, ReadOptions};
use tracing::{debug, warn};

use super::physical::{walk_raw_batch, Durability, LocalWriter, PhysicalBatch, PhysicalError};
use super::physical_vector::StandbyVectorHook;
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
    /// `apply` and `apply_run`, only before the first batch was written:
    /// nothing was.
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
/// The applier's own is `physical_vector::StandbyVectorHook`.
pub trait VectorHook: Send + Sync {
    /// Whether rows of `table` must be decoded for [`Self::on_row`] even when
    /// the table has no ART index.
    fn wants_rows(&self, _engine: &StorageEngine, _table: &str) -> bool {
        false
    }

    /// One row of `table` changed: `old` decoded before the batch (of a run:
    /// before its first batch), `new` after it (after the run's last), both
    /// shaped to the post-batch `schema`. Called once per row per run.
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

    /// One `__vidx:` record changed; `value` is its post-batch (post-run)
    /// value (`None` when deleted). Called in key order, after every other
    /// hook of the publication but [`Self::on_batch_published`].
    fn on_vidx_record(&self, _engine: &StorageEngine, _key: &[u8], _value: Option<&[u8]>) {}

    /// The publication of one applied batch — or of one run of batches, a
    /// receiver's poll (spec P4.1d) — is complete: every hook above has been
    /// called for it, and its re-derivations (step 5, which call
    /// [`Self::on_table_rederived`]) are next. Lets a hook publish the
    /// records as one unit: the state after the run's last write group, one
    /// the primary passed through.
    fn on_batch_published(&self, _engine: &StorageEngine) {}

    /// A vector-family `meta:index:` definition was written or deleted.
    fn on_index_definition(&self, _engine: &StorageEngine, _index_name: &str, _deleted: bool) {}

    /// `table` was dropped.
    fn on_table_dropped(&self, _engine: &StorageEngine, _table: &str) {}

    /// `table`'s ART indexes were re-derived from a scan.
    fn on_table_rederived(&self, _engine: &StorageEngine, _table: &str) {}

    /// The whole derived state was re-derived.
    fn on_full_rebuild(&self, _engine: &StorageEngine) {}
}

/// A [`VectorHook`] that maintains nothing: a standby's vector indexes keep
/// what hydration built (tools, and the tests' control case). The applier's
/// default is `physical_vector::StandbyVectorHook`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopVectorHook;

impl VectorHook for NoopVectorHook {}

/// What one [`PhysicalApplier::apply`] (or [`PhysicalApplier::apply_run`])
/// did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// The batch's (the run's last batch's) last source sequence: the cursor
    /// after it.
    pub source_sequence: u64,
    /// Distinct `data:` keys published (0 for a retransmission): a key
    /// several batches of a run touched counts once.
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
            vector: Arc::new(StandbyVectorHook::new()),
            stats: Mutex::new(ApplierStats::default()),
            state: Mutex::new(ApplierState::default()),
        })
    }

    /// Replace the default [`VectorHook`] ([`StandbyVectorHook`], spec P1.2).
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
    /// ART indexes, standard HNSW from rows, persistent HNSW reopened from its
    /// replicated keys, row counters in memory) and the
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
        self.apply_with(batch, Durability::Synced)
    }

    /// [`Self::apply`], with the write fsynced or not by `durability`
    /// ([`LocalWriter::apply_source_with`]). The publication is the same
    /// either way: a deferred group is applied (it and its cursor are in the
    /// store), only not yet fsynced; the receiver syncs once per poll.
    /// [`Self::apply_run`] over this one batch.
    pub fn apply_with(&self, batch: &PhysicalBatch, durability: Durability) -> Result<Applied> {
        self.apply_run(std::slice::from_ref(batch), durability)
    }

    /// Spec P4.1d: apply a run of contiguous batches — a receiver's poll —
    /// as ONE unit of work: the replay guard taken once; every batch decoded;
    /// ONE pre-read over the union of the keys they touch, before the first
    /// write; the batches written one RocksDB write each, in order, through
    /// [`LocalWriter::apply_source_with`] exactly as [`Self::apply_with`]
    /// writes one (each with its own cursor put: the writer's accounting does
    /// not depend on the run); then ONE publication over the union, with the
    /// post-run schemas, by the same per-class rules. The result's
    /// `source_sequence` is the last batch's; `rows` counts each `data:` key
    /// once, however many batches of the run touched it.
    ///
    /// A row touched by several batches of the run is published once, from
    /// the image it had before the FIRST of them (the pre-read runs before
    /// any batch is written, as a single batch's does for a key it writes
    /// twice) to the one it has after the last. A table schema, constraint
    /// set or index definition that more than one batch of the run wrote or
    /// deleted is not published from that net change alone — it can hide a
    /// DROP and a CREATE of the same name in between: the table is re-derived
    /// from the durable records instead (for an index, its registration is
    /// dropped and the table its final definition names re-derived).
    ///
    /// `Err` before the first write means nothing of the run was applied. A
    /// write that fails after earlier batches of the run were written leaves
    /// those written — each is a complete write group the primary made
    /// durable, and the cursor accounts for it — and replaces the run's
    /// publication with the re-derivation path (`rederive_failed_run`), as a
    /// row that does not decode does; the error is returned.
    pub fn apply_run(&self, batches: &[PhysicalBatch], durability: Durability) -> Result<Applied> {
        let _replay = self.engine.replay_guard();
        let mut state = self.state.lock();

        // An exact retransmission changes nothing, and anything else at or
        // below the cursor is refused by the writer; neither needs a plan.
        // Only a run's first batches can be at or below the cursor: the rest
        // follow them contiguously.
        let cursor = self.writer.cursor().source_sequence();
        let stale = batches
            .iter()
            .take_while(|batch| batch.last_sequence() <= cursor)
            .count();
        for batch in batches.get(..stale).unwrap_or_default() {
            if self.writer.apply_source_with(batch, durability)? {
                // Unreachable through `apply_source`'s own rules; if it ever
                // happens, the batch was applied without a publication.
                self.defer_full_rebuild(&mut state, "a batch at or below the cursor was applied");
            }
        }
        let run = batches.get(stale..).unwrap_or_default();
        let Some(last) = run.last() else {
            return Ok(Applied {
                source_sequence: batches.last().map_or(cursor, PhysicalBatch::last_sequence),
                ..Applied::default()
            });
        };

        // 1. Decode and classify every batch of the run; enumerate range
        //    deletions while the keys they remove still exist — against the
        //    store before the run's first write (a key an earlier batch of the
        //    run writes into a later one's range is noted by its own record).
        let mut touched = Touched::default();
        for (index, batch) in run.iter().enumerate() {
            touched.batch = index;
            for op in decode_raw_batch(batch.data())? {
                match op {
                    RawOp::Put { key, .. } => touched.note(key, true),
                    RawOp::Delete { key } => touched.note(key, false),
                    RawOp::DeleteRange { start, end } => self.enumerate_range(start, end, &mut touched)?,
                }
            }
        }

        // 2. Pre-read (the images before the run), census hold, invalidate
        //    before visible.
        let mut work = self.pre_read(&touched, &state)?;
        let held = touched.held_tables();
        let every_table = touched.holds_every_table();
        // A run that touches no table's rows or catalog (sequence state, the
        // logical WAL) leaves every index exact: no hold.
        let _hold =
            (every_table || !held.is_empty()).then(|| self.engine.write_census().hold_for_replay(held, every_table));
        for (table, rows) in &touched.data {
            self.engine.invalidate_write_watermark(table);
            for row_id in rows.keys() {
                self.engine.row_cache().invalidate(table, *row_id);
            }
        }

        // 3. Apply verbatim: one write per batch, in order.
        let mut written = 0_u64;
        for batch in run {
            match self.writer.apply_source_with(batch, durability) {
                Ok(true) => written += 1,
                // An exact retransmission of the batch before it: nothing changed.
                Ok(false) => {}
                Err(error) => {
                    if written > 0 {
                        self.rederive_failed_run(&touched, written, &mut state);
                    }
                    return Err(error.into());
                }
            }
        }
        if written == 0 {
            return Ok(Applied {
                source_sequence: last.last_sequence(),
                ..Applied::default()
            });
        }
        if touched.touches_catalog() {
            // Existence and plan caches key off the schema generation; bump
            // it as soon as the new catalog bytes are visible.
            self.engine.bump_schema_generation();
        }

        // 4. Publish, once for the run.
        self.publish(&touched, &mut work, &mut state);

        // 5. Re-derive what the run created or queued.
        let mut attempt: BTreeSet<String> = work.register.iter().chain(&work.rederive).cloned().collect();
        if touched.fixes_decoding {
            attempt.extend(state.pending.keys().cloned());
        }
        let fallback: BTreeSet<String> = work.rederive.iter().chain(state.pending.keys()).cloned().collect();
        let tables_rebuilt = self.rederive_tables(attempt, &fallback, &mut state);

        let rows: usize = touched.data.values().map(BTreeMap::len).sum();
        {
            let mut stats = self.stats.lock();
            stats.batches += written;
            stats.rows += rows as u64;
            stats.pending_tables = state.pending.len();
        }
        Ok(Applied {
            source_sequence: last.last_sequence(),
            rows,
            tables_rebuilt,
        })
    }

    /// Spec P4.1d: a run whose write failed after some of its batches were
    /// written. Those stay written: each is a complete write group the
    /// primary made durable, and the writer's cursor accounts for it, so the
    /// store is a state the primary passed through. The run's publication
    /// was planned for all of it, though — its post-run images, schemas and
    /// timestamps include batches that never reached the store — so it is
    /// not run. Instead, as for a row that does not decode, the derived
    /// state is re-derived from the store as it is, now, under the replay
    /// guard the run holds — the receiver stops on the error, so a deferred
    /// re-derivation would never run:
    ///
    /// * a run that touched the catalog, index definitions or keys no table
    ///   can be named for: the whole derived state (`rebuild_all_locked`:
    ///   every table, the snapshot map and timestamps, the version-GC
    ///   horizon, the branch registry, every cache, the listener);
    /// * any other run: every table it touched (one that cannot be re-derived
    ///   stays pending, holding the census, its readers on scans), and the
    ///   rest — the snapshot map and timestamps, row counters, caches — is
    ///   marked suspect and re-derived in full at the next idle poll
    ///   ([`Self::run_pending_rebuilds`]); a receiver that stops on the error
    ///   re-hydrates at its next start instead.
    ///
    /// `written` batches count as applied.
    fn rederive_failed_run(&self, touched: &Touched, written: u64, state: &mut ApplierState) {
        self.stats.lock().batches += written;
        if touched.holds_every_table() || touched.touches_catalog() {
            if let Err(e) = self.rebuild_all_locked(state) {
                warn!(
                    "physical applier: re-deriving the whole derived state after a failed run failed ({}); it \
                     stays suspect",
                    e
                );
                self.defer_full_rebuild(state, "a failed run's re-derivation failed");
            }
        } else {
            let tables: BTreeSet<String> = touched.held_tables().into_iter().collect();
            self.rederive_tables(tables.clone(), &tables, state);
            self.defer_full_rebuild(
                state,
                "a run of write groups failed part-way; its publication is replaced by a re-derivation",
            );
        }
        self.stats.lock().pending_tables = state.pending.len();
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
        self.vector.on_batch_published(&self.engine);
        self.publish_listener(touched);
        self.publish_epoch(touched);
        self.publish_unknown(touched, state);
    }

    /// Spec P5.1 §1: an applied run carried `repl:epoch` — the engine's
    /// cache follows the store, and the posture (which reads that cache)
    /// with it. Logged at INFO; no derived state. A value that does not
    /// decode keeps the cached epoch and says so: the next open of the store
    /// refuses it with the same reason.
    fn publish_epoch(&self, touched: &Touched) {
        if !touched.epoch {
            return;
        }
        match crate::replication::epoch::read_epoch_record(self.writer.db()) {
            Ok(record) => {
                let before = self.engine.replication_epoch();
                self.engine.note_replication_epoch(record.as_ref());
                tracing::info!(
                    "physical applier: the dataset's replication epoch is now {} (was {before}{})",
                    self.engine.replication_epoch(),
                    record
                        .as_ref()
                        .map(|record| format!(
                            "; promoted from {} by node {}, {}",
                            record.previous,
                            record.node_id,
                            if record.fenced {
                                "the old primary fenced first"
                            } else {
                                "the old primary NOT fenced"
                            }
                        ))
                        .unwrap_or_default()
                );
            }
            Err(e) => warn!(
                "physical applier: the applied replication epoch record cannot be read ({}); this standby keeps \
                 reporting epoch {}",
                e,
                self.engine.replication_epoch()
            ),
        }
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
                // Spec P4.1d: written by more than one batch of the run — a
                // DROP and a CREATE of the same name could sit in between,
                // which the net change does not show.
                (Some(_), Some(_)) if touched.redefined_tables.contains(table) => {
                    work.rederive.insert(table.clone());
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
    /// maintained for this run's rows. Puts are kept for after staging.
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
                    if touched.redefined_indexes.contains(name) {
                        // Spec P4.1d: written and deleted by different batches
                        // of the run (DROP INDEX, then CREATE INDEX of the same
                        // name): a registration of the name may be the old
                        // definition's, possibly on another table. It is
                        // dropped first, as a deleted definition's is (above):
                        // the re-derivation of the table the final definition
                        // names then registers it there — it would leave a
                        // name another table still holds, and that table's
                        // own re-derivation would drop it again.
                        if let Some((kind, table)) = art.index_kind_and_table(name) {
                            if let Err(e) = art.drop_index(name) {
                                debug!("physical applier: dropping index '{}': {}", name, e);
                            }
                            if kind != ArtIndexType::Manual {
                                // It may have been a constraint's index on
                                // that table (a name a record later took):
                                // that table's own records re-mint it.
                                work.rederive.insert(table);
                            }
                        }
                        if let Ok(Some(definition)) = self.engine.catalog().get_index_definition(name) {
                            work.rederive.insert(definition.table_name);
                        }
                    }
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
            // Spec P4.1d: a constraint set more than one batch of the run
            // wrote is re-derived: a constraint dropped and added again under
            // its name in between is invisible to the net comparison.
            if touched.redefined_tables.contains(table) || constraints_were_removed(pre, post.as_ref()) {
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

    /// `table`'s durable schema ([`durable_schema`]).
    fn durable_schema(&self, table: &str) -> crate::Result<Option<Schema>> {
        durable_schema(&self.engine, table)
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

/// `table`'s durable schema, read past the schema cache and the
/// session-private visibility rule (`Catalog::get_table_schema` applies
/// both; the applier has no session and must see every table), with the
/// same B31 `source_table_name` stamp. `None` when the table does not
/// exist. Shared with the standby's vector hook (spec P1.2).
pub(crate) fn durable_schema(engine: &StorageEngine, table: &str) -> crate::Result<Option<Schema>> {
    let Some(bytes) = engine.get(&format!("meta:table:{table}").into_bytes())? else {
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

/// Every key a batch — or a run of batches (spec P4.1d) — touched, sorted by
/// what publishing it involves. Keys are deduplicated: publication reads each
/// one's post-run value, which is the net effect of all of the run's records
/// on it.
#[derive(Default)]
struct Touched {
    /// The batch of the run whose records are being noted (0 for a run of
    /// one batch).
    batch: usize,
    /// The last batch of the run that wrote or deleted each table schema,
    /// constraint set and index definition, by key.
    definition_batches: BTreeMap<Vec<u8>, usize>,
    /// Tables whose schema or constraint set more than one batch of the run
    /// wrote or deleted: re-derived, since the net change can hide a DROP
    /// and a CREATE of the same name in between.
    redefined_tables: BTreeSet<String>,
    /// Index definitions more than one batch of the run wrote or deleted:
    /// the name's registration is dropped and the table the final definition
    /// names is re-derived, which registers it there.
    redefined_indexes: BTreeSet<String>,
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
    /// Spec P5.1: the run wrote (or deleted) `repl:epoch`.
    epoch: bool,
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
                        if self.rewritten_by_another_batch(key) {
                            self.redefined_tables.insert(table.clone());
                        }
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
                        if self.rewritten_by_another_batch(key) {
                            self.redefined_indexes.insert(name.clone());
                        }
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
                    if self.rewritten_by_another_batch(key) {
                        self.redefined_tables.insert(table.clone());
                    }
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
            // Spec P5.1: the dataset's epoch; no derived state, only the
            // engine's cache of it (what this standby reports and announces).
            KeyClass::ReplEpoch => self.epoch = true,
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

    /// Spec P4.1d: whether a batch of the run before the current one wrote
    /// or deleted the definition `key` too. Records the current batch as the
    /// last that did. Never true for a run of one batch.
    fn rewritten_by_another_batch(&mut self, key: &[u8]) -> bool {
        match self.definition_batches.insert(key.to_vec(), self.batch) {
            Some(previous) => previous != self.batch,
            None => false,
        }
    }

    /// The tables whose indexes this run's publication may leave behind
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
        assert!(!touched.epoch);
    }

    /// Spec P5.1 §1: `repl:epoch` is a classified key of its own — never an
    /// unknown one (which would mark the derived state suspect) — and it
    /// touches no table, catalog key or index.
    #[test]
    fn the_replication_epoch_is_noted_as_itself_and_touches_no_derived_state() {
        let mut touched = Touched::default();
        touched.note(b"repl:epoch", true);
        assert!(touched.epoch);
        assert!(touched.unknown.is_empty());
        assert!(touched.data.is_empty() && touched.meta_tables.is_empty());
        assert!(!touched.touches_catalog());
    }

    /// Spec P4.1d: a schema, constraint set or index definition that two
    /// batches of a run write is marked redefined (its net change could hide
    /// a DROP and a CREATE of the same name); one batch writing it twice is
    /// that group's own net change, as before runs existed.
    #[test]
    fn a_definition_two_batches_of_a_run_write_is_marked_redefined() {
        let mut touched = Touched::default();
        touched.note(b"meta:table:users", false);
        touched.note(b"meta:table:users", true);
        touched.note(b"meta:index:by_name", true);
        touched.note(b"meta:table:kept", true);
        assert!(touched.redefined_tables.is_empty() && touched.redefined_indexes.is_empty());
        touched.batch = 1;
        touched.note(b"meta:table:users", true);
        touched.note(b"table_constraints:orders", true);
        touched.note(b"meta:index:by_name", false);
        touched.batch = 2;
        touched.note(b"table_constraints:orders", true);
        touched.note(b"data:kept:1", true);
        assert_eq!(
            touched.redefined_tables,
            BTreeSet::from(["orders".to_string(), "users".to_string()])
        );
        assert_eq!(touched.redefined_indexes, BTreeSet::from(["by_name".to_string()]));
        assert!(touched.meta_tables.contains("kept") && !touched.redefined_tables.contains("kept"));
    }

    /// Spec P4.1d §3: runs of batches through a real standby — a primary
    /// `EmbeddedDatabase`, its checkpoint downloaded into a stage, the stage's
    /// writer under `StorageEngine::open_physical_standby` and an applier
    /// (the production path up to the first cursor, as in
    /// `tests/physical_standby_derived_state.rs`).
    #[cfg(target_os = "linux")]
    mod runs {
        use super::super::*;
        use crate::replication::checkpoint_transfer::{
            OnlineCheckpointExport, PhysicalStage, ReceiverStaging, TransferLimits,
        };
        use crate::replication::history::load_or_create_primary_history;
        use crate::replication::physical::{poll_source, range_digest, FlushMemo, MAX_BATCH_BYTES};
        use crate::storage::WriteGate;
        use crate::{Config, EmbeddedDatabase};
        use rocksdb::{WriteBatch, DB};
        use std::path::Path;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use uuid::Uuid;

        fn primary_config(path: &Path) -> Config {
            let mut config = Config::default();
            config.storage.path = Some(path.to_path_buf());
            config.storage.memory_only = false;
            config.replication.physical.enabled = true;
            config
        }

        /// A primary. Fields drop in declaration order, the directory last.
        struct Primary {
            raw: Arc<DB>,
            database: EmbeddedDatabase,
            history: Uuid,
            root: tempfile::TempDir,
        }

        impl Primary {
            fn new(setup: &[&str]) -> Self {
                let root = tempfile::tempdir().unwrap();
                let database = EmbeddedDatabase::with_config(primary_config(&root.path().join("primary"))).unwrap();
                let raw = database.storage.db();
                let primary = Self {
                    history: load_or_create_primary_history(raw.path()).unwrap(),
                    raw,
                    database,
                    root,
                };
                primary.run(setup);
                primary
            }

            fn run(&self, statements: &[&str]) {
                for sql in statements {
                    self.database
                        .execute(sql)
                        .map_err(|e| format!("primary `{sql}`: {e}"))
                        .unwrap();
                }
            }

            /// A physical standby staged from the primary as it is now, into
            /// `name` under the primary's directory; `hook` replaces the
            /// applier's vector hook.
            fn standby(&self, name: &str, hook: Option<Arc<dyn VectorHook>>) -> Standby {
                let root = self.root.path();
                let config = primary_config(&root.join("primary"));
                let limits = TransferLimits::default();
                let export = OnlineCheckpointExport::create(&self.raw, self.history, root, limits).unwrap();
                let mut receiver = ReceiverStaging::begin(
                    &root.join(name),
                    &export.manifest_bytes().unwrap(),
                    self.history,
                    limits,
                )
                .unwrap();
                for (index, file) in export.manifest().files.iter().enumerate() {
                    let mut offset = 0;
                    while offset < file.size {
                        let bytes = export.read_chunk(index as u32, offset, 64 * 1024).unwrap();
                        receiver.append_chunk(index as u32, offset, &bytes).unwrap();
                        offset += bytes.len() as u64;
                    }
                }
                let stage = receiver.finish_download(&config).unwrap().open(&config).unwrap();
                let path = stage.staging_path().to_path_buf();
                let engine = Arc::new(
                    StorageEngine::open_physical_standby(
                        &path,
                        &primary_config(&path),
                        Arc::clone(stage.writer()),
                        WriteGate::closed("physical standby"),
                    )
                    .unwrap(),
                );
                let mut applier = PhysicalApplier::new(
                    Arc::clone(&engine),
                    Arc::clone(stage.writer()),
                    Arc::new(NoopDerivedStateListener),
                )
                .unwrap();
                if let Some(hook) = hook {
                    applier = applier.with_vector_hook(hook);
                }
                applier.hydrate().unwrap();
                Standby {
                    applier,
                    engine,
                    _stage: stage,
                }
            }

            /// Every write group of the primary past `after`, in order.
            fn groups_after(&self, mut after: u64) -> Vec<PhysicalBatch> {
                let mut groups = Vec::new();
                loop {
                    let poll = poll_source(&self.raw, self.history, after, MAX_BATCH_BYTES, &FlushMemo::new()).unwrap();
                    if poll.batches.is_empty() {
                        return groups;
                    }
                    after = poll.through;
                    groups.extend(poll.batches);
                }
            }
        }

        /// A standby. The applier and the engine release the writer before
        /// the stage that owns it.
        struct Standby {
            applier: PhysicalApplier,
            engine: Arc<StorageEngine>,
            _stage: PhysicalStage,
        }

        impl Standby {
            fn cursor(&self) -> u64 {
                self.applier.writer().cursor().source_sequence()
            }

            fn digest(&self) -> [u8; 32] {
                range_digest(self.applier.writer().db(), None, b"").unwrap()
            }
        }

        /// The `data:{table}:` keys one batch's records write or delete.
        fn row_keys(batch: &PhysicalBatch, table: &str) -> BTreeSet<Vec<u8>> {
            let prefix = format!("data:{table}:").into_bytes();
            decode_raw_batch(batch.data())
                .unwrap()
                .into_iter()
                .filter_map(|op| match op {
                    RawOp::Put { key, .. } | RawOp::Delete { key } => key.starts_with(&prefix).then(|| key.to_vec()),
                    RawOp::DeleteRange { .. } => None,
                })
                .collect()
        }

        /// What a publication tells the vector hook: every row change it
        /// stages, and every publication it completes.
        #[derive(Default)]
        struct Counting {
            rows: Mutex<Vec<(String, u64, bool, bool)>>,
            published: AtomicUsize,
        }

        impl VectorHook for Counting {
            fn wants_rows(&self, _engine: &StorageEngine, _table: &str) -> bool {
                true
            }

            fn on_row(
                &self,
                _engine: &StorageEngine,
                table: &str,
                row_id: u64,
                _schema: &Schema,
                old: Option<&Tuple>,
                new: Option<&Tuple>,
            ) {
                self.rows
                    .lock()
                    .push((table.to_string(), row_id, old.is_some(), new.is_some()));
            }

            fn on_batch_published(&self, _engine: &StorageEngine) {
                self.published.fetch_add(1, Ordering::SeqCst);
            }
        }

        /// `table`'s ART indexes, entry for entry, by kind and columns.
        fn census(engine: &StorageEngine, table: &str) -> BTreeMap<(String, Vec<String>), Vec<(Vec<u8>, u64)>> {
            let art = engine.art_indexes();
            let mut census = BTreeMap::new();
            for (name, kind, columns) in art.list_table_indexes(table) {
                let mut entries = art.index_range_scan(&name, None, None, None);
                entries.sort();
                census.insert((format!("{kind:?}"), columns), entries);
            }
            census
        }

        fn rows_of(engine: &StorageEngine, table: &str) -> Vec<Tuple> {
            let mut rows = engine.scan_table(table).unwrap();
            rows.sort_by_key(|row| row.row_id);
            rows
        }

        /// Spec P4.1d §3: the groups of an INSERT, an UPDATE and a DELETE of
        /// one row, applied as one run, publish that row ONCE — from its image
        /// before the first of them (absent) to its final one (absent) — in
        /// one publication, and leave every index at the final state: the row
        /// is in none of them, the row the run did not touch still is.
        #[test]
        fn a_run_over_an_insert_update_and_delete_of_one_row_publishes_the_final_state_once() {
            let primary = Primary::new(&[
                "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)",
                "CREATE INDEX t_v ON t (v)",
                "INSERT INTO t VALUES (2, 'two')",
            ]);
            let hook = Arc::new(Counting::default());
            let standby = primary.standby("standby", Some(hook.clone() as Arc<dyn VectorHook>));
            let before = standby.applier.stats();
            primary.run(&[
                "INSERT INTO t VALUES (1, 'one')",
                "UPDATE t SET v = 'uno' WHERE id = 1",
                "DELETE FROM t WHERE id = 1",
            ]);
            let run = primary.groups_after(standby.cursor());
            let per_batch: Vec<BTreeSet<Vec<u8>>> = run.iter().map(|batch| row_keys(batch, "t")).collect();
            let distinct: BTreeSet<Vec<u8>> = per_batch.iter().flatten().cloned().collect();
            let touching = per_batch.iter().filter(|keys| !keys.is_empty()).count();
            assert!(
                touching >= 3,
                "the three statements wrote t's rows in {touching} of {} groups",
                run.len()
            );
            assert!(
                touching > distinct.len(),
                "precondition: a row more than one group of the run touches"
            );

            let applied = standby.applier.apply_run(&run, Durability::Synced).unwrap();
            assert_eq!(applied.source_sequence, run.last().unwrap().last_sequence());
            assert_eq!(standby.cursor(), applied.source_sequence);
            assert_eq!(
                applied.rows,
                distinct.len(),
                "each row the run touched is published once"
            );
            assert!(applied.tables_rebuilt.is_empty(), "{applied:?}");
            assert_eq!(hook.published.load(Ordering::SeqCst), 1, "one publication for the run");
            let changes: Vec<(String, u64, bool, bool)> = hook
                .rows
                .lock()
                .iter()
                .filter(|change| change.0 == "t")
                .cloned()
                .collect();
            assert_eq!(changes.len(), distinct.len(), "{changes:?}");
            assert!(
                changes.iter().all(|&(_, _, old, new)| !old && !new),
                "absent before the run's first group and after its last: {changes:?}"
            );
            let stats = standby.applier.stats();
            assert_eq!(stats.batches - before.batches, run.len() as u64, "every group written");
            assert_eq!(stats.rows - before.rows, distinct.len() as u64);
            assert_eq!(stats.rebuilds, 0, "{stats:?}");
            assert_eq!(stats.pending_tables, 0, "{stats:?}");
            assert!(!stats.derived_state_suspect, "{stats:?}");

            let engine = &standby.engine;
            let art = engine.art_indexes();
            assert_eq!(art.pk_index_len("t"), Some(1));
            assert_eq!(engine.get_row_by_pk("t", &Value::Int4(1)).unwrap(), None);
            let kept = engine.get_row_by_pk("t", &Value::Int4(2)).unwrap().unwrap();
            assert_eq!(engine.count_table_rows("t").unwrap(), 1);
            let on_v = art.find_column_index("t", "v").unwrap();
            let entries = art.index_range_scan(&on_v, None, None, None);
            assert_eq!(
                entries.iter().map(|(_, row_id)| Some(*row_id)).collect::<Vec<_>>(),
                vec![kept.row_id],
                "only the untouched row is in the index on v"
            );
        }

        /// Spec P4.1d §3: a run whose middle group carries a row that does
        /// not decode sends that row's table to the re-derivation path (re-
        /// derived, or — the scan meets the same row — left pending, its
        /// readers on scans), while the tables of the groups before and after
        /// it are published row by row; once the row is gone the pending
        /// table is re-derived.
        #[test]
        fn a_row_that_does_not_decode_mid_run_rederives_its_table_and_the_rest_is_published() {
            let primary = Primary::new(&[
                "CREATE TABLE alpha (id INT PRIMARY KEY, v TEXT)",
                "CREATE TABLE beta (id INT PRIMARY KEY, v TEXT)",
                "CREATE TABLE gamma (id INT PRIMARY KEY, v TEXT)",
                "INSERT INTO beta VALUES (1, 'kept')",
            ]);
            let standby = primary.standby("standby", None);
            primary.run(&["INSERT INTO alpha VALUES (1, 'first')"]);
            // A `beta` row no schema decodes, written below the engine.
            primary.raw.put(b"data:beta:999", b"\xff\xff\xffnot a row").unwrap();
            primary.run(&["INSERT INTO gamma VALUES (1, 'third')"]);
            let run = primary.groups_after(standby.cursor());
            let middle = run
                .iter()
                .position(|batch| row_keys(batch, "beta").contains(b"data:beta:999".as_slice()))
                .unwrap();
            assert!(
                middle > 0 && middle + 1 < run.len(),
                "precondition: groups before and after it"
            );

            let applied = standby.applier.apply_run(&run, Durability::Synced).unwrap();
            assert_eq!(standby.cursor(), applied.source_sequence);
            assert_eq!(applied.source_sequence, run.last().unwrap().last_sequence());
            let stats = standby.applier.stats();
            assert!(
                applied.tables_rebuilt == vec!["beta".to_string()] || stats.pending_tables == 1,
                "beta went to the re-derivation path: {applied:?} {stats:?}"
            );
            assert!(
                !applied
                    .tables_rebuilt
                    .iter()
                    .any(|table| table == "alpha" || table == "gamma"),
                "{applied:?}"
            );
            for table in ["alpha", "gamma"] {
                assert_eq!(standby.engine.art_indexes().pk_index_len(table), Some(1), "{table}");
                assert!(
                    standby.engine.get_row_by_pk(table, &Value::Int4(1)).unwrap().is_some(),
                    "the {table} row of the run is published"
                );
            }

            // Once the row is gone, `beta` is re-derived and exact again.
            let mut delete = WriteBatch::default();
            delete.delete(b"data:beta:999");
            primary.raw.write(delete).unwrap();
            for batch in primary.groups_after(standby.cursor()) {
                standby.applier.apply(&batch).unwrap();
            }
            standby.applier.run_pending_rebuilds().unwrap();
            let stats = standby.applier.stats();
            assert_eq!(stats.pending_tables, 0, "{stats:?}");
            assert!(stats.rebuilds >= 1, "{stats:?}");
            assert_eq!(standby.engine.art_indexes().pk_index_len("beta"), Some(1));
            assert!(standby.engine.get_row_by_pk("beta", &Value::Int4(1)).unwrap().is_some());
        }

        /// Spec P4.1d §3: byte identity of the store does not depend on the
        /// run size. Two standbys staged at the same cut, one applying the
        /// primary's groups one by one and one as a single run, hold the same
        /// default column family — the primary's — and the same derived
        /// state.
        #[test]
        fn a_run_leaves_the_store_byte_identical_to_applying_its_batches_one_by_one() {
            let primary = Primary::new(&[
                "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, qty INT)",
                "INSERT INTO items VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30)",
            ]);
            let one_by_one = primary.standby("one-by-one", None);
            let as_a_run = primary.standby("as-a-run", None);
            assert_eq!(one_by_one.cursor(), as_a_run.cursor(), "staged at the same cut");
            primary.run(&[
                "INSERT INTO items VALUES (4, 'four', 40)",
                "UPDATE items SET qty = qty + 1 WHERE id <= 2",
                "DELETE FROM items WHERE id = 3",
                "CREATE INDEX items_name ON items (name)",
                "INSERT INTO items VALUES (5, 'five', 50)",
                "UPDATE items SET name = 'FOUR' WHERE id = 4",
            ]);
            let run = primary.groups_after(as_a_run.cursor());
            assert!(run.len() > 1, "{} groups", run.len());
            for batch in &run {
                one_by_one.applier.apply(batch).unwrap();
            }
            as_a_run.applier.apply_run(&run, Durability::Synced).unwrap();
            assert_eq!(one_by_one.cursor(), as_a_run.cursor());
            assert_eq!(
                primary.raw.latest_sequence_number(),
                as_a_run.cursor(),
                "precondition: the primary wrote nothing since"
            );
            assert_eq!(
                one_by_one.digest(),
                as_a_run.digest(),
                "the same bytes, whatever the run size"
            );
            assert_eq!(
                as_a_run.digest(),
                range_digest(&primary.raw, None, b"").unwrap(),
                "the primary's default column family"
            );
            let (single, whole) = (&one_by_one.engine, &as_a_run.engine);
            assert_eq!(rows_of(single, "items"), rows_of(whole, "items"));
            assert_eq!(
                single.art_indexes().pk_index_len("items"),
                whole.art_indexes().pk_index_len("items")
            );
            assert_eq!(census(single, "items"), census(whole, "items"));
            assert!(
                whole.art_indexes().find_column_index("items", "name").is_some(),
                "the index the run created is registered"
            );
            let stats = as_a_run.applier.stats();
            assert_eq!(stats.batches, run.len() as u64, "{stats:?}");
            assert_eq!(stats.rebuilds, 0, "{stats:?}");
            assert!(!stats.derived_state_suspect, "{stats:?}");
        }

        /// Review P4.1d: an index name dropped on one table and created on
        /// another inside one run is registered on the new table, backfilled
        /// with its rows, and gone from the old one — with the new table
        /// sorting BEFORE the old one, so its re-derivation runs first and
        /// would have found the name still held by the old table.
        #[test]
        fn an_index_name_moved_to_a_table_that_sorts_first_in_one_run_is_registered_on_it() {
            let primary = Primary::new(&[
                "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
                "CREATE TABLE accounts (id INT PRIMARY KEY, owner TEXT)",
                "INSERT INTO users VALUES (1, 'ann'), (2, 'bob')",
                "INSERT INTO accounts VALUES (1, 'x'), (2, 'y'), (3, 'z')",
                "CREATE INDEX moved ON users (name)",
            ]);
            let standby = primary.standby("standby", None);
            let owner = |standby: &Standby| {
                standby
                    .engine
                    .art_indexes()
                    .index_kind_and_table("moved")
                    .map(|(_, table)| table)
            };
            assert_eq!(owner(&standby).as_deref(), Some("users"), "precondition");
            primary.run(&["DROP INDEX moved", "CREATE INDEX moved ON accounts (owner)"]);
            let run = primary.groups_after(standby.cursor());
            let definition = b"meta:index:moved".as_slice();
            let writes = run
                .iter()
                .filter(|batch| {
                    decode_raw_batch(batch.data()).unwrap().iter().any(|op| match op {
                        RawOp::Put { key, .. } | RawOp::Delete { key } => *key == definition,
                        RawOp::DeleteRange { .. } => false,
                    })
                })
                .count();
            assert!(writes >= 2, "precondition: two groups of the run write the definition");

            standby.applier.apply_run(&run, Durability::Synced).unwrap();
            let art = standby.engine.art_indexes();
            assert_eq!(
                owner(&standby).as_deref(),
                Some("accounts"),
                "registered on the new table"
            );
            assert_eq!(art.index_entry_count("moved"), Some(3), "backfilled with its rows");
            assert_eq!(art.find_column_index("accounts", "owner").as_deref(), Some("moved"));
            assert!(
                art.find_column_index("users", "name").is_none(),
                "gone from the old table"
            );
            let stats = standby.applier.stats();
            assert_eq!(stats.pending_tables, 0, "{stats:?}");
            assert!(!stats.derived_state_suspect, "{stats:?}");
        }
    }
}
