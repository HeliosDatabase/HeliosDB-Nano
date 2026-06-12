//! Write-write conflict detection (R0.2).
//!
//! First-committer-wins validation for snapshot-isolation transactions.
//! The registry keeps the last committed write timestamp per data key; a
//! committing transaction with conflict validation enabled aborts with a
//! serialization failure if any key in its write set was committed by
//! another transaction after this transaction's snapshot.
//!
//! Concurrency design (v2):
//! - Validation+recording is per-key (DashMap entry-level atomicity), so
//!   committers on disjoint keys never serialize against each other. A
//!   multi-key commit records as it validates and undoes its partial
//!   records when it loses, so two interleaved committers can never both
//!   pass on the same key.
//! - Commits with a non-empty write set announce themselves as IN-FLIGHT
//!   (commit timestamp allocated, RocksDB write not yet applied). New
//!   snapshots wait until no in-flight commit is at-or-below their
//!   timestamp — otherwise a fresh transaction could read pre-commit data
//!   while its snapshot claims to include the commit, and then validate
//!   clean (the lost-update leak this versions fixes).
//!
//!   R1.3 phase 2, v2 (lock-free fast path; replaces both the R0.2
//!   bounded-spin DashMap ledger and the first R1.3-p2 Mutex+BTreeMap
//!   ledger, whose global mutex sat on EVERY commit — once nested inside
//!   the engine timestamp lock and once at end-of-apply — and cost ~20%
//!   at 16T even with no barrier waiter): in-flight commits live in a
//!   fixed ring of packed atomics ordered by announcement. `begin_commit`
//!   runs inside the engine timestamp lock, so announcements are strictly
//!   ts-ordered and the ring needs no internal lock: begin is two atomic
//!   stores, `end_commit` is one CAS plus a flag-guarded reclamation scan,
//!   and `snapshot_barrier` decides from atomic loads alone. The contiguous
//!   applied prefix is reclaimed in announcement order, advancing
//!   `applied_watermark` = highest commit_ts such that ALL commits <= it
//!   have applied. Barrier waiters yield-spin briefly (a blocking commit is
//!   typically mid-apply, sub-µs) and then PARK on a condvar signaled by
//!   `end_commit` — the spin is strictly bounded, so waiters can never
//!   starve the committing threads they wait on (the 680x convoy the R0.2
//!   spin fix worked around), and `end_commit` skips the condvar mutex
//!   entirely unless a waiter is registered (SeqCst-fence handoff, see
//!   `end_commit`/`snapshot_barrier_slow`).
//! - INSERT-only transactions (insert_log) use engine-allocated row ids
//!   that are never reused; they skip the registry entirely, keeping bulk
//!   ingest registry-free.
//!
//! Semantics:
//! - ReadCommitted transactions do not validate (PostgreSQL parity) but
//!   their commits are recorded so snapshot-isolation transactions detect
//!   conflicts against them.
//! - Embedded global-slot and RepeatableRead/Serializable session
//!   transactions validate (previously they silently lost updates).
//!
//! Memory: entries older than the oldest registered validating snapshot
//! are pruned every `PRUNE_INTERVAL_COMMITS` commits. Only validating
//! transactions read `recent_writes`, so only they need to register.

use dashmap::DashMap;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::Key;

/// Prune the recent-writes map every N recorded commits.
const PRUNE_INTERVAL_COMMITS: u64 = 4096;

/// In-flight ring capacity (power of two). The ring holds every announced
/// commit from the oldest still-pending one upward, so overflow needs a
/// committer stalled mid-apply while this many later commits are announced.
/// On overflow `begin_commit` waits (yield, then 50µs sleeps) for the
/// straggler — safe because reclamation never takes the engine timestamp
/// lock the waiting allocator holds.
const INFLIGHT_RING_SIZE: usize = 4096;
const INFLIGHT_RING_MASK: u64 = (INFLIGHT_RING_SIZE as u64) - 1;

/// Barrier slow-path yield-spins before parking on the condvar.
const BARRIER_SPINS: u32 = 48;

