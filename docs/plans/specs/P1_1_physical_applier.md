# Spec P1.1 — `PhysicalApplier`: apply raw batches and keep the engine's derived state right

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.4. Sprinter `db11a1ef3b76` (P1). After P0 (36188199).

Value: with P0 a standby's RocksDB is a byte-identical copy, but a `StorageEngine` opened over it
answers queries from in-memory structures (ART indexes, time-travel snapshot map, caches) that
only the write path maintains. This step makes the applier maintain them from the keys in each
applied batch, so a physical standby can serve correct read-only SQL. No daemon or network work
here: in-process primary → batches → applier → engine, proven by exact-result tests.

## 1. Shapes (`src/replication/physical_applier.rs`, new; feature `ha-tier1`)
```rust
pub enum RawOp<'a> { Put { key: &'a [u8], value: &'a [u8] }, Delete { key: &'a [u8] }, DeleteRange { start: &'a [u8], end: &'a [u8] } }
/// Decode one raw WriteBatch (12-byte header + records). Refactor `validate_raw_batch`
/// (physical.rs) so validation = decode + the existing checks; both share one record walker.
pub fn decode_raw_batch(raw: &[u8]) -> Result<Vec<RawOp<'_>>>;

/// What the applier tells the layer above the engine (EmbeddedDatabase: function/trigger
/// registries, plan/parse caches). Implemented in P1.4; a no-op impl is fine here.
pub trait DerivedStateListener: Send + Sync {
    fn on_catalog_change(&self, class: KeyClass, key: &[u8], deleted: bool);
}
pub struct PhysicalApplier { engine: Arc<StorageEngine>, writer: Arc<LocalWriter>, listener: Arc<dyn DerivedStateListener>, stats: ApplierStats }
pub struct Applied { pub source_sequence: u64, pub rows: usize, pub tables_rebuilt: Vec<String> }
impl PhysicalApplier {
    pub fn new(engine, writer, listener) -> Self;
    /// Decode → pre-read → apply verbatim (LocalWriter::apply_source) → publish. One thread; holds
    /// `engine.replay_guard()` (expose `live_replay_lock`, engine.rs) for the whole call.
    pub fn apply(&self, batch: &PhysicalBatch) -> Result<Applied>;
    pub fn rebuild_table_derived_state(&self, table: &str) -> Result<()>;
    pub fn stats(&self) -> ApplierStats;   // batches, rows, rebuilds, unknown_keys
}
```

## 2. The per-batch algorithm (contract; re-verify every engine call before using it)
1. **Decode.** `decode_raw_batch`; classify every key with `storage::keyspace::classify`.
2. **Pre-read (before the write):**
   - For each `Data{table,row_id}` put/delete: `old = engine.get(key)` (the decrypting raw get,
     engine.rs ~4077 after P0 shifts; bypass caches) decoded with the **pre-apply** schema
     (`Catalog::get_table_schema`). Keep `(table, row_id, old_tuple)`.
   - For a `DeleteRange` over `data:{t}:` enumerate keys (cap 10 000; beyond → mark `t` for rebuild).
   - If the batch contains `meta:table:{t}` put/delete, capture the pre-apply schema of `t`.
   - Invalidate now (the "invalidate before visible" rule, see `apply_live_replicated_row`
     engine.rs ~11539-11690): `invalidate_write_watermark(t)`, row-cache rows for touched keys.
