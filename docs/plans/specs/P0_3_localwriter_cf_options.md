# Spec P0.3 — `LocalWriter`, the `__nano_local` column family, one DB-options builder

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.1, §D3.5. Sprinter `e707d7b4dd28`. After P0.2.

Value: the physical receiver today appends its cursor into every applied batch in the DEFAULT
column family (physical.rs:415-431), so a standby's keyspace is never byte-identical to the
primary's, and it opens RocksDB with `Options::default()` (checkpoint_transfer.rs:780, 958) —
none of the engine's options (prefix extractor, bloom filter, cache, compression). After this
spec: node-local state lives in its own column family, the default CF is a verbatim copy, and
the receiver and the engine open RocksDB with the same options. These are the two facts every
later phase (serving, archive, fast-forward) relies on.

## 1. `StorageEngine::build_db_options(config: &Config) -> (Options, Vec<ColumnFamilyDescriptor>)`
Factor the option construction out of `open_with_mode` (engine.rs ~2529-2600: compression,
block cache, block size, bloom, prefix extractor `create_fixed_prefix(5)`, memtable/background
jobs, pipelined write, bytes-per-sync, WAL TTL/size when `replication.physical.enabled`).
`open_with_mode` calls it; so do `CheckpointOpen::open` and `ReopenedPhysicalStage::open`
(checkpoint_transfer.rs ~770-795, ~947-967) and `VerifiedCheckpoint`'s read-only preflight.
The descriptor list is `[default, __nano_local]` when the directory already has the local CF
(`DB::list_cf`), `[default]` otherwise; the local CF uses small, plain options (no prefix
extractor, no bloom, LZ4 or none). Opening with `open_cf_descriptors`.
Test: options parity — open the same temp dir through the engine and through the stage path and
compare `OPTIONS-*` content for the keys we set (or compare a serialised `Options` where the crate
allows; at minimum assert the prefix extractor and block size appear in `OPTIONS-*`).

## 2. `LocalWriter` and the local CF (`src/replication/physical.rs`)
```rust
pub const LOCAL_CF: &str = "__nano_local";
pub const CURSOR_KEY: &[u8] = b"cursor";          // now a key INSIDE the local CF
/// A batch that can only touch the local column family.
pub struct LocalBatch { inner: WriteBatch, ops: usize }
impl LocalBatch { pub fn put(&mut self, db: &DB, key: &[u8], value: &[u8]); pub fn delete(..); }
/// Every write a physical standby makes goes through here; it owns the cursor invariant
/// `db.latest_sequence_number() == cursor.local_sequence` after every write.
pub struct LocalWriter { db: Arc<DB>, cursor: parking_lot::Mutex<PhysicalCursor>, poisoned: AtomicBool }
impl LocalWriter {
    pub fn apply_source(&self, batch: &PhysicalBatch) -> Result<bool>;   // today's StagedPhysicalReplica::apply
    pub fn write_local(&self, batch: LocalBatch) -> Result<()>;          // cursor.local_sequence += ops + 1; sync
    pub fn cursor(&self) -> PhysicalCursor; pub fn db(&self) -> &Arc<DB>; pub fn is_poisoned(&self) -> bool;
}
pub struct StagedPhysicalReplica { writer: Arc<LocalWriter> }   // was { db: DB, cursor, poisoned }
```
- Sequence accounting is unchanged: a source batch consumes `last - first + 2` local sequence
  numbers (the `+1` is now `put_cf(local, CURSOR_KEY)`); an owned batch consumes `ops + 1`.
  `read_cursor` reads from the local CF and keeps the invariant check; a cursor found in the
  DEFAULT CF (a pre-P0.3 staging dir) is refused with `PhysicalError::FullResyncRequired`-style
  typed error "staging directory predates the node-local namespace; rebuild".
- `from_checkpoint(mut db: DB, …)`: `db.create_cf(LOCAL_CF, &local_cf_options())` BEFORE wrapping
  in `Arc` (rocksdb 0.22 `DB` is `SingleThreaded`: `create_cf` needs `&mut DB`). Then write the
  initial cursor via `put_cf`.
