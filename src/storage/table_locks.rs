//! Spec R5.1 (`docs/plans/specs/R5_1_writer_lock.md`): the per-table writer lock.
//!
//! Every concurrency finding of the B3 reviews was one defect: nothing kept a
//! writer out of a table while something rewrote it. This is that lock: per
//! engine, one fair shared/exclusive lock per `(level, table)`, `level` being the
//! branch the statement runs on (main = [`MAIN_LEVEL`]), so a branch never blocks
//! main.
//!
//! * **Shared**: a statement or a transaction that may write the table, from before
//!   it reads the rows it will change until the end of the statement (autocommit)
//!   or the transaction (explicit).
//! * **Exclusive**: DDL or a MERGE BRANCH that reads or rewrites the table's rows
//!   or catalog records.
//! * **Writer preference**: a waiting exclusive holds new shared requests back, so
//!   DDL cannot starve; what it waits for is bounded by the timeout.
//! * **Readers never touch it.**
//! * **Reentrant for the owner**: an owner set per transaction or statement. A
//!   statement that holds a table never waits for it again.
//! * **A transaction keeps an exclusive until it ends**, as in PostgreSQL: the second
//!   schema statement of a migration finds the table already its own.
//! * **An upgrade waits** (shared held, exclusive wanted by the same owner), ahead of
//!   the exclusive requests already queued, which are waiting for its hold. Refused
//!   at once only where waiting is certain to deadlock: another owner is already
//!   waiting to upgrade the same table, and each would wait for the other's hold.
//! * **Bounded**: every wait ends at the session's `lock_timeout`, else `[session]
//!   lock_timeout_ms` (a `lock_timeout` of 0 means that one, never an unbounded wait):
//!   55P03 naming the holder kind ([`Error::table_lock_timeout`]). It also looks, at a
//!   short interval, at the statement's `statement_timeout` and at a cancel request:
//!   57014, with the statement's work not begun.
//! * **Admission**: at most [`MAX_LOCK_WAITERS`] statements wait on one engine; past
//!   that, 55P03 at once ([`Error::table_lock_too_many_waiters`]).
//! * **Waits happen off the async workers**: every wait goes through
//!   [`crate::runtime_offload::off_worker`], whatever the caller, so a path that did
//!   not offload its statement cannot pin a worker either.
//!
//! # The cost of the hot path
//!
//! With no exclusive held or wanted on the table, a shared acquire is the owner set's
//! bookkeeping plus one atomic load, and it allocates nothing for up to
//! [`INLINE_HELD`] tables: the holders are not counted anywhere central. An owner
//! PUBLISHES a shared hold in its own set, and an exclusive request finds the
//! holders by scanning the owner sets. The two are one protocol on two facts,
//! the same Dekker pairing as the write gate's fence:
//!
//! ```text
//! shared:    publish in my set;  SeqCst fence;  load `pressure` of my table's bucket
//! exclusive: raise that bucket;  SeqCst fence;  scan every owner set
//! ```
//!
//! so either the shared request sees the pressure and takes the slow path, or the
//! exclusive request sees the hold and waits for it. An exclusive request is rare
//! and may be slow; the table entries, created on demand and removed when idle, only
//! exist while one is around. The pressure is kept per bucket of the table's hash
//! rather than per engine, so that a writer of another table neither takes the state
//! mutex nor wakes anybody: a statement that ends notifies only the tables it held
//! that have somebody waiting, each on that table's own condition variable.
//!
//! A statement's owner is one per thread, reused, so it allocates nothing: the
//! thread's cached owner moves into [`CURRENT`] for the statement and back. A
//! transaction's owner is created at its first write and registered once.
//!
//! The table in a key is a 64-bit hash of its name. A collision makes two tables
//! share a lock: over-locking, never a missing lock.
//!
//! # Detector
//!
//! [`check_row_key`], from `WriteGate`: a write of a row key (`data:{t}:`,
//! `v:{t}:`, `bdata:{L}:{t}:`) on a thread whose statement holds no lock on
//! `(level, t)` is a missed path. It is counted in every build. In test builds
//! (`debug_assertions` or `internal-tests`) it also panics, naming the table and the
//! caller, unless the thread is unwinding already: a second panic aborts the process,
//! and one finding must not cost a whole test binary. For the same reason a batch's
//! keys are only noted inside RocksDB's `extern "C"` callback, and reported once it has
//! returned. In release it warns once per table, and only looks at keys while an
//! exclusive lock is held or wanted somewhere in the process: a missing lock matters
//! only then, and the write path pays one load the rest of the time. What cannot race
//! says so with [`allow_unlocked_row_writes`]; see the spec for the list and the
//! reasons.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::panic::Location;
use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::{Error, Result};

/// The level of main, in a lock key (a branch's level is its id).
pub const MAIN_LEVEL: u64 = 1;

/// How many tables an owner holds before its set allocates.
const INLINE_HELD: usize = 4;

/// The longest an exclusive request sleeps before it looks again. A statement letting
/// go of a shared hold wakes it, unless it read a stale 0 pressure (the shared path's
/// release takes no fence): this is what that costs at worst.
const EXCLUSIVE_TICK: Duration = Duration::from_millis(5);

/// The longest a shared request sleeps before it looks again. Every release that can
/// let it through takes the state mutex and wakes it, so this only bounds how late it
/// sees its statement's deadline or a cancel. It is not 5 ms because hundreds of them
/// may be waiting at once, on nothing.
const SHARED_TICK: Duration = Duration::from_millis(50);

/// How many counters the exclusive pressure is spread over. A collision costs a writer
/// of another table the slow path, no more, and 64 makes one unlikely while a handful
/// of tables have an exclusive around.
const PRESSURE_BUCKETS: usize = 64;

/// The most statements that wait for a table lock at once on one engine; the next is
/// refused with 55P03 at once.
///
/// Each one pins a thread of Tokio's blocking pool (`block_in_place`, on a server, for
/// the whole statement): 512 of them by default, and when the pool is full a worker's
/// core cannot be handed over, so the whole server stops answering, the COMMIT that
/// would free them included. Half the pool: the other half stays for the statements
/// actually running. It is far above what a connection limit lets through (100 by
/// default), so a refusal means a REST burst or a raised limit behind one long
/// statement, where failing fast is what the clients need anyway.
pub const MAX_LOCK_WAITERS: usize = 256;

/// Exclusive locks held or wanted anywhere in this process: the release build's
/// detector looks at keys only while this is not zero.
static EXCLUSIVE_ANYWHERE: AtomicUsize = AtomicUsize::new(0);

/// Row writes the detector found without a lock, in every build.
static UNLOCKED_ROW_WRITES: AtomicU64 = AtomicU64::new(0);

/// Tables the release detector has warned about.
#[cfg(not(any(debug_assertions, feature = "internal-tests")))]
static WARNED_TABLES: std::sync::LazyLock<Mutex<std::collections::HashSet<Vec<u8>>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Unique across engines, so an owner never compares equal to another engine's.
static NEXT_OWNER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The owner this thread's statement acts for: its transaction's, or the
    /// thread's statement owner (moved here from [`THREAD_OWNER`] for the statement).
    static CURRENT: RefCell<Option<Arc<LockOwner>>> = const { RefCell::new(None) };
    /// This thread's statement owner, between statements.
    static THREAD_OWNER: RefCell<Option<Arc<LockOwner>>> = const { RefCell::new(None) };
    /// Why this thread's row writes cannot race (the detector's allow-list).
    static UNLOCKED_ALLOWED: Cell<Option<&'static str>> = const { Cell::new(None) };
    /// [`ExclusiveTableLocks`] alive on this thread: a replication wait inside one
    /// is deferred until the locks are released.
    static EXCLUSIVE_GUARDS: Cell<u32> = const { Cell::new(0) };
    /// The highest replication barrier deferred on this thread.
    static DEFERRED_BARRIER: Cell<Option<u64>> = const { Cell::new(None) };
}

/// One lock's identity: the level and the hash of the table's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableKey {
    level: u64,
    table: u64,
}

impl TableKey {
    /// FNV-1a over the name: no allocation, and a collision only over-locks.
    #[inline]
    pub(crate) fn new(level: u64, table: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in table {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self { level, table: hash }
    }

    #[inline]
    fn bucket(self) -> usize {
        ((self.table ^ self.level) % PRESSURE_BUCKETS as u64) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Copy)]
struct Held {
    key: TableKey,
    mode: LockMode,
    /// It was held shared before this owner upgraded it.
    was_shared: bool,
}

impl Held {
    const fn shared(key: TableKey) -> Self {
        Self {
            key,
            mode: LockMode::Shared,
            was_shared: true,
        }
    }
}

const NO_KEY: TableKey = TableKey { level: 0, table: 0 };

/// What one owner holds: inline for the first few tables, so a statement touching
/// one table allocates nothing.
#[derive(Debug)]
struct HeldSet {
    /// In use in `inline`; past that, `spill`.
    inline_len: usize,
    inline: [Held; INLINE_HELD],
    spill: Vec<Held>,
    /// How many of them are exclusive.
    exclusive: usize,
}

impl HeldSet {
    const fn new() -> Self {
        Self {
            inline_len: 0,
            inline: [Held::shared(NO_KEY); INLINE_HELD],
            spill: Vec::new(),
            exclusive: 0,
        }
    }

    #[inline]
    fn iter(&self) -> impl Iterator<Item = &Held> {
        self.inline.iter().take(self.inline_len).chain(self.spill.iter())
    }

    #[inline]
    fn find(&self, key: TableKey) -> Option<LockMode> {
        self.iter().find(|held| held.key == key).map(|held| held.mode)
    }

    #[inline]
    fn push(&mut self, held: Held) {
        if held.mode == LockMode::Exclusive {
            self.exclusive += 1;
        }
        match self.inline.get_mut(self.inline_len) {
            Some(slot) => {
                *slot = held;
                self.inline_len += 1;
            }
            None => self.spill.push(held),
        }
    }

