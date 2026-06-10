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
//!   clean (the lost-update leak this versions fixes). The wait costs one
//!   atomic load when no commit is in flight.
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
    /// Commit timestamps allocated but not yet applied to storage.
    inflight_commits: DashMap<u64, ()>,
    /// Fast-path counter mirroring `inflight_commits` size.
    inflight_count: AtomicUsize,
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

    /// Snapshot barrier: spin until every in-flight commit at or below
    /// `snapshot_ts` has been applied. One atomic load when nothing is in
    /// flight (the overwhelmingly common case).
    pub fn snapshot_barrier(&self, snapshot_ts: u64) {
        loop {
            if self.inflight_count.load(Ordering::Acquire) == 0 {
                return;
            }
            let pending = self
                .inflight_commits
                .iter()
                .any(|entry| *entry.key() <= snapshot_ts);
            if !pending {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// Announce a commit whose write set is about to be validated/applied.
    /// MUST be paired with `end_commit` on every path (use try/finally
    /// discipline in the caller).
    pub fn begin_commit(&self, commit_ts: u64) {
        self.inflight_commits.insert(commit_ts, ());
        self.inflight_count.fetch_add(1, Ordering::AcqRel);
    }

    /// The commit's RocksDB write has been applied (or the commit aborted).
    pub fn end_commit(&self, commit_ts: u64) {
        if self.inflight_commits.remove(&commit_ts).is_some() {
            self.inflight_count.fetch_sub(1, Ordering::AcqRel);
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
}
