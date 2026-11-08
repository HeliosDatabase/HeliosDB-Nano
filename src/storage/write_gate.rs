//! The one decision of whether this process may write to a database's RocksDB
//! store.
//!
//! Every storage component that holds the raw `Arc<DB>` — the engine, the
//! logical WAL, the snapshot manager, the branch manager, the version GC,
//! persistent vector indexes, and the stateless helpers the engine hands its
//! handle to — used to be able to write on its own authority. That is harmless
//! on a primary and fatal on a physical standby, whose default column family
//! must stay a byte-for-byte copy of the primary's: a single engine-initiated
//! write (a counter reseed, a snapshot registration, a lazy backfill) moves the
//! standby's sequence number and poisons the replica.
//!
//! So the decision is taken once, at open, from the engine's `OpenMode`, and
//! every raw write site asks this gate instead of calling
//! `DB::write`/`put`/`delete` itself. `tests/write_gate_funnel.rs`
//! fails, naming `file:line`, if a raw write appears anywhere in `src/` outside
//! this module and the two physical-replication modules that own a standby's
//! applier (`replication::physical`, `replication::checkpoint_transfer`; their
//! writes go through the `LocalWriter`, which owns the cursor invariant).
//!
//! # Fencing (spec P5.1 §3.2)
//!
//! The mode is fixed for the gate's lifetime, with ONE transition, made here:
//! a primary that a promoted standby fenced ([`WriteGate::fence`]). From that
//! moment every write is refused with the reason the fence gave — the primary
//! keeps serving reads — and the fence waits for the writes that were already
//! past the gate ([`WriteGate::wait_for_writes`]) before it reads its final
//! sequence, so no write lands after the sequence it reports. The two are one
//! protocol on two atomics: a writer announces itself (`in_flight + 1`) BEFORE
//! it looks at the fence, the fencer raises the fence BEFORE it looks at
//! `in_flight`, both sequentially consistent — so either the writer sees the
//! fence and is refused, or the fencer sees the writer and waits for it.
//! There is no way back: a fenced primary is rebuilt as a standby.
//!
//! # The branch fork point (sprinter 6bc4a4040eb7, phase B4)
//!
//! Every write of a store passes here, so this is where a parent's write
//! freezes what its live child branches see (`storage::branch_freeze`). The
//! cost when no branch exists is the one sequentially consistent load of
//! `freeze_active` in [`WriteGate::write`], [`WriteGate::put`] and
//! [`WriteGate::delete`], after the writer announced itself: the creation of a
//! first branch raises the flag and then waits for what is announced, the same
//! protocol as the fence, so either the writer sees the flag or the creator
//! waits for the writer. Past the flag the one function is
//! `WriteGate::write_frozen`. A standby's gate never has a hook: nothing
//! installs one on a store that does not accept writes.
//!
//! # One write group per schema statement (spec R5.3)
//!
//! A schema statement writes its whole change with one call here, through its
//! `storage::DdlBatch`; while the batch is open, any other write of this gate on
//! the statement's thread is a finding of the detector in `storage::ddl_batch`
//! (`check_gate_write`). And every group, whoever builds it, is held to the one
//! ceiling a standby can receive ([`MAX_WRITE_GROUP_BYTES`], decision 6), after the
//! freeze hook added its copies: refused 54000 before RocksDB sees it.
//!
//! # Layout epochs (spec R5.6)
//!
//! Every write here is, first, the running statement's last validation of its read
//! set before its first durable effect (`storage::layout_epochs::at_durable_effect`):
//! a statement that read a table whose layout moved since is refused before RocksDB
//! sees anything. Then a write carrying a layout key (`meta:table:`, a missing value,
//! an index definition, ...; `layout_epochs::LAYOUT_PREFIXES`) marks each table it
//! names in flight, is applied, has the engine drop what readers cache of those
//! tables' layouts, and ends the change. Any writer, in a statement batch or not.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rocksdb::{WriteBatch, WriteBatchIterator, WriteOptions, DB};

use super::branch_freeze::BranchFreeze;
use super::write_outcome::WriteAttempt;

#[cfg(any(test, feature = "internal-tests"))]
thread_local! {
    static FAIL_NEXT_NATIVE_WRITE_AFTER_APPLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_NEXT_NATIVE_WRITE_BEFORE_APPLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
use super::layout_epochs::{LayoutChange, LayoutEpochs, LayoutHook, LayoutKey};
use crate::{Error, Result};

/// Spec R5 decision 6: the most one write group may hold and still be shipped to a
/// standby and archived, since a group is never split
/// (`replication::physical::MAX_BATCH_BYTES`; the same limit in a build without the
/// replication modules, whose store may be served by one that has them).
#[cfg(feature = "ha-tier1")]
pub const MAX_WRITE_GROUP_BYTES: usize = crate::replication::physical::MAX_BATCH_BYTES;
/// See the `ha-tier1` definition.
#[cfg(not(feature = "ha-tier1"))]
pub const MAX_WRITE_GROUP_BYTES: usize = u32::MAX as usize - 4096;

/// Whether a [`WriteGate`] admits writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Every write is admitted: a primary, or a handle its caller owns outright.
    Open,
    /// Every write is refused. `reason` names the open mode in the error, e.g.
    /// "read-only open", "physical snapshot", "physical standby".
    Closed {
        /// The open mode that closed the gate.
        reason: &'static str,
    },
}

pub type RowCounterValues = dashmap::DashMap<String, AtomicU64>;

