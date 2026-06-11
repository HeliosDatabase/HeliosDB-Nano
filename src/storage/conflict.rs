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
//!   R1.3 phase 2 (replacing the R0.2 bounded-spin barrier): in-flight
//!   commits live in a commit-ordered ledger (`BTreeMap<commit_ts, applied>`
//!   — insertion is monotonic because commit timestamps are allocated under
//!   the engine timestamp lock). `end_commit` marks its entry applied and
//!   pops the contiguous applied prefix, advancing `applied_watermark` =
//!   highest commit_ts such that ALL commits <= it have applied.
//!   `snapshot_barrier(ts)` returns immediately when nothing is pending or
//!   `ts <= applied_watermark` (two atomic loads), and otherwise parks on a
//!   condvar signaled by `end_commit` — no spinning, so barrier waiters can
//!   never starve the committing threads they wait on (the 680x convoy the
//!   R0.2 spin fix worked around).
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
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::Key;

/// Prune the recent-writes map every N recorded commits.
const PRUNE_INTERVAL_COMMITS: u64 = 4096;

#[derive(Default)]
pub struct WriteConflictRegistry {
    /// Last committed (or in-flight) write timestamp per data key.
    recent_writes: DashMap<Key, u64>,
    /// Snapshots of active VALIDATING transactions (txn_id -> snapshot_ts).
    active_snapshots: DashMap<u64, u64>,
    /// Commit-ordered in-flight ledger: commit_ts -> applied?. Keys are
    /// strictly increasing at insertion (`begin_commit` runs inside the
    /// engine timestamp lock). `end_commit` marks entries applied and pops
    /// the contiguous applied prefix; entries that applied out of order
    /// stay (marked) until the gap before them fills.
    inflight: Mutex<BTreeMap<u64, bool>>,
    /// Signaled by `end_commit` whenever an entry becomes applied.
    inflight_cv: Condvar,
    /// Number of PENDING (begun, not yet ended) commits. 0 = barrier fast
    /// path: nothing to wait for.
    pending_count: AtomicUsize,
    /// Highest commit_ts such that every in-flight commit <= it has
    /// applied. Monotonic; second barrier fast path.
    applied_watermark: AtomicU64,
    /// Recorded commits since creation (drives pruning cadence).
    commit_count: AtomicU64,
}

impl WriteConflictRegistry {
    pub fn new() -> Self {
        Self::default()
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
    /// `snapshot_ts` has been applied. Two atomic loads when nothing is
    /// pending or the watermark already covers the snapshot (the
    /// overwhelmingly common cases); otherwise parks on a condvar that
    /// `end_commit` signals — no spinning (R1.3 phase 2).
    ///
    /// Correctness (unchanged contract from R0.2): a snapshot must never be
    /// taken covering an unapplied commit. Any commit with ts <= snapshot_ts
    /// ran `begin_commit` inside the engine timestamp lock BEFORE this
    /// snapshot's ts was allocated under the same lock, so its ledger entry
    /// (and `pending_count` increment) are visible here — the fast paths
    /// cannot miss it.
    pub fn snapshot_barrier(&self, snapshot_ts: u64) {
        if self.pending_count.load(Ordering::Acquire) == 0 {
            return;
        }
        if self.applied_watermark.load(Ordering::Acquire) >= snapshot_ts {
            return;
        }
        let mut inflight = self.inflight.lock();
        while inflight.range(..=snapshot_ts).any(|(_, applied)| !applied) {
            self.inflight_cv.wait(&mut inflight);
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
    /// `commit_ts` (inside the engine timestamp lock).
    pub fn begin_commit(&self, commit_ts: u64) {
        let mut inflight = self.inflight.lock();
        inflight.insert(commit_ts, false);
        self.pending_count.fetch_add(1, Ordering::AcqRel);
    }

    /// The commit's RocksDB write has been applied (or the commit aborted).
    /// Marks the ledger entry applied, advances the watermark past the
    /// contiguous applied prefix, and wakes barrier waiters. Tolerates
    /// unknown timestamps and double calls (no-ops).
    pub fn end_commit(&self, commit_ts: u64) {
        let mut inflight = self.inflight.lock();
        match inflight.get_mut(&commit_ts) {
            Some(applied) if !*applied => *applied = true,
            _ => return, // unknown ts or already ended
        }
        self.pending_count.fetch_sub(1, Ordering::AcqRel);
        // Advance the watermark past leading applied entries. Entries that
        // applied out of order surface here once the gap before them fills.
        let mut watermark = None;
        while let Some(entry) = inflight.first_entry() {
            if *entry.get() {
                watermark = Some(*entry.key());
                entry.remove();
            } else {
                break;
            }
        }
        if let Some(w) = watermark {
            self.applied_watermark.fetch_max(w, Ordering::AcqRel);
        }
        drop(inflight);
        // Wake all waiters: an entry became applied, so any barrier whose
        // last blocking entry this was can now pass (watermark advance
        // alone is not enough — a waiter below an out-of-order gap may
        // unblock without the prefix moving).
        self.inflight_cv.notify_all();
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
        // The gap fills: the whole applied prefix pops in one advance.
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
                        // After the barrier no pending commit <= snap exists.
                        let inflight = reg.inflight.lock();
                        assert!(
                            inflight.range(..=snap).all(|(_, applied)| *applied),
                            "barrier passed a pending commit <= its snapshot"
                        );
                    }
                });
            }
        });
        reg.snapshot_barrier(u64::MAX);
        assert!(reg.inflight.lock().is_empty(), "ledger must drain");
    }
}