    fn remove(&mut self, key: TableKey) {
        let at = self
            .inline
            .iter()
            .take(self.inline_len)
            .position(|held| held.key == key);
        let removed = if let Some(at) = at {
            let last = self.inline_len.saturating_sub(1);
            self.inline.swap(at, last);
            let removed = self.inline.get(last).copied();
            match self.spill.pop() {
                Some(moved) => {
                    if let Some(slot) = self.inline.get_mut(last) {
                        *slot = moved;
                    }
                }
                None => self.inline_len = last,
            }
            removed
        } else {
            self.spill
                .iter()
                .position(|held| held.key == key)
                .map(|at| self.spill.swap_remove(at))
        };
        if removed.is_some_and(|held| held.mode == LockMode::Exclusive) {
            self.exclusive = self.exclusive.saturating_sub(1);
        }
    }

    /// `key` becomes exclusive (it was shared).
    fn upgrade(&mut self, key: TableKey) {
        let held = self
            .inline
            .iter_mut()
            .take(self.inline_len)
            .chain(self.spill.iter_mut())
            .find(|held| held.key == key);
        if let Some(held) = held {
            if held.mode == LockMode::Shared {
                held.mode = LockMode::Exclusive;
                held.was_shared = true;
                self.exclusive += 1;
            }
        }
    }

    /// `key` stops being exclusive: back to shared, when it was shared before, else
    /// gone.
    fn end_exclusive(&mut self, key: TableKey) {
        let mut remove = false;
        let held = self
            .inline
            .iter_mut()
            .take(self.inline_len)
            .chain(self.spill.iter_mut())
            .find(|held| held.key == key);
        if let Some(held) = held {
            if held.was_shared {
                if held.mode == LockMode::Exclusive {
                    held.mode = LockMode::Shared;
                    self.exclusive = self.exclusive.saturating_sub(1);
                }
            } else {
                remove = true;
            }
        }
        if remove {
            self.remove(key);
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.inline_len = 0;
        self.spill.clear();
        self.exclusive = 0;
    }
}

/// Who is holding locks: a statement (one per thread, reused) or a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnerKind {
    Statement,
    Transaction,
}

/// One owner of locks.
#[derive(Debug)]
pub struct LockOwner {
    id: u64,
    engine: u64,
    kind: OwnerKind,
    held: Mutex<HeldSet>,
}

impl LockOwner {
    /// Does this owner hold `key`, in any mode?
    fn holds(&self, key: TableKey) -> bool {
        self.held.lock().find(key).is_some()
    }
}

/// An exclusive holder of a table.
#[derive(Debug)]
struct ExclusiveHolder {
    owner: u64,
    what: String,
}

/// A request waiting for a table, in arrival order.
#[derive(Debug)]
struct ExclusiveWaiter {
    ticket: u64,
    what: String,
}

/// A table with an exclusive holder or request. Nothing else has an entry.
#[derive(Debug)]
struct TableEntry {
    holder: Option<ExclusiveHolder>,
    /// An owner holding the table shared that waits for everybody else to go. It goes
    /// ahead of `waiters`, which are waiting for its hold.
    upgrader: Option<ExclusiveHolder>,
    waiters: VecDeque<ExclusiveWaiter>,
    /// Shared requests waiting behind them (a test's handshake).
    shared_waiting: usize,
    /// Whoever waits for this table sleeps here, with [`TableLocks::state`]: nothing
    /// that happens to another table wakes it. An `Arc` because a waiter can still be
    /// on it when the entry goes.
    released: Arc<Condvar>,
}

impl TableEntry {
    fn new() -> Self {
        Self {
            holder: None,
            upgrader: None,
            waiters: VecDeque::new(),
            shared_waiting: 0,
            released: Arc::new(Condvar::new()),
        }
    }

    fn is_idle(&self) -> bool {
        self.holder.is_none() && self.upgrader.is_none() && self.waiters.is_empty()
    }

    /// What a request that cannot go ahead is behind.
    fn describe(&self) -> Option<String> {
        if let Some(holder) = &self.holder {
            return Some(format!("{} holds it", holder.what));
        }
        if let Some(upgrader) = &self.upgrader {
            return Some(format!("{} is waiting for it", upgrader.what));
        }
        self.waiters
            .front()
            .map(|waiter| format!("{} is waiting for it", waiter.what))
    }
}

#[derive(Debug, Default)]
struct LockState {
    tables: HashMap<TableKey, TableEntry>,
    next_ticket: u64,
}

impl LockState {
    #[inline]
    fn blocks_shared(&self, key: TableKey) -> bool {
        self.tables.get(&key).is_some_and(|entry| !entry.is_idle())
    }

    fn forget_if_idle(&mut self, key: TableKey) {
        if self.tables.get(&key).is_some_and(TableEntry::is_idle) {
            self.tables.remove(&key);
        }
    }

    /// Whoever waits for `key` looks again. Before [`Self::forget_if_idle`]: the waiters
    /// are found through the entry.
    fn wake(&self, key: TableKey) {
        if let Some(entry) = self.tables.get(&key) {
            entry.released.notify_all();
        }
    }

    /// Where a waiter for `key` sleeps.
    fn released(&self, key: TableKey) -> Option<Arc<Condvar>> {
        self.tables.get(&key).map(|entry| Arc::clone(&entry.released))
    }
}

/// One statement counted among the waiters, for as long as it lives.
struct WaitSlot<'a>(&'a AtomicUsize);

impl Drop for WaitSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What bounds one wait for one table.
struct WaitBudget {
    started: Instant,
    lock_timeout: Duration,
    statement_deadline: Option<Instant>,
}

impl WaitBudget {
    fn new(locks: &TableLocks, lock_timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            lock_timeout,
            statement_deadline: locks.statement_deadline(),
        }
    }

    /// Has the client given up, or the statement run out of time? Looked at FIRST
    /// whenever a waiter wakes: work a client gave up on must not start because the lock
    /// came free at that moment. The `statement_timeout` counts from the statement's
    /// start where an entry point recorded it ([`TableLocks::statement_deadline`]).
    /// Nothing it would change has been touched, so these are the normal 57014s.
    fn check_statement(&self, table: &str) -> Result<()> {
        if crate::session_take_cancel_request() {
            return Err(Error::query_cancelled(format!(
                "canceling statement due to user request while waiting for a lock on table {table}"
            )));
        }
        if self
            .statement_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::query_timeout(format!(
                "canceling statement due to statement timeout while waiting for a lock on table {table}"
            )));
        }
        Ok(())
    }

    /// How long it has waited, once that is all it may.
    fn lock_timed_out(&self) -> Option<Duration> {
        let elapsed = self.started.elapsed();
        (elapsed >= self.lock_timeout).then_some(elapsed)
    }

    /// How long to sleep: at most `longest`, and not past either bound.
    fn tick(&self, longest: Duration) -> Duration {
        let mut tick = longest.min(self.lock_timeout.saturating_sub(self.started.elapsed()));
        if let Some(deadline) = self.statement_deadline {
            tick = tick.min(deadline.saturating_duration_since(Instant::now()));
        }
        tick
    }
}

#[derive(Debug, Default)]
struct OwnerRegistry {
    owners: Vec<Weak<LockOwner>>,
    /// When to sweep out the owners that are gone.
    sweep_at: usize,
}

/// What an exclusive request came to.
#[derive(Debug, Clone, Copy)]
enum ExclusiveGrant {
    /// This owner already held it exclusively: the request adds nothing.
    AlreadyHeld,
    /// Granted, by this request. It ends by [`TableLocks::unlock_exclusive`], or
    /// with the owner.
    Granted(TableKey),
}

/// The locks of one engine.
#[derive(Debug)]
pub struct TableLocks {
    engine: u64,
    timeout_ms: AtomicU64,
    /// `[storage] statement_timeout_ms` (or `query_timeout_ms`), for a session that has
    /// not set `statement_timeout`.
    default_statement_timeout_ms: Option<u64>,
    /// The engine's schema generation: an exclusive holder moves it when it lets go,
    /// so a statement holding a plan or spec from before it can tell.
    schema_generation: Arc<AtomicU64>,
    /// Exclusive holders and requests, by bucket of the table's hash: the shared path's
    /// one load.
    pressure: [AtomicUsize; PRESSURE_BUCKETS],
    state: Mutex<LockState>,
    registry: Mutex<OwnerRegistry>,
    /// Statements waiting right now, for any table.
    waiting: AtomicUsize,
    /// [`MAX_LOCK_WAITERS`]; a test sets it small.
    max_waiters: AtomicUsize,
    /// Shared locks a transaction first took at its first staged write of the table
    /// rather than at the statement (see [`TransactionLocks::ensure_shared`]).
    late_acquisitions: AtomicU64,
}

