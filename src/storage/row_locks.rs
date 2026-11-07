//! Spec R5.4 (P12): per-row write locks, and the per-table change epochs that tell a
//! statement whether a row it read can have moved since.
//!
//! # Why
//!
//! An UPDATE reads a row, computes the new image from it and writes the image back.
//! Two statements doing that to one row at once both read the same old image, and
//! the second write throws the first away: three clients running
//! `UPDATE t SET n = n + 1 WHERE id = 1` were told 576 times that their increment
//! had committed, and the counter ended at 426. R5.1's table lock does not order
//! them: every writer of a table holds it shared. PostgreSQL's READ COMMITTED does
//! not lose those: an UPDATE or DELETE locks each row it is about to change until
//! its transaction ends, a second writer of the row waits for the first, and then
//! evaluates its WHERE clause and its assignments again over the row the first one
//! committed (EvalPlanQual). This module is that lock.
//!
//! # The protocol
//!
//! A writer of an existing row (UPDATE, DELETE, the `ON CONFLICT DO UPDATE` leg, the
//! fast PK paths) takes the row's lock for its owner before it computes the new
//! image, and holds it until its write is visible: a transaction until its commit
//! has been applied (or until it ends), a fast path until its one write group has
//! been applied. The image it changes is the one committed when it held the lock.
//! Re-reading every row under its lock would cost a point read per row of a bulk
//! UPDATE, so the table's change epoch decides: the statement loads it BEFORE it
//! reads its rows, and every writer bumps it AFTER its write is visible and BEFORE it
//! lets its row locks go. A row the statement read before somebody else's write was
//! visible is then always found stale, once its lock is held:
//!
//! ```text
//! writer:   write (visible); bump epoch;          unlock rows
//! reader:   load epoch;      read rows;            lock row; load epoch again
//! ```
//!
//! The unlock and the lock are one mutex, so the second load sees the bump. If the
//! reader's first load already saw it, its read came after the write. An unchanged
//! epoch proves the row read is the row committed; a moved one only says "read it
//! again", so a collision of two tables' epoch slots costs a read, never a lost write.
//!
//! A transaction that validates (REPEATABLE READ, SERIALIZABLE, the embedded handle's
//! own transactions) does not re-evaluate: a row that moved under it is a
//! serialization failure, 40001, as in PostgreSQL. Its first-committer-wins check at
//! COMMIT stays as it was.
//!
//! # Waiting
//!
//! A waiter sleeps on its shard's condition variable, off the async workers
//! ([`crate::runtime_offload::off_worker`]), bounded like a table-lock wait: the
//! session's `lock_timeout` (55P03, [`Error::row_lock_timeout`]), its
//! `statement_timeout` and a cancel request (57014). A wait that would close a cycle
//! of waiters is a deadlock (40P01), refused at once to the waiter that closes it.
//! An owner only waits for one row at a time, and lets its rows go only when it ends,
//! so the graph's edges always point at owners that are still running, or at owners
//! that ended and can no longer wait for anything: a stale edge never makes a cycle.
//!
//! # Lock order
//!
//! Inside the statement-long locks of `storage::branch_freeze` ("Lock order", 2): a
//! statement holds its table locks before it reads, so before any row lock, and holds
//! no stripe, registry or hook lock of the freeze while it waits for a row. Within one
//! shard the shard's mutex is taken before the wait graph's, never the other way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::{Error, Result};

/// How many independently locked maps the rows are spread over.
const SHARDS: usize = 64;

/// How many change epochs the tables are spread over.
const EPOCH_SLOTS: usize = 1024;

/// The longest a waiter sleeps before it looks at its deadlines and the cancel flag.
const WAIT_TICK: Duration = Duration::from_millis(50);

/// The longest chain of waiters the deadlock check follows.
const MAX_WAIT_CHAIN: usize = 128;