/// The one place that decides whether this process may write to the default
/// column family. Created at open and shared, as an `Arc`, by every component
/// that holds the raw `Arc<DB>`.
///
/// The mode is fixed for the gate's lifetime: a database's open mode does not
/// change while it is open. The one transition is the fence of a superseded
/// primary (spec P5.1, module documentation), made here, in one place.
#[derive(Debug)]
pub struct WriteGate {
    mode: GateMode,
    /// Writes refused so far. A diagnostic and a test surface: a read path that
    /// is genuinely write-free leaves it at zero because nothing even tried.
    refusals: AtomicU64,
    /// Actual reconciliation failure retires this table's trees. Until reopen
    /// rebuilds them, later writes must not bypass their constraints. Ordinary
    /// writes pay one false load, without a map lock or batch walk.
    has_unavailable_tables: AtomicBool,
    unavailable_tables: parking_lot::RwLock<std::collections::HashMap<String, String>>,
    /// Spec P5.1: raised once, never lowered, by [`Self::fence`].
    fenced: AtomicBool,
    /// The refusal a fenced gate answers; set before `fenced` is raised.
    fence_reason: OnceLock<String>,
    /// Writes past the gate whose RocksDB call has not returned yet. A write
    /// that goes through the freeze hook leaves this count once it holds the
    /// hook's registry ([`Self::write_frozen`]), which then answers for it.
    in_flight: AtomicU64,
    /// Spec R5.1: whether this gate's writes answer to the writer-lock detector
    /// (`storage::table_locks`): set when an `EmbeddedDatabase`'s open is done, so
    /// the open's own writes, and a component driven on a gate of its own, do not.
    row_lock_detector: AtomicBool,
    /// Sprinter 6bc4a4040eb7 (B4): some level has a live child branch. Shared
    /// with the hook, which alone changes it.
    freeze_active: Arc<AtomicBool>,
    /// The hook of the engine that owns this gate, set once at open, on a store
    /// that accepts writes.
    freeze: OnceLock<Arc<BranchFreeze>>,
    /// How many writes went through the hook. A test surface: this is what
    /// proves the path of a store with no branch never reaches it.
    freeze_runs: AtomicU64,
    /// Spec R5 decision 6: the largest write group this gate applies
    /// ([`MAX_WRITE_GROUP_BYTES`]; lowered only by a test).
    ceiling: AtomicUsize,
    /// All writers of one row counter stage and write under the same lock.
    /// Locks are sorted by table for multi-table transaction commits.
    counter_locks: dashmap::DashMap<String, Arc<parking_lot::Mutex<()>>>,
    /// The engine's allocation high-water map, shared with its transactions.
    counter_values: OnceLock<Arc<RowCounterValues>>,
    /// Spec R5.6: the layout epochs of the engine that owns this gate, and what to
    /// ask it about a batch that changes table layouts. Set once, at open, on a store
    /// that accepts writes.
    layout: OnceLock<LayoutWiring>,
    /// Shared by every DeltaTracker on this gate. Allocation never holds a lock
    /// across a write, and failed writes may leave harmless gaps.
    delta_id: AtomicU64,
    delta_ids_installed: AtomicBool,
    /// Acquired last, inside apply, after B4's registry/row work. Only writers
    /// touching the same allocator lane serialize; unrelated row groups do not.
    delta_floor_locks: [parking_lot::Mutex<()>; super::mv_delta::DELTA_ID_LANES],
}

/// Spec R5.6: [`WriteGate::install_layout`]'s two halves.
struct LayoutWiring {
    epochs: Arc<LayoutEpochs>,
    hook: Arc<dyn LayoutHook>,
}

impl std::fmt::Debug for LayoutWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutWiring").finish_non_exhaustive()
    }
}

/// The layout keys of a batch the raw walker could not read, read the copying way.
struct LayoutKeyCollector(Vec<(Box<[u8]>, Option<Box<[u8]>>)>);

impl WriteBatchIterator for LayoutKeyCollector {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        if super::layout_epochs::layout_key(&key).is_some() {
            self.0.push((key, Some(value)));
        }
    }

    fn delete(&mut self, key: Box<[u8]>) {
        if super::layout_epochs::layout_key(&key).is_some() {
            self.0.push((key, None));
        }
    }
}

/// Collected in the gate's existing borrowed layout-key pass. No row/value
/// copies, heap allocation or delta locks for an ordinary no-delta batch.
struct DeltaFloors {
    maxima: [u64; super::mv_delta::DELTA_ID_LANES],
    touched: u64,
}

impl Default for DeltaFloors {
    fn default() -> Self {
        Self {
            maxima: [0; super::mv_delta::DELTA_ID_LANES],
            touched: 0,
        }
    }
}

impl DeltaFloors {
    fn id(&mut self, id: u64) -> Result<()> {
        let lane = (id % super::mv_delta::DELTA_ID_LANES as u64) as usize;
        self.lane(lane, id)
    }

    fn lane(&mut self, lane: usize, id: u64) -> Result<()> {
        let maximum = self
            .maxima
            .get_mut(lane)
            .ok_or_else(|| Error::storage("Invalid delta allocator lane"))?;
        *maximum = (*maximum).max(id);
        self.touched |= 1u64 << lane;
        Ok(())
    }

    fn key(&mut self, db: &DB, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        use super::mv_delta::*;
        if key.starts_with(b"delta:") {
            self.id(delta_key_id(key).ok_or_else(|| Error::storage("Invalid delta record key"))?)?;
        } else if key.starts_with(DELTA_FLOOR_PREFIX) {
            let lane = delta_floor_lane(key).ok_or_else(|| Error::storage("Invalid delta allocator floor key"))?;
            let id = value.map(decode_delta_id).transpose()?.unwrap_or(0);
            self.lane(lane, id)?;
        } else if key == LEGACY_DELTA_ID_KEY {
            // Preserve even a legacy marker's deletion. A regressing late put
            // remains compatible input, but cannot lower a permanent floor.
            if let Some(value) = value {
                self.id(decode_delta_id(value)?)?;
            }
            if let Some(old) = db.get(key)? {
                self.id(decode_delta_id(&old)?)?;
            }
        }
        Ok(())
    }

    fn range(&mut self, db: &DB, start: &[u8], end: &[u8]) -> Result<()> {
        use super::mv_delta::*;
        // Only cold range cleanup scans retained IDs. Clip both scans to their
        // own prefixes rather than walking unrelated row payloads.
        for prefix in [b"delta:".as_slice(), DELTA_FLOOR_PREFIX] {
            let mut upper = prefix.to_vec();
            if let Some(last) = upper.last_mut() {
                *last += 1;
            }
            let lower = start.max(prefix);
            let upper = end.min(upper.as_slice());
            if lower >= upper {
                continue;
            }
            if prefix == DELTA_FLOOR_PREFIX {
                // A concurrent producer may create a previously absent lane
                // after this scan. Lock and republish every lane under the
                // range tombstone, so no such durable evidence is erased.
                self.touched = u64::MAX;
            }
            let mut options = rocksdb::ReadOptions::default();
            options.set_total_order_seek(true);
            options.set_iterate_upper_bound(upper.to_vec());
            let mut iter = db.raw_iterator_opt(options);
            iter.seek(lower);
            while iter.valid() {
                let key = iter.key().ok_or_else(|| Error::storage("Missing delta cleanup key"))?;
                if key >= upper {
                    break;
                }
                self.key(
                    db,
                    key,
                    if prefix == DELTA_FLOOR_PREFIX {
                        iter.value()
                    } else {
                        None
                    },
                )?;
                iter.next();
            }
            iter.status()?;
        }
        if start <= LEGACY_DELTA_ID_KEY && LEGACY_DELTA_ID_KEY < end {
            self.key(db, LEGACY_DELTA_ID_KEY, None)?;
        }
        Ok(())
    }