3. **Apply verbatim:** `writer.apply_source(batch)` (Ok(false) = exact retransmission → return early).
4. **Publish, in this order:**
   - `MetaTable` put → `invalidate_schema_cache(t)`, `bump_schema_generation()`,
     `statistics_cache.invalidate(t)`, `invalidate_table_constraints_cache(t)`; if the PK/UNIQUE
     column set changed vs the pre-apply schema → `register_table_indexes(t)` (factor the per-table
     pass out of `Catalog::rebuild_all_indexes`, catalog.rs ~2060: register PK/UNIQUE/FK indexes
     for one table, then backfill by scanning `data:{t}:` through `on_insert`); name starts with
     `pg_temp_` → `note_temp_table_present()`. `MetaTable` delete → `art.clear_table_indexes(t)`
     (art_manager.rs ~3719) + drop vector indexes on `t` (memory only) + invalidate caches.
   - `MetaIndex` put → register that index definition (factor from the index pass of
     `rebuild_all_indexes`): ART → backfill scan; vector → P1.2 (here: call a hook that P1.2
     fills; default rebuild-by-scan via the existing `rebuild_vector_indexes` per-table path).
     `MetaIndex` delete → memory-only drop on the ART manager (and vector hook).
   - `TableConstraints` → `cache_table_constraints` + UNIQUE/FK registration
     (`Catalog::register_unique_constraint_indexes`), backfill if new.
   - `Data` → decode `new` with the **post-apply** schema (the decode closure at
     engine.rs ~11564-11583); `art.stage_replicated_row(t, row_id, &schema, old, new)` then
     `.commit()` (art_manager.rs ~3396); vector hook (P1.2); `publish_live_replay_visibility(t,
     row_id)` (~11681). A decode failure (schema changed in the same batch) → queue `t` for rebuild.
   - `Snapshot` put → `snapshot_manager.ingest_snapshot_record(key, value)` (NEW, factor from
     `recover_snapshots` time_travel.rs ~1777-1812: parse ts + payload into the maps/counters) +
     `engine.raise_timestamp(ts)` (NEW `fetch_max` on `timestamp`); delete → `forget_snapshot(ts)`
     + `clear_cache()`. `VersionIdx` put `v_idx:{t}:{id}:{rev:020}` → `raise_timestamp(u64::MAX -
     rev)`. `Vmeta` → `CopyMarkers::load_record(key, value)` (copy_marker.rs ~114) / remove.
   - `Counter` put → decrypt (engine `decrypt_value`) + bincode u64 → `raise_row_counter_to(t, v)`.
   - `Dict` → `dict_manager.clear_cache()`. `Branch*`/`Git` → swap a fresh
     `BranchManager::open_existing` into `branch_manager`. `VgcWatermark` → `set_gc_low_watermark`
     + raise timestamp (pattern version_gc.rs ~142-149).
   - `MetaFunction/MetaProcedure/MetaTrigger/MetaMv/MetaSeqState/MetaRole/MetaAcl/ViewMeta/
     MetaEnumType/MetaIdentity/MetaPartition/MetaSchema/Statistics/Compression` → listener +
     the relevant engine cache invalidation (schema generation bump covers most).
   - `Columnar*/Cas/Delta` → invalidate row/stat caches for the table.
   - `WalEntries/WalLastLsn/WalCheckpoint/WalPrunedThrough/ReplAppliedLsn/PhysicalCursor/IdxSnap*/
     VecSnap*/MetaTdeKeyCheck/ElideSentinel/SnapshotMap/VgcAnchor/ChangeLog*/SyncAppliedLsn` → ignore.
   - `Unknown` → WARN with the key prefix; if a table can be inferred (`classify` gives one) queue
     a rebuild of it; else set `derived_state_suspect = true` in stats (posture will surface it,
     P1.3) and schedule `rebuild_all_derived_state` at the next idle poll.
5. **Rebuild fallback** `rebuild_table_derived_state(t)`: `clear_table_indexes(t)`, re-register,
   scan `data:{t}:` → `stage_replicated_row(None, Some(tuple)).commit()`, vector hook, caches.
   While a table is rebuilding, PK lookups must not miss: use the engine's existing
   "index unavailable → scan fallback" (lib.rs ~10573 area) — find the flag and set it per table.
6. **Ordering:** apply-then-publish; no concurrent writers exist on a standby. The row cache
   generation is bumped after publish; `pk_index_len` may lag ≤ 1 batch (document).
7. Range deletes: enumerated in step 2, published as per-key deletes in step 4.

## 3. Tests — `tests/physical_standby_derived_state.rs` (feature `ha-tier1`, real RocksDB, no daemons)
Harness: primary `EmbeddedDatabase` (persistent, `replication.physical.enabled = true` for WAL
retention); `OnlineCheckpointExport` → stage → `LocalWriter`; `StorageEngine::open_physical_standby`
+ a minimal `EmbeddedDatabase` over it (if `with_physical_standby` is not yet available, open the
engine only and query through `StorageEngine`/`Catalog` APIs the tests in
`tests/physical_snapshot_hydration.rs` already use); feed batches from `poll_source`.
Each case: run statements on the primary, apply, then compare the standby's answer with the
primary's for: PK point lookup, `COUNT(*)` (ART `pk_index_len`), secondary-index lookup, a UNIQUE
probe, `AS OF` read at a snapshot taken before/after, row counter high-water. Red today (no applier).
Cases: INSERT/UPDATE/DELETE single + multi-row; CREATE INDEX on populated table; ADD CONSTRAINT
UNIQUE; ALTER TABLE ADD COLUMN then INSERT (same and separate batches); RENAME COLUMN; TRUNCATE
(range delete); DROP TABLE; CREATE TABLE + INSERT in one explicit transaction; versioned COMMITs
(time travel); branch create + write + merge; dictionary/CAS migration; sequence nextval on the
primary (counter/seqstate keys arrive, nothing to publish); a batch with an injected `zzz:` key
(Unknown → suspect flag set, no panic). Plus: `rebuilds == 0` for the single-batch cases.
Throughput: a 100k-row corpus applied, print rows/s (no assertion; record in the report).

## Constraints
- No cargo/git. Read before edit. Match style. Engine additions must be `pub(crate)` and small
  (`replay_guard`, `raise_timestamp`, `ingest_snapshot_record`, `forget_snapshot`,
  `register_table_indexes`, `register_index_definition`); cite the code you factored from.
- Do not touch vector internals beyond calling a `VectorHook` trait with a default no-op (P1.2).
- Report: files; the publish table as implemented (class → calls); engine functions added;
  tests + which were red; measured rows/s; deviations; commands.
