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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rocksdb::{WriteBatch, WriteOptions, DB};

use super::branch_freeze::BranchFreeze;
use crate::{Error, Result};

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
}

/// A write past the gate, until its RocksDB call returned.
struct InFlight<'a>(&'a AtomicU64);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WriteGate {
    fn with_mode(mode: GateMode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            refusals: AtomicU64::new(0),
            fenced: AtomicBool::new(false),
            fence_reason: OnceLock::new(),
            in_flight: AtomicU64::new(0),
            row_lock_detector: AtomicBool::new(false),
            freeze_active: Arc::new(AtomicBool::new(false)),
            freeze: OnceLock::new(),
            freeze_runs: AtomicU64::new(0),
        })
    }

    /// A gate that admits every write.
    pub fn open() -> Arc<Self> {
        Self::with_mode(GateMode::Open)
    }

    /// A gate that refuses every write, naming `reason` in the error.
    pub fn closed(reason: &'static str) -> Arc<Self> {
        Self::with_mode(GateMode::Closed { reason })
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
        let applied = Self::apply(db, batch, opts);
        drop(slot);
        applied
    }

    /// How many writes went through the freeze hook. For the tests.
    #[doc(hidden)]
    pub fn debug_freeze_runs(&self) -> u64 {
        self.freeze_runs.load(Ordering::Relaxed)
    }

    fn apply(db: &DB, batch: WriteBatch, opts: Option<&WriteOptions>) -> Result<()> {
        match opts {
            Some(opts) => db.write_opt(batch, opts),
            None => db.write(batch),
        }
        .map_err(Error::from)
    }

    /// Sprinter 6bc4a4040eb7 (B4): THE write funnel while some level has a live
    /// child branch; see `storage::branch_freeze`. `slot` is this write's
    /// announcement.
    ///
    /// It runs on the writer's own thread. Past `stage` the batch holds what the
    /// hook adds to it, and it holds the table locks or row stripes of
    /// `BranchFreeze`, the innermost locks of a write: nothing is taken under them
    /// but RocksDB ("Lock order" in `storage::branch_freeze`).
    fn write_frozen(
        &self,
        db: &DB,
        mut batch: WriteBatch,
        opts: Option<&WriteOptions>,
        slot: InFlight<'_>,
    ) -> Result<()> {
        self.freeze_runs.fetch_add(1, Ordering::Relaxed);
        let Some(freeze) = self.freeze.get() else {
            let applied = Self::apply(db, batch, opts);
            drop(slot);
            return applied;
        };
        let touches = BranchFreeze::row_touches(&batch);
        if touches.is_empty() {
            let applied = Self::apply(db, batch, opts);
            drop(slot);
            return applied;
        }
        let registry = freeze.registry_for_write();
        // The registry answers for this write from here: holding it is what a
        // new branch waits for, and a fence too.
        drop(slot);
        let mut flush = |chunk: WriteBatch| Self::apply(db, chunk, opts);
        let work = freeze.stage(&registry, db, &touches, &mut batch, &mut flush)?;
        let applied = Self::apply(db, batch, opts);
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
        if self.freeze_active.load(Ordering::SeqCst) {
            return self.write_frozen(db, batch, opts, slot);
        }
        let applied = Self::apply(db, batch, opts);
        drop(slot);
        applied
    }

    /// `DB::put` / `DB::put_opt` behind the gate.
    #[track_caller]
    pub fn put<K, V>(&self, db: &DB, key: K, value: V, opts: Option<&WriteOptions>) -> Result<()>
    where
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let slot = self.admit()?;
        if super::table_locks::detector_active(&self.row_lock_detector) {
            super::table_locks::check_row_key(key.as_ref(), std::panic::Location::caller());
        }
        if self.freeze_active.load(Ordering::SeqCst) {
            let mut batch = WriteBatch::default();
            batch.put(key, value);
            return self.write_frozen(db, batch, opts, slot);
        }
        let applied = match opts {
            Some(opts) => db.put_opt(key, value, opts),
            None => db.put(key, value),
        }
        .map_err(Error::from);
        drop(slot);
        applied
    }

    /// `DB::delete` / `DB::delete_opt` behind the gate.
    #[track_caller]
    pub fn delete<K: AsRef<[u8]>>(&self, db: &DB, key: K, opts: Option<&WriteOptions>) -> Result<()> {
        let slot = self.admit()?;
        if super::table_locks::detector_active(&self.row_lock_detector) {
            super::table_locks::check_row_key(key.as_ref(), std::panic::Location::caller());
        }
        if self.freeze_active.load(Ordering::SeqCst) {
            let mut batch = WriteBatch::default();
            batch.delete(key);
            return self.write_frozen(db, batch, opts, slot);
        }
        let applied = match opts {
            Some(opts) => db.delete_opt(key, opts),
            None => db.delete(key),
        }
        .map_err(Error::from);
        drop(slot);
        applied
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