/// Owners of fast-path writes, which have no transaction: above every transaction id
/// (`storage::transaction`'s counter starts at 1 and counts up), so the two never meet.
static NEXT_STATEMENT_OWNER: AtomicU64 = AtomicU64::new(1 << 62);

/// A fresh owner for one fast-path write.
pub fn statement_owner() -> u64 {
    NEXT_STATEMENT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// This thread's identity for [`RowLocks::bind_owner_to_this_thread`]: never 0, never
/// handed to another thread, not reused after the thread ends.
fn current_thread_token() -> u64 {
    static NEXT_THREAD_TOKEN: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static THREAD_TOKEN: u64 = NEXT_THREAD_TOKEN.fetch_add(1, Ordering::Relaxed);
    }
    THREAD_TOKEN.with(|token| *token)
}

/// FNV-1a: a table's identity in a key and its epoch slot. A collision over-locks or
/// costs a read; it never lets two writers of one row through.
fn table_hash(table: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in table.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One row's identity: the hash of its table's name and its row id (main's rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowKey {
    table: u64,
    row: u64,
}

impl RowKey {
    /// The key of row `row_id` of `table`.
    pub fn new(table: &str, row_id: u64) -> Self {
        Self {
            table: table_hash(table),
            row: row_id,
        }
    }

    fn shard(self) -> usize {
        let mixed = self.table ^ self.row.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (mixed % SHARDS as u64) as usize
    }
}

#[derive(Debug, Default)]
struct ShardState {
    /// Row -> owner.
    rows: HashMap<RowKey, u64>,
    /// Owners sleeping on this shard's condition variable.
    waiting: usize,
}

#[derive(Debug, Default)]
struct Shard {
    state: Mutex<ShardState>,
    released: Condvar,
}

/// What a waiter is bounded by.
#[derive(Debug, Clone, Copy)]
pub struct RowLockBounds {
    /// `lock_timeout`: 55P03 after it.
    pub lock_timeout: Duration,
    /// `statement_timeout`, counted from the start of the wait: 57014 after it.
    pub statement_timeout: Option<Duration>,
}

/// The row write locks and change epochs of one engine.
#[derive(Debug)]
pub struct RowLocks {
    shards: Box<[Shard]>,
    /// Waiter -> the owner it waits for: edges of the wait graph, while contended.
    waits_for: Mutex<HashMap<u64, u64>>,
    epochs: Box<[AtomicU64]>,
    /// Acquisitions that had to wait. A test surface.
    contended: AtomicU64,
    /// Waits refused as deadlocks. A test surface.
    deadlocks: AtomicU64,
    /// Owners only one thread can end -> that thread: the embedded handle's own
    /// transaction (`BEGIN` on the handle), whose `COMMIT` only the thread that opened
    /// it may run. A statement on that thread waiting for one of its rows would wait
    /// for itself.
    thread_bound: Mutex<HashMap<u64, u64>>,
}

impl Default for RowLocks {
    fn default() -> Self {
        Self::new()
    }
}

impl RowLocks {
    /// No row locked, every epoch at zero.
    pub fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Shard::default()).collect(),
            waits_for: Mutex::new(HashMap::new()),
            epochs: (0..EPOCH_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            contended: AtomicU64::new(0),
            deadlocks: AtomicU64::new(0),
            thread_bound: Mutex::new(HashMap::new()),
        }
    }

    /// `owner` can only be ended by the calling thread (see `thread_bound`).
    pub fn bind_owner_to_this_thread(&self, owner: u64) {
        self.thread_bound.lock().insert(owner, current_thread_token());
    }

    /// `owner` ended.
    pub fn unbind_owner(&self, owner: u64) {
        self.thread_bound.lock().remove(&owner);
    }

    /// Is `holder` an owner only this thread can end? Asked on contention only.
    fn held_by_this_thread(&self, holder: u64) -> bool {
        self.thread_bound.lock().get(&holder) == Some(&current_thread_token())
    }

    /// In range: `RowKey::shard` is modulo the slice's length.
    #[inline]
    #[allow(clippy::indexing_slicing)]
    fn shard(&self, key: RowKey) -> &Shard {
        &self.shards[key.shard()]
    }

    /// In range: modulo the slice's length.
    #[inline]
    #[allow(clippy::indexing_slicing)]
    fn epoch_slot(&self, table: &str) -> &AtomicU64 {
        &self.epochs[(table_hash(table) % EPOCH_SLOTS as u64) as usize]
    }

    /// `table`'s change epoch. Load it BEFORE reading the rows a statement may change
    /// (module documentation).
    #[inline]
    pub fn table_epoch(&self, table: &str) -> u64 {
        self.epoch_slot(table).load(Ordering::SeqCst)
    }

    /// Has a row of `table` (or of a table sharing its slot) been changed since
    /// `epoch` was loaded? Asked once the row's lock is held.
    #[inline]
    pub fn changed_since(&self, table: &str, epoch: u64) -> bool {
        self.table_epoch(table) != epoch
    }

    /// Rows of `table` changed and the change is visible. Call BEFORE the writer's row
    /// locks are released (module documentation).
    #[inline]
    pub fn note_rows_changed(&self, table: &str) {
        self.epoch_slot(table).fetch_add(1, Ordering::SeqCst);
    }

    /// Acquisitions that had to wait, so far.
    pub fn contended(&self) -> u64 {
        self.contended.load(Ordering::Relaxed)
    }

    /// Waits refused as deadlocks, so far.
    pub fn deadlocks(&self) -> u64 {
        self.deadlocks.load(Ordering::Relaxed)
    }

    /// Lock row `row_id` of `table` for `owner`, waiting for another owner's hold to
    /// end. `Ok(true)` when this call took it, `Ok(false)` when `owner` held it already.
    pub fn lock(&self, owner: u64, table: &str, row_id: u64, bounds: RowLockBounds) -> Result<bool> {
        let key = RowKey::new(table, row_id);
        {
            let mut state = self.shard(key).state.lock();
            match state.rows.get(&key) {
                None => {
                    state.rows.insert(key, owner);
                    return Ok(true);
                }
                Some(holder) if *holder == owner => return Ok(false),
                Some(_) => {}
            }
        }
        self.lock_contended(owner, key, table, row_id, bounds)
    }

    /// Another owner holds the row.
    #[cold]
    fn lock_contended(&self, owner: u64, key: RowKey, table: &str, row_id: u64, bounds: RowLockBounds) -> Result<bool> {
        self.contended.fetch_add(1, Ordering::Relaxed);
        let shard = self.shard(key);
        let started = Instant::now();
        let statement_deadline = bounds
            .statement_timeout
            .and_then(|timeout| started.checked_add(timeout));
        crate::runtime_offload::off_worker(|| {
            let mut state = shard.state.lock();
            let outcome = loop {
                let holder = match state.rows.get(&key) {
                    None => {
                        state.rows.insert(key, owner);
                        break Ok(true);
                    }
                    Some(holder) if *holder == owner => break Ok(false),
                    Some(holder) => *holder,
                };
                // A wait that can never end: the holder is a transaction only this
                // thread can end, and this thread is busy waiting for it.
                if self.held_by_this_thread(holder) {
                    break Err(Error::row_held_by_this_threads_transaction(table, row_id));
                }
                if crate::session_take_cancel_request() {
                    break Err(Error::query_cancelled(format!(
                        "canceling statement due to user request while waiting for a lock on a row of table {table}"
                    )));
                }
                if statement_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    break Err(Error::query_timeout(format!(
                        "canceling statement due to statement timeout while waiting for a lock on a row of table \
                         {table}"
                    )));
                }
                if self.closes_cycle(owner, holder) {
                    self.deadlocks.fetch_add(1, Ordering::Relaxed);
                    break Err(Error::deadlock(format!(
                        "detected deadlock: waiting for row {row_id} of table {table}, held by a transaction that \
                         waits, directly or through others, for a row this one holds; retry the transaction"
                    )));
                }
                let waited = started.elapsed();
                if waited >= bounds.lock_timeout {
                    break Err(Error::row_lock_timeout(table, row_id, waited));
                }
                let mut tick = WAIT_TICK.min(bounds.lock_timeout.saturating_sub(waited));
                if let Some(deadline) = statement_deadline {
                    tick = tick.min(deadline.saturating_duration_since(Instant::now()));
                }
                state.waiting += 1;
                let _ = shard.released.wait_for(&mut state, tick);
                state.waiting = state.waiting.saturating_sub(1);
            };
            drop(state);
            self.waits_for.lock().remove(&owner);
            outcome
        })
    }

    /// Record that `owner` waits for `holder`, and say whether that closes a cycle.
    /// A refused wait leaves no edge behind.
    fn closes_cycle(&self, owner: u64, holder: u64) -> bool {
        let mut graph = self.waits_for.lock();
        graph.insert(owner, holder);
        let mut current = holder;
        for _ in 0..MAX_WAIT_CHAIN {
            if current == owner {
                graph.remove(&owner);
                return true;
            }
            match graph.get(&current) {
                Some(next) => current = *next,
                None => return false,
            }
        }
        false
    }

    /// Let `owner`'s hold of `key` go (nothing if another owner holds it).
    pub fn unlock(&self, owner: u64, key: RowKey) {
        let shard = self.shard(key);
        let mut state = shard.state.lock();
        if state.rows.get(&key).is_some_and(|holder| *holder == owner) {
            state.rows.remove(&key);
            if state.waiting > 0 {
                shard.released.notify_all();
            }
        }
    }

    /// Let every one of `keys` go, for `owner`.
    pub fn unlock_all(&self, owner: u64, keys: &[RowKey]) {
        for key in keys {
            self.unlock(owner, *key);
        }
    }

    /// Who holds row `row_id` of `table`, for the tests' handshakes.
    pub fn holder(&self, table: &str, row_id: u64) -> Option<u64> {
        let key = RowKey::new(table, row_id);
        self.shard(key).state.lock().rows.get(&key).copied()
    }

    /// How many owners wait on any row right now, for the tests' handshakes.
    pub fn waiting(&self) -> usize {
        self.shards.iter().map(|shard| shard.state.lock().waiting).sum()
    }
}

