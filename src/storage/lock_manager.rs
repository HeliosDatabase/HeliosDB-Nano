//! Lock Manager for Multi-User ACID Transactions
//!
//! Provides fine-grained locking with deadlock detection for concurrent access control.
//! Implements pessimistic concurrency control with timeout-based conflict resolution.
//!
//! Features:
//! - Read (Shared) and Write (Exclusive) locks
//! - Deadlock detection using wait-for graph and DFS cycle detection
//! - Configurable lock timeout with automatic victim selection
//! - Thread-safe using DashMap for lock-free concurrent access
//! - Automatic cleanup on transaction abort

use crate::config::LockConfig;
use crate::{Error, Result};
use dashmap::DashMap;
use rand::Rng;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{trace, warn};

/// Lock type - determines compatibility with other locks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockType {
    /// Shared lock for reads - multiple holders allowed
    Read,
    /// Exclusive lock for writes - single holder only
    Write,
}

impl LockType {
    /// Check if two lock types are compatible (can be held simultaneously)
    pub fn is_compatible_with(self, other: LockType) -> bool {
        match (self, other) {
            // Read locks are compatible with other read locks
            (LockType::Read, LockType::Read) => true,
            // Write locks are incompatible with all other locks
            _ => false,
        }
    }
}

/// State of a lock on a specific resource
#[derive(Debug, Clone)]
pub struct LockState {
    /// Transactions currently holding this lock
    pub holders: Vec<u64>,
    /// Lock type of current holders (all holders must have compatible types)
    pub lock_type: Option<LockType>,
    /// Transactions waiting to acquire this lock
    pub waiters: Vec<(u64, LockType)>,
    /// How many times each holder has acquired this lock (F1, sprinter
    /// 68b70030ba28). Re-entrancy (see `can_acquire`) lets one
    /// transaction take N guards on the same resource; without a count the
    /// FIRST guard to drop released the holder outright, handing the row to
    /// another transaction while the first still logically held it.
    ///
    /// Invariant: the key set is exactly `holders`, and every value is >= 1.
    /// `holders` stays a set of DISTINCT ids so `get_lock_holders`,
    /// `can_acquire`'s sole-holder test and the wait-for graph keep their
    /// existing meaning.
    pub acquisition_counts: HashMap<u64, u32>,
}

impl LockState {
    /// Create a new empty lock state
    fn new() -> Self {
        Self {
            holders: Vec::new(),
            lock_type: None,
            waiters: Vec::new(),
            acquisition_counts: HashMap::new(),
        }
    }

    /// Check if `transaction_id` can acquire this lock with `requested_type`.
    ///
    /// RE-ENTRANCY: a transaction never conflicts with ITSELF. Locks here are
    /// held for the whole transaction (every guard lives in
    /// `Transaction::acquired_locks` until commit/rollback), so the second
    /// statement of a transaction that writes a row its own earlier statement
    /// already wrote asks for a lock it is *already* holding. Without this
    /// clause that request took the conflict path, inserted the self-edge
    /// `txn -> txn` into the wait-for graph, and `detect_deadlock` — a plain DFS
    /// cycle check — dutifully reported a cycle: a transaction deadlocked
    /// against itself on its own row (`Deadlock: Deadlock detected for
    /// transaction N`, SQLSTATE 40P01) with no second party anywhere. That is
    /// reachable from every ordinary interactive session: `BEGIN; UPDATE t …
    /// WHERE id = 1; UPDATE t … WHERE id = 1;` — the shape Prisma's
    /// `$transaction` emits constantly — and, under `REPEATABLE READ` /
    /// `SERIALIZABLE`, any [`Transaction::get`](super::Transaction::get) of a
    /// key the same transaction later writes (`get` takes a Read lock on the
    /// very key `put` then wants for Write).
    ///
    /// Sole ownership is what makes this safe rather than a hole: when this
    /// transaction is the ONLY holder no other transaction can observe the
    /// difference, and a Read->Write upgrade is uncontended by construction. A
    /// transaction sharing a Read lock with OTHERS still cannot upgrade to
    /// Write — that is a genuine conflict between two parties and keeps
    /// failing closed, exactly as before.
    fn can_acquire(&self, transaction_id: u64, requested_type: LockType) -> bool {
        if self.holders.is_empty() {
            // No holders, lock is free
            return true;
        }

        // Sole holder re-acquiring (including a Read -> Write upgrade).
        if self.holders.len() == 1 && self.holders.first() == Some(&transaction_id) {
            return true;
        }

        // Check compatibility with current lock type
        if let Some(current_type) = self.lock_type {
            requested_type.is_compatible_with(current_type)
        } else {
            true
        }
    }

    /// Add a holder to this lock.
    ///
    /// The recorded `lock_type` only ever STRENGTHENS: with re-entrant
    /// acquisition a transaction holding the exclusive lock can ask for a Read
    /// on the same key (`REPEATABLE READ` re-reading a row it has written), and
    /// stamping `Read` there would advertise the resource as shared — another
    /// transaction's Read would then be granted against uncommitted data. Write
    /// wins over Read, always.
    ///
    /// Every call counts as one acquisition (F1): the Nth re-entrant acquire
    /// by an existing holder does not push a duplicate id, it bumps the count,
    /// so the matching Nth release is the one that actually frees the row.
    fn add_holder(&mut self, transaction_id: u64, lock_type: LockType) {
        if !self.holders.contains(&transaction_id) {
            self.holders.push(transaction_id);
        }
        *self.acquisition_counts.entry(transaction_id).or_insert(0) += 1;
        self.lock_type = match (self.lock_type, lock_type) {
            (Some(LockType::Write), _) => Some(LockType::Write),
            (_, requested) => Some(requested),
        };
    }

    /// Drop a holder ENTIRELY, discarding every re-entrant acquisition it had.
    ///
    /// This is the abort path (deadlock victim / lock timeout /
    /// `cleanup_transaction`): the transaction is being torn down, so all of
    /// its acquisitions on this resource go with it. The ordinary guard-drop
    /// path uses `release_once` instead.
    fn remove_holder(&mut self, transaction_id: u64) {
        self.holders.retain(|&id| id != transaction_id);
        self.acquisition_counts.remove(&transaction_id);
        if self.holders.is_empty() {
            self.lock_type = None;
        }
    }

