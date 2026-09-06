//! PostgreSQL advisory locks — the `pg_advisory_lock` family.
//!
//! Advisory locks are cooperative, application-defined locks: the engine never
//! takes one on its own, it only hands out and tracks the ones a client asks
//! for by integer key. Prisma Migrate serialises every migration run with
//! `SELECT pg_advisory_lock(72707369)` and releases it with
//! `SELECT pg_advisory_unlock(72707369)`; Rails, Flyway, Liquibase and Atlas do
//! the same. Without these functions an ORM cannot run migrations at all.
//!
//! # Model
//!
//! * The lock table is **process-global** ([`manager`]), matching PostgreSQL's
//!   shared-memory `LOCKTAG_ADVISORY`: keys are shared by every connection and
//!   every database served by this process.
//! * Locks are **exclusive** only. The `_shared` variants are deliberately NOT
//!   implemented — they fall through to the evaluator's `Unknown scalar
//!   function` arm (SQLSTATE 42883) rather than being silently served as
//!   exclusive locks, which would be a correctness lie in the unsafe direction.
//! * A lock is owned by a CONNECTION — one [`SessionId`] per wire connection
//!   (`EmbeddedDatabase::create_wire_session`) or per embedded session
//!   (`EmbeddedDatabase::create_session`). It is **re-entrant for its owner**:
//!   N acquisitions need N releases.
//! * A statement that arrives with NO session has no connection identity at
//!   all: the embedded `db.query()` / `db.execute()` funnels, and through them
//!   the REST/BaaS layer, the MCP `heliosdb_query` tool and the REPL. Those get
//!   an [`AdvisoryOwnership::Statement`] owner, minted for that ONE statement.
//!   It may take a TRANSACTION-scope lock — in autocommit the statement *is*
//!   the transaction, exactly as in PostgreSQL — and is REFUSED the
//!   session-scope family (`pg_advisory_lock`, `pg_try_advisory_lock`,
//!   `pg_advisory_unlock`, `pg_advisory_unlock_all`). Two concurrent HTTP or
//!   MCP callers share one `EmbeddedDatabase`, so a session-scope lock taken
//!   there would be re-entrant for BOTH of them (no mutual exclusion at all)
//!   and would be released by nothing short of the process exiting. Refusing
//!   is the fail-closed answer; reporting "lock acquired" for a lock that
//!   serialises nothing is the one outcome an ORM's migration lock cannot
//!   survive. The refusal carries [`UNSCOPED_ADVISORY_MARKER`], which the
//!   PostgreSQL error classifier maps to `0A000 feature_not_supported`.
//! * Two scopes, exactly as PostgreSQL: [`AdvisoryScope::Session`] locks live
//!   until they are unlocked or the connection ends;
//!   [`AdvisoryScope::Transaction`] locks are released automatically at
//!   COMMIT/ROLLBACK and cannot be unlocked explicitly.
//! * The two key forms are **distinct namespaces**, as in PostgreSQL (which
//!   distinguishes them by `objsubid` 1 vs 2): `pg_advisory_lock(1)` and
//!   `pg_advisory_lock(0, 1)` are different locks.
//!
//! # Blocking
//!
//! `pg_advisory_lock` / `pg_advisory_xact_lock` block until the key is free.
//! The wait is a condition-variable wait on this module's own mutex — that
//! mutex is released for the duration of the wait, and this module takes NO
//! engine lock (catalog, storage engine, transaction slot, the global
//! `current_transaction` mutex) at any point, so a waiter never stalls another
//! connection through shared engine state.
//!
//! Statements execute synchronously **on the connection's own Tokio task**:
//! `protocol::postgres::handler::run_guarded` calls straight into the engine —
//! there is no `spawn_blocking` and no dedicated statement thread. A bare
//! condvar wait there would pin a Tokio worker, and with more waiters than
//! workers the runtime would starve (the holder's own COMMIT could never be
//! scheduled). [`run_blocking`] therefore hands the wait to
//! `tokio::task::block_in_place` whenever it runs on a multi-thread runtime,
//! which releases the worker to the scheduler for the duration of the wait.
//! Outside a runtime (embedded callers) and on a current-thread runtime the
//! wait runs inline.
//!
//! What the CALLER holds while it waits matters as much. Every wire path is
//! clear: an autocommit statement reaches `query_with_columns` /
//! `query_params_*`, which take no engine-wide lock across execution, and a
//! statement inside an explicit transaction holds only that session's own
//! transaction slot (`SessionTxnSlot`), which no other connection contends.
//! The ONE caller that holds something wider is the embedded/REPL global
//! transaction slot: `EmbeddedDatabase::query()`'s in-`BEGIN` branch keeps the
//! handle's `current_transaction` mutex for the whole statement, so a blocking
//! `pg_advisory_xact_lock` (the only blocking form a session-less caller is
//! served — see the Model section) issued inside an embedded text-`BEGIN` block
//! parks that ONE handle's other statements until it is granted. That slot
//! already serialises every statement on the handle, and no wire connection
//! uses it.
//!
//! A blocking wait honours the caller's effective `statement_timeout`
//! (`SET statement_timeout` / `[storage] statement_timeout_ms`); expiry raises
//! PostgreSQL's own `57014 query_canceled`. With no timeout configured the wait
//! is unbounded, exactly like PostgreSQL. Deadlocks between advisory locks are
//! not detected — PostgreSQL does not detect them either.
//!
//! # Observability
//!
//! `SELECT * FROM pg_advisory_locks` (the Phase-3 system-view registry) lists
//! every held key with its holder session and per-scope counters, so an
//! operator can answer "the migration is stuck on 72707369 — who has it?".

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::session::SessionId;
use crate::{Error, Result, Value};

/// An advisory-lock key.
///
/// PostgreSQL exposes two overloads and keeps them in separate namespaces (the
/// lock tag's `objsubid` is 1 for the `bigint` form and 2 for the `(int, int)`
/// form). The enum discriminant models that, so a key acquired through one
/// overload can never be released through the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AdvisoryKey {
    /// `pg_advisory_lock(key bigint)`.
    BigInt(i64),
    /// `pg_advisory_lock(key1 int, key2 int)`.
    Pair(i32, i32),
}

