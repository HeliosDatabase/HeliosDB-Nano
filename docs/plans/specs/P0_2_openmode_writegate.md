# Spec P0.2 — `OpenMode` and the `WriteGate` funnel

Parent: `docs/plans/PHYSICAL_STANDBY_REPLICATION.md` §D3.3. Sprinter `e707d7b4dd28`. After P0.1.

Value: today the engine's open path is driven by two booleans (`read_only`, `physical_snapshot`)
and every component that holds an `Arc<DB>` can write to RocksDB on its own. A physical standby
must be a database where the ONLY writer is the replication applier (P0.3). This spec makes
"may this handle write?" a single decision taken at open and enforced at every raw write site,
with a CI tripwire so no new raw write can appear unnoticed.

## 1. `OpenMode`
In `src/storage/engine.rs` replace the `(read_only: bool, physical_snapshot: bool)` parameters of
`open_with_mode` (~2511) with
```rust
#[derive(Clone)]
pub(crate) enum OpenMode {
    /// Ordinary writable open: WAL recovery, workers, close-time persistence.
    Primary,
    /// `open_read_only`: RocksDB read-only handle, sees the state as of open.
    ReadOnly,
    /// `open_physical_snapshot`: read-only, strict hydration, no writes of any kind.
    PhysicalSnapshot,
    /// A serving physical standby: the handle is injected by the applier (P0.3), writable at the
    /// RocksDB level but the ENGINE never writes to it; `WriteGate` refuses every raw write.
    PhysicalStandby { db: Arc<DB>, gate: Arc<WriteGate> },
}
impl OpenMode {
    fn is_physical(&self) -> bool          // PhysicalSnapshot | PhysicalStandby
    fn rocksdb_read_only(&self) -> bool    // ReadOnly | PhysicalSnapshot
    fn engine_may_write(&self) -> bool     // Primary only
}
```
- `open`, `open_read_only`, `open_physical_snapshot` map to the first three; add
  `pub(crate) fn open_physical_standby(path, config, db: Arc<DB>, gate: Arc<WriteGate>)` that
  takes the injected handle instead of calling `DB::open*` (everything after the open — key
  check, snapshots, catalog hydration — runs as for `PhysicalSnapshot`, but on the writable
  handle). Keep `physical_snapshot_mode()` returning `mode.is_physical()` so the existing
  `rebuild_all_indexes_for_physical_snapshot` gate (catalog.rs ~561, lib.rs ~10570) covers both.
- Every `if read_only` / `if physical_snapshot` branch inside `open_with_mode` becomes a call on
  the mode. Table of required behaviour per mode (from the plan §D3.3, verify each against the
  code while editing):

| Step (engine.rs ref) | Primary | ReadOnly | PhysicalSnapshot | PhysicalStandby |
|---|---|---|---|---|
| marker file / cursor-key refusal (2518-2527, 2683-2690) | refuse | refuse | skip | skip |
| any mode but PhysicalStandby when `DB::list_cf` contains `__nano_local` | refuse (new) | refuse (new) | refuse (new) | required |
| TDE key check (2699-2703) | install | verify | `verify_physical_snapshot_key` | same as snapshot |
| snapshots (2716-2729) | recover + `configure_elision` | recover | `recover_snapshots_strict` + `configure_elision_read_only` | same as snapshot |
| branch registry (2743-2747) | `BranchManager::new` | `open_existing` | `open_existing` | `open_existing` |
| logical WAL (2770, 2973-2977) | on | off | off | off (`wal = None`) |
| change log (2855) | cfg | off | off | off |
| version GC worker (2875-2889, 4646-4660) | worker | none | watermark read only | watermark read only |
| `wire_index_snapshot_hooks` (2967) | on | on today → make it off | off | off |
| temp-table sweep (2993-3007) | sweep | `note_temp_table_present` | `note_temp_table_present` | `note_temp_table_present` |
| counter reseed (14149) | may write | memory-only | memory-only | memory-only |
| close: counter flush, index snapshots (2954-2955), WAL checkpoint (11149), memtable flush (11169) | on | off | off | off except memtable flush (allowed: a flush moves no sequence number) |

## 2. `WriteGate`
```rust
/// The one place that decides whether this process may write to the default column family.
/// Created at open, shared by every component that holds the raw `Arc<DB>`.
pub struct WriteGate { mode: GateMode /* atomic */ }
pub enum GateMode { Open, Closed { reason: &'static str } }
impl WriteGate {
    pub fn open() -> Arc<Self>; pub fn closed(reason: &'static str) -> Arc<Self>;
    pub fn check(&self) -> Result<()>;   // Err(Error::storage("write refused on a {reason}: …"))
    pub fn write(&self, db: &DB, batch: WriteBatch, opts: Option<&WriteOptions>) -> Result<()>;
    pub fn put(&self, db: &DB, k, v, opts: Option<&WriteOptions>) -> Result<()>;
    pub fn delete(&self, db: &DB, k, opts: Option<&WriteOptions>) -> Result<()>;
}
```
Put it in `src/storage/write_gate.rs`. `Primary` → `open()`; every other mode → `closed("read-only
open" | "physical snapshot" | "physical standby")`. The applier's own writes (P0.3) do NOT go
through the gate — they go through `LocalWriter`, which owns the cursor invariant.