    /// Release ONE acquisition by `transaction_id` (F1, sprinter 68b70030ba28).
    ///
    /// Returns `true` only when that was the last one and the holder is now
    /// really gone — which is the condition the caller needs before reaping the
    /// whole lock entry. A transaction that acquired the resource N times must
    /// drop N guards before anyone else can have it; dropping guard 1 of N (a
    /// statement-scoped lock, or a savepoint rollback releasing a subset) must
    /// leave the row locked, or the release fails OPEN and another transaction
    /// is granted a row this one is still writing.
    ///
    /// Releasing a transaction that is not a holder is a no-op: a stale guard
    /// (its transaction already aborted and was cleaned up, or the lock was
    /// released explicitly before the guard dropped) must never evict the
    /// CURRENT holder. The `retain` keeps that idempotent.
    fn release_once(&mut self, transaction_id: u64) -> bool {
        match self.acquisition_counts.get_mut(&transaction_id) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => {
                self.remove_holder(transaction_id);
                true
            }
            // Not a counted holder: nothing to release. Still re-assert the
            // invariant (`holders` == keys of `acquisition_counts`) defensively
            // — it is a no-op unless something desynced the two.
            None => {
                self.holders.retain(|&id| id != transaction_id);
                if self.holders.is_empty() {
                    self.lock_type = None;
                }
                false
            }
        }
    }

    /// Register one EXTRA acquisition for an existing holder, without granting
    /// anything new (F1). Used by `LockGuard`'s `Clone`: a cloned guard is a
    /// second live handle to the same lock and will run `Drop` of its own, so
    /// the count has to match the number of guards or the first drop releases
    /// a lock two handles still believe in.
    ///
    /// Fails closed: if `transaction_id` is not already a holder (a clone of a
    /// stale guard whose transaction was cleaned up) nothing is granted and the
    /// clone behaves like the stale guard it came from.
    fn duplicate_acquisition(&mut self, transaction_id: u64) {
        if let Some(count) = self.acquisition_counts.get_mut(&transaction_id) {
            *count += 1;
        }
    }

    /// Add a waiter to this lock
    fn add_waiter(&mut self, transaction_id: u64, lock_type: LockType) {
        if !self.waiters.iter().any(|(id, _)| *id == transaction_id) {
            self.waiters.push((transaction_id, lock_type));
        }
    }

    /// Remove a waiter from this lock
    fn remove_waiter(&mut self, transaction_id: u64) {
        self.waiters.retain(|(id, _)| *id != transaction_id);
    }
}

/// RAII guard for automatic lock release
#[derive(Debug)]
pub struct LockGuard {
    /// Unique identifier for this lock
    pub lock_id: String,
    /// Transaction holding this lock
    pub transaction_id: u64,
    /// Reference to lock manager for release on drop
    lock_manager: Option<Arc<LockManager>>,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(ref mgr) = self.lock_manager {
            // Releases exactly ONE acquisition (F1). A transaction holding N
            // guards on this resource stays the holder until the Nth drop.
            let _ = mgr.release_lock_internal(&self.lock_id, self.transaction_id);
        }
    }
}

/// Hand-written so a clone is COUNTED (F1, sprinter 68b70030ba28).
///
/// `LockGuard` is an RAII handle with a `Drop` that releases one acquisition.
/// The derived `Clone` produced a second handle sharing ONE acquisition, so the
/// first of the two drops released a lock the other clone still stood for —
/// the same fail-open shape the refcount exists to close. Cloning now registers
/// an extra acquisition, so N live guards always mean N pending releases.
impl Clone for LockGuard {
    fn clone(&self) -> Self {
        if let Some(ref mgr) = self.lock_manager {
            if let Some(mut state) = mgr.locks.get_mut(&self.lock_id) {
                state.duplicate_acquisition(self.transaction_id);
            }
        }
        Self {
            lock_id: self.lock_id.clone(),
            transaction_id: self.transaction_id,
            lock_manager: self.lock_manager.clone(),
        }
    }
}

impl LockGuard {
    /// Create a new lock guard
    fn new(lock_id: String, transaction_id: u64, lock_manager: Arc<LockManager>) -> Self {
        Self {
            lock_id,
            transaction_id,
            lock_manager: Some(lock_manager),
        }
    }

    /// Create a dummy lock guard (for tests or internal use)
    pub fn dummy(lock_id: String, transaction_id: u64) -> Self {
        Self {
            lock_id,
            transaction_id,
            lock_manager: None,
        }
    }
}

/// W3.3 autocommit statement-retry policy, derived from `[locks]` config.
///
/// Carries the decision logic for retrying a same-row write conflict
/// (`Error::WriteConflict`, SQLSTATE 40001). A `Copy` value so hot paths read
/// it without touching an `Arc` or a lock. Disabled by default
/// (`max_attempts == 0`), which makes the whole feature behavior-preserving.
///
/// The backoff is a *duration*, not a sleep — the caller decides HOW to wait.
/// The load-bearing constraint (W3.3 design §5.4 sub-case 2a): the wait MUST
/// yield the tokio worker (`tokio::time::sleep(..).await`), never a synchronous
/// `std::thread::sleep`, or it re-pins the worker and reproduces the very
/// worker-starvation livelock the retry exists to break.
#[derive(Debug, Clone, Copy)]
pub struct StatementRetryPolicy {
    /// Maximum retries after the first attempt (0 = feature OFF).
    max_attempts: u32,
    /// Base backoff in milliseconds (first retry's ceiling before jitter).
    backoff_base_ms: u64,
    /// Cap on the per-retry backoff in milliseconds.
    backoff_cap_ms: u64,
}

impl StatementRetryPolicy {
    /// The behavior-preserving default: no retries.
    pub fn disabled() -> Self {
        Self {
            max_attempts: 0,
            backoff_base_ms: 5,
            backoff_cap_ms: 100,
        }
    }

    /// Build from the `[locks]` config section.
    pub fn from_lock_config(cfg: &LockConfig) -> Self {
        Self {
            max_attempts: cfg.statement_retry_max,
            backoff_base_ms: cfg.statement_retry_backoff_ms,
            // Guard against a mis-ordered cap (validation rejects it, but a
            // programmatic Config could still set it): never below the base.
            backoff_cap_ms: cfg.statement_retry_backoff_max_ms.max(cfg.statement_retry_backoff_ms),
        }
    }

    /// True when auto-retry is switched on for autocommit statements.
    pub fn is_enabled(&self) -> bool {
        self.max_attempts > 0
    }

    /// Configured maximum retry count (after the first attempt).
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Full-jitter exponential backoff for the given 1-based retry number.
    ///
    /// Returns a uniform random duration in `[0, min(cap, base * 2^(n-1))]`
    /// (AWS "full jitter"): the exponential term desynchronizes waiters that
    /// lost a race in lockstep so they do not re-collide, and the `0` lower
    /// bound lets one contender go first.
    pub fn backoff_delay(&self, retry_number: u32) -> Duration {
        let base = self.backoff_base_ms.max(1);
        let cap = self.backoff_cap_ms.max(base);
        // base * 2^(retry_number - 1), saturating; shifts >= 63 are clamped so
        // the shift itself cannot overflow.
        let shift = retry_number.saturating_sub(1).min(63);
        let ceiling = base.saturating_mul(1u64 << shift).min(cap);
        // gen_range(0..=ceiling): ceiling >= base >= 1, so the range is valid.
        let jittered = rand::thread_rng().gen_range(0..=ceiling);
        Duration::from_millis(jittered)
    }

    /// Retry decision for an attempt that just failed.
    ///
    /// `retries_done` is how many retries have already been consumed (0 on the
    /// first failure). Returns `Some(backoff)` to retry, or `None` to surface
    /// the error. Keyed ONLY on [`Error::WriteConflict`] — a genuine deadlock
    /// (`Error::deadlock` → 40P01) or any other error is never retried, so a
    /// true deadlock cannot be spun into a livelock. Exhausting `max_attempts`
    /// returns `None`, which is how a long-held explicit holder (design §5.4
    /// sub-case 2b) terminates in a surfaced 40001.
    pub fn retry_after(&self, retries_done: u32, err: &Error) -> Option<Duration> {
        if self.max_attempts == 0 || retries_done >= self.max_attempts {
            return None;
        }
        if !matches!(err, Error::WriteConflict { .. }) {
            return None;
        }
        Some(self.backoff_delay(retries_done + 1))
    }
}