impl AdvisoryKey {
    /// `"bigint"` or `"int_pair"` — the `key_kind` column of `pg_advisory_locks`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BigInt(_) => "bigint",
            Self::Pair(_, _) => "int_pair",
        }
    }

    /// First key component of the `(int, int)` form; `None` for the `bigint` form.
    pub fn classid(&self) -> Option<i32> {
        match self {
            Self::BigInt(_) => None,
            Self::Pair(a, _) => Some(*a),
        }
    }

    /// The `bigint` key, or the second component of the `(int, int)` form.
    pub fn objid(&self) -> i64 {
        match self {
            Self::BigInt(k) => *k,
            Self::Pair(_, b) => i64::from(*b),
        }
    }
}

/// What the owner of an advisory lock actually *is*, and therefore how long it
/// can possibly live.
///
/// This is the difference between a lock that serialises two clients and a lock
/// that only looks like it does: [`AdvisoryOwnership::Statement`] owners are
/// unique per statement, so they can neither exclude a second statement beyond
/// their own lifetime nor be named again to release anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryOwnership {
    /// A real connection: a PostgreSQL/MySQL wire session, or an embedded
    /// session from `EmbeddedDatabase::create_session`. Session-scope locks
    /// live until `pg_advisory_unlock` or connection teardown
    /// (`destroy_session`).
    Connection,
    /// A session-less statement (embedded `db.query()`/`db.execute()`, and so
    /// the REST/BaaS layer, MCP and the REPL). The owner is minted for this one
    /// statement and everything it holds is released when the statement ends;
    /// the session-scope family is refused outright.
    Statement,
}

/// Substring carried by every "you have no connection to own this lock" error.
///
/// `sqlstate_for_query_execution_message` (src/protocol/postgres/handler.rs)
/// matches on it to report `0A000 feature_not_supported` instead of the
/// catch-all `XX000 internal_error`, which poolers and HA proxies read as a
/// server fault. Kept here, next to its only emitter, so the two cannot drift.
pub const UNSCOPED_ADVISORY_MARKER: &str = "advisory locks require a client session";

/// How long an advisory lock lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryScope {
    /// Held until explicitly unlocked or the owning session ends.
    Session,
    /// Released automatically at COMMIT/ROLLBACK; cannot be unlocked explicitly.
    Transaction,
}

/// The single holder of one key, with a re-entrancy counter per scope.
///
/// A key has at most one owner (locks are exclusive), but that owner may hold
/// it at both scopes at once — PostgreSQL allows `pg_advisory_lock(1)` and
/// `pg_advisory_xact_lock(1)` in the same session, and `pg_advisory_unlock(1)`
/// then releases only the session-level hold.
#[derive(Debug, Clone, Copy)]
struct Holder {
    owner: SessionId,
    session_count: u64,
    xact_count: u64,
}

impl Holder {
    fn new(owner: SessionId) -> Self {
        Self {
            owner,
            session_count: 0,
            xact_count: 0,
        }
    }

    fn bump(&mut self, scope: AdvisoryScope) {
        match scope {
            AdvisoryScope::Session => self.session_count += 1,
            AdvisoryScope::Transaction => self.xact_count += 1,
        }
    }

    fn is_empty(&self) -> bool {
        self.session_count == 0 && self.xact_count == 0
    }
}

/// One row of the `pg_advisory_locks` system view.
#[derive(Debug, Clone, Copy)]
pub struct AdvisoryLockInfo {
    /// The key.
    pub key: AdvisoryKey,
    /// The session that holds it.
    pub owner: SessionId,
    /// Session-level acquisitions outstanding (needs this many unlocks).
    pub session_count: u64,
    /// Transaction-level acquisitions outstanding (released at COMMIT/ROLLBACK).
    pub xact_count: u64,
}

/// The process-global advisory-lock table.
pub struct AdvisoryLockManager {
    locks: parking_lot::Mutex<HashMap<AdvisoryKey, Holder>>,
    /// Signalled every time a key becomes free, so blocking waiters re-check.
    released: parking_lot::Condvar,
    /// Lock-free mirror of `locks.len()`.
    ///
    /// PERF, and the reason it exists: the release hooks sit on paths that run
    /// for every connection teardown and every COMMIT/ROLLBACK — including on
    /// deployments that never call an advisory function. Taking one
    /// process-global mutex there would put every connection on the same cache
    /// line for nothing. These two counters make "there is nothing to release"
    /// a single load. They are only ever written while `locks` is held, so they
    /// cannot drift, and they are `Release`/`Acquire` so a session whose
    /// statements hop Tokio worker threads still observes its own acquisition
    /// when its COMMIT lands on a different worker.
    entries: std::sync::atomic::AtomicUsize,
    /// Lock-free count of entries with a TRANSACTION-scope hold. Lets
    /// COMMIT/ROLLBACK skip the mutex entirely in the overwhelmingly common
    /// case where no transaction-scope advisory lock exists in this process.
    xact_entries: std::sync::atomic::AtomicUsize,
}