- `apply_source`: `WriteBatch::from_data(&batch.raw)` + `put_cf(local, CURSOR_KEY, cursor)` in ONE
  `write_opt(sync=true)`; poison on error exactly as today.
- `poll_source` (physical.rs ~217-224): "a receiver cannot be a source" check becomes
  `DB::list_cf(&opts, path)` contains `LOCAL_CF` (do it where the path is known) — keep the
  behaviour fail-closed.
- `validate_raw_batch` (~119-172): unchanged (CF opcodes are still rejected: a standby's own WAL
  records contain `put_cf` and must not be shipped — cascading is P3). Remove the cursor-overlap
  special case (~157) since the cursor no longer lives in the default CF.
- `checkpoint_transfer.rs`: file census (`valid_file_name`, ~162-174, ~903-919) needs no change
  (a CF adds no files) — add a test proving a staging dir with the CF passes the census.
  `ReopenedPhysicalStage::open` / `CheckpointOpen::open` open with the descriptors from §1 and
  hand the `Arc<DB>` to `LocalWriter`.
- `physical_receiver.rs`: adapt to `StagedPhysicalReplica { writer }`; `ReceiverStatus`/`run_receiver`
  behaviour unchanged. The `resync` CLI keeps working end-to-end (it is the bootstrap path of P1).

## 3. Engine admission
- `open_with_mode` for `Primary`/`ReadOnly`/`PhysicalSnapshot`: refuse when `DB::list_cf` shows
  `LOCAL_CF` (P0.2 already specified the refusal; implement it here if P0.2 left a TODO).
- `open_physical_standby(path, config, db, gate)` (P0.2) is constructed by the receiver: the
  `Arc<DB>` from `LocalWriter::db()`; `StorageEngine::write_local(&self, LocalBatch)` delegates to
  the `LocalWriter` stored in `OpenMode::PhysicalStandby` (add the `writer: Arc<LocalWriter>` field
  to that variant) and errors on any other mode.

## 4. Tests (proof-first)
`tests/physical_standby_local_namespace.rs` (real RocksDB, `tests/physical_checkpoint_tests.rs`
patterns):
1. **Byte identity.** Primary temp DB with a corpus; `OnlineCheckpointExport` → staged replica →
   apply N source batches from `poll_source`. `range_digest(db, DEFAULT, b"")` (SHA-256 over sorted
   `len(k)||k||len(v)||v`) of the ENTIRE default CF on the standby == the primary's at the same
   `latest_sequence_number`. Red today: the cursor key is in the default CF.
   Put `range_digest` in `src/replication/physical.rs` (pub, reused by P3/P4 and a future CLI).
2. **Owned writes keep the invariant.** `write_local` × 3, `apply_source`, `write_local`; after each:
   `latest_sequence_number == cursor.local_sequence`; close/reopen via `resume` succeeds; a raw
   `db.put` on the default CF between two steps → the next `apply_source` returns the poison error.
3. **Legacy staging refused.** A dir with the cursor in the default CF (build it with the old
   layout in the test) → typed refusal; nothing modified.
4. **Engine refuses the CF.** `StorageEngine::open` / `open_read_only` / `open_physical_snapshot` on a
   staged dir with the CF → error naming the standby start path.
5. **Options parity** (§1).
Existing suites that must stay green: `physical_checkpoint_tests`, `physical_snapshot_hydration`,
`physical_storage_hydration`, `replication_history_transport`, `deployment_cli_tests` (resync),
lib `replication::physical` unit tests (update the ones that assert the cursor in the default CF).

## Constraints
- No cargo/git. Read before edit. Match style; the physical modules are headed "EXTERNAL DRAFT":
  remove that header from the files you touch and replace it with a one-line ownership note.
- Keep every existing fail-closed check (history, gap, duplicate, digest, census). If a check
  becomes impossible to keep as-is, report it instead of weakening it.
- Report: files changed; the final cursor/sequence accounting rules in one paragraph; any test
  you had to change and why; commands to run.