impl TableLocks {
    pub(crate) fn new(
        engine: u64,
        timeout_ms: u64,
        default_statement_timeout_ms: Option<u64>,
        schema_generation: Arc<AtomicU64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            timeout_ms: AtomicU64::new(timeout_ms),
            default_statement_timeout_ms,
            schema_generation,
            pressure: std::array::from_fn(|_| AtomicUsize::new(0)),
            state: Mutex::new(LockState::default()),
            registry: Mutex::new(OwnerRegistry::default()),
            waiting: AtomicUsize::new(0),
            max_waiters: AtomicUsize::new(MAX_LOCK_WAITERS),
            late_acquisitions: AtomicU64::new(0),
        })
    }

    /// How long this statement may wait for a table: its session's `lock_timeout`, else
    /// `[session] lock_timeout_ms`. A `lock_timeout` of 0 is that second one, where
    /// PostgreSQL would wait for ever: `pg_dump` scripts start with `SET lock_timeout =
    /// 0`, and a wait here stays bounded. Read when a wait begins, not on the hot path.
    pub(crate) fn timeout(&self) -> Duration {
        match crate::session_lock_timeout_tls() {
            Some(ms) if ms > 0 => Duration::from_millis(ms),
            _ => Duration::from_millis(self.timeout_ms.load(Ordering::Relaxed)),
        }
    }

    /// This statement's `statement_timeout`, the way `effective_statement_timeout_ms`
    /// reads it: the session's, where 0 is no limit, else the server's.
    fn statement_timeout(&self) -> Option<Duration> {
        let ms = match crate::session_statement_timeout_tls() {
            Some(ms) => (ms > 0).then_some(ms),
            None => self.default_statement_timeout_ms,
        };
        ms.map(Duration::from_millis)
    }

    /// When this statement's `statement_timeout` runs out: counted from the statement's
    /// start (`crate::statement_started_tls`, recorded by every session and embedded
    /// entry point), else from now. Spec R5.4 (review F12): PostgreSQL bounds the
    /// whole statement; counting from the start of each wait let a statement that
    /// reached a row wait 0.4 s in run to 0.9 s under a 500 ms timeout.
    fn statement_deadline(&self) -> Option<Instant> {
        let timeout = self.statement_timeout()?;
        crate::statement_started_tls()
            .unwrap_or_else(Instant::now)
            .checked_add(timeout)
    }

    /// Spec R5.4: what bounds a wait for a row's write lock (`storage::row_locks`): the
    /// same as a wait for a table's. Read when a wait begins.
    pub(crate) fn row_lock_bounds(&self) -> super::row_locks::RowLockBounds {
        super::row_locks::RowLockBounds {
            lock_timeout: self.timeout(),
            statement_deadline: self.statement_deadline(),
        }
    }

    /// For the tests, which set it small on a running engine.
    pub(crate) fn set_timeout(&self, timeout: Duration) {
        let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.timeout_ms.store(millis, Ordering::Relaxed);
    }

    /// For the tests: how many statements may wait.
    pub(crate) fn set_max_waiters(&self, max_waiters: usize) {
        self.max_waiters.store(max_waiters, Ordering::Relaxed);
    }

    /// One more statement is about to wait, unless too many are.
    fn begin_wait(&self, table: &str) -> Result<WaitSlot<'_>> {
        let max_waiters = self.max_waiters.load(Ordering::Relaxed);
        let slot = WaitSlot(&self.waiting);
        if self.waiting.fetch_add(1, Ordering::AcqRel) >= max_waiters {
            drop(slot);
            return Err(Error::table_lock_too_many_waiters(table, max_waiters));
        }
        Ok(slot)
    }

    /// For the tests' handshakes: exclusive requests waiting for `table`, and shared
    /// requests waiting behind them or the holder.
    pub(crate) fn waiters(&self, level: u64, table: &str) -> (usize, usize) {
        let key = TableKey::new(level, table.as_bytes());
        let state = self.state.lock();
        state.tables.get(&key).map_or((0, 0), |entry| {
            (
                entry.waiters.len() + usize::from(entry.upgrader.is_some()),
                entry.shared_waiting,
            )
        })
    }

    /// For the tests' handshakes: does anything hold `table` exclusively?
    pub(crate) fn has_exclusive_holder(&self, level: u64, table: &str) -> bool {
        let key = TableKey::new(level, table.as_bytes());
        self.state
            .lock()
            .tables
            .get(&key)
            .is_some_and(|entry| entry.holder.is_some())
    }

    /// Does this thread's statement hold `table` exclusively already?
    pub(crate) fn current_statement_holds_exclusively(&self, level: u64, table: &str) -> bool {
        let key = TableKey::new(level, table.as_bytes());
        CURRENT.with(|current| {
            current.borrow().as_ref().is_some_and(|owner| {
                owner.engine == self.engine && owner.held.lock().find(key) == Some(LockMode::Exclusive)
            })
        })
    }

    /// Does this thread's statement hold any table lock of this engine, in any mode?
    /// Sprinter 6bc4a4040eb7 (B4): such a statement does not wait for a fork that waits
    /// for the running statements, one of which may wait for these locks
    /// (`storage::branch_freeze`, "Lock order").
    pub(crate) fn current_statement_holds_any(&self) -> bool {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|owner| owner.engine == self.engine && owner.held.lock().iter().next().is_some())
        })
    }

    /// A raw autocommit write may use this statement's owner, which
    /// releases its locks before its deferred replication wait. An unrelated
    /// transaction owner cannot be borrowed: its locks outlive this commit.
    /// `false` means there is no ambient owner of this engine.
    pub(crate) fn lock_raw_write_shared_in_statement(&self, level: u64, table: &str) -> Result<bool> {
        let owner = CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .filter(|owner| owner.engine == self.engine)
                .cloned()
        });
        let Some(owner) = owner else { return Ok(false) };
        if owner.kind == OwnerKind::Transaction {
            return Err(Error::transaction(
                "raw autocommit writes cannot borrow an unrelated transaction's table locks",
            ));
        }
        self.lock_shared(&owner, level, table)?;
        Ok(true)
    }

    /// Sprinter 6bc4a4040eb7 (B4): the fork fence's wait, which stands in a schema
    /// statement's way as a table lock does, with a lock wait's budget: the session's
    /// `lock_timeout` (55P03), its `statement_timeout` and a cancel (57014), the last two
    /// looked at again once it is through, so a statement the client gave up on does not
    /// start. `wait(longest)` waits at most that long and says whether it is through;
    /// `give_back` undoes it. `table` and `holder` name what it waited for.
    pub(crate) fn wait_like_a_lock(
        &self,
        table: &str,
        holder: &str,
        mut wait: impl FnMut(Duration) -> bool,
        give_back: impl FnOnce(),
    ) -> Result<()> {
        let budget = WaitBudget::new(self, self.timeout());
        loop {
            budget.check_statement(table)?;
            if let Some(waited) = budget.lock_timed_out() {
                return Err(Error::table_lock_timeout(table, holder, waited));
            }
            if wait(budget.tick(SHARED_TICK)) {
                if let Err(e) = budget.check_statement(table) {
                    give_back();
                    return Err(e);
                }
                return Ok(());
            }
        }
    }

    /// Shared locks taken late, at a transaction's first staged write of the table.
    /// Every SQL path locks at its statement, so this stays 0 on them.
    pub(crate) fn late_acquisitions(&self) -> u64 {
        self.late_acquisitions.load(Ordering::Relaxed)
    }

    fn new_owner(&self, kind: OwnerKind) -> Arc<LockOwner> {
        let owner = Arc::new(LockOwner {
            id: NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            engine: self.engine,
            kind,
            held: Mutex::new(HeldSet::new()),
        });
        let mut registry = self.registry.lock();
        if registry.owners.len() >= registry.sweep_at {
            registry.owners.retain(|owner| owner.strong_count() > 0);
            registry.sweep_at = registry.owners.len().saturating_mul(2).max(64);
        }
        registry.owners.push(Arc::downgrade(&owner));
        owner
    }

    /// Open a statement's scope, or join the one this thread is in. Everything the
    /// statement locks through it is released when the outermost scope drops, except
    /// under a transaction's owner, which keeps it until the transaction ends.
    #[inline]
    pub(crate) fn statement_scope(&self) -> StatementLockScope<'_> {
        let nested = CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|owner| owner.engine == self.engine)
        });
        if nested {
            return StatementLockScope {
                locks: self,
                entered: None,
                _not_send: PhantomData,
            };
        }
        let cached = THREAD_OWNER.with(|cached| cached.borrow_mut().take());
        let owner = match cached {
            Some(owner) if owner.engine == self.engine => owner,
            _ => self.new_owner(OwnerKind::Statement),
        };
        let previous = CURRENT.with(|current| current.replace(Some(owner)));
        StatementLockScope {
            locks: self,
            entered: Some(Replaced(previous)),
            _not_send: PhantomData,
        }
    }

    /// The hot path: publish the hold, then look for pressure (module documentation).
    #[inline]
    fn lock_shared(&self, owner: &LockOwner, level: u64, table: &str) -> Result<()> {
        let key = TableKey::new(level, table.as_bytes());
        {
            let mut held = owner.held.lock();
            if held.find(key).is_some() {
                return Ok(());
            }
            held.push(Held::shared(key));
        }
        fence(Ordering::SeqCst);
        if self.bucket(key).load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        self.lock_shared_contended(owner, key, table)
    }

    /// An exclusive request exists on this table, or on one sharing its bucket.
    #[cold]
    fn lock_shared_contended(&self, owner: &LockOwner, key: TableKey, table: &str) -> Result<()> {
        let slot = {
            let mut state = self.state.lock();
            if !state.blocks_shared(key) {
                return Ok(());
            }
            // Back out under the state lock, and wake whoever saw the hold.
            owner.held.lock().remove(key);
            state.wake(key);
            let slot = self.begin_wait(table)?;
            if let Some(entry) = state.tables.get_mut(&key) {
                entry.shared_waiting += 1;
            }
            slot
        };
        let budget = WaitBudget::new(self, self.timeout());
        crate::runtime_offload::off_worker(|| {
            let _slot = slot;
            let mut state = self.state.lock();
            let outcome = loop {
                if let Err(e) = budget.check_statement(table) {
                    break Err(e);
                }
                if !state.blocks_shared(key) {
                    // Published under the state lock: an exclusive request looks
                    // under it too, and sees this.
                    owner.held.lock().push(Held::shared(key));
                    break Ok(());
                }
                if let Some(waited) = budget.lock_timed_out() {
                    let who = state
                        .tables
                        .get(&key)
                        .and_then(TableEntry::describe)
                        .unwrap_or_else(|| "a schema statement holds it".to_string());
                    break Err(Error::table_lock_timeout(table, &who, waited));
                }
                if let Some(released) = state.released(key) {
                    let _ = released.wait_for(&mut state, budget.tick(SHARED_TICK));
                }
            };
            if let Some(entry) = state.tables.get_mut(&key) {
                entry.shared_waiting = entry.shared_waiting.saturating_sub(1);
            }
            outcome
        })
    }

    /// In range: `TableKey::bucket` is modulo the array's length.
    #[inline]
    #[allow(clippy::indexing_slicing)]
    fn bucket(&self, key: TableKey) -> &AtomicUsize {
        &self.pressure[key.bucket()]
    }

    fn raise_pressure(&self, key: TableKey) {
        self.bucket(key).fetch_add(1, Ordering::SeqCst);
        EXCLUSIVE_ANYWHERE.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
    }

    fn lower_pressure(&self, key: TableKey) {
        self.bucket(key).fetch_sub(1, Ordering::SeqCst);
        EXCLUSIVE_ANYWHERE.fetch_sub(1, Ordering::Relaxed);
    }

    /// Another owner holding `key`: a transaction, when there is one, since that is
    /// what a client can do something about.
    fn other_holder(&self, owner: u64, key: TableKey) -> Option<OwnerKind> {
        let registry = self.registry.lock();
        let mut found = None;
        for candidate in &registry.owners {
            let Some(candidate) = candidate.upgrade() else {
                continue;
            };
            if candidate.id != owner && candidate.holds(key) {
                found = found.max(Some(candidate.kind));
            }
        }
        found
    }

    /// Take `key` for the waiter `ticket`, if it is its turn and nothing holds it.
    fn try_take_exclusive(
        &self,
        state: &mut LockState,
        owner: &LockOwner,
        key: TableKey,
        ticket: u64,
        what: &str,
    ) -> bool {
        let Some(entry) = state.tables.get(&key) else {
            return false;
        };
        if entry.holder.is_some()
            || entry.upgrader.is_some()
            || entry.waiters.front().map(|waiter| waiter.ticket) != Some(ticket)
        {
            return false;
        }
        if self.other_holder(owner.id, key).is_some() {
            return false;
        }
        if let Some(entry) = state.tables.get_mut(&key) {
            entry.waiters.pop_front();
            entry.holder = Some(ExclusiveHolder {
                owner: owner.id,
                what: what.to_string(),
            });
        }
        owner.held.lock().push(Held {
            key,
            mode: LockMode::Exclusive,
            was_shared: false,
        });
        true
    }

    fn lock_exclusive(
        &self,
        owner: &LockOwner,
        level: u64,
        table: &str,
        what: &str,
        timeout: Duration,
    ) -> Result<ExclusiveGrant> {
        let key = TableKey::new(level, table.as_bytes());
        let held = owner.held.lock().find(key);
        match held {
            Some(LockMode::Exclusive) => return Ok(ExclusiveGrant::AlreadyHeld),
            Some(LockMode::Shared) => return self.upgrade_exclusive(owner, key, table, what, timeout),
            None => {}
        }
        let (ticket, slot) = {
            let mut state = self.state.lock();
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state
                .tables
                .entry(key)
                .or_insert_with(TableEntry::new)
                .waiters
                .push_back(ExclusiveWaiter {
                    ticket,
                    what: what.to_string(),
                });
            self.raise_pressure(key);
            if self.try_take_exclusive(&mut state, owner, key, ticket, what) {
                return Ok(ExclusiveGrant::Granted(key));
            }
            match self.begin_wait(table) {
                Ok(slot) => (ticket, slot),
                Err(e) => {
                    self.abandon_exclusive_request(&mut state, key, ticket);
                    return Err(e);
                }
            }
        };
        let budget = WaitBudget::new(self, timeout);
        crate::runtime_offload::off_worker(|| {
            let _slot = slot;
            let mut state = self.state.lock();
            loop {
                if let Err(e) = budget.check_statement(table) {
                    self.abandon_exclusive_request(&mut state, key, ticket);
                    return Err(e);
                }
                if self.try_take_exclusive(&mut state, owner, key, ticket, what) {
                    return Ok(ExclusiveGrant::Granted(key));
                }
                if let Some(waited) = budget.lock_timed_out() {
                    let who = self.describe_blockers(&state, owner, key, ticket);
                    self.abandon_exclusive_request(&mut state, key, ticket);
                    return Err(Error::table_lock_timeout(table, &who, waited));
                }
                if let Some(released) = state.released(key) {
                    let _ = released.wait_for(&mut state, budget.tick(EXCLUSIVE_TICK));
                }
            }
        })
    }

    /// An exclusive request gives up.
    fn abandon_exclusive_request(&self, state: &mut LockState, key: TableKey, ticket: u64) {
        if let Some(entry) = state.tables.get_mut(&key) {
            entry.waiters.retain(|waiter| waiter.ticket != ticket);
        }
        state.wake(key);
        state.forget_if_idle(key);
        self.lower_pressure(key);
    }

    /// What an exclusive request timed out behind.
    fn describe_blockers(&self, state: &LockState, owner: &LockOwner, key: TableKey, ticket: u64) -> String {
        if let Some(entry) = state.tables.get(&key) {
            if let Some(holder) = &entry.holder {
                return format!("{} holds it", holder.what);
            }
            if let Some(first) = entry.waiters.front().filter(|waiter| waiter.ticket != ticket) {
                return format!("{} is waiting for it first", first.what);
            }
        }
        Self::describe_owner_kind(self.other_holder(owner.id, key))
    }

    fn describe_owner_kind(kind: Option<OwnerKind>) -> String {
        match kind {
            Some(OwnerKind::Transaction) => "an open transaction holds it".to_string(),
            _ => "a running statement holds it".to_string(),
        }
    }

    /// Shared held, exclusive wanted, by one owner. It waits, bounded like any other
    /// request, for the other holders to go: an autocommit statement is about to finish,
    /// and a transaction may. New shared requests wait behind it, and so do the exclusive
    /// requests already queued, which are waiting for this owner's hold and would never
    /// be granted ahead of it.
    ///
    /// Refused at once in the one case where waiting is certain to deadlock: another
    /// owner is already waiting to upgrade this table. Each holds it shared and waits for
    /// the other's hold to go.
    fn upgrade_exclusive(
        &self,
        owner: &LockOwner,
        key: TableKey,
        table: &str,
        what: &str,
        timeout: Duration,
    ) -> Result<ExclusiveGrant> {
        let slot = {
            let mut state = self.state.lock();
            let entry = state.tables.entry(key).or_insert_with(TableEntry::new);
            if let Some(other) = &entry.upgrader {
                let who = format!("{} is waiting to upgrade it too", other.what);
                return Err(Error::table_lock_upgrade_refused(table, &who));
            }
            entry.upgrader = Some(ExclusiveHolder {
                owner: owner.id,
                what: what.to_string(),
            });
            self.raise_pressure(key);
            if self.try_take_upgrade(&mut state, owner, key, what) {
                return Ok(ExclusiveGrant::Granted(key));
            }
            match self.begin_wait(table) {
                Ok(slot) => slot,
                Err(e) => {
                    self.abandon_upgrade(&mut state, key);
                    return Err(e);
                }
            }
        };
        let budget = WaitBudget::new(self, timeout);
        crate::runtime_offload::off_worker(|| {
            let _slot = slot;
            let mut state = self.state.lock();
            loop {
                if let Err(e) = budget.check_statement(table) {
                    self.abandon_upgrade(&mut state, key);
                    return Err(e);
                }
                if self.try_take_upgrade(&mut state, owner, key, what) {
                    return Ok(ExclusiveGrant::Granted(key));
                }
                if let Some(waited) = budget.lock_timed_out() {
                    let who = Self::describe_owner_kind(self.other_holder(owner.id, key));
                    self.abandon_upgrade(&mut state, key);
                    return Err(Error::table_lock_timeout(table, &who, waited));
                }
                if let Some(released) = state.released(key) {
                    let _ = released.wait_for(&mut state, budget.tick(EXCLUSIVE_TICK));
                }
            }
        })
    }

    /// The upgrade goes through, if nobody else holds the table.
    fn try_take_upgrade(&self, state: &mut LockState, owner: &LockOwner, key: TableKey, what: &str) -> bool {
        if self.other_holder(owner.id, key).is_some() {
            return false;
        }
        let Some(entry) = state.tables.get_mut(&key) else {
            return false;
        };
        entry.upgrader = None;
        entry.holder = Some(ExclusiveHolder {
            owner: owner.id,
            what: what.to_string(),
        });
        owner.held.lock().upgrade(key);
        true
    }

    /// An upgrade gives up. The owner still holds the table shared.
    fn abandon_upgrade(&self, state: &mut LockState, key: TableKey) {
        if let Some(entry) = state.tables.get_mut(&key) {
            entry.upgrader = None;
        }
        state.wake(key);
        state.forget_if_idle(key);
        self.lower_pressure(key);
    }

    /// End one exclusive hold before the owner ends: a schema statement. A transaction's
    /// stays until the transaction ends, as in PostgreSQL ([`Self::release_all`]): the
    /// next schema statement of a migration must not have to get back in behind the
    /// writers that were waiting. A statement that upgraded goes back to shared.
    fn unlock_exclusive(&self, owner: &LockOwner, key: TableKey) {
        if owner.kind == OwnerKind::Transaction {
            return;
        }
        // Before it lets go: whoever gets the lock next sees the new generation.
        self.schema_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.lock();
        if let Some(entry) = state.tables.get_mut(&key) {
            if entry.holder.as_ref().is_some_and(|holder| holder.owner == owner.id) {
                entry.holder = None;
                self.lower_pressure(key);
            }
        }
        state.wake(key);
        state.forget_if_idle(key);
        owner.held.lock().end_exclusive(key);
    }

    /// The owner is done: its statement or its transaction ended. `true` if it held
    /// anything exclusively.
    #[inline]
    fn release_all(&self, owner: &LockOwner) -> bool {
        let (exclusive, wanted): (Vec<TableKey>, Vec<TableKey>) = {
            let mut held = owner.held.lock();
            let exclusive = if held.exclusive == 0 {
                Vec::new()
            } else {
                held.iter()
                    .filter(|held| held.mode == LockMode::Exclusive)
                    .map(|held| held.key)
                    .collect()
            };
            // The tables somebody may be waiting to take from this owner: one load per
            // table held, and no allocation unless one is under pressure.
            let wanted = if held
                .iter()
                .any(|held| self.bucket(held.key).load(Ordering::Relaxed) != 0)
            {
                held.iter()
                    .filter(|held| self.bucket(held.key).load(Ordering::Relaxed) != 0)
                    .map(|held| held.key)
                    .collect()
            } else {
                Vec::new()
            };
            held.clear();
            (exclusive, wanted)
        };
        if exclusive.is_empty() {
            // A stale 0 costs a waiter its tick, no more.
            if !wanted.is_empty() {
                let state = self.state.lock();
                for key in wanted {
                    state.wake(key);
                }
            }
            return false;
        }
        // Before it lets go, as in `unlock_exclusive`: a transaction's DDL or TRUNCATE is
        // done, and a statement that waited for it must read what it left.
        self.schema_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.lock();
        for key in exclusive {
            if let Some(entry) = state.tables.get_mut(&key) {
                if entry.holder.as_ref().is_some_and(|holder| holder.owner == owner.id) {
                    entry.holder = None;
                    self.lower_pressure(key);
                }
            }
            state.wake(key);
            state.forget_if_idle(key);
        }
        for key in wanted {
            state.wake(key);
        }
        true
    }

    /// Wait, without taking anything, until no exclusive holder or request is left on
    /// these tables: a merge into a branch is exposed to main's DDL on the tables its
    /// target inherits. It registered its exclusion, so a new one is refused; this is
    /// for the one already past that check. It does not block main's writers.
    pub(crate) fn wait_for_exclusive_holders(&self, level: u64, tables: &[String], timeout: Duration) -> Result<()> {
        let keys: Vec<(TableKey, &str)> = tables
            .iter()
            .map(|table| (TableKey::new(level, table.as_bytes()), table.as_str()))
            .collect();
        let blocked = |state: &LockState| {
            keys.iter()
                .find(|(key, _)| state.blocks_shared(*key))
                .map(|(_, table)| *table)
        };
        let slot = {
            let state = self.state.lock();
            let Some(table) = blocked(&state) else {
                return Ok(());
            };
            self.begin_wait(table)?
        };
        let budget = WaitBudget::new(self, timeout);
        crate::runtime_offload::off_worker(|| {
            let _slot = slot;
            let mut state = self.state.lock();
            loop {
                let Some(table) = blocked(&state) else {
                    return Ok(());
                };
                budget.check_statement(table)?;
                if let Some(waited) = budget.lock_timed_out() {
                    return Err(Error::table_lock_timeout(table, "a schema statement holds it", waited));
                }
                let key = TableKey::new(level, table.as_bytes());
                if let Some(released) = state.released(key) {
                    let _ = released.wait_for(&mut state, budget.tick(SHARED_TICK));
                }
            }
        })
    }

    /// Take `tables` exclusively, sorted, for the guard's life. A request this
    /// thread's statement already holds adds nothing. A statement scope is opened if
    /// there is none. `timeout` bounds each table's wait.
    pub(crate) fn exclusive(
        self: &Arc<Self>,
        level: u64,
        tables: &[&str],
        what: &str,
        timeout: Duration,
    ) -> Result<ExclusiveTableLocks> {
        let mut guard = self.exclusive_guard();
        let mut sorted: Vec<&str> = tables.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for table in sorted {
            guard.take(level, table, what, timeout)?;
        }
        Ok(guard)
    }

    /// A guard holding nothing yet, for a caller that looks at each table as it gets
    /// it ([`ExclusiveTableLocks::take`]).
    pub(crate) fn exclusive_guard(self: &Arc<Self>) -> ExclusiveTableLocks {
        ExclusiveTableLocks::empty(self)
    }

    /// An owner of its own for what runs on this thread until it drops: a MERGE
    /// BRANCH, which lands its rows in commits of its own, so a transaction it runs
    /// inside must not count as itself (see [`SeparateLockOwner::outer_holds`]).
    pub(crate) fn separate_owner(self: &Arc<Self>) -> SeparateLockOwner {
        let outer = CURRENT.with(|current| current.borrow().clone());
        let owner = self.new_owner(OwnerKind::Statement);
        let bound = bind_owner(Arc::clone(&owner));
        SeparateLockOwner {
            locks: Arc::clone(self),
            owner,
            outer,
            _bound: bound,
        }
    }
}