impl AdvisoryLockManager {
    fn new() -> Self {
        Self {
            locks: parking_lot::Mutex::new(HashMap::new()),
            released: parking_lot::Condvar::new(),
            entries: std::sync::atomic::AtomicUsize::new(0),
            xact_entries: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Recompute the lock-free counters. Called under the `locks` mutex by
    /// every mutating operation — all of which are advisory-lock calls, i.e.
    /// rare, so the `O(table)` transaction-scope tally is not on any hot path.
    fn refresh_counters(&self, locks: &HashMap<AdvisoryKey, Holder>) {
        use std::sync::atomic::Ordering;
        self.entries.store(locks.len(), Ordering::Release);
        self.xact_entries
            .store(locks.values().filter(|h| h.xact_count > 0).count(), Ordering::Release);
    }

    /// Does this process hold ANY advisory lock? One atomic load — the gate the
    /// connection-teardown path uses.
    pub fn has_locks(&self) -> bool {
        self.entries.load(std::sync::atomic::Ordering::Acquire) > 0
    }

    /// Does this process hold any TRANSACTION-scope advisory lock? One atomic
    /// load — the gate COMMIT/ROLLBACK use.
    pub fn has_transaction_locks(&self) -> bool {
        self.xact_entries.load(std::sync::atomic::Ordering::Acquire) > 0
    }

    /// Refuse a brand-new key once the owner is at `max_per_session`
    /// (`[locks] max_advisory_locks_per_session`; 0 disables the cap).
    ///
    /// Fails CLOSED: an owner whose locks cannot be tracked does not get the
    /// lock. PostgreSQL reports the analogous shared-memory exhaustion as
    /// `53200 out_of_memory`. Only reached on the cold path that would add a
    /// NEW key for this owner, so the O(table) scan never runs on re-entry.
    fn check_quota(locks: &HashMap<AdvisoryKey, Holder>, owner: SessionId, max_per_session: u32) -> Result<()> {
        if max_per_session == 0 {
            return Ok(());
        }
        let held = locks.values().filter(|h| h.owner == owner).count();
        if held >= max_per_session as usize {
            return Err(Error::query_execution(format!(
                "out of advisory lock slots: session already holds {max_per_session} advisory locks \
                 (raise [locks] max_advisory_locks_per_session, or release locks with \
                 pg_advisory_unlock_all())"
            )));
        }
        Ok(())
    }

    /// Non-blocking acquisition — the `pg_try_advisory_*` family, and the fast
    /// path of the blocking ones.
    ///
    /// `Ok(true)` when the lock is now held by `owner` (including a re-entrant
    /// acquisition), `Ok(false)` when another session holds it, `Err` when the
    /// owner is at its lock quota.
    pub fn try_acquire(
        &self,
        key: AdvisoryKey,
        owner: SessionId,
        scope: AdvisoryScope,
        max_per_session: u32,
    ) -> Result<bool> {
        let mut locks = self.locks.lock();
        // Read the owner out first (a `Copy` value) so no borrow of the map
        // survives into the insert arm.
        let current_owner = locks.get(&key).map(|holder| holder.owner);
        match current_owner {
            Some(current) if current == owner => {
                if let Some(holder) = locks.get_mut(&key) {
                    holder.bump(scope);
                }
                self.refresh_counters(&locks);
                Ok(true)
            }
            Some(_) => Ok(false),
            None => {
                Self::check_quota(&locks, owner, max_per_session)?;
                let mut holder = Holder::new(owner);
                holder.bump(scope);
                locks.insert(key, holder);
                self.refresh_counters(&locks);
                Ok(true)
            }
        }
    }

    /// Blocking acquisition — `pg_advisory_lock` / `pg_advisory_xact_lock`.
    ///
    /// Waits on the condvar (which releases this module's mutex, and holds no
    /// engine lock) until the key is free. `timeout` is the caller's effective
    /// `statement_timeout`; `None` waits indefinitely, as PostgreSQL does.
    pub fn acquire(
        &self,
        key: AdvisoryKey,
        owner: SessionId,
        scope: AdvisoryScope,
        timeout: Option<Duration>,
        max_per_session: u32,
    ) -> Result<()> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut locks = self.locks.lock();
        loop {
            let current_owner = locks.get(&key).map(|holder| holder.owner);
            match current_owner {
                Some(current) if current == owner => {
                    if let Some(holder) = locks.get_mut(&key) {
                        holder.bump(scope);
                    }
                    self.refresh_counters(&locks);
                    return Ok(());
                }
                None => {
                    Self::check_quota(&locks, owner, max_per_session)?;
                    let mut holder = Holder::new(owner);
                    holder.bump(scope);
                    locks.insert(key, holder);
                    self.refresh_counters(&locks);
                    return Ok(());
                }
                Some(_) => {}
            }

            match deadline {
                None => {
                    self.released.wait(&mut locks);
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        // PostgreSQL cancels the STATEMENT on statement_timeout
                        // (57014); it does not report a lock-specific error.
                        return Err(Error::query_timeout(
                            "canceling statement due to statement timeout while waiting for advisory lock",
                        ));
                    }
                    let _ = self.released.wait_for(&mut locks, deadline - now);
                }
            }
        }
    }

    /// `pg_advisory_unlock(...)` — release ONE session-level acquisition.
    ///
    /// Returns `false` (and logs at WARN, where PostgreSQL emits
    /// `WARNING: you don't own a lock of type ExclusiveLock`) when this session
    /// holds no session-level lock on the key. A transaction-level hold is not
    /// unlockable, so it does not count.
    pub fn unlock(&self, key: AdvisoryKey, owner: SessionId) -> bool {
        let mut locks = self.locks.lock();
        let unlockable = locks
            .get(&key)
            .is_some_and(|holder| holder.owner == owner && holder.session_count > 0);
        if !unlockable {
            tracing::warn!(
                key = ?key,
                session = owner.0,
                "pg_advisory_unlock: you don't own a lock of type ExclusiveLock"
            );
            return false;
        }
        let mut now_free = false;
        if let Some(holder) = locks.get_mut(&key) {
            holder.session_count -= 1;
            now_free = holder.is_empty();
        }
        if now_free {
            locks.remove(&key);
        }
        self.refresh_counters(&locks);
        if now_free {
            drop(locks);
            self.released.notify_all();
        }
        true
    }

    /// `pg_advisory_unlock_all()` — drop every SESSION-level lock this session
    /// holds. Transaction-level locks are untouched (PostgreSQL parity).
    pub fn unlock_all(&self, owner: SessionId) -> u64 {
        self.release_matching(owner, |holder| {
            let n = holder.session_count;
            holder.session_count = 0;
            n
        })
    }

    /// Release this session's TRANSACTION-level locks. Called at COMMIT and at
    /// ROLLBACK, and at the end of an autocommit statement (whose implicit
    /// transaction ends there).
    pub fn release_transaction(&self, owner: SessionId) -> u64 {
        self.release_matching(owner, |holder| {
            let n = holder.xact_count;
            holder.xact_count = 0;
            n
        })
    }

    /// Release the TRANSACTION-scope acquisitions listed in `keys` — one
    /// decrement per entry, so a statement that took the same key twice gives
    /// back exactly two holds and no more.
    ///
    /// This is what an autocommit statement's [`AdvisoryContextGuard`] calls,
    /// and it exists because [`release_transaction`](Self::release_transaction)
    /// zeroes EVERY transaction-scope hold of the owner. That is right at
    /// COMMIT/ROLLBACK, where the whole transaction ends, and wrong at the end
    /// of ONE statement: it would hand back a key that a different statement of
    /// the same owner is still relying on, letting a third party take a key its
    /// holder still believes it owns.
    pub fn release_transaction_keys(&self, owner: SessionId, keys: &[AdvisoryKey]) -> u64 {
        if keys.is_empty() {
            return 0;
        }
        let mut locks = self.locks.lock();
        let mut freed = 0_u64;
        let mut any_dropped = false;
        for key in keys {
            let mut now_free = false;
            if let Some(holder) = locks.get_mut(key) {
                if holder.owner != owner || holder.xact_count == 0 {
                    continue;
                }
                holder.xact_count -= 1;
                freed += 1;
                now_free = holder.is_empty();
            }
            if now_free {
                locks.remove(key);
                any_dropped = true;
            }
        }
        self.refresh_counters(&locks);
        drop(locks);
        if any_dropped {
            self.released.notify_all();
        }
        freed
    }