/// A fast-path write's hold of one row, released when it drops: after the write and
/// the epoch bump.
#[derive(Debug)]
pub struct RowLockGuard<'a> {
    locks: &'a RowLocks,
    owner: u64,
    key: RowKey,
    taken: bool,
}

impl<'a> RowLockGuard<'a> {
    /// Lock row `row_id` of `table` for a fresh owner of its own.
    pub fn acquire(locks: &'a RowLocks, table: &str, row_id: u64, bounds: RowLockBounds) -> Result<Self> {
        let owner = statement_owner();
        let taken = locks.lock(owner, table, row_id, bounds)?;
        Ok(Self {
            locks,
            owner,
            key: RowKey::new(table, row_id),
            taken,
        })
    }
}

impl Drop for RowLockGuard<'_> {
    fn drop(&mut self) {
        if self.taken {
            self.locks.unlock(self.owner, self.key);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn bounds(ms: u64) -> RowLockBounds {
        RowLockBounds {
            lock_timeout: Duration::from_millis(ms),
            statement_timeout: None,
        }
    }

    #[test]
    fn a_held_row_is_reentrant_for_its_owner_and_free_after_unlock() {
        let locks = RowLocks::new();
        assert!(locks.lock(7, "t", 1, bounds(100)).unwrap());
        assert!(
            !locks.lock(7, "t", 1, bounds(100)).unwrap(),
            "the owner holds it already"
        );
        assert_eq!(locks.holder("t", 1), Some(7));
        locks.unlock(7, RowKey::new("t", 1));
        assert_eq!(locks.holder("t", 1), None);
        assert!(locks.lock(8, "t", 1, bounds(100)).unwrap());
    }

    #[test]
    fn a_second_owner_waits_for_the_first_and_times_out_55p03() {
        let locks = RowLocks::new();
        assert!(locks.lock(1, "t", 5, bounds(100)).unwrap());
        let refused = locks.lock(2, "t", 5, bounds(30)).unwrap_err().to_string();
        assert!(refused.contains(crate::error::ROW_LOCK_TIMEOUT_MARKER), "{refused}");
        assert_eq!(locks.holder("t", 5), Some(1), "the refused waiter took nothing");
        assert_eq!(locks.contended(), 1);
    }

    #[test]
    fn a_waiter_gets_the_row_when_its_holder_lets_go() {
        let locks = Arc::new(RowLocks::new());
        assert!(locks.lock(1, "t", 9, bounds(100)).unwrap());
        let waiter = {
            let locks = Arc::clone(&locks);
            std::thread::spawn(move || locks.lock(2, "t", 9, bounds(10_000)))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while locks.waiting() == 0 {
            assert!(Instant::now() < deadline, "the waiter never started waiting");
            std::thread::yield_now();
        }
        locks.unlock(1, RowKey::new("t", 9));
        assert!(waiter.join().unwrap().unwrap(), "the waiter took the row");
        assert_eq!(locks.holder("t", 9), Some(2));
    }

    #[test]
    fn two_owners_waiting_for_each_others_rows_are_a_deadlock_40p01() {
        let locks = Arc::new(RowLocks::new());
        assert!(locks.lock(1, "t", 1, bounds(100)).unwrap());
        assert!(locks.lock(2, "t", 2, bounds(100)).unwrap());
        let first = {
            let locks = Arc::clone(&locks);
            std::thread::spawn(move || locks.lock(1, "t", 2, bounds(10_000)))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while locks.waiting() == 0 {
            assert!(Instant::now() < deadline, "the first waiter never started waiting");
            std::thread::yield_now();
        }
        let refused = locks.lock(2, "t", 1, bounds(10_000)).unwrap_err();
        assert!(
            refused.to_string().to_ascii_lowercase().contains("deadlock"),
            "{refused}"
        );
        assert_eq!(locks.deadlocks(), 1);
        locks.unlock(2, RowKey::new("t", 2));
        assert!(first.join().unwrap().unwrap(), "the survivor gets its row");
    }

    #[test]
    fn a_row_held_by_a_transaction_only_this_thread_can_end_is_refused_at_once() {
        let locks = RowLocks::new();
        assert!(locks.lock(1, "t", 4, bounds(100)).unwrap());
        locks.bind_owner_to_this_thread(1);
        let started = Instant::now();
        let refused = locks.lock(2, "t", 4, bounds(60_000)).unwrap_err().to_string();
        assert!(refused.contains("serialization failure"), "{refused}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "refused at once, not at the timeout"
        );
        // Another thread may wait for it: the binding is this thread's.
        let other = std::thread::scope(|scope| scope.spawn(|| locks.lock(3, "t", 4, bounds(20))).join());
        let other = other.unwrap().unwrap_err().to_string();
        assert!(other.contains(crate::error::ROW_LOCK_TIMEOUT_MARKER), "{other}");
        locks.unbind_owner(1);
    }

    #[test]
    fn an_epoch_moves_only_when_rows_of_its_table_changed() {
        let locks = RowLocks::new();
        let before = locks.table_epoch("t");
        assert!(!locks.changed_since("t", before));
        locks.note_rows_changed("t");
        assert!(locks.changed_since("t", before));
    }

    #[test]
    fn a_guard_lets_its_row_go_when_it_drops() {
        let locks = RowLocks::new();
        {
            let _guard = RowLockGuard::acquire(&locks, "t", 3, bounds(100)).unwrap();
            assert!(locks.holder("t", 3).is_some());
        }
        assert_eq!(locks.holder("t", 3), None);
    }
}