/// See [`TableLocks::separate_owner`].
#[must_use]
pub struct SeparateLockOwner {
    locks: Arc<TableLocks>,
    owner: Arc<LockOwner>,
    outer: Option<Arc<LockOwner>>,
    _bound: BoundLockOwner,
}

impl SeparateLockOwner {
    /// Does the owner this one was opened under hold `table`: the transaction this
    /// statement runs in has written it. Waiting for it would be waiting for itself.
    pub(crate) fn outer_holds(&self, level: u64, table: &str) -> bool {
        let key = TableKey::new(level, table.as_bytes());
        self.outer.as_ref().is_some_and(|outer| outer.holds(key))
    }
}

impl Drop for SeparateLockOwner {
    fn drop(&mut self) {
        let _ = self.locks.release_all(&self.owner);
    }
}

/// What [`CURRENT`] held before a scope or a binding replaced it, and gets back when
/// that ends. `None` inside: the thread was acting for nobody.
#[derive(Debug)]
struct Replaced(Option<Arc<LockOwner>>);

/// A statement's scope (see [`TableLocks::statement_scope`]). Its methods lock on
/// behalf of whatever owner the thread is acting for.
#[must_use]
pub struct StatementLockScope<'a> {
    locks: &'a TableLocks,
    /// Set on the outermost scope: what [`CURRENT`] held before it.
    entered: Option<Replaced>,
    _not_send: PhantomData<*const ()>,
}