    /// Release EVERY lock this session holds — the connection ended.
    pub fn release_session(&self, owner: SessionId) -> u64 {
        self.release_matching(owner, |holder| {
            let n = holder.session_count + holder.xact_count;
            holder.session_count = 0;
            holder.xact_count = 0;
            n
        })
    }

    fn release_matching(&self, owner: SessionId, mut zero: impl FnMut(&mut Holder) -> u64) -> u64 {
        let mut locks = self.locks.lock();
        let mut freed = 0_u64;
        let mut drop_keys: Vec<AdvisoryKey> = Vec::new();
        for (key, holder) in locks.iter_mut() {
            if holder.owner != owner {
                continue;
            }
            freed += zero(holder);
            if holder.is_empty() {
                drop_keys.push(*key);
            }
        }
        let any_dropped = !drop_keys.is_empty();
        for key in drop_keys {
            locks.remove(&key);
        }
        self.refresh_counters(&locks);
        drop(locks);
        if any_dropped {
            self.released.notify_all();
        }
        freed
    }

    /// Rows for the `pg_advisory_locks` system view, in a stable order.
    pub fn snapshot(&self) -> Vec<AdvisoryLockInfo> {
        let mut rows: Vec<AdvisoryLockInfo> = self
            .locks
            .lock()
            .iter()
            .map(|(key, holder)| AdvisoryLockInfo {
                key: *key,
                owner: holder.owner,
                session_count: holder.session_count,
                xact_count: holder.xact_count,
            })
            .collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key).then(a.owner.0.cmp(&b.owner.0)));
        rows
    }
}

/// The one process-global lock table (PostgreSQL's shared-memory equivalent).
pub fn manager() -> &'static AdvisoryLockManager {
    static MANAGER: std::sync::LazyLock<AdvisoryLockManager> = std::sync::LazyLock::new(AdvisoryLockManager::new);
    &MANAGER
}

// ============================================================================
// Per-statement execution context
// ============================================================================

/// Who is asking, and under what statement budget.
///
/// The expression evaluator is storage-less and session-less, so the engine
/// installs this on the calling thread for the duration of ONE statement — the
/// same mechanism `current_schema()` uses (`crate::session_current_schema_tls`).
#[derive(Debug, Clone, Copy)]
pub struct AdvisoryContext {
    /// Lock owner, when this statement has one.
    ///
    /// `None` means the statement can own nothing — a session-less path whose
    /// text cannot reach an advisory call (so no owner was minted) — and every
    /// advisory function refuses with [`UNSCOPED_ADVISORY_MARKER`] rather than
    /// attributing the lock to somebody else.
    pub owner: Option<SessionId>,
    /// Whether [`Self::owner`] is a real connection or a one-statement stand-in.
    /// Decides both which functions are served and how the guard releases.
    pub ownership: AdvisoryOwnership,
    /// Effective `statement_timeout` for this statement, if any.
    pub statement_timeout: Option<Duration>,
    /// `[locks] max_advisory_locks_per_session` (0 = unlimited).
    pub max_locks_per_session: u32,
    /// Was an explicit transaction already open when this statement started?
    ///
    /// When it was not, the statement runs in its own implicit transaction and
    /// the [`AdvisoryScope::Transaction`] locks it takes end with the statement
    /// — exactly what PostgreSQL does with `pg_advisory_xact_lock()` in
    /// autocommit.
    pub in_explicit_transaction: bool,
    /// Set by [`evaluate`] when this statement takes a transaction-scope lock.
    ///
    /// PERF: without it, the guard's `Drop` would have to take the
    /// process-global lock mutex after EVERY autocommit statement just to find
    /// nothing to release. With it, a statement that never called an advisory
    /// function ends with two thread-local accesses and no shared state at all.
    pub took_transaction_lock: bool,
    /// Set by [`evaluate`] when this statement takes a session-scope lock.
    ///
    /// Only an [`AdvisoryOwnership::Connection`] owner can reach that today, but
    /// the guard's statement-scoped sweep is gated on "did this statement take
    /// ANYTHING", so the flag is what keeps that sweep correct if a future path
    /// ever grants one under a [`AdvisoryOwnership::Statement`] owner.
    pub took_session_lock: bool,
}