/// Lock Manager - coordinates concurrent access with deadlock detection
///
/// Thread-safe implementation using DashMap for lock-free concurrent operations.
/// Supports automatic deadlock detection and resolution with configurable timeouts.
#[derive(Debug)]
pub struct LockManager {
    /// Map of resource -> lock state (lock-free concurrent access)
    locks: Arc<DashMap<String, LockState>>,
    /// Wait-for graph: transaction -> transactions it's waiting for
    wait_graph: Arc<DashMap<u64, Vec<u64>>>,
    /// Lock acquisition timeout in milliseconds
    timeout_ms: u64,
    /// W3.3 autocommit statement-retry policy (read by the wire handler).
    retry_policy: StatementRetryPolicy,
}

impl LockManager {
    /// Create a new LockManager with specified timeout (retry disabled).
    pub fn new(timeout_ms: u64) -> Self {
        Self::new_with_retry(timeout_ms, StatementRetryPolicy::disabled())
    }

    /// Create a new LockManager with an explicit timeout and retry policy.
    pub fn new_with_retry(timeout_ms: u64, retry_policy: StatementRetryPolicy) -> Self {
        Self {
            locks: Arc::new(DashMap::new()),
            wait_graph: Arc::new(DashMap::new()),
            timeout_ms,
            retry_policy,
        }
    }

    /// Build a `LockManager` from the `[locks]` config section.
    ///
    /// Wires both the spin timeout and the W3.3 statement-retry policy from
    /// config (previously `[locks].timeout_ms` was validated but orphaned —
    /// never reached the manager). Precedence for the timeout:
    /// env `NANO_LOCK_TIMEOUT_MS` > `[locks].timeout_ms` > built-in default.
    /// The env override is retained for backward compatibility with existing
    /// deployments and the contended-writer microbench.
    pub fn from_lock_config(cfg: &LockConfig) -> Self {
        let timeout_ms = Self::resolve_timeout_ms(cfg.timeout_ms as u64, Self::env_timeout_override());
        Self::new_with_retry(timeout_ms, StatementRetryPolicy::from_lock_config(cfg))
    }