    fn inspect(&mut self, db: &DB, operation: super::layout_epochs::BatchKey<'_>) -> Result<()> {
        use super::layout_epochs::BatchKey;
        match operation {
            BatchKey::Put(key, value) => self.key(db, key, Some(value)),
            BatchKey::Delete(key) => self.key(db, key, None),
            BatchKey::Range(start, end) => self.range(db, start, end),
            BatchKey::Merge(key) if key.starts_with(b"delta:") || key.starts_with(b"meta:delta:") => Err(
                Error::storage("MV delta allocator records do not support merge operands"),
            ),
            BatchKey::Merge(_) => Ok(()),
        }
    }
}

/// A write past the gate, until its RocksDB call returned.
struct InFlight<'a>(&'a AtomicU64);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WriteGate {
    pub(crate) fn quarantine_table(&self, table: &str, reason: &str) {
        self.unavailable_tables
            .write()
            .insert(table.to_string(), reason.to_string());
        self.has_unavailable_tables.store(true, Ordering::Release);
    }

    #[inline]
    pub(crate) fn has_quarantined_tables(&self) -> bool {
        self.has_unavailable_tables.load(Ordering::Acquire)
    }

    pub(crate) fn check_table_available(&self, table: &str) -> Result<()> {
        if !self.has_unavailable_tables.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(reason) = self.unavailable_tables.read().get(table) {
            return Err(Error::storage(format!(
                "writes to table '{table}' are unavailable after index reconciliation failed ({reason}); reopen to rebuild its indexes"
            )));
        }
        Ok(())
    }

    fn check_available_key(&self, key: &[u8]) -> Result<()> {
        if !self.has_unavailable_tables.load(Ordering::Acquire) {
            return Ok(());
        }
        let unavailable = self.unavailable_tables.read();
        for (table, reason) in unavailable.iter() {
            if ["data:", "dict:", "col:", "colz:", "colp:"]
                .iter()
                .any(|prefix| key.starts_with(format!("{prefix}{table}:").as_bytes()))
            {
                return Err(Error::storage(format!(
                    "writes to table '{table}' are unavailable after index reconciliation failed ({reason}); reopen to rebuild its indexes"
                )));
            }
        }
        Ok(())
    }