### Funnel every raw write site through the gate
Current raw sites (re-grep before editing; the list below is from `grep -n 'db\.\(write\|put\|delete\)'`):
- `src/storage/engine.rs` (5 sites on `self.db`, plus any `put_opt`/`write_opt`/`delete_range`)
- `src/storage/wal.rs:943, 982, 1036, 1040, 1064-1065` (`WriteAheadLog` holds its own `Arc<DB>`)
- `src/storage/transaction.rs:1678, 1680, 1686`
- `src/storage/version_gc.rs:185` (+ the delete paths ~243, ~451)
- `src/storage/time_travel.rs:479` (elide sentinel; snapshot registration writes ~1622-1630)
- `src/vector/persistent.rs:865, 880, 938, 1134, 1220`
- `src/sync/delta_applicator.rs:197, 488`
- `src/storage/catalog.rs` (~1636, ~1709 and any `storage.db.*`), `src/storage/branch.rs`,
  `src/storage/index_snapshot.rs`, `src/storage/dictionary.rs`, `src/storage/mv_delta.rs`,
  `src/storage/copy_marker.rs`, `src/sql/sequences.rs` — grep `\.db\.\(write\|put\|delete\|write_opt\|put_opt\|delete_opt\|delete_range\)` across `src/` and cover every hit.
- NOT funnelled (by design, document in a comment): `src/replication/physical.rs` (the applier —
  will become `LocalWriter` in P0.3) and `checkpoint_transfer.rs` (operates on staging DBs).
Each component that holds `Arc<DB>` gets an `Arc<WriteGate>` at construction (constructor
parameter; the engine passes its gate). Replace `self.db.write(b)` with `self.gate.write(&self.db, b, None)` etc.
Where a component is only ever constructed on a primary (e.g. `VersionGcWorker`), still take the
gate — it is cheap and makes the tripwire uniform.

### Tripwire test C — no raw write outside the gate
`tests/write_gate_funnel.rs`: scan `src/**/*.rs` for `\.(write|write_opt|put|put_opt|delete|delete_opt|delete_range|delete_range_cf|merge)\(` on a receiver that is `db`, `self.db`, `storage.db`, `engine.db`, `self.storage.db` (regex on the token before the dot), excluding `src/storage/write_gate.rs`, `src/replication/physical.rs`, `src/replication/checkpoint_transfer.rs`, and `#[cfg(test)]`/`tests/` code (skip lines inside a `mod tests` block — a simple brace-depth scanner from the `#[cfg(test)]` line is enough). Any hit fails with file:line. Prove it fires by running it before funnelling (red), then green.

### Behavioural tests (red today)
`tests/physical_standby_open_mode.rs` (uses `open_physical_snapshot` until P0.3 lands, which
exercises the same gate):
1. Open a populated store with `open_physical_snapshot`; `latest_sequence_number()` before == after
   open, after `SELECT count(*)`, after `BEGIN; SELECT 1; COMMIT`, after `SELECT nextval('s')`
   (expect an error, not a write), and after drop/close. Today the read-only open still ATTEMPTS
   counter reseed/hook/close writes (they fail only because RocksDB is read-only) — the assertion
   that matters is on the gate: instrument `WriteGate` with a `refusals()` counter and assert
   refusals == 0 on a clean read path (nothing even tries), and == 1 for the `nextval` attempt.
2. The `__nano_local` CF refusal: create a temp DB with that CF (plain rocksdb in the test),
   assert `StorageEngine::open` and `open_read_only` refuse with a message naming the physical
   standby start path, and that `open_physical_standby` (P0.3) is the only admitted mode (for
   now assert the refusal only).

## 3. Read-side writes that must become memory-only (land here, used by P1)
- `SnapshotManager`: add `set_persistence(enabled: bool)`; when false, `register_snapshot`
  updates the in-memory map/counters but writes nothing (time_travel.rs ~1622-1630 and the GC
  at ~1638-1715 skip the delete batch). `PhysicalSnapshot`/`PhysicalStandby` set it false.
- `sql/sequences.rs`: `nextval/setval` go through the gate (their persistence write at
  `catalog.rs:4428` + `flush_wal(true)`); on a closed gate they return the gate's error. Remove
  `nextval`/`setval` from `guarded_standby_statement_allowed` (handler.rs ~7297-7300) so the wire
  returns SQLSTATE 25006 before reaching the engine; keep the gate as the backstop.

## Constraints
- No cargo/git. Read before edit. Match style. Keep the per-mode table's semantics exact — when
  the code disagrees with the table, STOP and report the discrepancy rather than guessing.
- Public API: `open`, `open_read_only`, `open_physical_snapshot` signatures unchanged.
- Report: files changed; the full list of funnelled write sites (file:line → gate call);
  discrepancies found vs the table; commands to run (`cargo test --test write_gate_funnel --test
  physical_standby_open_mode`, `cargo test --lib storage::`, plus `--test physical_snapshot_hydration
  --test physical_storage_hydration --test physical_checkpoint_tests`).
