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

use crate::{Error, Result};
use dashmap::DashMap;
use std::collections::{HashSet, VecDeque};
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
}

impl LockState {
    /// Create a new empty lock state
    fn new() -> Self {
        Self {
            holders: Vec::new(),
            lock_type: None,
            waiters: Vec::new(),
        }
    }

    /// Check if a transaction can acquire this lock
    fn can_acquire(&self, requested_type: LockType) -> bool {
        if self.holders.is_empty() {
            // No holders, lock is free
            return true;
        }

        // Check compatibility with current lock type
        if let Some(current_type) = self.lock_type {
            requested_type.is_compatible_with(current_type)
        } else {
            true
        }
    }

    /// Add a holder to this lock
    fn add_holder(&mut self, transaction_id: u64, lock_type: LockType) {
        if !self.holders.contains(&transaction_id) {
            self.holders.push(transaction_id);
        }
        self.lock_type = Some(lock_type);
    }

    /// Remove a holder from this lock
    fn remove_holder(&mut self, transaction_id: u64) {
        self.holders.retain(|&id| id != transaction_id);
        if self.holders.is_empty() {
            self.lock_type = None;
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
#[derive(Debug, Clone)]
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
            let _ = mgr.release_lock_internal(&self.lock_id, self.transaction_id);
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
}

impl LockManager {
    /// Create a new LockManager with specified timeout
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            locks: Arc::new(DashMap::new()),
            wait_graph: Arc::new(DashMap::new()),
            timeout_ms,
        }
    }

    /// Create a new LockManager with default timeout (60 seconds)
    pub fn with_default_timeout() -> Self {
        Self::new(60_000)
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
                        self.cleanup_transaction(transaction_id);
                        return Err(Error::transaction(format!(
                            "Lock acquisition timeout after {}ms for transaction {}",
                            self.timeout_ms, transaction_id
                        )));
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
        if lock_state.can_acquire(lock_type) {
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

            // Update wait-for graph: this transaction waits for all current holders
            let holders = lock_state.holders.clone();
            self.wait_graph.insert(transaction_id, holders);

            Err(Error::transaction(format!(
                "Lock conflict on resource '{}': transaction {} waiting for {:?}",
                resource, transaction_id, lock_type
            )))
        }
    }

    /// Internal lock release logic
    pub fn release_lock_internal(&self, resource: &str, transaction_id: u64) -> Result<()> {
        trace!(
            txn_id = transaction_id,
            resource = %resource,
            "Releasing lock"
        );

        // Remove holder from lock state
        if let Some(mut lock_state) = self.locks.get_mut(resource) {
            lock_state.remove_holder(transaction_id);

            // If no more holders and no waiters, remove the lock entry
            if lock_state.holders.is_empty() && lock_state.waiters.is_empty() {
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
    fn has_cycle(
        &self,
        node: u64,
        visited: &mut HashSet<u64>,
        rec_stack: &mut HashSet<u64>,
    ) -> Result<bool> {
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
        }
    }
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
        let guard = manager.acquire_lock("resource1", 1, LockType::Read)
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
        let guard1 = manager.acquire_lock("resource1", 1, LockType::Read)
            .expect("Failed to acquire read lock for tx 1");
        let guard2 = manager.acquire_lock("resource1", 2, LockType::Read)
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
        let guard1 = manager.acquire_lock("resource1", 1, LockType::Write)
            .expect("Failed to acquire write lock");

        // Second transaction tries to acquire read lock (should timeout)
        let manager_clone = Arc::clone(&manager);
        let handle = thread::spawn(move || {
            manager_clone.acquire_lock("resource1", 2, LockType::Read)
        });

        // Wait a bit to ensure second thread starts
        thread::sleep(Duration::from_millis(100));

        // Release first lock
        drop(guard1); // Should trigger auto-release

        // Second thread should now succeed
        let result = handle.join().expect("Thread panicked");
        assert!(result.is_ok());
    }

    #[test]
    fn test_deadlock_detection_simple() {
        let manager = Arc::new(LockManager::new(5000));

        // Transaction 1 holds lock on resource A
        let guard1 = manager.acquire_lock("resourceA", 1, LockType::Write)
            .expect("Failed to acquire lock A for tx 1");

        // Transaction 2 holds lock on resource B
        let guard2 = manager.acquire_lock("resourceB", 2, LockType::Write)
            .expect("Failed to acquire lock B for tx 2");

        // Transaction 1 tries to acquire lock on resource B (will wait)
        let manager1 = Arc::clone(&manager);
        let handle1 = thread::spawn(move || {
            manager1.acquire_lock("resourceB", 1, LockType::Write)
        });

        // Give tx1 time to start waiting
        thread::sleep(Duration::from_millis(50));

        // Transaction 2 tries to acquire lock on resource A (deadlock!)
        let result = manager.acquire_lock("resourceA", 2, LockType::Write);

        // Should detect deadlock
        assert!(result.is_err());
        
        // Clean up
        drop(guard1);
        drop(guard2);
        let _ = handle1.join();
    }
}