/// Ring slot encoding. `0` = empty (reclaimed / never used); low bit set =
/// pending; low bit clear (nonzero) = applied, awaiting in-order
/// reclamation. Timestamps are engine-allocated counters (always >= 1 and
/// far below 2^63), so encodings never collide with the empty marker.
const SLOT_EMPTY: u64 = 0;
#[inline]
fn slot_pending(ts: u64) -> u64 {
    (ts << 1) | 1
}
#[inline]
fn slot_applied(ts: u64) -> u64 {
    ts << 1
}
#[inline]
fn slot_is_pending(v: u64) -> bool {
    v & 1 == 1
}
#[inline]
fn slot_ts(v: u64) -> u64 {
    v >> 1
}

thread_local! {
    /// `(registry address, commit_ts, ring seq)` of this thread's most
    /// recent `begin_commit`. Engine commit paths begin and end on the same
    /// thread, so `end_commit` can usually mark its slot directly instead
    /// of scanning. Pure optimization: a miss (cross-thread end, double
    /// end, another registry instance) falls back to the window scan, and
    /// the CAS validates the slot still holds exactly this pending entry.
    static LAST_BEGIN_HINT: std::cell::Cell<(usize, u64, u64)> =
        const { std::cell::Cell::new((0, 0, 0)) };
}

pub struct WriteConflictRegistry {
    /// Last committed (or in-flight) write timestamp per data key.
    recent_writes: DashMap<Key, u64>,
    /// Snapshots of active VALIDATING transactions (txn_id -> snapshot_ts).
    active_snapshots: DashMap<u64, u64>,
    /// Commit-ordered in-flight ledger: a ring of packed slots (see
    /// `slot_pending`/`slot_applied`). Slot for announcement #seq is
    /// `ring[seq % INFLIGHT_RING_SIZE]`. Announced timestamps strictly
    /// increase with seq (`begin_commit` runs inside the engine timestamp
    /// lock).
    ring: Box<[AtomicU64]>,
    /// Next announcement seq. Written only by `begin_commit`, whose calls
    /// are serialized by the engine timestamp lock.
    head: AtomicU64,
    /// First unreclaimed seq: every seq below it was applied and zeroed.
    /// Written only while `reclaim_flag` is held.
    tail: AtomicU64,
    /// Single-reclaimer gate for advancing `tail` (try-lock; losers rely on
    /// the holder's post-release re-check, see `try_reclaim`).
    reclaim_flag: AtomicBool,
    /// Number of PENDING (begun, not yet ended) commits. 0 = barrier fast
    /// path: nothing to wait for.
    pending_count: AtomicUsize,
    /// Highest commit_ts such that every in-flight commit <= it has
    /// applied. Monotonic; second barrier fast path.
    applied_watermark: AtomicU64,
    /// Barrier threads registered for condvar parking. `end_commit` touches
    /// `barrier_lock` only when this is nonzero, so the commit fast path
    /// acquires no mutex while no one waits.
    barrier_waiters: AtomicUsize,
    /// Parking lot for barrier waiters (guards nothing — pure condvar
    /// pairing; the wait predicate reads the ring atomics).
    barrier_lock: Mutex<()>,
    /// Signaled by `end_commit` whenever an entry becomes applied.
    barrier_cv: Condvar,
    /// Recorded commits since creation (drives pruning cadence).
    commit_count: AtomicU64,
    /// R4.3: snapshots pinned against MVCC version GC (pin_id ->
    /// snapshot_ts). Covers EVERY transaction holding this registry (not
    /// just validating ones — ReadCommitted session transactions also scan
    /// at their snapshot) plus statement-scoped historical (`AS OF`)
    /// readers and in-flight `CREATE BRANCH ... AS OF` anchors. The GC
    /// horizon never advances past `min(gc_pins)`; a horizon EQUAL to a
    /// pin is safe (the newest version at-or-below the horizon is kept).
    ///
    /// Design note: this deliberately extends the conflict registry rather
    /// than adding a separate reader-registry object — every Transaction
    /// already receives this registry (`set_conflict_registry`), so pinning
    /// reuses existing plumbing, and the GC consults one place. Pins use
    /// their own id space because the two `Transaction` constructors keep
    /// independent txn-id counters whose ids can collide.
    gc_pins: DashMap<u64, u64>,
    /// Pin id allocator for `gc_pins`.
    next_gc_pin: AtomicU64,
}