    /// The `NANO_LOCK_TIMEOUT_MS` override, if set to a positive integer.
    fn env_timeout_override() -> Option<u64> {
        std::env::var("NANO_LOCK_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
    }

    /// Resolve the effective spin timeout: env override wins over the config
    /// value (which is itself the default when no env var is set). Pure so the
    /// precedence is unit-testable without touching process env.
    fn resolve_timeout_ms(config_ms: u64, env_override: Option<u64>) -> u64 {
        env_override.unwrap_or(config_ms)
    }

    /// The W3.3 autocommit statement-retry policy for this manager.
    pub fn statement_retry_policy(&self) -> StatementRetryPolicy {
        self.retry_policy
    }

    /// Create a `LockManager` with the configured default acquisition timeout.
    ///
    /// Honors `NANO_LOCK_TIMEOUT_MS`; defaults to a short, bounded wait.
    ///
    /// The previous 60-second default was harmful: the lock-acquire wait is a
    /// synchronous spin, and a single same-row write conflict wedged the *whole
    /// server* for up to 60s — new connections could not complete startup and
    /// unrelated statements stalled. Worse, the lock holder's own COMMIT cannot
    /// make progress until the waiter gives up, so for a write-write conflict
    /// *waiting is futile*: the wait can only ever end in a timeout, never in
    /// the lock being granted. A short bound turns that 60s server-wide stall
    /// into a fast, retriable serialization/lock-timeout error.
    ///
    /// The proper fix (NANO_v3.58 HTAP spec, Option 2) is to drop this
    /// redundant pessimistic write lock entirely and rely on the optimistic
    /// first-committer-wins registry (`WriteConflictRegistry::validate_and_record`),
    /// which already reports write-write conflicts at COMMIT with no spin.
    pub fn with_default_timeout() -> Self {
        // Delegates to the config path with the default `[locks]` section:
        // env `NANO_LOCK_TIMEOUT_MS` override, else the 1000 ms default, retry
        // disabled. Retained for API/back-compat; the DB constructors now build
        // via `from_lock_config` so the config value reaches the manager.
        Self::from_lock_config(&LockConfig::default())
    }

    /// Acquire a lock on a resource
    ///
    /// This method will block until the lock can be acquired or timeout occurs.
    /// Returns a LockGuard that must be kept alive while the lock is held.
    ///
    /// # Arguments
    /// * `resource` - Resource identifier (e.g., "table:users:row:42")
    /// * `transaction_id` - ID of transaction acquiring the lock
    /// * `lock_type` - Type of lock (Read or Write)
    ///
    /// # Returns
    /// * `Ok(LockGuard)` - Lock acquired successfully
    /// * `Err(Error::Deadlock)` - Deadlock detected
    /// * `Err(Error::Timeout)` - Lock acquisition timeout
    pub fn acquire_lock(
        self: &Arc<Self>,
        resource: &str,
        transaction_id: u64,
        lock_type: LockType,
    ) -> Result<LockGuard> {
        let start = Instant::now();
        let timeout = Duration::from_millis(self.timeout_ms);

        trace!(
            txn_id = transaction_id,
            resource = %resource,
            lock_type = ?lock_type,
            "Acquiring lock"
        );

        loop {
            // Try to acquire the lock
            match self.try_acquire_lock(resource, transaction_id, lock_type) {
                Ok(guard) => {
                    trace!(
                        txn_id = transaction_id,
                        resource = %resource,
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "Lock acquired"
                    );
                    return Ok(guard);
                }
                Err(e) if e.to_string().contains("Lock conflict") => {
                    // Lock is held, check for deadlock
                    if self.detect_deadlock(transaction_id)? {
                        // Deadlock detected, abort this transaction
                        warn!(
                            txn_id = transaction_id,
                            resource = %resource,
                            "Deadlock detected, aborting transaction"
                        );
                        self.cleanup_transaction(transaction_id);
                        return Err(Error::deadlock(format!(
                            "Deadlock detected for transaction {}",
                            transaction_id
                        )));
                    }

                    // Check timeout
                    if start.elapsed() >= timeout {
                        warn!(
                            txn_id = transaction_id,
                            resource = %resource,
                            timeout_ms = self.timeout_ms,
                            "Lock acquisition timeout"
                        );
                        // Capture the holder before cleanup so the typed
                        // conflict can name who won the row. Waiting was futile
                        // (the futility note on `with_default_timeout`): the
                        // waiter could never have been granted this lock, so a
                        // timeout here is always a write-write conflict.
                        let holder_txn = self.primary_holder(resource);
                        self.cleanup_transaction(transaction_id);
                        let (table, row) = split_row_resource(resource);
                        return Err(Error::write_conflict(
                            table,
                            row,
                            holder_txn,
                            transaction_id,
                            self.timeout_ms,
                        ));
                    }

                    // Wait briefly before retrying
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Try to acquire a lock without blocking
    ///
    /// Returns immediately with success or failure.
    fn try_acquire_lock(
        self: &Arc<Self>,
        resource: &str,
        transaction_id: u64,
        lock_type: LockType,
    ) -> Result<LockGuard> {
        let mut lock_state = self.locks.entry(resource.to_string()).or_insert_with(LockState::new);

        // Check if we can acquire the lock
        if lock_state.can_acquire(transaction_id, lock_type) {
            // Acquire the lock
            lock_state.add_holder(transaction_id, lock_type);

            // Remove from waiters if present
            lock_state.remove_waiter(transaction_id);

            // Remove from wait graph
            self.wait_graph.remove(&transaction_id);

            Ok(LockGuard::new(resource.to_string(), transaction_id, Arc::clone(self)))
        } else {
            // Cannot acquire - add to waiters and update wait graph
            lock_state.add_waiter(transaction_id, lock_type);

            // Update wait-for graph: this transaction waits for all current
            // holders OTHER than itself. A self-edge is never a wait — nothing
            // it holds will be released by anyone but itself — but the DFS in
            // `has_cycle` cannot tell the two apart and reports `txn -> txn` as
            // a deadlock. The only way to reach this arm while already holding
            // the lock is a contended Read -> Write upgrade, and there the real
            // waiting is on the OTHER readers, which is what stays in the graph.
            let holders: Vec<u64> = lock_state
                .holders
                .iter()
                .copied()
                .filter(|holder| *holder != transaction_id)
                .collect();
            self.wait_graph.insert(transaction_id, holders);

            Err(Error::transaction(format!(
                "Lock conflict on resource '{}': transaction {} waiting for {:?}",
                resource, transaction_id, lock_type
            )))
        }
    }

    /// Best-effort current holder of `resource`, for diagnostics on a lock
    /// timeout. Returns 0 if the lock was released between the timeout check
    /// and this read (a benign race — the conflict is already decided).
    fn primary_holder(&self, resource: &str) -> u64 {
        self.locks
            .get(resource)
            .and_then(|state| state.holders.first().copied())
            .unwrap_or(0)
    }

    /// Internal lock release logic — releases ONE acquisition.
    ///
    /// F1 (sprinter 68b70030ba28): with a re-entrant holder this is a
    /// decrement, not a removal. The lock entry is only reaped once the last
    /// acquisition is gone; releasing guard 1 of N leaves the transaction
    /// holding the resource, so a partial release (statement-scoped lock,
    /// savepoint rollback dropping a subset of guards) cannot hand the row to
    /// another transaction while this one still holds it.
    pub fn release_lock_internal(&self, resource: &str, transaction_id: u64) -> Result<()> {
        trace!(
            txn_id = transaction_id,
            resource = %resource,
            "Releasing lock"
        );

        // Decrement this transaction's acquisition count on the lock state
        if let Some(mut lock_state) = self.locks.get_mut(resource) {
            let fully_released = lock_state.release_once(transaction_id);

            // If the holder is really gone and nothing else references the
            // resource, remove the lock entry. Guarded on `fully_released` so a
            // still-held re-entrant lock is never reaped.
            if fully_released && lock_state.holders.is_empty() && lock_state.waiters.is_empty() {
                drop(lock_state);
                self.locks.remove(resource);
            }
        }

        // Remove from wait graph
        self.wait_graph.remove(&transaction_id);

        Ok(())
    }

    /// Release a lock held by a transaction
    ///
    /// Releases ONE acquisition — the same unit `LockGuard::drop` releases. A
    /// transaction that acquired the resource N times still holds it after this
    /// call until the remaining N-1 acquisitions are released (F1).
    ///
    /// # Arguments
    /// * `lock_guard` - Guard returned from acquire_lock
    pub fn release_lock(&self, lock_guard: &LockGuard) -> Result<()> {
        self.release_lock_internal(&lock_guard.lock_id, lock_guard.transaction_id)
    }

    /// Detect if a transaction is involved in a deadlock
    ///
    /// Uses depth-first search to detect cycles in the wait-for graph.
    ///
    /// # Arguments
    /// * `transaction_id` - Transaction to check for deadlock
    ///
    /// # Returns
    /// * `Ok(true)` - Deadlock detected
    /// * `Ok(false)` - No deadlock
    pub fn detect_deadlock(&self, transaction_id: u64) -> Result<bool> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        self.has_cycle(transaction_id, &mut visited, &mut rec_stack)
    }

    /// DFS helper for cycle detection
    fn has_cycle(&self, node: u64, visited: &mut HashSet<u64>, rec_stack: &mut HashSet<u64>) -> Result<bool> {
        // Mark current node as visited and in recursion stack
        visited.insert(node);
        rec_stack.insert(node);

        // Get all nodes this transaction is waiting for
        if let Some(waiting_for) = self.wait_graph.get(&node) {
            for &neighbor in waiting_for.iter() {
                if !visited.contains(&neighbor) {
                    // Recursively check unvisited neighbors
                    if self.has_cycle(neighbor, visited, rec_stack)? {
                        return Ok(true);
                    }
                } else if rec_stack.contains(&neighbor) {
                    // Found a cycle
                    return Ok(true);
                }
            }
        }

        // Remove from recursion stack before returning
        rec_stack.remove(&node);
        Ok(false)
    }

    /// Resolve a deadlock by aborting the victim transaction
    ///
    /// # Arguments
    /// * `victim_id` - Transaction ID to abort
    pub fn resolve_deadlock(&self, victim_id: u64) -> Result<()> {
        self.cleanup_transaction(victim_id);
        Ok(())
    }

    /// Get all transactions currently holding locks on a resource
    ///
    /// # Arguments
    /// * `resource` - Resource identifier
    ///
    /// # Returns
    /// Vector of transaction IDs holding locks
    pub fn get_lock_holders(&self, resource: &str) -> Vec<u64> {
        self.locks
            .get(resource)
            .map(|state| state.holders.clone())
            .unwrap_or_default()
    }

    /// Check if a resource is currently locked
    ///
    /// # Arguments
    /// * `resource` - Resource identifier
    ///
    /// # Returns
    /// * `true` - Resource has active locks
    /// * `false` - Resource is unlocked
    pub fn is_locked(&self, resource: &str) -> bool {
        self.locks
            .get(resource)
            .map(|state| !state.holders.is_empty())
            .unwrap_or(false)
    }

    /// Timeout a transaction's lock acquisition attempt
    ///
    /// Removes the transaction from all wait queues and the wait-for graph.
    ///
    /// # Arguments
    /// * `transaction_id` - Transaction that timed out
    pub fn timeout_lock(&self, transaction_id: u64) -> Result<()> {
        self.cleanup_transaction(transaction_id);
        Ok(())
    }

    /// Clean up all state for a transaction
    ///
    /// Removes transaction from all locks (holders and waiters) and wait graph.
    /// F1: this is the abort path, so it uses `remove_holder` — every
    /// re-entrant acquisition the transaction had on every resource goes at
    /// once. Only the ordinary guard-drop path decrements one at a time.
    fn cleanup_transaction(&self, transaction_id: u64) {
        // Remove from wait graph
        self.wait_graph.remove(&transaction_id);

        // Remove from all lock states
        let keys: Vec<String> = self.locks.iter().map(|entry| entry.key().clone()).collect();

        for key in keys {
            if let Some(mut lock_state) = self.locks.get_mut(&key) {
                lock_state.remove_holder(transaction_id);
                lock_state.remove_waiter(transaction_id);

                // Clean up empty lock entries
                if lock_state.holders.is_empty() && lock_state.waiters.is_empty() {
                    drop(lock_state);
                    self.locks.remove(&key);
                }
            }
        }
    }

    /// Get statistics about current lock state
    ///
    /// Returns (total_locks, total_holders, total_waiters)
    pub fn get_statistics(&self) -> (usize, usize, usize) {
        let total_locks = self.locks.len();
        let mut total_holders = 0;
        let mut total_waiters = 0;

        for entry in self.locks.iter() {
            total_holders += entry.holders.len();
            total_waiters += entry.waiters.len();
        }

        (total_locks, total_holders, total_waiters)
    }

    /// Get all active transactions in the wait-for graph
    pub fn get_active_transactions(&self) -> Vec<u64> {
        self.wait_graph.iter().map(|entry| *entry.key()).collect()
    }

    /// Find deadlock cycles using BFS
    ///
    /// Returns all transactions involved in deadlock cycles.
    pub fn find_deadlock_cycles(&self) -> Vec<Vec<u64>> {
        let mut cycles = Vec::new();
        let mut visited = HashSet::new();

        for entry in self.wait_graph.iter() {
            let start_node = *entry.key();
            if visited.contains(&start_node) {
                continue;
            }

            // Try to find a cycle starting from this node
            if let Some(cycle) = self.find_cycle_from(start_node, &mut visited) {
                cycles.push(cycle);
            }
        }

        cycles
    }

    /// Find a cycle starting from a specific node
    fn find_cycle_from(&self, start: u64, visited: &mut HashSet<u64>) -> Option<Vec<u64>> {
        let mut queue: VecDeque<(u64, Vec<u64>)> = VecDeque::new();

        queue.push_back((start, vec![start]));

        while let Some((node, current_path)) = queue.pop_front() {
            if visited.contains(&node) && node != start {
                continue;
            }

            visited.insert(node);

            if let Some(waiting_for) = self.wait_graph.get(&node) {
                for &neighbor in waiting_for.iter() {
                    if neighbor == start && current_path.len() > 1 {
                        // Found a cycle back to start
                        return Some(current_path.clone());
                    }

                    if !current_path.contains(&neighbor) {
                        let mut new_path = current_path.clone();
                        new_path.push(neighbor);
                        queue.push_back((neighbor, new_path));
                    }
                }
            }
        }

        None
    }
}

impl Clone for LockManager {
    fn clone(&self) -> Self {
        Self {
            locks: Arc::clone(&self.locks),
            wait_graph: Arc::clone(&self.wait_graph),
            timeout_ms: self.timeout_ms,
            retry_policy: self.retry_policy,
        }
    }
}

/// Split a storage lock resource key (`data:{table}:{row_id}`) into its table
/// and row-identity parts for the typed [`Error::WriteConflict`]. Resources
/// that do not follow the storage-key convention (non-row locks, test strings)
/// yield `("", resource)`.
fn split_row_resource(resource: &str) -> (&str, &str) {
    if let Some(body) = resource.strip_prefix("data:") {
        if let Some((table, row)) = body.split_once(':') {
            return (table, row);
        }
    }
    ("", resource)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_lock_acquire_release() {
        let manager = Arc::new(LockManager::new(5000));

        // Acquire read lock
        let guard = manager
            .acquire_lock("resource1", 1, LockType::Read)
            .expect("Failed to acquire read lock");

        assert!(manager.is_locked("resource1"));
        assert_eq!(manager.get_lock_holders("resource1"), vec![1]);

        // Release lock
        manager.release_lock(&guard).expect("Failed to release lock");
        assert!(!manager.is_locked("resource1"));
    }

    #[test]
    fn test_multiple_read_locks() {
        let manager = Arc::new(LockManager::new(5000));

        // Multiple transactions can hold read locks simultaneously
        let guard1 = manager
            .acquire_lock("resource1", 1, LockType::Read)
            .expect("Failed to acquire read lock for tx 1");
        let guard2 = manager
            .acquire_lock("resource1", 2, LockType::Read)
            .expect("Failed to acquire read lock for tx 2");

        let holders = manager.get_lock_holders("resource1");
        assert_eq!(holders.len(), 2);
        assert!(holders.contains(&1));
        assert!(holders.contains(&2));

        manager.release_lock(&guard1).expect("Failed to release lock 1");
        manager.release_lock(&guard2).expect("Failed to release lock 2");
    }

    #[test]
    fn test_write_lock_exclusive() {
        let manager = Arc::new(LockManager::new(1000));

        // First transaction acquires write lock
        let guard1 = manager
            .acquire_lock("resource1", 1, LockType::Write)
            .expect("Failed to acquire write lock");

        // Second transaction tries to acquire read lock (should timeout)
        let manager_clone = Arc::clone(&manager);
        let handle = thread::spawn(move || manager_clone.acquire_lock("resource1", 2, LockType::Read));

        // Wait a bit to ensure second thread starts
        thread::sleep(Duration::from_millis(100));

        // Release first lock
        drop(guard1); // Should trigger auto-release

        // Second thread should now succeed
        let result = handle.join().expect("Thread panicked");
        assert!(result.is_ok());
    }

    /// True for the error the detector raises on a cycle. `Error::deadlock` is
    /// not its own variant — it formats into `Error::Transaction` with a
    /// `Deadlock:` prefix — and the *conflict* error `try_acquire_lock` returns
    /// is also an `Error::Transaction`, so the prefix is what separates "you
    /// are the deadlock victim" from "the lock is simply busy".
    fn is_deadlock_error<T>(result: &Result<T>) -> bool {
        matches!(result, Err(Error::Transaction(msg)) if msg.starts_with("Deadlock:"))
    }

    #[test]
    fn test_deadlock_detection_simple() {
        // F2 (sprinter fab4dd25bf29). The previous shape of this test was a
        // PROVEN flake (GH Actions run 34426473254 failed only this test on
        // d1c48b3; the same-commit rerun passed): it spawned a thread to make
        // tx1 block on B, slept 50 ms hoping the wait had registered, then
        // asserted that tx2 — specifically tx2 — was the victim. Two things
        // there are not properties of the lock manager: (a) 50 ms of wall clock
        // is not a happens-before edge, so on a slow runner tx2 could ask for A
        // before tx1's wait edge existed and simply be granted it; (b) with
        // both parties spinning in `acquire_lock`, EITHER can run the detector
        // first, and the detector aborts whoever asked — so tx1 could be the
        // victim, its cleanup frees A, and tx2's acquire then SUCCEEDS.
        //
        // The fix removes the race instead of widening the sleep: tx1's wait is
        // registered SYNCHRONOUSLY on this thread. `try_acquire_lock` is
        // literally the first (failing) iteration of the blocking
        // `acquire_lock` loop — it enqueues tx1 as a waiter on B and writes the
        // wait-for edge 1 -> 2 — so after it returns, "tx1 is registered as
        // waiting" is a fact, not a hope. It also needs ZERO production-code
        // surface (no test-only wait-count accessor). With only one party
        // running the detector there is no second detector to race, and no
        // sleep anywhere, so the outcome is deterministic on any runner.
        let manager = Arc::new(LockManager::new(5000));

        // Transaction 1 holds lock on resource A
        let guard1 = manager
            .acquire_lock("resourceA", 1, LockType::Write)
            .expect("Failed to acquire lock A for tx 1");

        // Transaction 2 holds lock on resource B
        let guard2 = manager
            .acquire_lock("resourceB", 2, LockType::Write)
            .expect("Failed to acquire lock B for tx 2");

        // Transaction 1 asks for B and is enqueued behind tx2: edge 1 -> 2.
        let tx1_result = manager.try_acquire_lock("resourceB", 1, LockType::Write);
        assert!(tx1_result.is_err(), "tx1 must not be granted B while tx2 holds it");
        assert_eq!(
            manager.wait_graph.get(&1).map(|e| e.value().clone()),
            Some(vec![2]),
            "tx1's wait for tx2 must be registered before tx2 closes the cycle"
        );

        // Transaction 2 asks for A, closing the cycle 1 -> 2 -> 1.
        let tx2_result = manager.acquire_lock("resourceA", 2, LockType::Write);

        // The property: a cycle aborts EXACTLY ONE of the two parties with a
        // deadlock error. Which one is the manager's choice, not this test's.
        let victims = usize::from(is_deadlock_error(&tx1_result)) + usize::from(is_deadlock_error(&tx2_result));
        assert_eq!(
            victims, 1,
            "exactly one party must be aborted as the deadlock victim \
             (tx1: {tx1_result:?}, tx2: {tx2_result:?})"
        );

        // …and the cycle must actually be BROKEN, not merely reported: the
        // victim's locks are gone, so the survivor now makes progress. (The old
        // test never checked this.)
        let survivor = manager.acquire_lock("resourceB", 1, LockType::Write);
        assert!(
            survivor.is_ok(),
            "aborting the victim must release its locks so the survivor proceeds: {survivor:?}"
        );

        // Clean up
        drop(survivor);
        drop(guard1);
        drop(guard2);
    }

    #[test]
    fn write_lock_timeout_yields_typed_write_conflict() {
        // A short timeout bounds the futile spin: while the holder keeps the
        // lock the waiter can never be granted it, so it always times out.
        let manager = Arc::new(LockManager::new(200));
        let _held = manager
            .acquire_lock("data:accounts:42", 1, LockType::Write)
            .expect("holder acquires write lock");

        let err = manager
            .acquire_lock("data:accounts:42", 2, LockType::Write)
            .expect_err("second writer must time out");

        match err {
            Error::WriteConflict {
                table,
                row,
                holder_txn,
                waiter_txn,
                waited_ms,
            } => {
                assert_eq!(table, "accounts");
                assert_eq!(row, "42");
                assert_eq!(holder_txn, 1);
                assert_eq!(waiter_txn, 2);
                assert_eq!(waited_ms, 200);
            }
            other => panic!("expected WriteConflict, got {other:?}"),
        }
    }

    #[test]
    fn split_row_resource_parses_only_data_keys() {
        assert_eq!(split_row_resource("data:users:7"), ("users", "7"));
        assert_eq!(split_row_resource("data:orders:a:b"), ("orders", "a:b"));
        // Non-storage resources (tests, non-row locks) keep the whole string.
        assert_eq!(split_row_resource("resource1"), ("", "resource1"));
        assert_eq!(split_row_resource("data:"), ("", "data:"));
    }

    // ---- W3.3 statement-retry policy (autocommit same-row write conflict) ----

    fn a_write_conflict() -> Error {
        Error::write_conflict("accounts", "42", 1, 2, 1000)
    }

    /// Mirror of the wire handler's retry loop, minus the async backoff, so the
    /// retry *contract* is deterministically testable: it drives `op` and uses
    /// `retry_after` for the exact same decision the handler makes, returning
    /// the final outcome and the number of retries consumed.
    fn drive_retry<T>(policy: &StatementRetryPolicy, mut op: impl FnMut() -> Result<T>) -> (Result<T>, u32) {
        let mut retries = 0u32;
        loop {
            let outcome = op();
            if let Err(ref e) = outcome {
                if policy.retry_after(retries, e).is_some() {
                    retries += 1;
                    continue;
                }
            }
            return (outcome, retries);
        }
    }

    #[test]
    fn statement_retry_disabled_by_default_never_retries() {
        // max_attempts == 0 (the shipped default): the backoff/retry path must
        // never execute even for a write conflict — behavior-preserving.
        let policy = StatementRetryPolicy::disabled();
        assert!(!policy.is_enabled());
        assert!(policy.retry_after(0, &a_write_conflict()).is_none());

        let calls = std::cell::Cell::new(0u32);
        let (res, retries) = drive_retry(&policy, || -> Result<u64> {
            calls.set(calls.get() + 1);
            Err(a_write_conflict())
        });
        assert!(matches!(res, Err(Error::WriteConflict { .. })));
        assert_eq!(retries, 0, "disabled policy must not retry");
        assert_eq!(calls.get(), 1, "op runs exactly once when retry is OFF");
    }

    #[test]
    fn statement_retry_only_write_conflict_not_deadlock() {
        let policy = StatementRetryPolicy::from_lock_config(&LockConfig {
            statement_retry_max: 3,
            ..LockConfig::default()
        });
        assert!(policy.is_enabled());
        // A real write conflict is retriable...
        assert!(policy.retry_after(0, &a_write_conflict()).is_some());
        // ...but a genuine deadlock (40P01) is a DIFFERENT variant and must NOT
        // be retried into a livelock — the detector already chose a victim.
        assert!(policy.retry_after(0, &Error::deadlock("cycle")).is_none());
        // Any other error surfaces immediately too.
        assert!(policy.retry_after(0, &Error::transaction("boom")).is_none());
    }

    #[test]
    fn statement_retry_exhaustion_surfaces_write_conflict() {
        // Sub-case 2b: a long-held explicit holder never releases within the
        // window, so every attempt conflicts. The waiter exhausts max_attempts
        // (bounded) and surfaces 40001 — the correct terminating outcome.
        let policy = StatementRetryPolicy::from_lock_config(&LockConfig {
            statement_retry_max: 3,
            ..LockConfig::default()
        });
        let (res, retries) = drive_retry(&policy, || -> Result<u64> { Err(a_write_conflict()) });
        assert!(matches!(res, Err(Error::WriteConflict { .. })));
        assert_eq!(retries, 3, "retries are hard-bounded by statement_retry_max");
    }

    #[test]
    fn statement_retry_succeeds_after_transient_conflict() {
        let policy = StatementRetryPolicy::from_lock_config(&LockConfig {
            statement_retry_max: 5,
            ..LockConfig::default()
        });
        let attempt = std::cell::Cell::new(0u32);
        let (res, retries) = drive_retry(&policy, || -> Result<u64> {
            let n = attempt.get();
            attempt.set(n + 1);
            if n < 2 {
                Err(a_write_conflict())
            } else {
                Ok(7)
            }
        });
        assert_eq!(res.unwrap(), 7);
        assert_eq!(retries, 2, "two transient conflicts, then success");
    }

    #[test]
    fn statement_retry_skips_sequence_values_but_never_double_applies() {
        // Design §5.5 (PG parity): a retried INSERT re-draws durable sequence
        // values (they are NOT rolled back), so it may SKIP values — but each
        // failed attempt's write-set is rolled back, so rows are applied
        // exactly once, never doubled.
        let policy = StatementRetryPolicy::from_lock_config(&LockConfig {
            statement_retry_max: 5,
            ..LockConfig::default()
        });
        let seq_draws = std::cell::Cell::new(0u64);
        let rows_applied = std::cell::Cell::new(0u64);
        let attempt = std::cell::Cell::new(0u32);
        let (res, retries) = drive_retry(&policy, || -> Result<u64> {
            // Every attempt draws a durable sequence value up-front.
            seq_draws.set(seq_draws.get() + 1);
            let n = attempt.get();
            attempt.set(n + 1);
            if n < 2 {
                // Rolled-back attempt: its would-be row write never commits.
                Err(a_write_conflict())
            } else {
                // Committing attempt: the row is applied here, once.
                rows_applied.set(rows_applied.get() + 1);
                Ok(1)
            }
        });
        assert!(res.is_ok());
        assert_eq!(retries, 2);
        assert_eq!(rows_applied.get(), 1, "row applied exactly once — never double-applied");
        assert_eq!(
            seq_draws.get(),
            3,
            "sequence advanced on every attempt — the retried INSERT skips values"
        );
    }

    // -----------------------------------------------------------------------
    // Re-entrancy: a transaction never conflicts with itself
    //
    // Row locks live for the whole transaction, so a transaction's second write
    // of a row it already wrote re-requests a lock it is still holding. Before
    // this, that request took the conflict path and the wait-for graph gained
    // the self-edge `txn -> txn`, which the DFS reported as a cycle: a
    // transaction deadlocked against itself. `BEGIN; UPDATE t … WHERE id = 1;
    // UPDATE t … WHERE id = 1;` — and every Prisma `$transaction` that touches
    // a row twice — hit it.
    // -----------------------------------------------------------------------

    #[test]
    fn same_transaction_may_reacquire_its_own_write_lock() {
        let manager = Arc::new(LockManager::new(1000));

        let first = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("the first write lock must be granted");
        // Re-acquired by the SAME transaction: granted immediately, no deadlock
        // and no spin (the manager's timeout is 1 s, so a conflict would show).
        let second = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("*** a transaction deadlocked against its own row lock ***");

        assert_eq!(manager.get_lock_holders("data:t:1"), vec![7]);
        assert!(
            !manager.detect_deadlock(7).expect("deadlock check"),
            "re-locking a row you already hold is not a deadlock"
        );

        drop(second);
        drop(first);
        assert!(!manager.is_locked("data:t:1"));
    }

    #[test]
    fn sole_reader_may_upgrade_its_own_lock_to_write() {
        let manager = Arc::new(LockManager::new(1000));

        // REPEATABLE READ takes a Read lock on a row, then writes the same row.
        let read = manager
            .acquire_lock("data:t:1", 7, LockType::Read)
            .expect("the sole reader's read lock must be granted");
        let write = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("*** the sole reader could not upgrade its own lock ***");

        // The resource is now EXCLUSIVE, and a re-entrant Read must not
        // advertise it as shared again — another transaction's Read would then
        // be granted against this transaction's uncommitted write.
        let read_again = manager
            .acquire_lock("data:t:1", 7, LockType::Read)
            .expect("a re-entrant read must be granted");
        assert!(
            manager.try_acquire_lock("data:t:1", 8, LockType::Read).is_err(),
            "a re-entrant Read must not downgrade a held Write lock to shared"
        );

        drop(read_again);
        drop(write);
        drop(read);
    }

    #[test]
    fn another_transaction_is_still_refused_while_the_holder_re_locks() {
        let manager = Arc::new(LockManager::new(1000));

        let held = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("the write lock must be granted");
        let held_again = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("the re-entrant write lock must be granted");

        // Fail closed: re-entrancy is scoped to the holder itself. A DIFFERENT
        // transaction still cannot take the row, in either mode.
        assert!(
            manager.try_acquire_lock("data:t:1", 8, LockType::Write).is_err(),
            "another transaction must still be refused the row"
        );
        assert!(
            manager.try_acquire_lock("data:t:1", 8, LockType::Read).is_err(),
            "another transaction must still be refused a read of the row"
        );

        drop(held_again);
        drop(held);
    }

    #[test]
    fn contended_read_upgrade_waits_on_the_other_reader_only() {
        let manager = Arc::new(LockManager::new(1000));

        let mine = manager
            .acquire_lock("data:t:1", 7, LockType::Read)
            .expect("this transaction's read lock must be granted");
        let theirs = manager
            .acquire_lock("data:t:1", 8, LockType::Read)
            .expect("the other transaction's read lock must be granted");

        // Sharing the read with someone else, an upgrade is a REAL conflict
        // between two parties and must not be granted.
        assert!(
            manager.try_acquire_lock("data:t:1", 7, LockType::Write).is_err(),
            "an upgrade contended by another reader must fail closed"
        );
        // …and the wait-for graph must record the OTHER reader, never a
        // self-edge — a self-edge is what the DFS misreports as a deadlock.
        let waits_for = manager.wait_graph.get(&7).map(|e| e.value().clone());
        assert_eq!(waits_for, Some(vec![8]));
        assert!(
            !manager.detect_deadlock(7).expect("deadlock check"),
            "waiting for one other reader is not a cycle"
        );

        drop(theirs);
        drop(mine);
    }

    /// The recorded acquisition count for `(resource, txn)`, or `None` when
    /// the transaction is not a holder. Reads the refcount F1 added directly,
    /// so the tests below assert on the mechanism, not on a proxy for it.
    fn acquisition_count(manager: &LockManager, resource: &str, txn: u64) -> Option<u32> {
        manager
            .locks
            .get(resource)
            .and_then(|state| state.acquisition_counts.get(&txn).copied())
    }

    // -----------------------------------------------------------------------
    // F1 (sprinter 68b70030ba28): re-entrancy needs an acquisition REFCOUNT.
    //
    // Re-entrancy (above) lets one transaction hold N guards for one resource.
    // Before the refcount, `LockGuard::drop` released the holder on the FIRST
    // drop, so guard 1 of N freed a row the transaction was still writing.
    // Nothing observable broke *today* only because every guard is parked in
    // `Transaction::acquired_locks` until commit/rollback and they all drop
    // together — a latent fail-open that any early-drop path (statement-scoped
    // locks, a savepoint rollback releasing a subset) would have turned live.
    // These tests therefore drive the lock-manager API DIRECTLY rather than a
    // SQL path, which is the only way to construct the early drop.
    // -----------------------------------------------------------------------

    #[test]
    fn dropping_one_of_two_re_entrant_guards_keeps_the_lock_held() {
        let manager = Arc::new(LockManager::new(200));

        let first = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("the first write lock must be granted");
        let second = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("the re-entrant write lock must be granted");
        assert_eq!(
            acquisition_count(&manager, "data:t:1", 7),
            Some(2),
            "two acquires must be counted as two"
        );

        // The early drop: one guard goes, the transaction still logically holds
        // the row. Pre-fix this single `remove_holder` emptied `holders`, the
        // entry was reaped, and the three assertions below all failed.
        drop(second);

        assert!(
            manager.is_locked("data:t:1"),
            "*** the row was released while its transaction still held a guard ***"
        );
        assert_eq!(manager.get_lock_holders("data:t:1"), vec![7]);
        assert!(
            manager.try_acquire_lock("data:t:1", 8, LockType::Write).is_err(),
            "*** another transaction was granted a row txn 7 still holds ***"
        );

        // The LAST release is the one that frees it.
        drop(first);
        assert!(!manager.is_locked("data:t:1"));
        let other = manager
            .acquire_lock("data:t:1", 8, LockType::Write)
            .expect("once every acquisition is released the row is free");
        drop(other);
    }

    #[test]
    fn n_acquires_need_n_drops_before_the_lock_is_free() {
        let manager = Arc::new(LockManager::new(200));

        // Savepoint-style: a nested scope takes extra locks on the same row and
        // its rollback drops just that subset. The outer statement's lock must
        // survive it.
        let mut guards: Vec<LockGuard> = (0..4)
            .map(|_| {
                manager
                    .acquire_lock("data:t:1", 7, LockType::Write)
                    .expect("every re-entrant acquire must be granted")
            })
            .collect();
        assert_eq!(acquisition_count(&manager, "data:t:1", 7), Some(4));

        // Partial release (the savepoint subset): 4 -> 1 acquisitions.
        guards.truncate(1);
        assert!(manager.is_locked("data:t:1"), "the outer lock outlives the subset");
        assert_eq!(manager.get_lock_holders("data:t:1"), vec![7]);
        assert!(
            manager.try_acquire_lock("data:t:1", 8, LockType::Read).is_err(),
            "a partially released row must still fail closed for everyone else"
        );

        // The final drop releases it.
        guards.clear();
        assert!(!manager.is_locked("data:t:1"));
    }

    #[test]
    fn mixed_read_write_re_entrancy_is_counted_once_per_acquire() {
        let manager = Arc::new(LockManager::new(200));

        // REPEATABLE READ shape: read the row, write it, read it again.
        let read = manager.acquire_lock("data:t:1", 7, LockType::Read).expect("read");
        let write = manager.acquire_lock("data:t:1", 7, LockType::Write).expect("upgrade");
        let read_again = manager.acquire_lock("data:t:1", 7, LockType::Read).expect("re-read");

        drop(read_again);
        drop(write);
        // One acquisition (the original Read) is still outstanding, and the
        // resource stays EXCLUSIVE — `lock_type` only ever strengthens, so a
        // dropped upgrade must not re-advertise the row as shared.
        assert!(manager.is_locked("data:t:1"));
        assert!(
            manager.try_acquire_lock("data:t:1", 8, LockType::Read).is_err(),
            "another transaction must not be let in while txn 7 holds an acquisition"
        );

        drop(read);
        assert!(!manager.is_locked("data:t:1"));
    }

    #[test]
    fn aborting_a_transaction_discards_all_its_acquisitions() {
        let manager = Arc::new(LockManager::new(200));

        let guards: Vec<LockGuard> = (0..3)
            .map(|_| manager.acquire_lock("data:t:1", 7, LockType::Write).expect("acquire"))
            .collect();

        // The abort path (deadlock victim / lock timeout) must NOT need three
        // decrements: `cleanup_transaction` drops the holder outright.
        manager.resolve_deadlock(7).expect("victim cleanup");
        assert!(!manager.is_locked("data:t:1"), "an aborted transaction holds nothing");

        let other = manager
            .acquire_lock("data:t:1", 8, LockType::Write)
            .expect("the row is free for the survivor");

        // The victim's stale guards now drop. Each is a no-op: they must never
        // evict txn 8, which acquired the row after the abort.
        drop(guards);
        assert_eq!(
            manager.get_lock_holders("data:t:1"),
            vec![8],
            "stale guards from an aborted transaction must not release the new holder"
        );
        drop(other);
        assert!(!manager.is_locked("data:t:1"));
    }

    #[test]
    fn cloning_a_guard_registers_another_acquisition() {
        let manager = Arc::new(LockManager::new(200));

        let guard = manager
            .acquire_lock("data:t:1", 7, LockType::Write)
            .expect("the write lock must be granted");
        // A clone is a second live RAII handle with its own `Drop`; with the
        // derived `Clone` the first of the two drops released the lock while
        // the other clone was still alive — the same fail-open as the missing
        // refcount, arriving by a different door.
        let copy = guard.clone();
        assert_eq!(acquisition_count(&manager, "data:t:1", 7), Some(2));

        drop(copy);
        assert!(manager.is_locked("data:t:1"), "the surviving clone still holds the row");
        drop(guard);
        assert!(!manager.is_locked("data:t:1"));
    }

    #[test]
    fn statement_retry_backoff_stays_within_cap() {
        let policy = StatementRetryPolicy::from_lock_config(&LockConfig {
            statement_retry_max: 10,
            statement_retry_backoff_ms: 5,
            statement_retry_backoff_max_ms: 100,
            ..LockConfig::default()
        });
        // Full jitter: every draw is in [0, cap]; sample many to exercise the
        // random path across growing exponential ceilings.
        for retry_number in 1..=20 {
            for _ in 0..64 {
                let d = policy.backoff_delay(retry_number).as_millis() as u64;
                assert!(d <= 100, "retry {retry_number} backoff {d}ms exceeded cap 100ms");
            }
        }
    }

    #[test]
    fn from_lock_config_wires_timeout_and_retry() {
        // Timeout precedence (pure, env-independent): env override wins, else
        // the config value reaches the manager (previously orphaned).
        assert_eq!(LockManager::resolve_timeout_ms(750, None), 750);
        assert_eq!(LockManager::resolve_timeout_ms(750, Some(50)), 50);
        assert_eq!(LockManager::resolve_timeout_ms(30000, None), 30000);

        // The retry policy is wired from config and is env-independent.
        let cfg = LockConfig {
            statement_retry_max: 4,
            statement_retry_backoff_ms: 10,
            statement_retry_backoff_max_ms: 250,
            ..LockConfig::default()
        };
        let mgr = LockManager::from_lock_config(&cfg);
        let policy = mgr.statement_retry_policy();
        assert!(policy.is_enabled());
        assert_eq!(policy.max_attempts(), 4);
    }
}