impl StatementLockScope<'_> {
    /// Take `table` shared, for as long as the owner lives.
    #[inline]
    pub(crate) fn lock_shared(&self, level: u64, table: &str) -> Result<()> {
        CURRENT.with(|current| match current.borrow().as_ref() {
            Some(owner) => self.locks.lock_shared(owner, level, table),
            None => Ok(()),
        })
    }

    /// Take `table` exclusively, for as long as the owner lives: TRUNCATE, whose rows
    /// commit with the statement's or the transaction's commit.
    pub(crate) fn lock_exclusive(&self, level: u64, table: &str, what: &str) -> Result<()> {
        let timeout = self.locks.timeout();
        CURRENT.with(|current| match current.borrow().as_ref() {
            Some(owner) => self
                .locks
                .lock_exclusive(owner, level, table, what, timeout)
                .map(|_| ()),
            None => Ok(()),
        })
    }
}

impl Drop for StatementLockScope<'_> {
    #[inline]
    fn drop(&mut self) {
        let Some(Replaced(previous)) = self.entered.take() else {
            return;
        };
        let owner = CURRENT.with(|current| current.replace(previous));
        if let Some(owner) = owner {
            self.locks.release_all(&owner);
            THREAD_OWNER.with(|cached| *cached.borrow_mut() = Some(owner));
            // Direct DML and autocommit TRUNCATE left this until their row
            // guards and the statement's shared/exclusive table locks let go.
            if EXCLUSIVE_GUARDS.with(Cell::get) == 0 {
                if let Some(barrier) = DEFERRED_BARRIER.with(Cell::take) {
                    super::wal::WriteAheadLog::wait_replicated_sequence(Some(barrier));
                }
            }
        }
    }
}

/// Tables held exclusively for this guard's life, or under a transaction's owner until
/// the transaction ends. Dropping it moves the schema generation, releases what it took,
/// and then runs the replication wait a DDL write inside it deferred, so no writer waits
/// for a standby behind an autocommit statement.
#[must_use]
pub struct ExclusiveTableLocks {
    locks: Arc<TableLocks>,
    /// What this guard took, in order.
    taken: Vec<TableKey>,
    /// Set when this guard opened the thread's statement scope.
    entered: Option<Replaced>,
    _not_send: PhantomData<*const ()>,
}

impl ExclusiveTableLocks {
    fn empty(locks: &Arc<TableLocks>) -> Self {
        let nested = CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|owner| owner.engine == locks.engine)
        });
        let entered = if nested {
            None
        } else {
            let cached = THREAD_OWNER.with(|cached| cached.borrow_mut().take());
            let owner = match cached {
                Some(owner) if owner.engine == locks.engine => owner,
                _ => locks.new_owner(OwnerKind::Statement),
            };
            Some(Replaced(CURRENT.with(|current| current.replace(Some(owner)))))
        };
        EXCLUSIVE_GUARDS.with(|guards| guards.set(guards.get().saturating_add(1)));
        Self {
            locks: Arc::clone(locks),
            taken: Vec::new(),
            entered,
            _not_send: PhantomData,
        }
    }

    /// Take `table` too. `true` when this call took it, `false` when the statement
    /// already held it.
    pub(crate) fn take(&mut self, level: u64, table: &str, what: &str, timeout: Duration) -> Result<bool> {
        let locks = Arc::clone(&self.locks);
        let grant = CURRENT.with(|current| match current.borrow().as_ref() {
            Some(owner) => locks.lock_exclusive(owner, level, table, what, timeout),
            None => Ok(ExclusiveGrant::AlreadyHeld),
        })?;
        match grant {
            ExclusiveGrant::Granted(key) => {
                self.taken.push(key);
                Ok(true)
            }
            ExclusiveGrant::AlreadyHeld => Ok(false),
        }
    }

    /// Whether this guard took anything: `false`, the statement already held it all.
    #[cfg(test)]
    pub(crate) fn took_any(&self) -> bool {
        !self.taken.is_empty()
    }
}

impl std::fmt::Debug for ExclusiveTableLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExclusiveTableLocks")
            .field("taken", &self.taken)
            .finish_non_exhaustive()
    }
}

impl Drop for ExclusiveTableLocks {
    fn drop(&mut self) {
        let owner = CURRENT.with(|current| current.borrow().clone());
        if let Some(owner) = owner {
            for key in self.taken.drain(..).rev() {
                self.locks.unlock_exclusive(&owner, key);
            }
            if let Some(Replaced(previous)) = self.entered.take() {
                let mine = CURRENT.with(|current| current.replace(previous));
                if let Some(mine) = mine {
                    let _ = self.locks.release_all(&mine);
                    THREAD_OWNER.with(|cached| *cached.borrow_mut() = Some(mine));
                }
            }
        }
        let outermost = EXCLUSIVE_GUARDS.with(|guards| {
            let left = guards.get().saturating_sub(1);
            guards.set(left);
            left == 0
        });
        if outermost {
            if let Some(barrier) = DEFERRED_BARRIER.with(Cell::take) {
                super::wal::WriteAheadLog::wait_replicated_sequence(Some(barrier));
            }
        }
    }
}

/// Spec R5.1: a durable statement's replication wait is deferred until its
/// statement scope has released shared or exclusive table locks. Its row/zone
/// guards must be declared after that scope and dropped before it. Transaction
/// owners retain locks across statements and never defer to that later lifetime.
/// An explicit exclusive guard uses `defer_replication_wait` instead.
pub fn defer_commit_replication_wait(barrier: Option<u64>) -> bool {
    let Some(barrier) = barrier else {
        return false;
    };
    if EXCLUSIVE_GUARDS.with(Cell::get) != 0 {
        return false;
    }
    let holds_statement_locks = CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|owner| owner.kind == OwnerKind::Statement && owner.held.lock().iter().next().is_some())
    });
    if holds_statement_locks {
        DEFERRED_BARRIER.with(|deferred| {
            let highest = deferred.get().map_or(barrier, |earlier| earlier.max(barrier));
            deferred.set(Some(highest));
        });
    }
    holds_statement_locks
}