    fn check_available_batch(&self, batch: &WriteBatch) -> Result<()> {
        if !self.has_unavailable_tables.load(Ordering::Acquire) {
            return Ok(());
        }
        // The C put/delete iterator omits range deletions. Decode this rare
        // failure-mode check from the pinned RocksDB record format, including
        // explicit column families, so a range cannot bypass the quarantine.
        fn varint(input: &mut &[u8]) -> Option<u32> {
            let mut value = 0;
            for shift in (0..35).step_by(7) {
                let (&byte, rest) = input.split_first()?;
                *input = rest;
                value |= u32::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Some(value);
                }
            }
            None
        }
        fn bytes<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
            let length = varint(input)? as usize;
            let (value, rest) = (*input).split_at_checked(length)?;
            *input = rest;
            Some(value)
        }
        let invalid = || Error::storage("cannot validate write batch while a table's indexes are unavailable");
        let mut input = batch.data().get(12..).ok_or_else(invalid)?;
        while let Some((&tag, rest)) = input.split_first() {
            input = rest;
            let (family, value, range) = match tag {
                0 | 7 => (false, false, false),
                1 | 2 => (false, true, false),
                4 | 8 => (true, false, false),
                5 | 6 => (true, true, false),
                15 => (false, false, true),
                14 => (true, false, true),
                3 => {
                    bytes(&mut input).ok_or_else(invalid)?;
                    continue;
                }
                _ => return Err(invalid()),
            };
            let family = if family {
                varint(&mut input).ok_or_else(invalid)?
            } else {
                0
            };
            let key = bytes(&mut input).ok_or_else(invalid)?;
            let second = if value || range {
                Some(bytes(&mut input).ok_or_else(invalid)?)
            } else {
                None
            };
            if family != 0 {
                continue;
            }
            if range {
                let end = second.ok_or_else(invalid)?;
                for (table, reason) in self.unavailable_tables.read().iter() {
                    for prefix in ["data:", "dict:", "col:", "colz:", "colp:"] {
                        let start = format!("{prefix}{table}:").into_bytes();
                        let mut limit = start.clone();
                        if let Some(last) = limit.last_mut() {
                            *last += 1;
                        }
                        if key < limit.as_slice() && end > start.as_slice() {
                            return Err(Error::storage(format!(
                                "writes to table '{table}' are unavailable after index reconciliation failed ({reason}); reopen to rebuild its indexes"
                            )));
                        }
                    }
                }
            } else {
                self.check_available_key(key)?;
            }
        }
        Ok(())
    }

    fn with_mode(mode: GateMode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            refusals: AtomicU64::new(0),
            has_unavailable_tables: AtomicBool::new(false),
            unavailable_tables: parking_lot::RwLock::new(std::collections::HashMap::new()),
            fenced: AtomicBool::new(false),
            fence_reason: OnceLock::new(),
            in_flight: AtomicU64::new(0),
            row_lock_detector: AtomicBool::new(false),
            freeze_active: Arc::new(AtomicBool::new(false)),
            freeze: OnceLock::new(),
            freeze_runs: AtomicU64::new(0),
            ceiling: AtomicUsize::new(MAX_WRITE_GROUP_BYTES),
            layout: OnceLock::new(),
            counter_locks: dashmap::DashMap::new(),
            counter_values: OnceLock::new(),
            delta_id: AtomicU64::new(0),
            delta_ids_installed: AtomicBool::new(false),
            delta_floor_locks: std::array::from_fn(|_| parking_lot::Mutex::new(())),
        })
    }

    pub(crate) fn install_delta_ids(&self, recovered: u64) {
        self.observe_delta_id(recovered);
        self.delta_ids_installed.store(true, Ordering::Release);
    }

    pub(crate) fn observe_delta_id(&self, id: u64) {
        self.delta_id.fetch_max(id, Ordering::AcqRel);
    }

    pub(crate) fn current_delta_id(&self) -> u64 {
        self.delta_id.load(Ordering::Acquire)
    }

    pub(crate) fn next_delta_id(&self) -> Result<u64> {
        self.delta_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map(|previous| previous + 1)
            .map_err(|_| Error::storage("MV delta ID space is exhausted"))
    }

    /// A gate that admits every write.
    pub fn open() -> Arc<Self> {
        Self::with_mode(GateMode::Open)
    }

    /// A gate that refuses every write, naming `reason` in the error.
    pub fn closed(reason: &'static str) -> Arc<Self> {
        Self::with_mode(GateMode::Closed { reason })
    }

    /// Installed once while the owning engine is constructed, before writes.
    pub(crate) fn install_counter_values(&self, values: Arc<RowCounterValues>) {
        let _ = self.counter_values.set(values);
    }

    pub(crate) fn counter_lock(&self, table: &str) -> Arc<parking_lot::Mutex<()>> {
        self.counter_locks
            .entry(table.to_owned())
            .or_insert_with(|| Arc::new(parking_lot::Mutex::new(())))
            .clone()
    }

    /// Call under the counter lock, before staging its bytes and logical entry.
    /// Raising allocation state on a failed write only leaves harmless ID gaps.
    /// A standalone Transaction without an engine map reads its durable counter.
    pub(crate) fn raise_counter_high_water(&self, table: &str, minimum: u64) -> Option<u64> {
        self.counter_values.get().map(|values| {
            let value = values.entry(table.to_owned()).or_insert_with(|| AtomicU64::new(0));
            value.fetch_max(minimum, Ordering::AcqRel).max(minimum)
        })
    }

    /// This gate's mode (as opened; [`Self::is_fenced`] says whether a fence
    /// closed it since).
    pub fn mode(&self) -> GateMode {
        self.mode
    }

    /// Whether writes are admitted. Does not count as a refusal: callers whose
    /// write is an optional optimisation (a lazy backfill, a cache marker) ask
    /// this first and skip the write, so that only a write somebody actually
    /// needed shows up in [`Self::refusals`].
    pub fn is_open(&self) -> bool {
        matches!(self.mode, GateMode::Open) && !self.is_fenced()
    }

    /// How many writes this gate has refused.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    /// Spec P5.1: whether a promoted standby fenced this primary.
    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    /// The refusal a fenced gate answers every write with.
    pub fn fence_reason(&self) -> Option<&str> {
        if self.is_fenced() {
            self.fence_reason.get().map(String::as_str)
        } else {
            None
        }
    }

    /// Spec P5.1 §3.2 step 1: refuse every write from now on with `reason`
    /// (which must start with [`crate::error::PRIMARY_FENCED_WRITE_REFUSED`]
    /// so the PostgreSQL wire answers it 25006). Returns whether this call
    /// raised the fence; a second fence keeps the first one's reason. Writes
    /// already past the gate are not interrupted: [`Self::wait_for_writes`]
    /// waits for them.
    pub fn fence(&self, reason: impl Into<String>) -> bool {
        let _ = self.fence_reason.set(reason.into());
        !self.fenced.swap(true, Ordering::SeqCst)
    }

    /// Wait until no write that passed the gate before the fence is still in
    /// its RocksDB call, for at most `timeout`. True when none is; false when
    /// the time ran out (a write blocked on a stalled disk) — the caller then
    /// reports its final sequence as uncertain instead of guessing.
    pub fn wait_for_writes(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        if !self.wait_for_announced(deadline) {
            return false;
        }
        // A write that went through the hook left the count when it took the
        // hook's registry, and holds it until its RocksDB call returned.
        match self.freeze.get() {
            Some(freeze) => freeze.wait_for_writers(deadline.saturating_duration_since(Instant::now())),
            None => true,
        }
    }

    fn wait_for_announced(&self, deadline: Instant) -> bool {
        loop {
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Sprinter 6bc4a4040eb7 (B4): the flag `BranchFreeze` raises while some
    /// level has a live child.
    pub(crate) fn freeze_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.freeze_active)
    }

    /// Install the engine's hook. Once, at open, before the store takes writes.
    pub(crate) fn install_freeze(&self, freeze: Arc<BranchFreeze>) {
        let _ = self.freeze.set(freeze);
    }

    /// Spec R5.6: install the engine's layout epochs and hook. Once, at open, before
    /// the store takes writes.
    pub fn install_layout(&self, epochs: Arc<LayoutEpochs>, hook: Arc<dyn LayoutHook>) {
        let _ = self.layout.set(LayoutWiring { epochs, hook });
    }

    /// Spec R5.6: the tables a layout key names, into `tables`.
    fn layout_tables_of(wiring: &LayoutWiring, key: LayoutKey<'_>, value: Option<&[u8]>, tables: &mut Vec<String>) {
        match key {
            LayoutKey::Tables(table, other) => {
                tables.push(table.to_string());
                if let Some(other) = other {
                    tables.push(other.to_string());
                }
            }
            LayoutKey::Index(name) => {
                if let Some(table) = wiring.hook.index_table(name, value) {
                    tables.push(table);
                }
            }
        }
    }

    /// Spec R5.6: mark the tables whose layout `batch` changes in flight, before
    /// RocksDB sees it. No change for a batch with no layout key: a walk of the
    /// batch's bytes in place, one or two byte comparisons per key.
    fn begin_batch_change(&self, db: &DB, batch: &WriteBatch) -> Result<(LayoutChange, DeltaFloors)> {
        use super::layout_epochs::BatchKey;
        let wiring = self.layout.get();
        let delta_ids = self.delta_ids_installed.load(Ordering::Acquire);
        let mut floors = DeltaFloors::default();
        if wiring.is_none() && !delta_ids {
            return Ok((LayoutChange::none(), floors));
        }
        let mut tables: Vec<String> = Vec::new();
        let mut inspected = Ok(());
        // One borrowed key pass, shared with the layout detector already on the
        // hot write path. Values are skipped in place; only metadata is decoded.
        let walked = super::layout_epochs::walk_batch_keys(batch.data(), |operation| {
            if delta_ids && inspected.is_ok() {
                inspected = floors.inspect(db, operation);
            }
            if let Some(wiring) = wiring {
                let (key, value) = match operation {
                    BatchKey::Put(key, value) => (key, Some(value)),
                    BatchKey::Delete(key) | BatchKey::Merge(key) | BatchKey::Range(key, _) => (key, None),
                };
                if let Some(found) = super::layout_epochs::layout_key(key) {
                    Self::layout_tables_of(wiring, found, value, &mut tables);
                }
            }
        });
        inspected?;
        if !walked {
            if delta_ids {
                return Err(Error::storage(
                    "Cannot inspect MV delta allocator operations in this write batch",
                ));
            }
            tables.clear();
            let mut collector = LayoutKeyCollector(Vec::new());
            batch.iterate(&mut collector);
            if let Some(wiring) = wiring {
                for (key, value) in &collector.0 {
                    if let Some(found) = super::layout_epochs::layout_key(key) {
                        Self::layout_tables_of(wiring, found, value.as_deref(), &mut tables);
                    }
                }
            }
        }
        let change = match wiring {
            Some(wiring) if !tables.is_empty() => wiring.epochs.begin_change(tables.iter().map(String::as_str)),
            _ => LayoutChange::none(),
        };
        Ok((change, floors))
    }

    /// Spec R5.6: [`Self::begin_batch_change`] for one key.
    fn begin_layout_change_of_key(&self, key: &[u8], value: Option<&[u8]>) -> LayoutChange {
        let Some(wiring) = self.layout.get() else {
            return LayoutChange::none();
        };
        let Some(found) = super::layout_epochs::layout_key(key) else {
            return LayoutChange::none();
        };
        let mut tables = Vec::new();
        Self::layout_tables_of(wiring, found, value, &mut tables);
        wiring.epochs.begin_change(tables.iter().map(String::as_str))
    }

    /// Spec R5.6: the write is done (or failed: what readers cache is dropped either
    /// way, which costs a reload): drop what readers cache of the changed tables'
    /// layouts, then end the change, so a reader that loads an epoch after it finds no
    /// cache from before it. Inside a schema statement's publication scope
    /// (`layout_epochs::PublicationScope`, `DdlBatch::commit`) the change ends when its
    /// publication is done instead.
    fn end_layout_change(&self, change: LayoutChange) {
        if change.is_empty() {
            return;
        }
        if let Some(wiring) = self.layout.get() {
            wiring.hook.publish(&change.tables());
        }
        super::layout_epochs::end_or_defer(change);
    }

    /// The hook, while some level has a live child. One atomic load otherwise.
    pub(crate) fn active_freeze(&self) -> Option<&Arc<BranchFreeze>> {
        if self.freeze_active.load(Ordering::SeqCst) {
            self.freeze.get()
        } else {
            None
        }
    }

    /// For the creation of a branch: a write announced before `freeze_active`
    /// was raised may not have seen it. Wait for those, until `deadline`.
    ///
    /// A write that does see the flag and has row keys leaves the count as soon
    /// as it holds the hook's registry, and one with no row keys cannot change
    /// what a child sees. So the count is only ever held for an instant under
    /// load, and a sustained stream of writers cannot starve this.
    pub(crate) fn wait_for_unhooked_writes(&self, deadline: Instant) -> bool {
        self.wait_for_announced(deadline)
    }

    /// Sprinter 6bc4a4040eb7 (B4): apply copies the hook staged itself, outside a
    /// write of the gate (`StorageEngine::freeze_row_before_side_write`). A copy
    /// changes no level's view, so it goes through nothing further.
    pub(crate) fn write_without_freeze(&self, db: &DB, batch: WriteBatch, opts: Option<&WriteOptions>) -> Result<()> {
        let slot = self.admit()?;
        let applied = self.apply(db, batch, opts, DeltaFloors::default(), None);
        drop(slot);
        applied
    }

    /// How many writes went through the freeze hook. For the tests.
    #[doc(hidden)]
    pub fn debug_freeze_runs(&self) -> u64 {
        self.freeze_runs.load(Ordering::Relaxed)
    }

    fn apply(
        &self,
        db: &DB,
        mut batch: WriteBatch,
        opts: Option<&WriteOptions>,
        floors: DeltaFloors,
        attempt: Option<&WriteAttempt>,
    ) -> Result<()> {
        // This is after the B4 registry and freeze-row locks. Nothing takes a
        // dictionary/stats/table lock while holding these sorted lane locks.
        let mut guards = Vec::new();
        if floors.touched != 0 {
            for (lane, lock) in self.delta_floor_locks.iter().enumerate() {
                if floors.touched & (1u64 << lane) == 0 {
                    continue;
                }
                guards.push(lock.lock());
            }
            for (lane, maximum) in floors.maxima.iter().enumerate() {
                if floors.touched & (1u64 << lane) == 0 {
                    continue;
                }
                let key = super::mv_delta::delta_floor_key(lane);
                let durable = db
                    .get(key.as_bytes())?
                    .as_deref()
                    .map(super::mv_delta::decode_delta_id)
                    .transpose()?
                    .unwrap_or(0);
                let floor = durable.max(*maximum);
                self.observe_delta_id(floor);
                // Appending overrides a stale put/delete/range in the original
                // group without cloning any payload. Floor evidence is permanent.
                batch.put(key.as_bytes(), floor.to_le_bytes());
            }
        }
        // Include normalized metadata as well as B4 copies in the physical cap.
        self.refuse_oversized(&batch)?;
        if let Some(attempt) = attempt {
            attempt.native_started();
        }
        #[cfg(any(test, feature = "internal-tests"))]
        if FAIL_NEXT_NATIVE_WRITE_BEFORE_APPLY.with(|fail| fail.replace(false)) {
            return Err(Error::storage(
                "injected native write error before apply; outcome is unknown",
            ));
        }
        let applied = match opts {
            Some(opts) => db.write_opt(batch, opts),
            None => db.write(batch),
        }
        .map_err(Error::from);
        #[cfg(any(test, feature = "internal-tests"))]
        let applied = if applied.is_ok() && FAIL_NEXT_NATIVE_WRITE_AFTER_APPLY.with(|fail| fail.replace(false)) {
            Err(Error::storage(
                "injected native write error after apply; outcome is unknown",
            ))
        } else {
            applied
        };
        if applied.is_ok() {
            if let Some(attempt) = attempt {
                attempt.native_succeeded();
            }
        }
        drop(guards);
        applied
    }

    /// Simulate an ambiguous native error after applying the observed owning group.
    #[cfg(any(test, feature = "internal-tests"))]
    #[doc(hidden)]
    pub fn debug_fail_next_native_write_after_apply(&self) {
        FAIL_NEXT_NATIVE_WRITE_AFTER_APPLY.with(|fail| fail.set(true));
    }

    /// Simulate a native error that did not apply the owning group. An observer
    /// has already entered Unknown; this also exercises unobserved owners.
    #[cfg(any(test, feature = "internal-tests"))]
    #[doc(hidden)]
    pub fn debug_fail_next_native_write_before_apply(&self) {
        FAIL_NEXT_NATIVE_WRITE_BEFORE_APPLY.with(|fail| fail.set(true));
    }

    /// Spec R5 decision 6 (R5.3): the one ceiling of a write group. A group above
    /// [`MAX_WRITE_GROUP_BYTES`] can be neither served to a
    /// standby nor archived (`GroupTooLarge` at every poll, for good), so it is
    /// refused here, before RocksDB sees it: 54000, nothing written. Here, because
    /// this is the one place that sees B4's frozen copies, the logical-WAL entries a
    /// commit seals in last, and every writer that builds its own batch.
    pub(crate) fn refuse_oversized(&self, batch: &WriteBatch) -> Result<()> {
        self.refuse_encoded_group_bytes(batch.size_in_bytes())
    }

    #[inline]
    fn refuse_encoded_group_bytes(&self, bytes: usize) -> Result<()> {
        let limit = self.ceiling.load(Ordering::Relaxed);
        if bytes > limit {
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return Err(Error::write_group_too_large(bytes, limit));
        }
        Ok(())
    }

    /// Exact CF0 point-operation encoding in the pinned RocksDB 8.10 native
    /// WriteBatch: 12-byte header, one-byte operation tag, then varint32-prefixed
    /// key and (for PUT) value. See write_batch_internal.h::kHeader and
    /// write_batch.cc::WriteBatchInternal::{Put,Delete}. Other CFs/operations
    /// and B4 augmentation always use the actual batch through `apply`.
    /// This avoids building/copying a second batch around DB::put/delete.
    #[inline]
    fn refuse_point_group(&self, key_len: usize, value_len: Option<usize>) -> Result<()> {
        #[inline]
        fn prefixed_bytes(bytes: usize) -> usize {
            let mut remaining = bytes;
            let mut prefix = 1usize;
            while remaining >= 128 {
                remaining >>= 7;
                prefix += 1;
            }
            bytes.saturating_add(prefix)
        }
        let bytes = 12usize
            .saturating_add(1)
            .saturating_add(prefixed_bytes(key_len))
            .saturating_add(value_len.map(prefixed_bytes).unwrap_or(0));
        self.refuse_encoded_group_bytes(bytes)
    }

    /// The ceiling [`Self::refuse_oversized`] applies now: [`MAX_WRITE_GROUP_BYTES`],
    /// unless a test lowered it. Spec R5.4 (review F13): a transaction staging a group
    /// refuses it as soon as what it staged passes this (`Transaction::put`), instead
    /// of building the whole batch to be refused here.
    pub fn write_group_ceiling(&self) -> usize {
        self.ceiling.load(Ordering::Relaxed)
    }

    /// Tests only: lower the ceiling of [`Self::refuse_oversized`] (it is
    /// [`MAX_WRITE_GROUP_BYTES`], about 4 GiB, otherwise), so a test can show a refusal
    /// without building a 4 GiB batch.
    #[doc(hidden)]
    pub fn debug_set_write_group_ceiling(&self, bytes: Option<usize>) {
        self.ceiling
            .store(bytes.unwrap_or(MAX_WRITE_GROUP_BYTES), Ordering::Relaxed);
    }

    /// Sprinter 6bc4a4040eb7 (B4): THE write funnel while some level has a live
    /// child branch; see `storage::branch_freeze`. `slot` is this write's
    /// announcement.
    ///
    /// It runs on the writer's own thread. Past `stage` the batch holds what the
    /// hook adds to it, and it holds the table locks or row stripes of
    /// `BranchFreeze`. Only the private, sorted delta-floor lane locks may be
    /// taken below them before RocksDB; they never acquire storage/table locks
    /// and are released before publication or replication waits.
    fn write_frozen(
        &self,
        db: &DB,
        mut batch: WriteBatch,
        opts: Option<&WriteOptions>,
        slot: InFlight<'_>,
        floors: DeltaFloors,
        attempt: Option<&WriteAttempt>,
    ) -> Result<()> {
        self.freeze_runs.fetch_add(1, Ordering::Relaxed);
        let Some(freeze) = self.freeze.get() else {
            let applied = self.apply(db, batch, opts, floors, attempt);
            drop(slot);
            return applied;
        };
        let touches = BranchFreeze::row_touches(&batch);
        if touches.is_empty() {
            let applied = self.apply(db, batch, opts, floors, attempt);
            drop(slot);
            return applied;
        }
        let registry = freeze.registry_for_write();
        // The registry answers for this write from here: holding it is what a
        // new branch waits for, and a fence too.
        drop(slot);
        let mut flush = |chunk: WriteBatch| self.apply(db, chunk, opts, DeltaFloors::default(), None);
        let work = freeze.stage(&registry, db, &touches, &mut batch, &mut flush)?;
        let applied = self.apply(db, batch, opts, floors, attempt);
        // Bumps what the frozen rows' readers cache, also after a failure: the
        // write may have reached RocksDB.
        drop(work);
        drop(registry);
        applied
    }

    /// The refusal of a fenced gate, counted.
    fn fenced_refusal(&self) -> Error {
        self.refusals.fetch_add(1, Ordering::Relaxed);
        Error::primary_fenced_write_refused(
            self.fence_reason
                .get()
                .cloned()
                .unwrap_or_else(|| crate::error::PRIMARY_FENCED_WRITE_REFUSED.to_string()),
        )
    }

    /// `Ok` when writes are admitted; otherwise counts a refusal and returns
    /// the error a refused write reports. The mode is asked first, then the
    /// fence (spec P5.1).
    pub fn check(&self) -> Result<()> {
        match self.mode {
            GateMode::Open if self.is_fenced() => Err(self.fenced_refusal()),
            GateMode::Open => Ok(()),
            GateMode::Closed { reason } => {
                self.refusals.fetch_add(1, Ordering::Relaxed);
                Err(Error::storage(format!(
                    "write refused on a {reason}: this database was opened without write access, and only a \
                     primary open may change it"
                )))
            }
        }
    }

    /// Admit one write: announce it, then [`Self::check`] (module
    /// documentation, "Fencing"). The returned guard ends the announcement
    /// once the RocksDB call returned.
    fn admit(&self) -> Result<InFlight<'_>> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let slot = InFlight(&self.in_flight);
        self.check()?;
        Ok(slot)
    }

    /// Spec R5.1: this database's statements are running from now on, so a row write
    /// with no lock on its table is a missed path.
    pub(crate) fn arm_row_lock_detector(&self) {
        self.row_lock_detector.store(true, Ordering::Relaxed);
    }

    /// `DB::write` / `DB::write_opt` behind the gate.
    ///
    /// A batch with no operations changes nothing, so on a closed gate it is a
    /// no-op rather than a refusal: a read-only transaction's COMMIT applies an
    /// empty batch, and that is not an attempt to write. On an open gate it is
    /// handed to RocksDB unchanged, exactly as before the gate existed.
    #[track_caller]
    pub fn write(&self, db: &DB, batch: WriteBatch, opts: Option<&WriteOptions>) -> Result<()> {
        self.write_inner(db, batch, opts, None)
    }

    /// Observe only the owning native group, never B4's preliminary copy chunks.
    #[track_caller]
    pub(crate) fn write_observed(
        &self,
        db: &DB,
        batch: WriteBatch,
        opts: Option<&WriteOptions>,
        attempt: &WriteAttempt,
    ) -> Result<()> {
        self.write_inner(db, batch, opts, Some(attempt))
    }

    #[track_caller]
    fn write_inner(
        &self,
        db: &DB,
        batch: WriteBatch,
        opts: Option<&WriteOptions>,
        attempt: Option<&WriteAttempt>,
    ) -> Result<()> {
        self.check_available_batch(&batch)?;
        if batch.is_empty() && !self.is_open() {
            return Ok(());
        }
        let slot = self.admit()?;
        // The writer's own keys, before the freeze hook adds the children's copies: those
        // are the parent statement's writes under its lock on the parent's table, ordered
        // against the child's own writers by the hook's locks (`storage::branch_freeze`,
        // "Lock order"), and no statement of the child's can hold that lock.
        if super::table_locks::detector_active(&self.row_lock_detector) {
            super::table_locks::check_row_batch(&batch, std::panic::Location::caller());
        }
        // Spec R5.3: a schema statement's writes are its batch's one commit.
        super::ddl_batch::check_gate_write(self, std::panic::Location::caller());
        // Spec R5.6: the statement's last validation before its first durable effect
        // (refused here, nothing written), then the layout change the batch makes.
        if !batch.is_empty() {
            super::layout_epochs::at_durable_effect()?;
        }
        let (change, floors) = self.begin_batch_change(db, &batch)?;
        let applied = if self.freeze_active.load(Ordering::SeqCst) {
            self.write_frozen(db, batch, opts, slot, floors, attempt)
        } else {
            let applied = self.apply(db, batch, opts, floors, attempt);
            drop(slot);
            applied
        };
        self.end_layout_change(change);
        applied
    }

    /// `DB::put` / `DB::put_opt` behind the gate.
    #[track_caller]
    pub fn put<K, V>(&self, db: &DB, key: K, value: V, opts: Option<&WriteOptions>) -> Result<()>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let key = key.as_ref();
        let value = value.as_ref();
        if self.delta_ids_installed.load(Ordering::Acquire)
            && (key.starts_with(b"delta:") || key.starts_with(b"meta:delta:"))
        {
            let mut batch = WriteBatch::default();
            batch.put(key, value);
            return self.write(db, batch, opts);
        }
        self.check_available_key(key)?;
        let slot = self.admit()?;
        if super::table_locks::detector_active(&self.row_lock_detector) {
            super::table_locks::check_row_key(key, std::panic::Location::caller());
        }
        super::ddl_batch::check_gate_write(self, std::panic::Location::caller());
        // Spec R5.6: as `write`.
        super::layout_epochs::at_durable_effect()?;
        let change = self.begin_layout_change_of_key(key, Some(value));
        let applied = if self.freeze_active.load(Ordering::SeqCst) {
            let mut batch = WriteBatch::default();
            batch.put(key, value);
            self.write_frozen(db, batch, opts, slot, DeltaFloors::default(), None)
        } else {
            let applied = self.refuse_point_group(key.len(), Some(value.len())).and_then(|()| {
                // Retain the native owner and test seam. A cap refusal occurs
                // before consuming a native fault and still ends layout below.
                #[cfg(any(test, feature = "internal-tests"))]
                if FAIL_NEXT_NATIVE_WRITE_BEFORE_APPLY.with(|fail| fail.replace(false)) {
                    return Err(Error::storage(
                        "injected native write error before apply; outcome is unknown",
                    ));
                }
                let applied = match opts {
                    Some(opts) => db.put_opt(key, value, opts),
                    None => db.put(key, value),
                }
                .map_err(Error::from);
                #[cfg(any(test, feature = "internal-tests"))]
                let applied = if applied.is_ok() && FAIL_NEXT_NATIVE_WRITE_AFTER_APPLY.with(|fail| fail.replace(false))
                {
                    Err(Error::storage(
                        "injected native write error after apply; outcome is unknown",
                    ))
                } else {
                    applied
                };
                applied
            });
            drop(slot);
            applied
        };
        self.end_layout_change(change);
        applied
    }

    /// `DB::delete` / `DB::delete_opt` behind the gate.
    #[track_caller]
    pub fn delete<K: AsRef<[u8]>>(&self, db: &DB, key: K, opts: Option<&WriteOptions>) -> Result<()> {
        let key = key.as_ref();
        if self.delta_ids_installed.load(Ordering::Acquire)
            && (key.starts_with(b"delta:") || key.starts_with(b"meta:delta:"))
        {
            let mut batch = WriteBatch::default();
            batch.delete(key);
            return self.write(db, batch, opts);
        }
        self.check_available_key(key)?;
        let slot = self.admit()?;
        if super::table_locks::detector_active(&self.row_lock_detector) {
            super::table_locks::check_row_key(key, std::panic::Location::caller());
        }
        super::ddl_batch::check_gate_write(self, std::panic::Location::caller());
        // Spec R5.6: as `write`.
        super::layout_epochs::at_durable_effect()?;
        let change = self.begin_layout_change_of_key(key, None);
        let applied = if self.freeze_active.load(Ordering::SeqCst) {
            let mut batch = WriteBatch::default();
            batch.delete(key);
            self.write_frozen(db, batch, opts, slot, DeltaFloors::default(), None)
        } else {
            let applied = self.refuse_point_group(key.len(), None).and_then(|()| {
                match opts {
                    Some(opts) => db.delete_opt(key, opts),
                    None => db.delete(key),
                }
                .map_err(Error::from)
            });
            drop(slot);
            applied
        };
        self.end_layout_change(change);
        applied
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn observed_owning_write_distinguishes_refusal_applied_and_unknown() {
        use super::super::write_outcome::{WriteAttempt, WriteOutcome};
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        let refused = WriteAttempt::default();
        let mut batch = WriteBatch::default();
        batch.put(b"proof:refused", [7; 128]);
        gate.debug_set_write_group_ceiling(Some(64));
        assert!(gate.write_observed(&db, batch, None, &refused).is_err());
        assert_eq!(refused.outcome(), WriteOutcome::NotWritten);
        assert!(db.get(b"proof:refused").unwrap().is_none());
        gate.debug_set_write_group_ceiling(None);
        let applied = WriteAttempt::default();
        let mut batch = WriteBatch::default();
        batch.put(b"proof:applied", b"yes");
        gate.write_observed(&db, batch, None, &applied).unwrap();
        assert_eq!(applied.outcome(), WriteOutcome::Applied);
        assert_eq!(db.get(b"proof:applied").unwrap().unwrap(), b"yes");
        let unknown = WriteAttempt::default();
        let mut batch = WriteBatch::default();
        batch.put(b"proof:unknown", b"actual");
        gate.debug_fail_next_native_write_after_apply();
        let error = gate.write_observed(&db, batch, None, &unknown).unwrap_err();
        assert!(error.to_string().contains("outcome is unknown"));
        assert_eq!(unknown.outcome(), WriteOutcome::Unknown);
        assert_eq!(db.get(b"proof:unknown").unwrap().unwrap(), b"actual");
    }

    #[test]
    fn a_quarantined_table_refuses_point_and_range_batches_without_blocking_other_tables() {
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        gate.quarantine_table("broken", "a committed image could not be decoded");
        assert!(gate.put(&db, b"data:broken:1", b"row", None).is_err());
        assert!(gate.delete(&db, b"data:broken:1", None).is_err());
        let mut point = WriteBatch::default();
        point.put(b"data:broken:2", b"row");
        assert!(gate.write(&db, point, None).is_err());
        let mut range = WriteBatch::default();
        range.delete_range(b"data:", b"data;");
        assert!(
            gate.write(&db, range, None).is_err(),
            "a C-iterator-invisible range must not bypass quarantine"
        );
        gate.put(&db, b"data:healthy:1", b"row", None).unwrap();
        assert_eq!(db.get(b"data:healthy:1").unwrap().as_deref(), Some(b"row".as_slice()));
    }

    fn scratch_db() -> (tempfile::TempDir, DB) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, dir.path()).expect("open");
        (dir, db)
    }

    #[test]
    fn an_open_gate_admits_every_write_and_counts_nothing() {
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        gate.put(&db, b"k", b"v", None).unwrap();
        let mut batch = WriteBatch::default();
        batch.put(b"b", b"1");
        gate.write(&db, batch, None).unwrap();
        gate.delete(&db, b"k", None).unwrap();
        assert!(gate.check().is_ok());
        assert_eq!(db.get(b"k").unwrap(), None);
        assert_eq!(db.get(b"b").unwrap().as_deref(), Some(b"1".as_slice()));
        assert_eq!(gate.refusals(), 0);
        assert_eq!(gate.mode(), GateMode::Open);
        assert!(!gate.is_fenced());
        assert!(
            gate.wait_for_writes(Duration::ZERO),
            "nothing is in flight after the calls returned"
        );
    }

    #[test]
    fn a_closed_gate_refuses_before_rocksdb_sees_the_write() {
        let (_dir, db) = scratch_db();
        db.put(b"present", b"before").unwrap();
        let sequence = db.latest_sequence_number();
        let gate = WriteGate::closed("physical snapshot");

        let error = gate.put(&db, b"k", b"v", None).unwrap_err().to_string();
        assert!(error.contains("write refused on a physical snapshot"), "{error}");
        let mut batch = WriteBatch::default();
        batch.delete(b"present");
        assert!(gate.write(&db, batch, None).is_err());
        assert!(gate.delete(&db, b"present", None).is_err());
        assert!(gate.check().is_err());

        assert_eq!(gate.refusals(), 4);
        assert_eq!(db.latest_sequence_number(), sequence);
        assert_eq!(db.get(b"present").unwrap().as_deref(), Some(b"before".as_slice()));
        assert_eq!(db.get(b"k").unwrap(), None);
        assert!(
            gate.wait_for_writes(Duration::ZERO),
            "a refused write leaves nothing in flight"
        );
    }

    #[test]
    fn asking_and_empty_batches_are_not_refusals() {
        let (_dir, db) = scratch_db();
        let sequence = db.latest_sequence_number();
        let gate = WriteGate::closed("read-only open");
        assert!(!gate.is_open());
        gate.write(&db, WriteBatch::default(), None).unwrap();
        assert_eq!(gate.refusals(), 0);
        assert_eq!(db.latest_sequence_number(), sequence);
    }

    /// Spec P5.1 §8: a fence makes `check()` fail with the fence's own text,
    /// closes `is_open()`, counts every refused write, is idempotent (the
    /// first reason stays), and refuses before RocksDB sees anything.
    #[test]
    fn a_fence_refuses_every_later_write_with_its_reason_and_is_idempotent() {
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        gate.put(&db, b"before", b"1", None).unwrap();
        let sequence = db.latest_sequence_number();
        let reason = format!(
            "{}: at 2026-10-24T00:00:00Z by epoch 2 (a standby was promoted)",
            crate::error::PRIMARY_FENCED_WRITE_REFUSED
        );
        assert!(gate.fence(reason.clone()), "the first fence raises it");
        assert!(!gate.fence("another reason"), "a second fence changes nothing");
        assert!(gate.is_fenced());
        assert!(!gate.is_open());
        assert_eq!(
            gate.mode(),
            GateMode::Open,
            "the open mode is unchanged; the fence is a state of its own"
        );
        assert_eq!(gate.fence_reason(), Some(reason.as_str()));

        let error = gate.check().unwrap_err();
        assert!(error.is_primary_fenced_write_refusal(), "{error:?}");
        assert!(error.to_string().contains(&reason), "{error}");
        assert!(gate.put(&db, b"k", b"v", None).is_err());
        let mut batch = WriteBatch::default();
        batch.put(b"b", b"1");
        assert!(gate.write(&db, batch, None).is_err());
        assert!(gate.delete(&db, b"before", None).is_err());
        // An empty batch is still not an attempt to write.
        gate.write(&db, WriteBatch::default(), None).unwrap();
        assert_eq!(gate.refusals(), 4);
        assert_eq!(
            db.latest_sequence_number(),
            sequence,
            "nothing reached RocksDB after the fence"
        );
        assert!(gate.wait_for_writes(Duration::ZERO));
    }

    /// Spec R5 decision 6: a group above the ceiling is refused, 54000, and nothing
    /// reaches RocksDB; one at the ceiling goes through.
    #[test]
    fn a_group_above_the_ceiling_is_refused_before_rocksdb_sees_it() {
        let (_dir, db) = scratch_db();
        let gate = WriteGate::open();
        let mut small = WriteBatch::default();
        small.put(b"k", b"v");
        gate.debug_set_write_group_ceiling(Some(small.size_in_bytes()));
        gate.write(&db, small, None).unwrap();
        let sequence = db.latest_sequence_number();
        let mut large = WriteBatch::default();
        large.put(b"k2", vec![7_u8; 64]);
        let refused = gate.write(&db, large, None).unwrap_err().to_string();
        assert!(
            refused.contains(crate::error::BRANCH_MATERIALIZE_TOO_LARGE_MARKER),
            "{refused}"
        );
        assert_eq!(db.latest_sequence_number(), sequence, "nothing reached RocksDB");
        assert_eq!(db.get(b"k2").unwrap(), None);
        gate.debug_set_write_group_ceiling(None);
        let mut again = WriteBatch::default();
        again.put(b"k2", vec![7_u8; 64]);
        gate.write(&db, again, None).unwrap();
        assert!(
            gate.wait_for_writes(Duration::ZERO),
            "a refused write leaves nothing in flight"
        );
    }

    /// The fence waits for a write that was past the gate when it was raised
    /// (module documentation, "Fencing").
    #[test]
    fn the_fence_waits_for_a_write_already_past_the_gate() {
        let gate = WriteGate::open();
        let slot = gate.admit().unwrap();
        assert!(gate.fence(crate::error::PRIMARY_FENCED_WRITE_REFUSED));
        assert!(
            !gate.wait_for_writes(Duration::from_millis(20)),
            "the admitted write is still in flight"
        );
        assert!(gate.admit().is_err(), "a write after the fence is refused");
        drop(slot);
        assert!(gate.wait_for_writes(Duration::from_secs(1)));
    }
}

#[cfg(test)]
#[path = "point_write_cap_tests.rs"]
mod point_write_cap_tests;
