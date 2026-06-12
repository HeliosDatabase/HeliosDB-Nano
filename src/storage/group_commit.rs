//! R1.3 phase 2: leader/follower group commit.
//!
//! Durable committers write their RocksDB WriteBatch UNSYNCED, then call
//! [`GroupCommitter::wait_durable`]. The first waiter becomes the cohort
//! leader: it optionally accumulates joiners for a small window
//! (`[storage] group_commit_window_us`, default 200µs, 0 = no wait),
//! waits out any in-flight flush, then issues ONE `flush_wal(sync)` that
//! covers the whole cohort and wakes every follower.
//!
//! Correctness: RocksDB's WAL is sequential. A commit's batch enters the
//! WAL before the commit joins a cohort (program order in
//! `Transaction::commit_with_timestamp`), and the cohort's flush is
//! issued by the leader strictly after the cohort is closed, so the fsync
//! persists every member's bytes. A later generation's fsync likewise
//! covers all earlier generations.
//!
//! Pipelining (two-generation group commit): while generation N's fsync
//! is in flight, generation N+1 accumulates; its leader blocks until N's
//! flush completes before issuing its own. Under load every fsync covers
//! all commits that arrived during the previous fsync, so durable
//! throughput scales with the arrival rate instead of the fsync rate —
//! and beyond what RocksDB's internal write groups capture, because the
//! accumulation happens *across* RocksDB write calls, not within one.
//!
//! The accumulation window is slept *before* waiting on the in-flight
//! flush, so at saturation (fsync latency >= window) the window is fully
//! hidden behind the previous generation's fsync and adds no serial
//! latency; it only buys extra batching when commits arrive while the
//! WAL is otherwise idle.
//!
//! Error contract: a failed group fsync fails the leader and every
//! follower of that generation. Failures are recorded PER GENERATION
//! (not just "last error"), so a follower of failed generation N that is
//! scheduled late — after generation N+1 has also flushed — still sees
//! its own generation's error. RocksDB additionally records WAL-sync
//! failures as background errors and rejects subsequent writes.

use parking_lot::{Condvar, Mutex};
use std::time::Duration;

/// Leader/follower group committer with an accumulation window.
pub struct GroupCommitter {
    state: Mutex<State>,
    cv: Condvar,
    window: Duration,
}

struct State {
    /// Generation currently accepting joiners. Starts at 1.
    accumulating: u64,
    /// Highest generation whose flush has finished (successfully or not).
    completed: u64,
    /// A leader has claimed `accumulating` and is collecting its cohort.
    leader_present: bool,
    /// A `flush_wal` call is in flight (at most one at a time).
    flush_in_progress: bool,
    /// Failed flushes by generation. Normally empty (fsync failures are
    /// rare and usually wedge RocksDB via its background error); pruned to
    /// the trailing `ERROR_RETENTION_GENS` generations so a pathological
    /// failure storm cannot grow it unboundedly.
    failed: std::collections::BTreeMap<u64, String>,
}

/// How many generations of flush failures to retain for late-waking
/// followers (a follower only ever needs its own generation, which is at
/// most one flush behind under normal scheduling).
const ERROR_RETENTION_GENS: u64 = 1024;