/// Spec R5.1: a DDL write's replication wait, while this thread holds an exclusive
/// guard, waits until the guard let go. `true`: deferred.
pub fn defer_replication_wait(barrier: Option<u64>) -> bool {
    if EXCLUSIVE_GUARDS.with(Cell::get) == 0 {
        return false;
    }
    if let Some(barrier) = barrier {
        DEFERRED_BARRIER.with(|deferred| {
            let highest = deferred.get().map_or(barrier, |earlier| earlier.max(barrier));
            deferred.set(Some(highest));
        });
    }
    true
}

/// A transaction's lock owner, once it has one, with its hold released when the last
/// clone goes: the transaction's, or the copy a caller keeps across a failed commit
/// until its index undo has run ([`crate::storage::Transaction::table_locks_hold`]).
#[derive(Debug)]
pub struct TransactionLockHold {
    locks: Arc<TableLocks>,
    owner: Arc<LockOwner>,
}

impl Drop for TransactionLockHold {
    fn drop(&mut self) {
        let _ = self.locks.release_all(&self.owner);
    }
}

/// A transaction's connection to the locks. The owner comes with its first write, so
/// a read-only transaction allocates nothing.
#[derive(Debug)]
pub struct TransactionLocks {
    locks: Arc<TableLocks>,
    hold: OnceLock<Arc<TransactionLockHold>>,
}

impl TransactionLocks {
    pub(crate) fn new(locks: Arc<TableLocks>) -> Self {
        Self {
            locks,
            hold: OnceLock::new(),
        }
    }

    fn hold(&self) -> &Arc<TransactionLockHold> {
        self.hold.get_or_init(|| {
            Arc::new(TransactionLockHold {
                locks: Arc::clone(&self.locks),
                owner: self.locks.new_owner(OwnerKind::Transaction),
            })
        })
    }

    /// This thread's statement acts for this transaction until the binding drops.
    pub(crate) fn bind(&self) -> BoundLockOwner {
        bind_owner(Arc::clone(&self.hold().owner))
    }

    /// [`Self::bind`], if the transaction has an owner at all.
    pub(crate) fn bind_existing(&self) -> BoundLockOwner {
        match self.hold.get() {
            Some(hold) => bind_owner(Arc::clone(&hold.owner)),
            None => BoundLockOwner::inert(),
        }
    }

    pub(crate) fn hold_copy(&self) -> Option<Arc<TransactionLockHold>> {
        self.hold.get().cloned()
    }

    /// The transaction's commit is applied and durable here, and only its standbys'
    /// acknowledgement is left: nobody waits for a standby behind this transaction.
    pub(crate) fn release_for_replication_wait(&self) {
        if let Some(hold) = self.hold.get() {
            let _ = hold.locks.release_all(&hold.owner);
        }
    }

    /// Raw staging must retain THIS transaction's locks. An unrelated
    /// bound transaction cannot lend its longer-lived owner to this commit.
    pub(crate) fn ensure_raw_write_shared(&self, level: u64, table: &str) -> Result<()> {
        let unrelated_transaction = CURRENT.with(|current| {
            current.borrow().as_ref().is_some_and(|owner| {
                owner.engine == self.locks.engine
                    && owner.kind == OwnerKind::Transaction
                    && self.hold.get().is_none_or(|hold| hold.owner.id != owner.id)
            })
        });
        if unrelated_transaction {
            return Err(Error::transaction(
                "raw writes cannot borrow an unrelated transaction's table locks",
            ));
        }
        self.ensure_shared(level, table)
    }

    /// The transaction staged its first row of `table` (decision 1's "first staged
    /// write"). Every SQL path locked at its statement by now, so this is for a
    /// transaction driven directly on the storage API, which has no statement. What
    /// the thread's statement already holds is enough: a merge's own commit runs on
    /// its exclusive locks.
    pub(crate) fn ensure_shared(&self, level: u64, table: &str) -> Result<()> {
        let key = TableKey::new(level, table.as_bytes());
        let held = CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|owner| owner.engine == self.locks.engine && owner.holds(key))
        });
        if held {
            return Ok(());
        }
        let hold = self.hold();
        if !hold.owner.holds(key) {
            self.locks.late_acquisitions.fetch_add(1, Ordering::Relaxed);
        }
        self.locks.lock_shared(&hold.owner, level, table)
    }
}

/// This thread's statement acting for a transaction (see [`TransactionLocks::bind`]).
#[must_use]
pub struct BoundLockOwner {
    /// What [`CURRENT`] held before; `None` when this binding changed nothing.
    previous: Option<Replaced>,
    _not_send: PhantomData<*const ()>,
}

impl BoundLockOwner {
    pub(crate) const fn inert() -> Self {
        Self {
            previous: None,
            _not_send: PhantomData,
        }
    }
}

impl Drop for BoundLockOwner {
    fn drop(&mut self) {
        if let Some(Replaced(previous)) = self.previous.take() {
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
}

fn bind_owner(owner: Arc<LockOwner>) -> BoundLockOwner {
    let same = CURRENT.with(|current| current.borrow().as_ref().is_some_and(|bound| bound.id == owner.id));
    if same {
        return BoundLockOwner::inert();
    }
    BoundLockOwner {
        previous: Some(Replaced(CURRENT.with(|current| current.replace(Some(owner))))),
        _not_send: PhantomData,
    }
}

// ---------------------------------------------------------------------------
// The detector
// ---------------------------------------------------------------------------

/// `Some((level, table))` for a row key: `data:{t}:{row}`, `v:{t}:{row}:{ts}`,
/// `bdata:{L}:{t}:{row}`.
pub fn row_key_table(key: &[u8]) -> Option<(u64, &[u8])> {
    fn before_last_colon(bytes: &[u8]) -> Option<&[u8]> {
        let at = bytes.iter().rposition(|byte| *byte == b':')?;
        bytes.get(..at)
    }
    if let Some(rest) = key.strip_prefix(b"data:".as_slice()) {
        return before_last_colon(rest).map(|table| (MAIN_LEVEL, table));
    }
    if let Some(rest) = key.strip_prefix(b"v:".as_slice()) {
        return before_last_colon(rest)
            .and_then(before_last_colon)
            .map(|table| (MAIN_LEVEL, table));
    }
    if let Some(rest) = key.strip_prefix(b"bdata:".as_slice()) {
        let at = rest.iter().position(|byte| *byte == b':')?;
        let level = std::str::from_utf8(rest.get(..at)?).ok()?.parse::<u64>().ok()?;
        let table = before_last_colon(rest.get(at + 1..)?)?;
        return Some((level, table));
    }
    None
}

/// Whether the detector looks at this write at all. In a release build, one load
/// while no exclusive lock is held or wanted anywhere in the process.
#[inline]
pub fn detector_active(armed: &AtomicBool) -> bool {
    if cfg!(any(debug_assertions, feature = "internal-tests")) {
        armed.load(Ordering::Relaxed)
    } else {
        EXCLUSIVE_ANYWHERE.load(Ordering::Relaxed) != 0 && armed.load(Ordering::Relaxed)
    }
}

/// The level and table of `key`, if it is a row of a table this thread's statement
/// holds no lock on, and nothing says it cannot race. Reports nothing, and cannot panic:
/// it runs inside RocksDB's batch callback.
fn unlocked_row_key(key: &[u8]) -> Option<(u64, &[u8])> {
    let (level, table) = row_key_table(key)?;
    if UNLOCKED_ALLOWED.with(Cell::get).is_some() {
        return None;
    }
    let lock = TableKey::new(level, table);
    let held = CURRENT.with(|current| current.borrow().as_ref().is_some_and(|owner| owner.holds(lock)));
    (!held).then_some((level, table))
}

/// `count` rows of `table` were about to be written with no lock on them.
// The panic is the detector's point in a test build (debug_assertions or internal-tests),
// where a row written with no lock must fail the test that did it; a release build counts
// and warns instead.
#[allow(clippy::panic)]
fn report_unlocked_row_writes(level: u64, table: &[u8], count: u64, caller: &'static Location<'static>) {
    UNLOCKED_ROW_WRITES.fetch_add(count, Ordering::Relaxed);
    let table = String::from_utf8_lossy(table);
    #[cfg(any(debug_assertions, feature = "internal-tests"))]
    {
        // A panic now would be a panic during a panic, which aborts the process and
        // takes the rest of the test binary with it. The first panic fails the test;
        // this one is in the counter and in the log.
        if std::thread::panicking() {
            tracing::error!(
                "R5.1 writer lock: a row of table {table} (level {level}) is written with no lock on it, by \
                 {caller}, while this thread is unwinding"
            );
            return;
        }
        panic!(
            "R5.1 writer lock: a row of table {table} (level {level}) is written with no lock on it, by {caller}. \
             Take it at the statement, or, if this path cannot race, say why with allow_unlocked_row_writes"
        );
    }
    #[cfg(not(any(debug_assertions, feature = "internal-tests")))]
    {
        if WARNED_TABLES.lock().insert(table.as_bytes().to_vec()) {
            tracing::warn!(
                "a row of table {table} (level {level}) was written with no writer lock on it, by {caller} \
                 (reported once per table)"
            );
        }
    }
}

/// Report `key` if it is a row of a table this thread's statement holds no lock on.
pub fn check_row_key(key: &[u8], caller: &'static Location<'static>) {
    if let Some((level, table)) = unlocked_row_key(key) {
        report_unlocked_row_writes(level, table, 1, caller);
    }
}

/// [`check_row_key`] for every key of a batch.
pub fn check_row_batch(batch: &rocksdb::WriteBatch, caller: &'static Location<'static>) {
    /// RocksDB calls these from an `extern "C"` frame, which a panic cannot unwind
    /// through: it aborts the process. So they only take notes, and the report is made
    /// once `iterate` has returned.
    #[derive(Default)]
    struct Keys {
        first: Option<(u64, Vec<u8>)>,
        unlocked: u64,
    }
    impl Keys {
        fn look_at(&mut self, key: &[u8]) {
            if let Some((level, table)) = unlocked_row_key(key) {
                self.unlocked = self.unlocked.saturating_add(1);
                if self.first.is_none() {
                    self.first = Some((level, table.to_vec()));
                }
            }
        }
    }
    impl rocksdb::WriteBatchIterator for Keys {
        fn put(&mut self, key: Box<[u8]>, _value: Box<[u8]>) {
            self.look_at(&key);
        }
        fn delete(&mut self, key: Box<[u8]>) {
            self.look_at(&key);
        }
    }
    let mut keys = Keys::default();
    batch.iterate(&mut keys);
    if let Some((level, table)) = keys.first {
        report_unlocked_row_writes(level, &table, keys.unlocked, caller);
    }
}

/// Row writes the detector found with no lock on them.
pub fn unlocked_row_writes() -> u64 {
    UNLOCKED_ROW_WRITES.load(Ordering::Relaxed)
}

/// This thread's row writes cannot race, for the reason given, until it drops (the
/// detector's allow-list).
#[must_use]
pub struct UnlockedWritesAllowed {
    previous: Option<&'static str>,
    _not_send: PhantomData<*const ()>,
}

impl Drop for UnlockedWritesAllowed {
    fn drop(&mut self) {
        UNLOCKED_ALLOWED.with(|allowed| allowed.set(self.previous));
    }
}

/// See [`UnlockedWritesAllowed`].
pub fn allow_unlocked_row_writes(reason: &'static str) -> UnlockedWritesAllowed {
    UnlockedWritesAllowed {
        previous: UNLOCKED_ALLOWED.with(|allowed| allowed.replace(Some(reason))),
        _not_send: PhantomData,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn locks(timeout_ms: u64) -> Arc<TableLocks> {
        TableLocks::new(
            NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            timeout_ms,
            None,
            Arc::new(AtomicU64::new(0)),
        )
    }

    fn message(result: Result<ExclusiveTableLocks>) -> String {
        result.expect_err("refused").to_string()
    }

    /// RocksDB's batch callback is `extern "C"`: a panic inside it would not fail this
    /// test, it would abort the process (`panic in a function that cannot unwind`).
    #[cfg(any(debug_assertions, feature = "internal-tests"))]
    #[test]
    fn a_finding_in_a_batch_is_a_panic_on_this_side_of_rocksdb() {
        let mut batch = rocksdb::WriteBatch::default();
        batch.put(b"data:nobody:1", b"row");
        batch.delete(b"data:nobody:2");
        let before = unlocked_row_writes();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_row_batch(&batch, Location::caller());
        }));
        assert!(unwound.is_err(), "no lock on either row");
        assert!(unlocked_row_writes() >= before + 2, "both are counted");
    }