thread_local! {
    /// Installed by [`AdvisoryContextGuard`] for the duration of one statement.
    static CONTEXT: std::cell::Cell<Option<AdvisoryContext>> = const { std::cell::Cell::new(None) };
    /// The TRANSACTION-scope keys the statement in flight actually acquired,
    /// one entry per acquisition.
    ///
    /// The guard releases exactly these and nothing else. Owner-wide release
    /// (`release_transaction`) belongs to COMMIT/ROLLBACK, where the whole
    /// transaction ends; at the end of a single statement it would give back a
    /// key another statement of the same owner still holds.
    ///
    /// Never allocates for a statement that takes no transaction-scope lock,
    /// which is every statement in every workload that does not call the
    /// advisory family.
    static XACT_KEYS: std::cell::RefCell<Vec<AdvisoryKey>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// RAII installer for [`AdvisoryContext`].
///
/// Installs only when no context is present, so nested engine calls (a wire
/// `_for_session` entry point delegating to the embedded autocommit funnel, a
/// trigger body, a PL/pgSQL `CALL`) keep the OUTERMOST owner rather than
/// re-attributing the lock to the inner handle. Only the outermost guard clears
/// the slot, and only it ends the implicit transaction's xact-scope locks.
#[must_use = "the advisory-lock context is uninstalled when this guard drops"]
pub struct AdvisoryContextGuard(Option<AdvisoryContext>);

impl AdvisoryContextGuard {
    /// Install the context `build` produces, unless one is already active on
    /// this thread.
    ///
    /// `build` is a closure, not a value, so the nested case (a `_for_session`
    /// entry point delegating into the shared embedded funnel, which installs
    /// again) costs one thread-local read and nothing else — none of the
    /// context's inputs are gathered a second time.
    pub fn install_if_absent(build: impl FnOnce() -> AdvisoryContext) -> Self {
        CONTEXT.with(|slot| {
            if slot.get().is_some() {
                return Self(None);
            }
            let ctx = build();
            slot.set(Some(ctx));
            Self(Some(ctx))
        })
    }
}

impl Drop for AdvisoryContextGuard {
    fn drop(&mut self) {
        let Some(installed) = self.0 else { return };
        // Take the LIVE context, not the copy this guard installed: `evaluate`
        // updates it in place to record what the statement acquired.
        let live = CONTEXT.with(|slot| slot.replace(None));
        let Some(live) = live else { return };
        if !live.took_transaction_lock && !live.took_session_lock {
            // The overwhelming majority of statements: never called an advisory
            // function, so there is nothing to release and no shared state is
            // touched at all.
            return;
        }
        let Some(owner) = installed.owner else { return };
        // Drain the per-statement key list in EVERY branch below. Leaving keys
        // behind would make the NEXT statement on this thread release holds it
        // never took.
        let took = XACT_KEYS.with(|keys| std::mem::take(&mut *keys.borrow_mut()));
        match installed.ownership {
            AdvisoryOwnership::Statement => {
                // The owner was minted for this statement and can never be
                // named again, so everything it holds dies here. This is what
                // stops a session-less caller (REST/BaaS, MCP, the embedded
                // funnel, the REPL) from stranding a key for the life of the
                // process on a lock table no connection can reach.
                manager().release_session(owner);
            }
            AdvisoryOwnership::Connection => {
                if live.took_transaction_lock && !installed.in_explicit_transaction {
                    // Autocommit: the statement WAS the transaction, so the
                    // transaction-scope locks IT took end here (PostgreSQL
                    // parity) — exactly those keys, never every
                    // transaction-scope hold of the connection.
                    manager().release_transaction_keys(owner, &took);
                }
            }
        }
    }
}

/// The advisory context installed on this thread, if any.
pub fn current_context() -> Option<AdvisoryContext> {
    CONTEXT.with(std::cell::Cell::get)
}

/// Record that the statement in flight took a transaction-scope advisory lock
/// on `key`, so the guard's `Drop` knows what to release in autocommit.
fn mark_transaction_lock_taken(key: AdvisoryKey) {
    CONTEXT.with(|slot| {
        if let Some(mut ctx) = slot.get() {
            ctx.took_transaction_lock = true;
            slot.set(Some(ctx));
        }
    });
    XACT_KEYS.with(|keys| keys.borrow_mut().push(key));
}

/// Record that the statement in flight took a session-scope advisory lock.
fn mark_session_lock_taken() {
    CONTEXT.with(|slot| {
        if let Some(mut ctx) = slot.get() {
            ctx.took_session_lock = true;
            slot.set(Some(ctx));
        }
    });
}

/// The session-scope half of the family: locks (and unlocks) that must outlive
/// the statement that took them, and therefore need a real connection to own
/// them. Refused under an [`AdvisoryOwnership::Statement`] owner.
fn is_session_scoped(function: &str) -> bool {
    matches!(
        function,
        "pg_advisory_lock" | "pg_try_advisory_lock" | "pg_advisory_unlock" | "pg_advisory_unlock_all"
    )
}

/// The error every session-less path gets for the session-scope family.
///
/// Fails CLOSED and says how to get the feature: the alternative — granting a
/// lock that is re-entrant for every other caller of the same handle and that
/// nothing ever releases — is a silent serialisation failure in a migration
/// runner, which is strictly worse than an error the client can see.
fn unscoped_advisory_error(function: &str) -> Error {
    Error::query_execution(format!(
        "{function}(): session-level {marker}, and this statement arrived on a \
         session-less execution path (embedded db.query()/db.execute(), the REST/BaaS layer, the \
         MCP query tool, the REPL). A lock taken there would be shared by every concurrent caller \
         of the same database handle — no mutual exclusion — and nothing would ever release it. \
         Use the PostgreSQL or MySQL wire protocol, or EmbeddedDatabase::create_session() with the \
         *_for_session entry points. pg_advisory_xact_lock() and pg_try_advisory_xact_lock() ARE \
         served here: they are released when the statement ends, exactly as PostgreSQL releases a \
         transaction-scope lock taken in autocommit.",
        marker = UNSCOPED_ADVISORY_MARKER,
    ))
}

// ============================================================================
// SQL surface
// ============================================================================

/// Every function name this module serves — the canonical list.
///
/// The evaluator's dispatch arm (`sql/evaluator.rs`) and
/// `EmbeddedDatabase::query_is_non_deterministic`'s `PG_ADVISORY` /
/// `PG_TRY_ADVISORY` needles must both cover exactly these;
/// `every_listed_function_is_dispatched` below asserts the first half, so a name
/// added here without an [`evaluate`] arm fails the build's tests rather than
/// silently reporting `Unknown scalar function` to a client.
///
/// The `_shared` variants are deliberately absent — see the module docs.
pub const FUNCTION_NAMES: &[&str] = &[
    "pg_advisory_lock",
    "pg_advisory_xact_lock",
    "pg_try_advisory_lock",
    "pg_try_advisory_xact_lock",
    "pg_advisory_unlock",
    "pg_advisory_unlock_all",
];

/// Coerce one argument to an integer key component.
fn key_component(value: &Value, function: &str) -> Result<i64> {
    match value {
        Value::Int2(v) => Ok(i64::from(*v)),
        Value::Int4(v) => Ok(i64::from(*v)),
        Value::Int8(v) => Ok(*v),
        // A numeric/text literal that is exactly an integer is accepted — the
        // same widening PostgreSQL's implicit cast to bigint performs. Anything
        // fractional or non-numeric is a type error, never a silent truncation.
        Value::Numeric(s) | Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| Error::query_execution(format!("{function}() requires integer key arguments, got '{s}'"))),
        other => Err(Error::query_execution(format!(
            "{function}() requires integer key arguments, got {:?}",
            other.data_type()
        ))),
    }
}

/// Build the key from the evaluated argument list.
///
/// One argument = the `bigint` overload; two = the `(int, int)` overload. NULL
/// is rejected rather than treated as a key: PostgreSQL's advisory functions are
/// STRICT, and silently locking key 0 for a NULL would hand out a lock the
/// caller never asked for.
fn key_from_args(function: &str, args: &[Value]) -> Result<AdvisoryKey> {
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Err(Error::query_execution(format!(
            "{function}() does not accept NULL key arguments"
        )));
    }
    match args {
        [single] => Ok(AdvisoryKey::BigInt(key_component(single, function)?)),
        [first, second] => {
            let to_i32 = |v: &Value| -> Result<i32> {
                let raw = key_component(v, function)?;
                i32::try_from(raw).map_err(|_| {
                    Error::query_execution(format!(
                        "{function}(int, int) key component {raw} is out of range for integer"
                    ))
                })
            };
            Ok(AdvisoryKey::Pair(to_i32(first)?, to_i32(second)?))
        }
        _ => Err(Error::query_execution(format!(
            "{function}() takes one bigint key or two integer keys, got {} arguments",
            args.len()
        ))),
    }
}