impl GroupCommitter {
    pub fn new(window: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                accumulating: 1,
                completed: 0,
                leader_present: false,
                flush_in_progress: false,
                failed: std::collections::BTreeMap::new(),
            }),
            cv: Condvar::new(),
            window,
        }
    }

    /// Accumulation window this committer was configured with.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Block until a group fsync covering the caller's already-written WAL
    /// bytes has completed. The caller's batch MUST have been written
    /// (unsynced) before calling. Exactly one waiter per generation — the
    /// leader — executes `flush`; everyone else parks on the condvar.
    pub fn wait_durable<F>(&self, flush: F) -> std::result::Result<(), String>
    where
        F: FnOnce() -> std::result::Result<(), String>,
    {
        let mut st = self.state.lock();
        let my_gen = st.accumulating;
        if st.leader_present {
            // Follower: durable once our generation's flush has finished.
            while st.completed < my_gen {
                self.cv.wait(&mut st);
            }
            return match st.failed.get(&my_gen) {
                Some(msg) => Err(msg.clone()),
                None => Ok(()),
            };
        }

        // Leader for `my_gen`.
        st.leader_present = true;

        // 1) Accumulation window: let near-simultaneous committers join.
        //    Slept before waiting on the previous flush so it overlaps (and
        //    at saturation is fully hidden by) that fsync.
        if !self.window.is_zero() {
            drop(st);
            std::thread::sleep(self.window);
            st = self.state.lock();
        }

        // 2) One flush at a time: wait out the previous generation's fsync.
        //    Commits keep arriving and joining this cohort while we wait.
        while st.flush_in_progress {
            self.cv.wait(&mut st);
        }

        // 3) Close the cohort; later arrivals form the next generation.
        debug_assert_eq!(st.accumulating, my_gen);
        st.accumulating = my_gen + 1;
        st.leader_present = false;
        st.flush_in_progress = true;
        drop(st);

        let result = flush();

        let mut st = self.state.lock();
        st.flush_in_progress = false;
        st.completed = st.completed.max(my_gen);
        if let Err(msg) = &result {
            st.failed.insert(my_gen, msg.clone());
            let cutoff = st.completed.saturating_sub(ERROR_RETENTION_GENS);
            st.failed.retain(|gen, _| *gen > cutoff);
        }
        self.cv.notify_all();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn single_committer_flushes_once() {
        let gc = GroupCommitter::new(Duration::ZERO);
        let flushes = AtomicUsize::new(0);
        gc.wait_durable(|| {
            flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cohort_shares_one_flush() {
        // With a generous window, N concurrent committers must complete with
        // far fewer flushes than committers (the leader's flush covers the
        // cohort). Bound loosely to stay robust on loaded CI machines.
        let gc = Arc::new(GroupCommitter::new(Duration::from_millis(50)));
        let flushes = Arc::new(AtomicUsize::new(0));
        let threads = 8;
        let mut handles = Vec::new();
        for _ in 0..threads {
            let gc = Arc::clone(&gc);
            let flushes = Arc::clone(&flushes);
            handles.push(std::thread::spawn(move || {
                gc.wait_durable(|| {
                    flushes.fetch_add(1, Ordering::SeqCst);
                    // Make the flush slow enough that stragglers join the
                    // next generation rather than each leading their own.
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                })
                .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let n = flushes.load(Ordering::SeqCst);
        assert!(n >= 1 && n < threads, "expected grouped flushes, got {n} for {threads} committers");
    }

    #[test]
    fn flush_error_propagates_to_whole_generation() {
        let gc = Arc::new(GroupCommitter::new(Duration::from_millis(30)));
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let gc = Arc::clone(&gc);
            let attempts = Arc::clone(&attempts);
            handles.push(std::thread::spawn(move || {
                gc.wait_durable(|| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    Err("disk on fire".to_string())
                })
            }));
        }
        let mut errs = 0;
        for h in handles {
            if h.join().unwrap().is_err() {
                errs += 1;
            }
        }
        // Every member of a failed generation sees the error; members that
        // landed in a later (also failing) generation see theirs.
        assert_eq!(errs, 4);
    }

    #[test]
    fn generations_advance_after_completion() {
        // Sequential committers each lead their own generation.
        let gc = GroupCommitter::new(Duration::ZERO);
        for _ in 0..3 {
            gc.wait_durable(|| Ok(())).unwrap();
        }
        let st = gc.state.lock();
        assert_eq!(st.accumulating, 4);
        assert_eq!(st.completed, 3);
        assert!(!st.leader_present);
        assert!(!st.flush_in_progress);
    }

    #[test]
    fn error_does_not_poison_next_generation() {
        let gc = GroupCommitter::new(Duration::ZERO);
        assert!(gc.wait_durable(|| Err("boom".into())).is_err());
        assert!(gc.wait_durable(|| Ok(())).is_ok());
    }

    #[test]
    fn failures_are_recorded_per_generation() {
        // Back-to-back failing generations must each keep their own error:
        // a follower of failed generation N that wakes late (after N+1 also
        // flushed) must still see N's failure, not N+1's — and never Ok.
        let gc = GroupCommitter::new(Duration::ZERO);
        assert!(gc.wait_durable(|| Err("gen1 fsync lost".into())).is_err());
        assert!(gc.wait_durable(|| Err("gen2 fsync lost".into())).is_err());
        assert!(gc.wait_durable(|| Ok(())).is_ok());
        let st = gc.state.lock();
        assert_eq!(st.failed.get(&1).map(String::as_str), Some("gen1 fsync lost"));
        assert_eq!(st.failed.get(&2).map(String::as_str), Some("gen2 fsync lost"));
        assert!(!st.failed.contains_key(&3));
    }
}