    /// A second panic would abort the process. The first one is what fails the test.
    #[cfg(any(debug_assertions, feature = "internal-tests"))]
    #[test]
    fn a_finding_while_unwinding_is_counted_and_does_not_abort() {
        struct WritesOnTheWayOut;
        impl Drop for WritesOnTheWayOut {
            fn drop(&mut self) {
                check_row_key(b"data:nobody:1", Location::caller());
            }
        }
        let before = unlocked_row_writes();
        let payload = std::panic::catch_unwind(|| {
            let _on_the_way_out = WritesOnTheWayOut;
            panic!("the first finding");
        })
        .expect_err("it unwound");
        assert_eq!(payload.downcast_ref::<&str>().copied(), Some("the first finding"));
        assert!(unlocked_row_writes() > before, "it is counted");
    }

    #[test]
    fn the_keys_of_every_row_family_name_their_table() {
        assert_eq!(row_key_table(b"data:t:17"), Some((MAIN_LEVEL, b"t".as_slice())));
        assert_eq!(row_key_table(b"data:s.t:17"), Some((MAIN_LEVEL, b"s.t".as_slice())));
        assert_eq!(row_key_table(b"v:t:17:900"), Some((MAIN_LEVEL, b"t".as_slice())));
        assert_eq!(row_key_table(b"bdata:7:t:17"), Some((7, b"t".as_slice())));
        assert_eq!(row_key_table(b"v_idx:t:17:900"), None);
        assert_eq!(row_key_table(b"meta:table:t"), None);
        assert_eq!(row_key_table(b"counter:t"), None);
        assert_eq!(
            row_key_table(b"bdata:\x00\x00\x00\x00\x00\x00\x00\x07:k"),
            None,
            "the legacy binary branch key is no SQL row"
        );
    }