/// Run a blocking wait without wedging the Tokio worker serving this connection.
///
/// Statements run synchronously on the connection's own task, so a bare condvar
/// wait would park a worker thread; with more waiters than workers the holder's
/// COMMIT could never be scheduled and the server would livelock.
/// `block_in_place` tells the multi-thread scheduler to move this worker's
/// remaining tasks elsewhere first. It is only legal on the multi-thread
/// flavour, so a current-thread runtime (and the embedded, runtime-less caller)
/// waits inline.
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if matches!(handle.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread) => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Evaluate one `pg_advisory_*` call. `function` is the lowercased, unqualified
/// name; `args` are the already-evaluated arguments.
///
/// Fails CLOSED when the statement has no owner that could hold the lock:
///
/// * no [`AdvisoryContext`] installed at all, or one with no `owner` — the lock
///   could be neither attributed nor released; and
/// * the session-scope family under an [`AdvisoryOwnership::Statement`] owner —
///   a session-less caller (REST/BaaS, MCP, the embedded funnel, the REPL)
///   shares its handle with every other concurrent caller, so the lock would
///   exclude nobody and outlive everybody.
///
/// Reporting "acquired" for a lock nobody owns is exactly the failure an ORM's
/// migration serialiser cannot survive, so both cases raise instead.
pub fn evaluate(function: &str, args: &[Value]) -> Result<Value> {
    let Some(ctx) = current_context() else {
        return Err(unscoped_advisory_error(function));
    };
    let Some(owner) = ctx.owner else {
        return Err(unscoped_advisory_error(function));
    };
    if matches!(ctx.ownership, AdvisoryOwnership::Statement) && is_session_scoped(function) {
        return Err(unscoped_advisory_error(function));
    }
    let mgr = manager();

    match function {
        "pg_advisory_unlock_all" => {
            if !args.is_empty() {
                return Err(Error::query_execution(
                    "pg_advisory_unlock_all() takes no arguments".to_string(),
                ));
            }
            mgr.unlock_all(owner);
            // `void`
            Ok(Value::Null)
        }
        "pg_advisory_unlock" => {
            let key = key_from_args(function, args)?;
            Ok(Value::Boolean(mgr.unlock(key, owner)))
        }
        "pg_try_advisory_lock" | "pg_try_advisory_xact_lock" => {
            let key = key_from_args(function, args)?;
            let scope = if function == "pg_try_advisory_xact_lock" {
                AdvisoryScope::Transaction
            } else {
                AdvisoryScope::Session
            };
            let acquired = mgr.try_acquire(key, owner, scope, ctx.max_locks_per_session)?;
            if acquired {
                if scope == AdvisoryScope::Transaction {
                    mark_transaction_lock_taken(key);
                } else {
                    mark_session_lock_taken();
                }
            }
            Ok(Value::Boolean(acquired))
        }
        "pg_advisory_lock" | "pg_advisory_xact_lock" => {
            let key = key_from_args(function, args)?;
            let scope = if function == "pg_advisory_xact_lock" {
                AdvisoryScope::Transaction
            } else {
                AdvisoryScope::Session
            };
            // Fast path: an uncontended acquisition never touches the scheduler.
            if !mgr.try_acquire(key, owner, scope, ctx.max_locks_per_session)? {
                run_blocking(|| mgr.acquire(key, owner, scope, ctx.statement_timeout, ctx.max_locks_per_session))?;
            }
            if scope == AdvisoryScope::Transaction {
                mark_transaction_lock_taken(key);
            } else {
                mark_session_lock_taken();
            }
            // `void`
            Ok(Value::Null)
        }
        other => Err(Error::query_execution(format!("Unknown scalar function: {other}"))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A wire/embedded SESSION context — the connection-owned case.
    fn ctx(owner: SessionId) -> AdvisoryContext {
        AdvisoryContext {
            owner: Some(owner),
            ownership: AdvisoryOwnership::Connection,
            statement_timeout: None,
            max_locks_per_session: 0,
            in_explicit_transaction: true,
            took_transaction_lock: false,
            took_session_lock: false,
        }
    }

    /// A session-LESS context — the embedded funnel / REST / MCP / REPL case,
    /// where the owner is minted for this one statement.
    fn statement_ctx(owner: SessionId) -> AdvisoryContext {
        AdvisoryContext {
            owner: Some(owner),
            ownership: AdvisoryOwnership::Statement,
            statement_timeout: None,
            max_locks_per_session: 0,
            // A statement-scoped owner is never inside anybody's explicit
            // transaction: its locks end with the statement, whatever some
            // unrelated caller of the same handle is doing.
            in_explicit_transaction: false,
            took_transaction_lock: false,
            took_session_lock: false,
        }
    }

    /// Distinct keys per test: the table is process-global by design, and the
    /// unit tests in this binary run concurrently.
    fn key(n: i64) -> AdvisoryKey {
        AdvisoryKey::BigInt(-900_000_000_000 - n)
    }

    #[test]
    fn reentrant_counter_needs_matching_unlocks() {
        let mgr = manager();
        let a = SessionId::new();
        let k = key(1);
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Session, 0).unwrap());
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Session, 0).unwrap());
        assert!(mgr.unlock(k, a));
        // Still held after ONE unlock.
        let b = SessionId::new();
        assert!(!mgr.try_acquire(k, b, AdvisoryScope::Session, 0).unwrap());
        assert!(mgr.unlock(k, a));
        assert!(mgr.try_acquire(k, b, AdvisoryScope::Session, 0).unwrap());
        mgr.release_session(b);
    }

    #[test]
    fn unlock_of_a_lock_you_do_not_own_is_false() {
        let mgr = manager();
        let a = SessionId::new();
        let b = SessionId::new();
        let k = key(2);
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Session, 0).unwrap());
        assert!(!mgr.unlock(k, b));
        assert!(!mgr.unlock(key(3), b));
        mgr.release_session(a);
    }

    #[test]
    fn transaction_scope_is_released_independently_of_session_scope() {
        let mgr = manager();
        let a = SessionId::new();
        let k = key(4);
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Session, 0).unwrap());
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Transaction, 0).unwrap());
        // pg_advisory_unlock only releases the session-level hold.
        assert!(mgr.unlock(k, a));
        let b = SessionId::new();
        assert!(!mgr.try_acquire(k, b, AdvisoryScope::Session, 0).unwrap());
        assert_eq!(mgr.release_transaction(a), 1);
        assert!(mgr.try_acquire(k, b, AdvisoryScope::Session, 0).unwrap());
        mgr.release_session(b);
    }

    #[test]
    fn unlock_all_leaves_transaction_locks_in_place() {
        let mgr = manager();
        let a = SessionId::new();
        let session_key = key(5);
        let xact_key = key(6);
        assert!(mgr.try_acquire(session_key, a, AdvisoryScope::Session, 0).unwrap());
        assert!(mgr.try_acquire(xact_key, a, AdvisoryScope::Transaction, 0).unwrap());
        assert_eq!(mgr.unlock_all(a), 1);
        let b = SessionId::new();
        assert!(mgr.try_acquire(session_key, b, AdvisoryScope::Session, 0).unwrap());
        assert!(!mgr.try_acquire(xact_key, b, AdvisoryScope::Session, 0).unwrap());
        mgr.release_session(a);
        mgr.release_session(b);
    }

    #[test]
    fn bigint_and_int_pair_keys_are_distinct_namespaces() {
        let mgr = manager();
        let a = SessionId::new();
        let b = SessionId::new();
        assert!(mgr
            .try_acquire(AdvisoryKey::BigInt(-777_000_001), a, AdvisoryScope::Session, 0)
            .unwrap());
        // (0, k) is a different lock tag even though the numbers line up.
        assert!(mgr
            .try_acquire(AdvisoryKey::Pair(0, -777_000_001), b, AdvisoryScope::Session, 0)
            .unwrap());
        mgr.release_session(a);
        mgr.release_session(b);
    }

    #[test]
    fn quota_refuses_a_new_key_and_still_allows_reentry() {
        let mgr = manager();
        let a = SessionId::new();
        assert!(mgr.try_acquire(key(7), a, AdvisoryScope::Session, 1).unwrap());
        // Re-entering an already-held key must not be refused by the quota.
        assert!(mgr.try_acquire(key(7), a, AdvisoryScope::Session, 1).unwrap());
        let err = mgr.try_acquire(key(8), a, AdvisoryScope::Session, 1).unwrap_err();
        assert!(err.to_string().contains("out of advisory lock slots"), "{err}");
        mgr.release_session(a);
    }

    #[test]
    fn blocking_acquire_times_out_with_query_canceled() {
        let mgr = manager();
        let a = SessionId::new();
        let b = SessionId::new();
        let k = key(9);
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Session, 0).unwrap());
        let err = mgr
            .acquire(k, b, AdvisoryScope::Session, Some(Duration::from_millis(50)), 0)
            .unwrap_err();
        assert!(matches!(err, Error::QueryTimeout(_)), "{err:?}");
        mgr.release_session(a);
    }

    #[test]
    fn blocking_acquire_wakes_when_the_holder_releases() {
        let mgr = manager();
        let a = SessionId::new();
        let b = SessionId::new();
        let k = key(11);
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Session, 0).unwrap());
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            manager().unlock(k, a);
        });
        // Would time out if the release never woke the waiter.
        mgr.acquire(k, b, AdvisoryScope::Session, Some(Duration::from_secs(10)), 0)
            .expect("waiter must acquire once the holder releases");
        let _ = releaser.join();
        mgr.release_session(b);
    }

    #[test]
    fn key_parsing_rejects_null_and_wrong_arity() {
        assert!(key_from_args("pg_advisory_lock", &[Value::Null]).is_err());
        assert!(key_from_args("pg_advisory_lock", &[]).is_err());
        assert!(key_from_args("pg_advisory_lock", &[Value::Int4(1), Value::Int4(2), Value::Int4(3)]).is_err());
        assert_eq!(
            key_from_args("pg_advisory_lock", &[Value::Int8(72_707_369)]).unwrap(),
            AdvisoryKey::BigInt(72_707_369)
        );
        assert_eq!(
            key_from_args("pg_advisory_lock", &[Value::Int4(1), Value::Int4(2)]).unwrap(),
            AdvisoryKey::Pair(1, 2)
        );
    }

    #[test]
    fn evaluate_without_a_context_fails_closed() {
        let err = evaluate("pg_advisory_lock", &[Value::Int8(1)]).unwrap_err();
        assert!(err.to_string().contains(UNSCOPED_ADVISORY_MARKER), "{err}");
    }

    /// A session-less statement must NOT be handed a session-scope lock: its
    /// owner is unique to the statement, so the lock would exclude nobody and
    /// could be released by nothing.
    #[test]
    fn statement_scoped_owner_refuses_the_session_scope_family() {
        let owner = SessionId::new();
        let _guard = AdvisoryContextGuard::install_if_absent(|| statement_ctx(owner));
        for name in ["pg_advisory_lock", "pg_try_advisory_lock", "pg_advisory_unlock"] {
            let err = evaluate(name, &[Value::Int8(key(20).objid())]).unwrap_err();
            assert!(err.to_string().contains(UNSCOPED_ADVISORY_MARKER), "{name}: {err}");
        }
        let err = evaluate("pg_advisory_unlock_all", &[]).unwrap_err();
        assert!(err.to_string().contains(UNSCOPED_ADVISORY_MARKER), "{err}");
        // Nothing was taken, so nothing can be stranded.
        let other = SessionId::new();
        assert!(manager()
            .try_acquire(key(20), other, AdvisoryScope::Session, 0)
            .unwrap());
        manager().release_session(other);
    }

    /// The transaction-scope half IS served to a session-less statement — in
    /// autocommit the statement is the transaction — and it is released when
    /// the statement ends, so nothing strands.
    #[test]
    fn statement_scoped_owner_serves_xact_locks_and_releases_them() {
        let owner = SessionId::new();
        let k = key(21);
        {
            let _guard = AdvisoryContextGuard::install_if_absent(|| statement_ctx(owner));
            let out = evaluate("pg_try_advisory_xact_lock", &[Value::Int8(k.objid())]).unwrap();
            assert_eq!(out, Value::Boolean(true));
        }
        let other = SessionId::new();
        assert!(
            manager().try_acquire(k, other, AdvisoryScope::Session, 0).unwrap(),
            "a statement-scoped xact lock must not outlive its statement"
        );
        manager().release_session(other);
    }

    /// Regression: an autocommit statement must release ONLY the
    /// transaction-scope keys it took itself.
    ///
    /// Before the fix the guard called `release_transaction(owner)`, which
    /// zeroes every transaction-scope hold of the owner — so a statement that
    /// locked key A gave back key B, which a concurrent statement of the same
    /// owner was still relying on, and a third party could take it while the
    /// holder still believed it was serialised.
    #[test]
    fn autocommit_guard_releases_only_the_keys_this_statement_took() {
        let mgr = manager();
        let owner = SessionId::new();
        let mine = key(22);
        let someone_elses = key(23);

        // A transaction-scope hold of the SAME owner that this statement does
        // not touch (in the real defect: another thread's in-flight statement).
        assert!(mgr
            .try_acquire(someone_elses, owner, AdvisoryScope::Transaction, 0)
            .unwrap());

        {
            let mut autocommit = ctx(owner);
            autocommit.in_explicit_transaction = false;
            let _guard = AdvisoryContextGuard::install_if_absent(|| autocommit);
            evaluate("pg_advisory_xact_lock", &[Value::Int8(mine.objid())]).unwrap();
        }

        let other = SessionId::new();
        assert!(
            mgr.try_acquire(mine, other, AdvisoryScope::Session, 0).unwrap(),
            "the statement's own xact key must be released at statement end"
        );
        assert!(
            !mgr.try_acquire(someone_elses, other, AdvisoryScope::Session, 0)
                .unwrap(),
            "*** the guard released a transaction-scope key this statement never took ***"
        );
        mgr.release_session(other);
        mgr.release_transaction(owner);
        mgr.release_session(owner);
    }

    /// The per-statement key list must not leak into the next statement on the
    /// same thread: a second, unrelated statement must not release the first
    /// one's keys.
    #[test]
    fn xact_key_list_does_not_leak_between_statements() {
        let mgr = manager();
        let owner = SessionId::new();
        let k = key(24);
        {
            // Statement 1 takes the key inside an explicit transaction, so the
            // guard must NOT release it (COMMIT/ROLLBACK does) — but it must
            // still drain the key list.
            let _guard = AdvisoryContextGuard::install_if_absent(|| ctx(owner));
            evaluate("pg_advisory_xact_lock", &[Value::Int8(k.objid())]).unwrap();
        }
        {
            // Statement 2 takes a different key in autocommit.
            let mut autocommit = ctx(owner);
            autocommit.in_explicit_transaction = false;
            let _guard = AdvisoryContextGuard::install_if_absent(|| autocommit);
            evaluate("pg_advisory_xact_lock", &[Value::Int8(key(25).objid())]).unwrap();
        }
        let other = SessionId::new();
        assert!(
            !mgr.try_acquire(k, other, AdvisoryScope::Session, 0).unwrap(),
            "*** statement 2 released statement 1's transaction-scope key ***"
        );
        mgr.release_transaction(owner);
        mgr.release_session(owner);
        mgr.release_session(other);
    }

    /// `release_transaction_keys` decrements once per listed acquisition and
    /// ignores keys held by somebody else.
    #[test]
    fn release_transaction_keys_is_per_acquisition_and_owner_checked() {
        let mgr = manager();
        let a = SessionId::new();
        let b = SessionId::new();
        let k = key(26);
        let theirs = key(27);
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Transaction, 0).unwrap());
        assert!(mgr.try_acquire(k, a, AdvisoryScope::Transaction, 0).unwrap());
        assert!(mgr.try_acquire(theirs, b, AdvisoryScope::Transaction, 0).unwrap());

        // One decrement: still held.
        assert_eq!(mgr.release_transaction_keys(a, &[k]), 1);
        let c = SessionId::new();
        assert!(!mgr.try_acquire(k, c, AdvisoryScope::Session, 0).unwrap());
        // A's release must not touch B's key.
        assert_eq!(mgr.release_transaction_keys(a, &[theirs]), 0);
        assert!(!mgr.try_acquire(theirs, c, AdvisoryScope::Session, 0).unwrap());
        // Second decrement frees it.
        assert_eq!(mgr.release_transaction_keys(a, &[k]), 1);
        assert!(mgr.try_acquire(k, c, AdvisoryScope::Session, 0).unwrap());

        mgr.release_session(b);
        mgr.release_session(c);
    }

    #[test]
    fn guard_releases_transaction_locks_at_statement_end_in_autocommit() {
        let mgr = manager();
        let owner = SessionId::new();
        let k = key(10);
        {
            let mut autocommit = ctx(owner);
            autocommit.in_explicit_transaction = false;
            let _guard = AdvisoryContextGuard::install_if_absent(|| autocommit);
            evaluate("pg_advisory_xact_lock", &[Value::Int8(k.objid())]).unwrap();
        }
        // The statement ended, and so did the implicit transaction.
        let other = SessionId::new();
        assert!(mgr.try_acquire(k, other, AdvisoryScope::Session, 0).unwrap());
        mgr.release_session(other);
    }

    /// Every name in [`FUNCTION_NAMES`] must reach a real [`evaluate`] arm —
    /// a name listed but not dispatched would fall through to
    /// `Unknown scalar function`, which is the exact defect this spec fixes.
    #[test]
    fn every_listed_function_is_dispatched() {
        let owner = SessionId::new();
        let _guard = AdvisoryContextGuard::install_if_absent(|| ctx(owner));
        for name in FUNCTION_NAMES {
            let args = if *name == "pg_advisory_unlock_all" {
                Vec::new()
            } else {
                vec![Value::Int8(key(12).objid())]
            };
            let out = evaluate(name, &args);
            assert!(out.is_ok(), "{name} must be dispatched, got {out:?}");
        }
        manager().release_session(owner);
    }

    #[test]
    fn nested_guard_keeps_the_outer_owner() {
        let outer = SessionId::new();
        let inner = SessionId::new();
        let _g1 = AdvisoryContextGuard::install_if_absent(|| ctx(outer));
        {
            let _g2 = AdvisoryContextGuard::install_if_absent(|| ctx(inner));
            assert_eq!(current_context().map(|c| c.owner), Some(Some(outer)));
        }
        // The inner guard must not have cleared the outer context.
        assert_eq!(current_context().map(|c| c.owner), Some(Some(outer)));
    }
}