impl Default for WriteConflictRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteConflictRegistry {
    pub fn new() -> Self {
        let ring: Vec<AtomicU64> = (0..INFLIGHT_RING_SIZE).map(|_| AtomicU64::new(SLOT_EMPTY)).collect();
        Self {
            recent_writes: DashMap::new(),
            active_snapshots: DashMap::new(),
            ring: ring.into_boxed_slice(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            reclaim_flag: AtomicBool::new(false),
            pending_count: AtomicUsize::new(0),
            applied_watermark: AtomicU64::new(0),
            barrier_waiters: AtomicUsize::new(0),
            barrier_lock: Mutex::new(()),
            barrier_cv: Condvar::new(),
            commit_count: AtomicU64::new(0),
            gc_pins: DashMap::new(),
            next_gc_pin: AtomicU64::new(0),
        }
    }

    /// Ring slot for announcement seq (index provably in-bounds: masked by
    /// `INFLIGHT_RING_MASK` against the power-of-two ring length).
    #[inline]
    #[allow(clippy::indexing_slicing)]
    fn slot(&self, seq: u64) -> &AtomicU64 {
        &self.ring[(seq & INFLIGHT_RING_MASK) as usize]
    }

    /// Identity for the same-thread begin/end hint.
    #[inline]
    fn instance_id(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    pub fn register_txn(&self, txn_id: u64, snapshot_ts: u64) {
        self.active_snapshots.insert(txn_id, snapshot_ts);
    }

    pub fn refresh_txn(&self, txn_id: u64, snapshot_ts: u64) {
        self.active_snapshots.insert(txn_id, snapshot_ts);
    }

    pub fn deregister_txn(&self, txn_id: u64) {
        self.active_snapshots.remove(&txn_id);
    }

    /// Snapshot barrier: block until every in-flight commit at or below
    /// `snapshot_ts` has been applied. Atomic loads only when nothing is
    /// pending, the watermark already covers the snapshot, or every pending
    /// commit is above it (the overwhelmingly common cases); otherwise a
    /// short bounded yield-spin, then a condvar park that `end_commit`
    /// signals.
    ///
    /// Correctness (unchanged contract since R0.2): a snapshot must never
    /// be taken covering an unapplied commit. Any commit with
    /// ts <= snapshot_ts ran `begin_commit` inside the engine timestamp
    /// lock BEFORE this snapshot's ts was allocated under the same lock, so
    /// its ring entry and `pending_count` increment are visible here — the
    /// fast paths cannot miss it.
    pub fn snapshot_barrier(&self, snapshot_ts: u64) {
        if self.pending_count.load(Ordering::Acquire) == 0 {
            return;
        }
        if self.applied_watermark.load(Ordering::Acquire) >= snapshot_ts {
            return;
        }
        if !self.has_pending_at_or_below(snapshot_ts) {
            return;
        }
        self.snapshot_barrier_slow(snapshot_ts);
    }

    #[cold]
    fn snapshot_barrier_slow(&self, snapshot_ts: u64) {
        // A blocking commit is normally mid-apply (sub-µs for non-durable
        // batches; durable commits lift the barrier BEFORE their group
        // fsync), so a short read-only yield-spin usually beats paying
        // futex park/wake latency. Strictly bounded: past it we park, so
        // waiters can never starve the committers they wait on.
        for _ in 0..BARRIER_SPINS {
            std::thread::yield_now();
            if !self.has_pending_at_or_below(snapshot_ts) {
                return;
            }
        }
        let mut guard = self.barrier_lock.lock();
        self.barrier_waiters.fetch_add(1, Ordering::Relaxed);
        loop {
            // SeqCst handoff with `end_commit`: either the ender sees our
            // waiter registration (and takes the lock + notifies), or this
            // fence orders after its fence and the re-check below sees its
            // applied mark. Either way no wakeup is lost.
            fence(Ordering::SeqCst);
            if !self.has_pending_at_or_below(snapshot_ts) {
                break;
            }
            self.barrier_cv.wait(&mut guard);
        }
        self.barrier_waiters.fetch_sub(1, Ordering::Relaxed);
    }

    /// Is any announced-but-unapplied commit at or below `snapshot_ts`?
    /// Lock-free scan of the unreclaimed ring window. Pending entries are
    /// read exactly (a pending slot only ever transitions by its own
    /// committer), so a `false` answer is sound: commits announced after
    /// the scan started carry timestamps above `snapshot_ts` by the
    /// allocation order argument in [`Self::snapshot_barrier`].
    fn has_pending_at_or_below(&self, snapshot_ts: u64) -> bool {
        loop {
            let t = self.tail.load(Ordering::Acquire);
            let h = self.head.load(Ordering::Acquire);
            let mut s = t;
            let mut early_false = false;
            while s != h {
                let v = self.slot(s).load(Ordering::Acquire);
                if slot_is_pending(v) {
                    if slot_ts(v) <= snapshot_ts {
                        return true;
                    }
                    // Announced ts increase with seq: every later entry is
                    // above this one — provided this slot still belongs to
                    // seq `s` (validated below via the unmoved tail).
                    early_false = true;
                    break;
                }
                s += 1;
            }
            // Slot reuse (a later announcement aliasing into [t, h)) is only
            // possible after `tail` advances past `t`; if it hasn't, every
            // value read above belonged to its seq and the scan is exact.
            if !early_false || self.tail.load(Ordering::Acquire) == t {
                return false;
            }
            // Ring advanced mid-scan (needs ~INFLIGHT_RING_SIZE commits
            // during one scan — effectively never): retry.
        }
    }

    /// Highest commit timestamp such that every in-flight commit at or
    /// below it has been applied (diagnostics/tests).
    pub fn applied_watermark(&self) -> u64 {
        self.applied_watermark.load(Ordering::Acquire)
    }

    /// Announce a commit whose write set is about to be validated/applied.
    /// MUST be paired with `end_commit` on every path (use try/finally
    /// discipline in the caller). Called with strictly increasing
    /// `commit_ts` (inside the engine timestamp lock — calls are
    /// serialized, which is what lets the ring run without a lock of its
    /// own). Two atomic stores plus one atomic RMW on the fast path.
    pub fn begin_commit(&self, commit_ts: u64) {
        let h = self.head.load(Ordering::Relaxed);
        if h.wrapping_sub(self.tail.load(Ordering::Acquire)) >= INFLIGHT_RING_SIZE as u64 {
            self.wait_for_ring_space(h);
        }
        // Slot store BEFORE the head bump: a reader that observes seq `h`
        // inside the window (head Acquire) is guaranteed to see this entry.
        self.slot(h).store(slot_pending(commit_ts), Ordering::Release);
        self.pending_count.fetch_add(1, Ordering::AcqRel);
        self.head.store(h + 1, Ordering::Release);
        LAST_BEGIN_HINT.with(|c| c.set((self.instance_id(), commit_ts, h)));
    }

    /// Ring full: every slot holds a commit that is pending or stuck behind
    /// one (a committer stalled mid-apply for the time it took
    /// `INFLIGHT_RING_SIZE` later commits to be announced). Wait it out;
    /// cannot deadlock — reclamation is driven by `end_commit`, which never
    /// takes the engine timestamp lock this thread holds.
    #[cold]
    fn wait_for_ring_space(&self, h: u64) {
        let mut spins: u32 = 0;
        loop {
            self.try_reclaim();
            if h.wrapping_sub(self.tail.load(Ordering::Acquire)) < INFLIGHT_RING_SIZE as u64 {
                return;
            }
            spins += 1;
            if spins < 64 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        }
    }

    /// The commit's RocksDB write has been applied (or the commit aborted).
    /// Marks the ring entry applied (one CAS via the same-thread hint, or a
    /// bounded scan of the unreclaimed window), reclaims the contiguous
    /// applied prefix (advancing the watermark), and wakes barrier waiters
    /// — touching the condvar mutex only if someone is registered.
    /// Tolerates unknown timestamps and double calls, including concurrent
    /// ones (the pending→applied CAS has exactly one winner; only the
    /// winner decrements `pending_count`).
    pub fn end_commit(&self, commit_ts: u64) {
        let pending = slot_pending(commit_ts);
        let applied = slot_applied(commit_ts);
        let mut marked = false;
        let hint = LAST_BEGIN_HINT.with(|c| c.get());
        if hint.0 == self.instance_id() && hint.1 == commit_ts {
            marked = self
                .slot(hint.2)
                .compare_exchange(pending, applied, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok();
        }
        if !marked {
            // The entry (if it exists) is in [tail, head): a pending entry
            // cannot be reclaimed, and announcement order puts it at or
            // above tail. Aliased newer entries can never equal `pending`
            // (timestamps are unique), so a CAS match is always our entry.
            let t = self.tail.load(Ordering::Acquire);
            let h = self.head.load(Ordering::Acquire);
            let mut s = t;
            while s != h {
                let slot = self.slot(s);
                if slot.load(Ordering::Acquire) == pending {
                    marked = slot
                        .compare_exchange(pending, applied, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok();
                    break;
                }
                s += 1;
            }
            if !marked {
                return; // unknown ts or already ended: no-op
            }
        }
        self.pending_count.fetch_sub(1, Ordering::AcqRel);
        self.try_reclaim();
        // SeqCst handoff (see `snapshot_barrier_slow`): skip the condvar
        // mutex entirely unless a waiter registered before our fence.
        fence(Ordering::SeqCst);
        if self.barrier_waiters.load(Ordering::Relaxed) > 0 {
            let _guard = self.barrier_lock.lock();
            self.barrier_cv.notify_all();
        }
    }

    /// Reclaim the contiguous applied prefix at `tail`, advancing the
    /// watermark in announcement (= timestamp) order. Single-reclaimer
    /// try-lock: a loser returns immediately and relies on the holder's
    /// post-release re-check — if the loser's freshly applied entry sits at
    /// the tail, either the holder still sees it in its sweep, or the
    /// re-check does and the holder loops to take the flag again. Entries
    /// thus never linger applied-at-tail with no reclaimer scheduled
    /// (and even a pathological race only delays reclamation until the
    /// next `end_commit`/overflow wait — barrier correctness never depends
    /// on `tail`, only on slot states, see `has_pending_at_or_below`).
    fn try_reclaim(&self) {
        while self
            .reclaim_flag
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            loop {
                let t = self.tail.load(Ordering::Relaxed); // sole writer: us
                if t == self.head.load(Ordering::Acquire) {
                    break;
                }
                let slot = self.slot(t);
                let v = slot.load(Ordering::Acquire);
                if v == SLOT_EMPTY || slot_is_pending(v) {
                    break;
                }
                // Applied at tail: everything below already reclaimed, so
                // every announced commit <= this ts has applied.
                self.applied_watermark.fetch_max(slot_ts(v), Ordering::AcqRel);
                // Zero BEFORE advancing tail: `begin_commit` only reuses a
                // slot after observing the advanced tail, so the zeroing
                // store is ordered before any reuse write.
                slot.store(SLOT_EMPTY, Ordering::Release);
                self.tail.store(t + 1, Ordering::Release);
            }
            self.reclaim_flag.store(false, Ordering::Release);
            // Post-release re-check (missed-wakeup closure, see above).
            let t = self.tail.load(Ordering::Acquire);
            if t == self.head.load(Ordering::Acquire) {
                return;
            }
            let v = self.slot(t).load(Ordering::Acquire);
            if v == SLOT_EMPTY || slot_is_pending(v) {
                return;
            }
            // Applied entry surfaced at the tail while we held the flag:
            // loop to re-acquire and reclaim it.
        }
    }

    /// First-committer-wins gate, per-key atomic. For each key: conflict if
    /// its recorded timestamp is newer than `snapshot_ts` (when `validate`),
    /// otherwise record `commit_ts`. On conflict, partial records made by
    /// this call are rolled back before returning.
    pub fn validate_and_record(
        &self,
        write_set: &DashMap<Key, Option<Vec<u8>>>,
        validate: bool,
        snapshot_ts: u64,
        commit_ts: u64,
    ) -> std::result::Result<(), (Key, u64)> {
        if write_set.is_empty() {
            return Ok(());
        }

        let mut recorded: Vec<(Key, Option<u64>)> = Vec::with_capacity(write_set.len());
        let mut conflict: Option<(Key, u64)> = None;

        for item in write_set.iter() {
            let key = item.key();
            let entry = self.recent_writes.entry(key.clone());
            match entry {
                dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
                    let prior = *occupied.get();
                    if validate && prior > snapshot_ts {
                        conflict = Some((key.clone(), prior));
                        break;
                    }
                    occupied.insert(commit_ts);
                    recorded.push((key.clone(), Some(prior)));
                }
                dashmap::mapref::entry::Entry::Vacant(vacant) => {
                    vacant.insert(commit_ts);
                    recorded.push((key.clone(), None));
                }
            }
        }

        if let Some((key, ts)) = conflict {
            // Undo partial records so the losing commit leaves no trace.
            for (k, prior) in recorded {
                match prior {
                    Some(ts) => {
                        self.recent_writes.insert(k, ts);
                    }
                    None => {
                        self.recent_writes.remove(&k);
                    }
                }
            }
            return Err((key, ts));
        }

        let n = self.commit_count.fetch_add(1, Ordering::Relaxed);
        if n % PRUNE_INTERVAL_COMMITS == PRUNE_INTERVAL_COMMITS - 1 {
            self.prune();
        }
        Ok(())
    }

    /// Drop entries no registered validating snapshot can conflict with.
    fn prune(&self) {
        let min_active = self
            .active_snapshots
            .iter()
            .map(|entry| *entry.value())
            .min()
            .unwrap_or(u64::MAX);
        self.recent_writes.retain(|_, ts| *ts >= min_active);
    }

    /// Test/diagnostic surface.
    pub fn tracked_keys(&self) -> usize {
        self.recent_writes.len()
    }

    /// R4.3: pin a snapshot against MVCC version GC. Returns a pin id that
    /// MUST be released with [`unpin_snapshot`](Self::unpin_snapshot)
    /// (RAII-wrap it in the caller — leaked pins block GC forever).
    pub fn pin_snapshot(&self, snapshot_ts: u64) -> u64 {
        let id = self.next_gc_pin.fetch_add(1, Ordering::Relaxed);
        self.gc_pins.insert(id, snapshot_ts);
        id
    }

    /// R4.3: release a GC pin.
    pub fn unpin_snapshot(&self, pin_id: u64) {
        self.gc_pins.remove(&pin_id);
    }

    /// R4.3: oldest snapshot any active transaction or historical reader
    /// is pinned to. The version-GC horizon must not exceed this value.
    /// Includes `active_snapshots` defensively (validating transactions are
    /// also in `gc_pins`, but a direct `register_txn` caller would not be).
    pub fn min_pinned_snapshot(&self) -> Option<u64> {
        let pin_min = self.gc_pins.iter().map(|e| *e.value()).min();
        let active_min = self.active_snapshots.iter().map(|e| *e.value()).min();
        match (pin_min, active_min) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Test/diagnostic surface: number of live GC pins.
    pub fn gc_pin_count(&self) -> usize {
        self.gc_pins.len()
    }
}

/// RAII guard for an R4.3 version-GC snapshot pin: releases the pin on drop.
/// Held by statement-scoped historical (`AS OF`) readers and by
/// `CREATE BRANCH ... AS OF` while the branch anchor is being persisted.
pub struct GcPinGuard {
    registry: std::sync::Arc<WriteConflictRegistry>,
    pin_id: u64,
}

impl GcPinGuard {
    pub fn new(registry: std::sync::Arc<WriteConflictRegistry>, snapshot_ts: u64) -> Self {
        let pin_id = registry.pin_snapshot(snapshot_ts);
        Self { registry, pin_id }
    }
}

impl Drop for GcPinGuard {
    fn drop(&mut self) {
        self.registry.unpin_snapshot(self.pin_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(keys: &[&str]) -> DashMap<Key, Option<Vec<u8>>> {
        let m = DashMap::new();
        for k in keys {
            m.insert(k.as_bytes().to_vec(), Some(vec![1]));
        }
        m
    }

    #[test]
    fn first_committer_wins() {
        let reg = WriteConflictRegistry::new();
        // T1 (snapshot 10) commits at 20.
        assert!(reg.validate_and_record(&ws(&["k1"]), true, 10, 20).is_ok());
        // T2 (snapshot 15 < 20) conflicts.
        let err = reg.validate_and_record(&ws(&["k1"]), true, 15, 25).unwrap_err();
        assert_eq!(err.1, 20);
        // T3 (snapshot 30 > 20) passes.
        assert!(reg.validate_and_record(&ws(&["k1"]), true, 30, 35).is_ok());
    }

    #[test]
    fn losing_commit_undoes_partial_records() {
        let reg = WriteConflictRegistry::new();
        assert!(reg.validate_and_record(&ws(&["b"]), true, 10, 20).is_ok());
        // Loser writes {a, b}: 'a' records, 'b' conflicts, 'a' must be undone.
        let _ = reg.validate_and_record(&ws(&["a", "b"]), true, 15, 25).unwrap_err();
        // A later txn writing 'a' at snapshot 5 must NOT see the loser's 25.
        assert!(reg.validate_and_record(&ws(&["a"]), true, 5, 30).is_ok());
    }

    #[test]
    fn non_validating_commits_record() {
        let reg = WriteConflictRegistry::new();
        assert!(reg.validate_and_record(&ws(&["k"]), false, 0, 20).is_ok());
        let err = reg.validate_and_record(&ws(&["k"]), true, 10, 25).unwrap_err();
        assert_eq!(err.1, 20);
    }

    #[test]
    fn prune_respects_active_snapshots() {
        let reg = WriteConflictRegistry::new();
        reg.register_txn(1, 50);
        for i in 0..(PRUNE_INTERVAL_COMMITS + 1) {
            let m = ws(&[format!("k{i}").as_str()]);
            let _ = reg.validate_and_record(&m, false, 0, 40 + i);
        }
        // Entries at ts >= 50 survive; the prune ran at least once.
        assert!(reg.tracked_keys() > 0);
        reg.deregister_txn(1);
    }

    #[test]
    fn gc_pins_track_min_snapshot() {
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        assert_eq!(reg.min_pinned_snapshot(), None);

        let p1 = reg.pin_snapshot(100);
        let _p2 = reg.pin_snapshot(50);
        reg.register_txn(7, 80);
        assert_eq!(reg.min_pinned_snapshot(), Some(50));

        reg.unpin_snapshot(p1);
        assert_eq!(reg.min_pinned_snapshot(), Some(50));
        reg.deregister_txn(7);

        // RAII guard releases on drop.
        {
            let _g = GcPinGuard::new(reg.clone(), 10);
            assert_eq!(reg.min_pinned_snapshot(), Some(10));
        }
        assert_eq!(reg.min_pinned_snapshot(), Some(50));
    }

    #[test]
    fn snapshot_barrier_waits_for_inflight() {
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        reg.begin_commit(10);
        let r2 = reg.clone();
        let h = std::thread::spawn(move || {
            // Barrier at 15 must wait for in-flight commit 10.
            r2.snapshot_barrier(15);
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!h.is_finished(), "barrier returned while commit in flight");
        reg.end_commit(10);
        h.join().unwrap();
        // Barrier below the in-flight ts does not wait.
        reg.begin_commit(100);
        reg.snapshot_barrier(50);
        reg.end_commit(100);
    }

    #[test]
    fn watermark_advances_past_contiguous_applied_prefix() {
        let reg = WriteConflictRegistry::new();
        reg.begin_commit(10);
        reg.begin_commit(20);
        reg.begin_commit(30);
        assert_eq!(reg.applied_watermark(), 0);
        reg.end_commit(10);
        assert_eq!(reg.applied_watermark(), 10);
        reg.end_commit(20);
        assert_eq!(reg.applied_watermark(), 20);
        reg.end_commit(30);
        assert_eq!(reg.applied_watermark(), 30);
    }

    #[test]
    fn out_of_order_end_commit_holds_watermark_until_gap_fills() {
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        reg.begin_commit(10);
        reg.begin_commit(20);
        reg.begin_commit(30);
        // 30 and 20 apply before 10: watermark must NOT pass the pending 10.
        reg.end_commit(30);
        reg.end_commit(20);
        assert_eq!(reg.applied_watermark(), 0);
        let r2 = reg.clone();
        let h = std::thread::spawn(move || r2.snapshot_barrier(25));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!h.is_finished(), "barrier(25) returned while commit 10 pending");
        // The gap fills: the whole applied prefix reclaims in one advance.
        reg.end_commit(10);
        h.join().unwrap();
        assert_eq!(reg.applied_watermark(), 30);
    }

    #[test]
    fn barrier_ignores_gaps_from_never_begun_timestamps() {
        // Timestamps allocated to read-only/insert-only transactions never
        // call begin_commit; the ledger must not wait on those gaps.
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        reg.begin_commit(10);
        reg.begin_commit(40); // 11..=39 never begun
        let r2 = reg.clone();
        let h = std::thread::spawn(move || r2.snapshot_barrier(25));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!h.is_finished(), "barrier(25) returned while commit 10 pending");
        reg.end_commit(10);
        // Barrier at 25 must pass with 40 still pending.
        h.join().unwrap();
        assert_eq!(reg.applied_watermark(), 10);
        // A barrier above the pending 40 still waits.
        let r3 = reg.clone();
        let h2 = std::thread::spawn(move || r3.snapshot_barrier(45));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!h2.is_finished(), "barrier(45) returned while commit 40 pending");
        reg.end_commit(40);
        h2.join().unwrap();
        assert_eq!(reg.applied_watermark(), 40);
    }

    #[test]
    fn end_commit_is_idempotent_and_ignores_unknown_ts() {
        let reg = WriteConflictRegistry::new();
        reg.begin_commit(10);
        reg.begin_commit(20);
        reg.end_commit(999); // never begun: no-op
        reg.end_commit(20);
        reg.end_commit(20); // double end: no-op (pending_count stays balanced)
        assert_eq!(reg.applied_watermark(), 0);
        reg.end_commit(10);
        assert_eq!(reg.applied_watermark(), 20);
        // pending_count balanced => barrier fast path returns immediately.
        reg.snapshot_barrier(u64::MAX);
        assert_eq!(reg.pending_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cross_thread_end_commit_marks_via_scan() {
        // The same-thread hint misses when another thread ends the commit;
        // the window scan must find and mark the entry.
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        reg.begin_commit(10);
        let r2 = reg.clone();
        std::thread::spawn(move || r2.end_commit(10)).join().unwrap();
        assert_eq!(reg.applied_watermark(), 10);
        reg.snapshot_barrier(u64::MAX);
    }

    #[test]
    fn ring_overflow_blocks_begin_until_straggler_applies() {
        // Fill the ring behind one stalled commit; the next begin_commit
        // must wait for the straggler, then proceed.
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        reg.begin_commit(1); // the straggler
        for i in 0..(INFLIGHT_RING_SIZE as u64 - 1) {
            let ts = 2 + i;
            reg.begin_commit(ts);
            reg.end_commit(ts); // applied, but unreclaimable behind ts=1
        }
        let r2 = reg.clone();
        let h = std::thread::spawn(move || {
            let ts = 2 + INFLIGHT_RING_SIZE as u64;
            r2.begin_commit(ts); // ring full: must block
            r2.end_commit(ts);
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(!h.is_finished(), "begin_commit proceeded on a full ring");
        reg.end_commit(1);
        h.join().unwrap();
        reg.snapshot_barrier(u64::MAX);
        assert_eq!(reg.pending_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn concurrent_begin_end_barrier_stress() {
        // Hammer the ledger from committers + barrier threads and verify
        // every barrier respects the contract (no snapshot passes a pending
        // commit at-or-below its ts). Timestamp allocation and begin_commit
        // share one mutex, mirroring the engine timestamp lock (commit
        // announcement is atomic with ts allocation; see
        // StorageEngine::next_commit_timestamp).
        let reg = std::sync::Arc::new(WriteConflictRegistry::new());
        let alloc = std::sync::Arc::new(Mutex::new(0u64));
        let committers = 8;
        let per_thread = 500;
        std::thread::scope(|s| {
            for _ in 0..committers {
                let reg = std::sync::Arc::clone(&reg);
                let alloc = std::sync::Arc::clone(&alloc);
                s.spawn(move || {
                    for i in 0..per_thread {
                        let ts = {
                            let mut next = alloc.lock();
                            *next += 1;
                            reg.begin_commit(*next);
                            *next
                        };
                        if i % 3 == 0 {
                            std::thread::yield_now();
                        }
                        reg.end_commit(ts);
                    }
                });
            }
            for _ in 0..4 {
                let reg = std::sync::Arc::clone(&reg);
                let alloc = std::sync::Arc::clone(&alloc);
                s.spawn(move || {
                    for _ in 0..per_thread {
                        let snap = {
                            let mut next = alloc.lock();
                            *next += 1;
                            *next
                        };
                        reg.snapshot_barrier(snap);
                        // After the barrier no pending commit <= snap exists
                        // (commits announced later carry higher timestamps,
                        // so this stays false forever).
                        assert!(
                            !reg.has_pending_at_or_below(snap),
                            "barrier passed a pending commit <= its snapshot"
                        );
                    }
                });
            }
        });
        reg.snapshot_barrier(u64::MAX);
        assert_eq!(reg.pending_count.load(Ordering::Acquire), 0, "ledger must drain");
        // Quiescent now: one reclaim pass must drain the ring completely.
        reg.try_reclaim();
        assert_eq!(
            reg.tail.load(Ordering::Acquire),
            reg.head.load(Ordering::Acquire),
            "ring must drain"
        );
    }
}