    #[test]
    fn a_shared_hold_is_found_by_an_exclusive_request_and_released_with_its_scope() {
        let locks = locks(20);
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let holder = Arc::clone(&locks);
            scope.spawn(move || {
                let statement = holder.statement_scope();
                statement.lock_shared(MAIN_LEVEL, "t").unwrap();
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            });
            held_rx.recv().unwrap();
            let refused = message(locks.exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout()));
            assert!(refused.contains(crate::error::TABLE_LOCK_TIMEOUT_MARKER), "{refused}");
            assert!(refused.contains("a running statement holds it"), "{refused}");
            assert_eq!(locks.waiters(MAIN_LEVEL, "t"), (0, 0), "the request is gone");
            go_tx.send(()).unwrap();
        });
        let granted = locks
            .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout())
            .unwrap();
        assert!(granted.took_any());
        assert!(locks.has_exclusive_holder(MAIN_LEVEL, "t"));
        drop(granted);
        assert!(!locks.has_exclusive_holder(MAIN_LEVEL, "t"));
    }

    #[test]
    fn a_waiting_exclusive_holds_new_shared_requests_back() {
        let locks = locks(10_000);
        let statement = locks.statement_scope();
        statement.lock_shared(MAIN_LEVEL, "t").unwrap();
        std::thread::scope(|scope| {
            let waiting = Arc::clone(&locks);
            let exclusive = scope.spawn(move || {
                let guard = waiting
                    .exclusive(MAIN_LEVEL, &["t"], "DROP TABLE", waiting.timeout())
                    .unwrap();
                drop(guard);
            });
            while locks.waiters(MAIN_LEVEL, "t").0 == 0 {
                std::thread::yield_now();
            }
            let held_back = Arc::clone(&locks);
            let shared = scope.spawn(move || {
                let later = held_back.statement_scope();
                later.lock_shared(MAIN_LEVEL, "t").unwrap();
            });
            while locks.waiters(MAIN_LEVEL, "t").1 == 0 {
                std::thread::yield_now();
            }
            // Reentrant: this owner holds it, so it never queues behind the waiter.
            statement.lock_shared(MAIN_LEVEL, "t").unwrap();
            assert!(!exclusive.is_finished() && !shared.is_finished());
            drop(statement);
            exclusive.join().unwrap();
            shared.join().unwrap();
        });
    }

    #[test]
    fn an_exclusive_request_on_another_table_makes_nobody_wait() {
        let locks = locks(10_000);
        let _on_a = locks
            .exclusive(MAIN_LEVEL, &["a"], "ALTER TABLE", locks.timeout())
            .unwrap();
        std::thread::scope(|scope| {
            let other = Arc::clone(&locks);
            scope
                .spawn(move || {
                    let statement = other.statement_scope();
                    statement.lock_shared(MAIN_LEVEL, "b").unwrap();
                    statement.lock_shared(7, "a").unwrap();
                })
                .join()
                .unwrap();
        });
    }

    /// Alone there, an upgrade goes through at once, and from then on the table is the
    /// transaction's until it ends: its next schema statement has nothing to wait for.
    #[test]
    fn a_transaction_keeps_an_exclusive_until_it_ends() {
        let locks = locks(20);
        let txn = TransactionLocks::new(Arc::clone(&locks));
        let bound = txn.bind();
        let statement = locks.statement_scope();
        statement.lock_shared(MAIN_LEVEL, "t").unwrap();
        {
            let upgrade = locks
                .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout())
                .unwrap();
            assert!(upgrade.took_any());
        }
        assert!(
            locks.has_exclusive_holder(MAIN_LEVEL, "t"),
            "the statement ended, not it"
        );
        {
            let again = locks
                .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout())
                .unwrap();
            assert!(!again.took_any(), "it has it already");
        }
        std::thread::scope(|scope| {
            let other = Arc::clone(&locks);
            scope
                .spawn(move || {
                    let writer = other.statement_scope();
                    let refused = writer.lock_shared(MAIN_LEVEL, "t").unwrap_err().to_string();
                    assert!(refused.contains("ALTER TABLE holds it"), "{refused}");
                })
                .join()
                .unwrap();
        });
        drop(statement);
        drop(bound);
        drop(txn);
        assert!(!locks.has_exclusive_holder(MAIN_LEVEL, "t"), "it ended");
    }

    /// The statement that is there is about to finish: an upgrade waits for it, and a new
    /// writer waits behind the upgrade.
    #[test]
    fn an_upgrade_waits_for_the_other_holders() {
        let locks = locks(60_000);
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let other = Arc::clone(&locks);
            scope.spawn(move || {
                let statement = other.statement_scope();
                statement.lock_shared(MAIN_LEVEL, "t").unwrap();
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            });
            held_rx.recv().unwrap();
            let upgrading = Arc::clone(&locks);
            let upgrade = scope.spawn(move || {
                let txn = TransactionLocks::new(Arc::clone(&upgrading));
                let _bound = txn.bind();
                let statement = upgrading.statement_scope();
                statement.lock_shared(MAIN_LEVEL, "t").unwrap();
                let granted = upgrading
                    .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", upgrading.timeout())
                    .unwrap();
                assert!(granted.took_any());
            });
            while locks.waiters(MAIN_LEVEL, "t").0 == 0 {
                std::thread::yield_now();
            }
            assert!(!upgrade.is_finished(), "it waits");
            let later = Arc::clone(&locks);
            let writer = scope.spawn(move || {
                later.statement_scope().lock_shared(MAIN_LEVEL, "t").unwrap();
            });
            while locks.waiters(MAIN_LEVEL, "t").1 == 0 {
                std::thread::yield_now();
            }
            go_tx.send(()).unwrap();
            upgrade.join().unwrap();
            writer.join().unwrap();
        });
    }

    /// A DDL queued behind this transaction's hold cannot be granted while it holds, so
    /// the upgrade goes ahead of it.
    #[test]
    fn an_upgrade_goes_ahead_of_the_exclusive_requests_waiting_for_its_hold() {
        let locks = locks(60_000);
        let txn = TransactionLocks::new(Arc::clone(&locks));
        let bound = txn.bind();
        let statement = locks.statement_scope();
        statement.lock_shared(MAIN_LEVEL, "t").unwrap();
        std::thread::scope(|scope| {
            let queued = Arc::clone(&locks);
            let ddl = scope.spawn(move || {
                drop(
                    queued
                        .exclusive(MAIN_LEVEL, &["t"], "DROP TABLE", queued.timeout())
                        .unwrap(),
                );
            });
            while locks.waiters(MAIN_LEVEL, "t").0 == 0 {
                std::thread::yield_now();
            }
            let upgrade = locks
                .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout())
                .unwrap();
            assert!(upgrade.took_any(), "at once, ahead of the DROP");
            assert!(!ddl.is_finished());
            drop(upgrade);
            drop(statement);
            drop(bound);
            drop(txn);
            ddl.join().unwrap();
        });
    }

    /// Two owners holding a table shared, both wanting it exclusively: each waits for the
    /// other's hold, for certain. The second is told at once.
    #[test]
    fn a_second_upgrade_of_one_table_is_refused_at_once() {
        let locks = locks(60_000);
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let other = Arc::clone(&locks);
            let first = scope.spawn(move || {
                let txn = TransactionLocks::new(Arc::clone(&other));
                let _bound = txn.bind();
                let statement = other.statement_scope();
                statement.lock_shared(MAIN_LEVEL, "t").unwrap();
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                drop(
                    other
                        .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", other.timeout())
                        .unwrap(),
                );
            });
            held_rx.recv().unwrap();
            let txn = TransactionLocks::new(Arc::clone(&locks));
            let bound = txn.bind();
            let statement = locks.statement_scope();
            statement.lock_shared(MAIN_LEVEL, "t").unwrap();
            go_tx.send(()).unwrap();
            while locks.waiters(MAIN_LEVEL, "t").0 == 0 {
                std::thread::yield_now();
            }
            let started = Instant::now();
            let refused = message(locks.exclusive(MAIN_LEVEL, &["t"], "CREATE INDEX", locks.timeout()));
            assert!(refused.contains(crate::error::TABLE_LOCK_TIMEOUT_MARKER), "{refused}");
            assert!(refused.contains("waiting to upgrade it too"), "{refused}");
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "at once, not at the timeout"
            );
            drop(statement);
            drop(bound);
            drop(txn);
            first.join().unwrap();
        });
    }

    /// Past the cap a statement is told at once, not parked on a pool thread.
    #[test]
    fn a_waiter_past_the_cap_is_refused_at_once_and_the_slot_comes_back() {
        let locks = locks(60_000);
        locks.set_max_waiters(1);
        let exclusive = locks
            .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout())
            .unwrap();
        std::thread::scope(|scope| {
            let first = Arc::clone(&locks);
            let waiting = scope.spawn(move || first.statement_scope().lock_shared(MAIN_LEVEL, "t").is_ok());
            while locks.waiting.load(Ordering::Acquire) == 0 {
                std::thread::yield_now();
            }
            let second = Arc::clone(&locks);
            scope
                .spawn(move || {
                    let refused = second.statement_scope().lock_shared(MAIN_LEVEL, "t").unwrap_err();
                    let refused = refused.to_string();
                    assert!(refused.contains(crate::error::TABLE_LOCK_TIMEOUT_MARKER), "{refused}");
                    assert!(refused.contains("already waiting"), "{refused}");
                })
                .join()
                .unwrap();
            drop(exclusive);
            assert!(waiting.join().unwrap());
        });
        assert_eq!(locks.waiting.load(Ordering::Acquire), 0, "every slot came back");
    }

    /// A wait looks at the statement's own deadline: 57014, well before the lock timeout.
    #[test]
    fn a_wait_ends_at_the_statement_timeout() {
        let locks = TableLocks::new(
            NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            60_000,
            Some(20),
            Arc::new(AtomicU64::new(0)),
        );
        let _exclusive = locks
            .exclusive(MAIN_LEVEL, &["t"], "ALTER TABLE", locks.timeout())
            .unwrap();
        std::thread::scope(|scope| {
            let waiter = Arc::clone(&locks);
            scope
                .spawn(move || {
                    let started = Instant::now();
                    let err = waiter.statement_scope().lock_shared(MAIN_LEVEL, "t").unwrap_err();
                    assert!(matches!(err, Error::QueryTimeout(_)), "{err}");
                    assert!(started.elapsed() < Duration::from_secs(30), "not the lock timeout");
                })
                .join()
                .unwrap();
        });
    }

    #[test]
    fn statement_waits_defer_until_nested_row_and_table_scopes_have_ended() {
        use super::super::row_locks::{RowLockBounds, RowLockGuard, RowLocks};
        let locks = locks(20);
        assert!(!defer_commit_replication_wait(Some(7)), "no statement");
        let statement = locks.statement_scope();
        assert!(!defer_commit_replication_wait(Some(7)), "no table held");
        statement.lock_shared(MAIN_LEVEL, "t").unwrap();
        let rows = RowLocks::new();
        let row = RowLockGuard::acquire(
            &rows,
            "t",
            1,
            RowLockBounds {
                lock_timeout: Duration::from_millis(20),
                statement_deadline: None,
            },
        )
        .unwrap();
        let nested = locks.statement_scope();
        assert!(!defer_commit_replication_wait(None));
        assert!(defer_commit_replication_wait(Some(7)), "shared table holder defers");
        drop(nested);
        assert_eq!(DEFERRED_BARRIER.with(Cell::get), Some(7));
        assert!(
            rows.holder("t", 1).is_some(),
            "row remains protected until publication ends"
        );
        drop(row);
        assert_eq!(rows.holder("t", 1), None);
        assert_eq!(DEFERRED_BARRIER.with(Cell::get), Some(7), "no wait while table held");
        statement.lock_exclusive(MAIN_LEVEL, "u", "TRUNCATE").unwrap();
        assert!(defer_commit_replication_wait(Some(9)), "exclusive table holder defers");
        assert_eq!(DEFERRED_BARRIER.with(Cell::get), Some(9));
        drop(statement);
        assert_eq!(
            DEFERRED_BARRIER.with(Cell::get),
            None,
            "the unlocked outer scope ran it"
        );
    }

    #[test]
    fn a_direct_write_does_not_defer_its_reply_to_a_transaction_owner() {
        let locks = locks(20);
        let txn = TransactionLocks::new(Arc::clone(&locks));
        {
            let _bound = txn.bind();
            let statement = locks.statement_scope();
            statement.lock_shared(MAIN_LEVEL, "t").unwrap();
            assert!(!defer_commit_replication_wait(Some(7)));
            assert_eq!(DEFERRED_BARRIER.with(Cell::get), None);
        }
    }

    #[test]
    fn a_transaction_keeps_its_locks_across_statements_and_a_hold_outlives_it() {
        let locks = locks(20);
        let txn = TransactionLocks::new(Arc::clone(&locks));
        {
            let _bound = txn.bind();
            locks.statement_scope().lock_shared(MAIN_LEVEL, "t").unwrap();
        }
        let refused = message(locks.exclusive(MAIN_LEVEL, &["t"], "DROP TABLE", locks.timeout()));
        assert!(refused.contains("an open transaction holds it"), "{refused}");
        let hold = txn.hold_copy().expect("it has an owner");
        drop(txn);
        assert!(
            locks
                .exclusive(MAIN_LEVEL, &["t"], "DROP TABLE", locks.timeout())
                .is_err(),
            "the copy keeps it, across an index undo"
        );
        drop(hold);
        drop(
            locks
                .exclusive(MAIN_LEVEL, &["t"], "DROP TABLE", locks.timeout())
                .unwrap(),
        );
    }

    #[test]
    fn a_held_exclusive_is_reentrant_and_moves_the_generation_when_it_lets_go() {
        let generation = Arc::new(AtomicU64::new(0));
        let locks = TableLocks::new(
            NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            20,
            None,
            Arc::clone(&generation),
        );
        let outer = locks
            .exclusive(MAIN_LEVEL, &["b", "a", "a"], "DROP TABLE", locks.timeout())
            .unwrap();
        let inner = locks
            .exclusive(MAIN_LEVEL, &["a"], "the catalog write", locks.timeout())
            .unwrap();
        assert!(!inner.took_any(), "the statement holds it");
        locks.statement_scope().lock_shared(MAIN_LEVEL, "a").unwrap();
        drop(inner);
        assert_eq!(generation.load(Ordering::Acquire), 0);
        assert!(locks.has_exclusive_holder(MAIN_LEVEL, "a"));
        drop(outer);
        assert_eq!(generation.load(Ordering::Acquire), 2, "one per table");
        assert!(!locks.has_exclusive_holder(MAIN_LEVEL, "a"));
    }

    #[test]
    fn the_inline_set_spills_and_comes_back() {
        let mut held = HeldSet::new();
        let keys: [TableKey; 7] = std::array::from_fn(|level| TableKey::new(level as u64, b"t"));
        let [k0, k1, k2, k3, k4, k5, k6] = keys;
        for key in &keys {
            held.push(Held::shared(*key));
        }
        assert_eq!(held.iter().count(), 7);
        held.remove(k1);
        held.remove(k6);
        assert_eq!(held.find(k1), None);
        assert_eq!(held.find(k6), None);
        for key in [k0, k2, k3, k4, k5] {
            assert_eq!(held.find(key), Some(LockMode::Shared));
        }
        held.upgrade(k4);
        assert_eq!(held.exclusive, 1);
        held.end_exclusive(k4);
        assert_eq!(held.find(k4), Some(LockMode::Shared), "it was shared before");
        assert_eq!(held.exclusive, 0);
        held.clear();
        assert_eq!(held.iter().count(), 0);
    }

    /// No assertion on time: a figure to read with `--nocapture --ignored` when the
    /// perf smoke moves.
    #[test]
    #[ignore = "a measurement, not a test"]
    fn the_cost_of_a_shared_acquire_with_nothing_exclusive_around() {
        let locks = locks(1000);
        let rounds: u32 = 10_000_000;
        let started = Instant::now();
        for _ in 0..rounds {
            let scope = locks.statement_scope();
            scope.lock_shared(MAIN_LEVEL, "a_table_of_a_usual_name").unwrap();
            drop(scope);
        }
        println!("{} ns per statement", started.elapsed().as_nanos() / u128::from(rounds));
    }
}
