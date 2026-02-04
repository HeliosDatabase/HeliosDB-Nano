//! Database statistics tracking
//!
//! Tracks database-wide statistics for monitoring and system views.
//! Includes:
//! - Basic database statistics (commits, rollbacks, block I/O, tuple operations)
//! - Query execution history for system views
//! - Transaction statistics tracking
//! - Replication status (when enabled)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Database-wide statistics
#[derive(Debug)]
pub struct DatabaseStats {
    /// Number of committed transactions
    xact_commit: AtomicU64,
    /// Number of rolled back transactions
    xact_rollback: AtomicU64,
    /// Number of blocks read from disk
    blks_read: AtomicU64,
    /// Number of blocks found in cache
    blks_hit: AtomicU64,
    /// Number of tuples returned by queries
    tup_returned: AtomicU64,
    /// Number of tuples fetched
    tup_fetched: AtomicU64,
    /// Number of tuples inserted
    tup_inserted: AtomicU64,
    /// Number of tuples updated
    tup_updated: AtomicU64,
    /// Number of tuples deleted
    tup_deleted: AtomicU64,
}

impl DatabaseStats {
    /// Create new statistics tracker
    pub fn new() -> Self {
        Self {
            xact_commit: AtomicU64::new(0),
            xact_rollback: AtomicU64::new(0),
            blks_read: AtomicU64::new(0),
            blks_hit: AtomicU64::new(0),
            tup_returned: AtomicU64::new(0),
            tup_fetched: AtomicU64::new(0),
            tup_inserted: AtomicU64::new(0),
            tup_updated: AtomicU64::new(0),
            tup_deleted: AtomicU64::new(0),
        }
    }

    /// Increment transaction commit counter
    pub fn increment_commit(&self) {
        self.xact_commit.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment transaction rollback counter
    pub fn increment_rollback(&self) {
        self.xact_rollback.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment block read counter
    pub fn increment_blks_read(&self, count: u64) {
        self.blks_read.fetch_add(count, Ordering::Relaxed);
    }

    /// Increment block cache hit counter
    pub fn increment_blks_hit(&self, count: u64) {
        self.blks_hit.fetch_add(count, Ordering::Relaxed);
    }

    /// Increment tuples returned counter
    pub fn increment_tup_returned(&self, count: u64) {
        self.tup_returned.fetch_add(count, Ordering::Relaxed);
    }

    /// Increment tuples fetched counter
    pub fn increment_tup_fetched(&self, count: u64) {
        self.tup_fetched.fetch_add(count, Ordering::Relaxed);
    }

    /// Increment tuples inserted counter
    pub fn increment_tup_inserted(&self, count: u64) {
        self.tup_inserted.fetch_add(count, Ordering::Relaxed);
    }

    /// Increment tuples updated counter
    pub fn increment_tup_updated(&self, count: u64) {
        self.tup_updated.fetch_add(count, Ordering::Relaxed);
    }

    /// Increment tuples deleted counter
    pub fn increment_tup_deleted(&self, count: u64) {
        self.tup_deleted.fetch_add(count, Ordering::Relaxed);
    }

    /// Get transaction commit count
    pub fn get_commit_count(&self) -> u64 {
        self.xact_commit.load(Ordering::Relaxed)
    }

    /// Get transaction rollback count
    pub fn get_rollback_count(&self) -> u64 {
        self.xact_rollback.load(Ordering::Relaxed)
    }

    /// Get blocks read count
    pub fn get_blks_read(&self) -> u64 {
        self.blks_read.load(Ordering::Relaxed)
    }

    /// Get blocks hit count
    pub fn get_blks_hit(&self) -> u64 {
        self.blks_hit.load(Ordering::Relaxed)
    }

    /// Get tuples returned count
    pub fn get_tup_returned(&self) -> u64 {
        self.tup_returned.load(Ordering::Relaxed)
    }

    /// Get tuples fetched count
    pub fn get_tup_fetched(&self) -> u64 {
        self.tup_fetched.load(Ordering::Relaxed)
    }

    /// Get tuples inserted count
    pub fn get_tup_inserted(&self) -> u64 {
        self.tup_inserted.load(Ordering::Relaxed)
    }

    /// Get tuples updated count
    pub fn get_tup_updated(&self) -> u64 {
        self.tup_updated.load(Ordering::Relaxed)
    }

    /// Get tuples deleted count
    pub fn get_tup_deleted(&self) -> u64 {
        self.tup_deleted.load(Ordering::Relaxed)
    }

    /// Get snapshot of all statistics
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            xact_commit: self.get_commit_count(),
            xact_rollback: self.get_rollback_count(),
            blks_read: self.get_blks_read(),
            blks_hit: self.get_blks_hit(),
            tup_returned: self.get_tup_returned(),
            tup_fetched: self.get_tup_fetched(),
            tup_inserted: self.get_tup_inserted(),
            tup_updated: self.get_tup_updated(),
            tup_deleted: self.get_tup_deleted(),
        }
    }

    /// Reset all statistics
    pub fn reset(&self) {
        self.xact_commit.store(0, Ordering::Relaxed);
        self.xact_rollback.store(0, Ordering::Relaxed);
        self.blks_read.store(0, Ordering::Relaxed);
        self.blks_hit.store(0, Ordering::Relaxed);
        self.tup_returned.store(0, Ordering::Relaxed);
        self.tup_fetched.store(0, Ordering::Relaxed);
        self.tup_inserted.store(0, Ordering::Relaxed);
        self.tup_updated.store(0, Ordering::Relaxed);
        self.tup_deleted.store(0, Ordering::Relaxed);
    }
}

impl Default for DatabaseStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of statistics at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub xact_commit: u64,
    pub xact_rollback: u64,
    pub blks_read: u64,
    pub blks_hit: u64,
    pub tup_returned: u64,
    pub tup_fetched: u64,
    pub tup_inserted: u64,
    pub tup_updated: u64,
    pub tup_deleted: u64,
}

// ============================================================================
// Query History Tracking
// ============================================================================

/// Status of a query execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryStatus {
    /// Query is currently running
    Running,
    /// Query completed successfully
    Completed,
    /// Query failed with an error
    Failed,
    /// Query was cancelled
    Cancelled,
}

impl std::fmt::Display for QueryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryStatus::Running => write!(f, "running"),
            QueryStatus::Completed => write!(f, "completed"),
            QueryStatus::Failed => write!(f, "failed"),
            QueryStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Single query execution history entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    /// Unique query identifier
    pub query_id: u64,
    /// Hash of the normalized query text (for grouping similar queries)
    pub query_hash: u64,
    /// Original query text (may be truncated for very long queries)
    pub query_text: String,
    /// Query start timestamp
    pub start_time: DateTime<Utc>,
    /// Query end timestamp (None if still running)
    pub end_time: Option<DateTime<Utc>>,
    /// Query duration in milliseconds
    pub duration_ms: Option<u64>,
    /// Number of rows returned
    pub rows_returned: u64,
    /// Number of rows examined during execution
    pub rows_examined: u64,
    /// Query execution status
    pub status: QueryStatus,
    /// Error message if query failed
    pub error_message: Option<String>,
    /// User/session that ran the query
    pub user_name: String,
    /// Database name
    pub database_name: String,
    /// Client IP address
    pub client_addr: Option<String>,
    /// Application name
    pub application_name: Option<String>,
    /// Query type (SELECT, INSERT, UPDATE, DELETE, etc.)
    pub query_type: String,
    /// Whether this was a prepared statement
    pub is_prepared: bool,
    /// Plan execution time in milliseconds
    pub plan_time_ms: Option<f64>,
    /// Execution time in milliseconds
    pub exec_time_ms: Option<f64>,
    /// Shared blocks hit (from cache)
    pub shared_blks_hit: u64,
    /// Shared blocks read (from disk)
    pub shared_blks_read: u64,
    /// Shared blocks written
    pub shared_blks_written: u64,
    /// Temporary blocks read
    pub temp_blks_read: u64,
    /// Temporary blocks written
    pub temp_blks_written: u64,
}

impl QueryHistoryEntry {
    /// Create a new query history entry for a starting query
    pub fn new_running(
        query_id: u64,
        query_text: String,
        user_name: String,
        database_name: String,
    ) -> Self {
        let query_hash = Self::compute_query_hash(&query_text);
        let query_type = Self::extract_query_type(&query_text);

        Self {
            query_id,
            query_hash,
            query_text: if query_text.len() > 4096 {
                format!("{}...", &query_text[..4096])
            } else {
                query_text
            },
            start_time: Utc::now(),
            end_time: None,
            duration_ms: None,
            rows_returned: 0,
            rows_examined: 0,
            status: QueryStatus::Running,
            error_message: None,
            user_name,
            database_name,
            client_addr: None,
            application_name: None,
            query_type,
            is_prepared: false,
            plan_time_ms: None,
            exec_time_ms: None,
            shared_blks_hit: 0,
            shared_blks_read: 0,
            shared_blks_written: 0,
            temp_blks_read: 0,
            temp_blks_written: 0,
        }
    }

    /// Mark query as completed
    pub fn mark_completed(&mut self, rows_returned: u64, rows_examined: u64) {
        let end_time = Utc::now();
        self.end_time = Some(end_time);
        self.duration_ms = Some(
            (end_time - self.start_time).num_milliseconds().max(0) as u64
        );
        self.rows_returned = rows_returned;
        self.rows_examined = rows_examined;
        self.status = QueryStatus::Completed;
    }

    /// Mark query as failed
    pub fn mark_failed(&mut self, error: String) {
        let end_time = Utc::now();
        self.end_time = Some(end_time);
        self.duration_ms = Some(
            (end_time - self.start_time).num_milliseconds().max(0) as u64
        );
        self.status = QueryStatus::Failed;
        self.error_message = Some(error);
    }

    /// Mark query as cancelled
    pub fn mark_cancelled(&mut self) {
        let end_time = Utc::now();
        self.end_time = Some(end_time);
        self.duration_ms = Some(
            (end_time - self.start_time).num_milliseconds().max(0) as u64
        );
        self.status = QueryStatus::Cancelled;
    }

    /// Compute a hash for query normalization (replaces literals with placeholders)
    fn compute_query_hash(query: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Simple normalization: lowercase and collapse whitespace
        let normalized = query
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let mut hasher = DefaultHasher::new();
        normalized.hash(&mut hasher);
        hasher.finish()
    }

    /// Extract query type from SQL
    fn extract_query_type(query: &str) -> String {
        let trimmed = query.trim().to_uppercase();
        let first_word = trimmed.split_whitespace().next().unwrap_or("UNKNOWN");

        match first_word {
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "CREATE" | "DROP" |
            "ALTER" | "TRUNCATE" | "BEGIN" | "COMMIT" | "ROLLBACK" | "EXPLAIN" |
            "ANALYZE" | "VACUUM" | "COPY" | "GRANT" | "REVOKE" | "SET" | "SHOW" => {
                first_word.to_string()
            }
            "WITH" => "SELECT".to_string(), // CTEs are typically SELECTs
            _ => "OTHER".to_string(),
        }
    }

    /// Set block statistics
    pub fn set_block_stats(
        &mut self,
        blks_hit: u64,
        blks_read: u64,
        blks_written: u64,
        temp_read: u64,
        temp_written: u64,
    ) {
        self.shared_blks_hit = blks_hit;
        self.shared_blks_read = blks_read;
        self.shared_blks_written = blks_written;
        self.temp_blks_read = temp_read;
        self.temp_blks_written = temp_written;
    }

    /// Set timing information
    pub fn set_timing(&mut self, plan_time_ms: f64, exec_time_ms: f64) {
        self.plan_time_ms = Some(plan_time_ms);
        self.exec_time_ms = Some(exec_time_ms);
    }
}

/// Query history tracker with circular buffer
pub struct QueryHistoryTracker {
    /// Maximum number of entries to keep
    max_entries: usize,
    /// Query history entries (most recent last)
    entries: RwLock<VecDeque<QueryHistoryEntry>>,
    /// Next query ID
    next_query_id: AtomicU64,
    /// Currently running queries by ID
    running_queries: RwLock<HashMap<u64, QueryHistoryEntry>>,
}

impl QueryHistoryTracker {
    /// Create new query history tracker
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            entries: RwLock::new(VecDeque::with_capacity(max_entries)),
            next_query_id: AtomicU64::new(1),
            running_queries: RwLock::new(HashMap::new()),
        }
    }

    /// Start tracking a new query, returns query ID
    pub fn start_query(
        &self,
        query_text: String,
        user_name: String,
        database_name: String,
    ) -> u64 {
        let query_id = self.next_query_id.fetch_add(1, Ordering::SeqCst);
        let entry = QueryHistoryEntry::new_running(
            query_id,
            query_text,
            user_name,
            database_name,
        );

        if let Ok(mut running) = self.running_queries.write() {
            running.insert(query_id, entry);
        }

        query_id
    }

    /// Complete a query and move to history
    pub fn complete_query(&self, query_id: u64, rows_returned: u64, rows_examined: u64) {
        let entry = {
            let mut running = match self.running_queries.write() {
                Ok(r) => r,
                Err(_) => return,
            };

            let mut entry = match running.remove(&query_id) {
                Some(e) => e,
                None => return,
            };

            entry.mark_completed(rows_returned, rows_examined);
            entry
        };

        self.add_to_history(entry);
    }

    /// Fail a query and move to history
    pub fn fail_query(&self, query_id: u64, error: String) {
        let entry = {
            let mut running = match self.running_queries.write() {
                Ok(r) => r,
                Err(_) => return,
            };

            let mut entry = match running.remove(&query_id) {
                Some(e) => e,
                None => return,
            };

            entry.mark_failed(error);
            entry
        };

        self.add_to_history(entry);
    }

    /// Cancel a query and move to history
    pub fn cancel_query(&self, query_id: u64) {
        let entry = {
            let mut running = match self.running_queries.write() {
                Ok(r) => r,
                Err(_) => return,
            };

            let mut entry = match running.remove(&query_id) {
                Some(e) => e,
                None => return,
            };

            entry.mark_cancelled();
            entry
        };

        self.add_to_history(entry);
    }

    /// Add entry to history (internal)
    fn add_to_history(&self, entry: QueryHistoryEntry) {
        if let Ok(mut entries) = self.entries.write() {
            if entries.len() >= self.max_entries {
                entries.pop_front();
            }
            entries.push_back(entry);
        }
    }

    /// Get all history entries
    pub fn get_history(&self) -> Vec<QueryHistoryEntry> {
        self.entries.read()
            .map(|e| e.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get recent history entries (up to limit)
    pub fn get_recent(&self, limit: usize) -> Vec<QueryHistoryEntry> {
        self.entries.read()
            .map(|e| e.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Get currently running queries
    pub fn get_running(&self) -> Vec<QueryHistoryEntry> {
        self.running_queries.read()
            .map(|r| r.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Get query by ID (from running or history)
    pub fn get_query(&self, query_id: u64) -> Option<QueryHistoryEntry> {
        // Check running first
        if let Ok(running) = self.running_queries.read() {
            if let Some(entry) = running.get(&query_id) {
                return Some(entry.clone());
            }
        }

        // Check history
        if let Ok(entries) = self.entries.read() {
            return entries.iter().find(|e| e.query_id == query_id).cloned();
        }

        None
    }

    /// Get statistics summary
    pub fn get_stats(&self) -> QueryHistoryStats {
        let entries = self.entries.read().ok();
        let running = self.running_queries.read().ok();

        let total_queries = entries.as_ref().map(|e| e.len()).unwrap_or(0);
        let running_queries = running.as_ref().map(|r| r.len()).unwrap_or(0);

        let (completed, failed, cancelled) = entries.as_ref()
            .map(|e| {
                let mut completed = 0u64;
                let mut failed = 0u64;
                let mut cancelled = 0u64;
                for entry in e.iter() {
                    match entry.status {
                        QueryStatus::Completed => completed += 1,
                        QueryStatus::Failed => failed += 1,
                        QueryStatus::Cancelled => cancelled += 1,
                        QueryStatus::Running => {}
                    }
                }
                (completed, failed, cancelled)
            })
            .unwrap_or((0, 0, 0));

        let avg_duration_ms = entries.as_ref()
            .map(|e| {
                let durations: Vec<u64> = e.iter()
                    .filter_map(|entry| entry.duration_ms)
                    .collect();
                if durations.is_empty() {
                    0.0
                } else {
                    durations.iter().sum::<u64>() as f64 / durations.len() as f64
                }
            })
            .unwrap_or(0.0);

        QueryHistoryStats {
            total_queries: total_queries as u64,
            running_queries: running_queries as u64,
            completed_queries: completed,
            failed_queries: failed,
            cancelled_queries: cancelled,
            avg_duration_ms,
        }
    }

    /// Clear all history
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}

impl Default for QueryHistoryTracker {
    fn default() -> Self {
        Self::new(10000) // Keep last 10k queries by default
    }
}

/// Summary statistics for query history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryHistoryStats {
    pub total_queries: u64,
    pub running_queries: u64,
    pub completed_queries: u64,
    pub failed_queries: u64,
    pub cancelled_queries: u64,
    pub avg_duration_ms: f64,
}

// ============================================================================
// Transaction Statistics
// ============================================================================

/// Transaction state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionState {
    /// Transaction is active
    Active,
    /// Transaction is idle (between statements)
    Idle,
    /// Transaction is idle in a transaction block
    IdleInTransaction,
    /// Transaction is idle in a failed transaction block
    IdleInTransactionAborted,
    /// Transaction is being committed
    Committing,
    /// Transaction is being rolled back
    Aborting,
}

impl std::fmt::Display for TransactionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransactionState::Active => write!(f, "active"),
            TransactionState::Idle => write!(f, "idle"),
            TransactionState::IdleInTransaction => write!(f, "idle in transaction"),
            TransactionState::IdleInTransactionAborted => write!(f, "idle in transaction (aborted)"),
            TransactionState::Committing => write!(f, "committing"),
            TransactionState::Aborting => write!(f, "aborting"),
        }
    }
}

/// Individual transaction statistics entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionEntry {
    /// Transaction ID
    pub xact_id: u64,
    /// Transaction start time
    pub start_time: DateTime<Utc>,
    /// Current transaction state
    pub state: TransactionState,
    /// User name
    pub user_name: String,
    /// Database name
    pub database_name: String,
    /// Client address
    pub client_addr: Option<String>,
    /// Application name
    pub application_name: Option<String>,
    /// Number of statements executed in this transaction
    pub statement_count: u64,
    /// Time of last statement
    pub last_statement_time: Option<DateTime<Utc>>,
    /// Current/last query text
    pub current_query: Option<String>,
    /// Backend PID (process ID)
    pub backend_pid: u32,
    /// Wait event type (if waiting)
    pub wait_event_type: Option<String>,
    /// Wait event (if waiting)
    pub wait_event: Option<String>,
    /// Whether this is a prepared transaction
    pub is_prepared: bool,
}

impl TransactionEntry {
    /// Create a new transaction entry
    pub fn new(
        xact_id: u64,
        user_name: String,
        database_name: String,
        backend_pid: u32,
    ) -> Self {
        Self {
            xact_id,
            start_time: Utc::now(),
            state: TransactionState::Idle,
            user_name,
            database_name,
            client_addr: None,
            application_name: None,
            statement_count: 0,
            last_statement_time: None,
            current_query: None,
            backend_pid,
            wait_event_type: None,
            wait_event: None,
            is_prepared: false,
        }
    }

    /// Set transaction as active with a query
    pub fn set_active(&mut self, query: String) {
        self.state = TransactionState::Active;
        self.current_query = Some(query);
        self.last_statement_time = Some(Utc::now());
        self.statement_count += 1;
    }

    /// Set transaction as idle
    pub fn set_idle(&mut self) {
        self.state = TransactionState::Idle;
    }

    /// Set transaction as idle in transaction
    pub fn set_idle_in_transaction(&mut self) {
        self.state = TransactionState::IdleInTransaction;
    }

    /// Get transaction duration in milliseconds
    pub fn duration_ms(&self) -> i64 {
        (Utc::now() - self.start_time).num_milliseconds()
    }
}

/// Transaction tracker for monitoring active transactions
pub struct TransactionTracker {
    /// Active transactions by ID
    active_transactions: RwLock<HashMap<u64, TransactionEntry>>,
    /// Next transaction ID
    next_xact_id: AtomicU64,
    /// Total transactions started
    total_started: AtomicU64,
    /// Total transactions committed
    total_committed: AtomicU64,
    /// Total transactions rolled back
    total_rolled_back: AtomicU64,
    /// Total deadlocks detected
    total_deadlocks: AtomicU64,
}

impl TransactionTracker {
    /// Create new transaction tracker
    pub fn new() -> Self {
        Self {
            active_transactions: RwLock::new(HashMap::new()),
            next_xact_id: AtomicU64::new(1),
            total_started: AtomicU64::new(0),
            total_committed: AtomicU64::new(0),
            total_rolled_back: AtomicU64::new(0),
            total_deadlocks: AtomicU64::new(0),
        }
    }

    /// Start a new transaction, returns transaction ID
    pub fn start_transaction(
        &self,
        user_name: String,
        database_name: String,
        backend_pid: u32,
    ) -> u64 {
        let xact_id = self.next_xact_id.fetch_add(1, Ordering::SeqCst);
        let entry = TransactionEntry::new(xact_id, user_name, database_name, backend_pid);

        if let Ok(mut active) = self.active_transactions.write() {
            active.insert(xact_id, entry);
        }

        self.total_started.fetch_add(1, Ordering::Relaxed);
        xact_id
    }

    /// Update transaction state to active with a query
    pub fn set_active(&self, xact_id: u64, query: String) {
        if let Ok(mut active) = self.active_transactions.write() {
            if let Some(entry) = active.get_mut(&xact_id) {
                entry.set_active(query);
            }
        }
    }

    /// Update transaction state to idle
    pub fn set_idle(&self, xact_id: u64) {
        if let Ok(mut active) = self.active_transactions.write() {
            if let Some(entry) = active.get_mut(&xact_id) {
                entry.set_idle();
            }
        }
    }

    /// Update transaction state to idle in transaction
    pub fn set_idle_in_transaction(&self, xact_id: u64) {
        if let Ok(mut active) = self.active_transactions.write() {
            if let Some(entry) = active.get_mut(&xact_id) {
                entry.set_idle_in_transaction();
            }
        }
    }

    /// Commit a transaction
    pub fn commit(&self, xact_id: u64) {
        if let Ok(mut active) = self.active_transactions.write() {
            active.remove(&xact_id);
        }
        self.total_committed.fetch_add(1, Ordering::Relaxed);
    }

    /// Rollback a transaction
    pub fn rollback(&self, xact_id: u64) {
        if let Ok(mut active) = self.active_transactions.write() {
            active.remove(&xact_id);
        }
        self.total_rolled_back.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a deadlock
    pub fn record_deadlock(&self) {
        self.total_deadlocks.fetch_add(1, Ordering::Relaxed);
    }

    /// Get all active transactions
    pub fn get_active(&self) -> Vec<TransactionEntry> {
        self.active_transactions.read()
            .map(|a| a.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Get transaction by ID
    pub fn get_transaction(&self, xact_id: u64) -> Option<TransactionEntry> {
        self.active_transactions.read()
            .ok()
            .and_then(|a| a.get(&xact_id).cloned())
    }

    /// Get transaction statistics
    pub fn get_stats(&self) -> TransactionTrackerStats {
        let active_count = self.active_transactions.read()
            .map(|a| a.len() as u64)
            .unwrap_or(0);

        TransactionTrackerStats {
            active_transactions: active_count,
            total_started: self.total_started.load(Ordering::Relaxed),
            total_committed: self.total_committed.load(Ordering::Relaxed),
            total_rolled_back: self.total_rolled_back.load(Ordering::Relaxed),
            total_deadlocks: self.total_deadlocks.load(Ordering::Relaxed),
        }
    }

    /// Get longest running transactions
    pub fn get_longest_running(&self, limit: usize) -> Vec<TransactionEntry> {
        let mut entries: Vec<TransactionEntry> = self.active_transactions.read()
            .map(|a| a.values().cloned().collect())
            .unwrap_or_default();

        entries.sort_by(|a, b| a.start_time.cmp(&b.start_time));
        entries.truncate(limit);
        entries
    }
}

impl Default for TransactionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Transaction tracker statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionTrackerStats {
    pub active_transactions: u64,
    pub total_started: u64,
    pub total_committed: u64,
    pub total_rolled_back: u64,
    pub total_deadlocks: u64,
}

// ============================================================================
// Replication Status
// ============================================================================

/// Replication role
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationRole {
    /// Primary/master server
    Primary,
    /// Replica/standby server
    Replica,
    /// Not in a replication configuration
    Standalone,
}

impl std::fmt::Display for ReplicationRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicationRole::Primary => write!(f, "primary"),
            ReplicationRole::Replica => write!(f, "replica"),
            ReplicationRole::Standalone => write!(f, "standalone"),
        }
    }
}

/// Replication state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationState {
    /// Replication is streaming normally
    Streaming,
    /// Replica is catching up
    CatchUp,
    /// Replication is paused
    Paused,
    /// Replication connection lost
    Disconnected,
    /// Initial synchronization
    Initializing,
}

impl std::fmt::Display for ReplicationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicationState::Streaming => write!(f, "streaming"),
            ReplicationState::CatchUp => write!(f, "catchup"),
            ReplicationState::Paused => write!(f, "paused"),
            ReplicationState::Disconnected => write!(f, "disconnected"),
            ReplicationState::Initializing => write!(f, "initializing"),
        }
    }
}

/// Replication slot information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationSlot {
    /// Slot name
    pub slot_name: String,
    /// Slot type (physical or logical)
    pub slot_type: String,
    /// Database name (for logical slots)
    pub database: Option<String>,
    /// Whether the slot is active
    pub active: bool,
    /// LSN at which this slot became active
    pub active_pid: Option<u32>,
    /// Oldest transaction ID this slot needs
    pub xmin: Option<u64>,
    /// Catalog transaction ID
    pub catalog_xmin: Option<u64>,
    /// Restart LSN
    pub restart_lsn: Option<String>,
    /// Confirmed flush LSN
    pub confirmed_flush_lsn: Option<String>,
}

/// Replication status for a replica
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaStatus {
    /// Replica identifier
    pub replica_id: String,
    /// Replica hostname or IP
    pub host: String,
    /// Replica port
    pub port: u16,
    /// Current replication state
    pub state: ReplicationState,
    /// Current LSN position
    pub current_lsn: String,
    /// Replay LSN position
    pub replay_lsn: String,
    /// Bytes behind primary
    pub bytes_lag: u64,
    /// Time lag in milliseconds
    pub time_lag_ms: u64,
    /// Last communication time
    pub last_msg_time: DateTime<Utc>,
    /// Application name
    pub application_name: Option<String>,
}

/// Replication status tracker
pub struct ReplicationStatus {
    /// Current role
    role: RwLock<ReplicationRole>,
    /// Replication state (if replica)
    state: RwLock<Option<ReplicationState>>,
    /// Primary server info (if replica)
    primary_host: RwLock<Option<String>>,
    primary_port: RwLock<Option<u16>>,
    /// Current write-ahead log position
    current_lsn: RwLock<String>,
    /// Replicas (if primary)
    replicas: RwLock<Vec<ReplicaStatus>>,
    /// Replication slots
    slots: RwLock<Vec<ReplicationSlot>>,
    /// Total bytes sent (if primary)
    bytes_sent: AtomicU64,
    /// Total bytes received (if replica)
    bytes_received: AtomicU64,
}

impl ReplicationStatus {
    /// Create new replication status tracker
    pub fn new() -> Self {
        Self {
            role: RwLock::new(ReplicationRole::Standalone),
            state: RwLock::new(None),
            primary_host: RwLock::new(None),
            primary_port: RwLock::new(None),
            current_lsn: RwLock::new("0/0".to_string()),
            replicas: RwLock::new(Vec::new()),
            slots: RwLock::new(Vec::new()),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
        }
    }

    /// Set as primary server
    pub fn set_primary(&self) {
        if let Ok(mut role) = self.role.write() {
            *role = ReplicationRole::Primary;
        }
        if let Ok(mut state) = self.state.write() {
            *state = None;
        }
    }

    /// Set as replica server
    pub fn set_replica(&self, primary_host: String, primary_port: u16) {
        if let Ok(mut role) = self.role.write() {
            *role = ReplicationRole::Replica;
        }
        if let Ok(mut state) = self.state.write() {
            *state = Some(ReplicationState::Initializing);
        }
        if let Ok(mut host) = self.primary_host.write() {
            *host = Some(primary_host);
        }
        if let Ok(mut port) = self.primary_port.write() {
            *port = Some(primary_port);
        }
    }

    /// Update replication state
    pub fn set_state(&self, new_state: ReplicationState) {
        if let Ok(mut state) = self.state.write() {
            *state = Some(new_state);
        }
    }

    /// Update current LSN
    pub fn set_lsn(&self, lsn: String) {
        if let Ok(mut current) = self.current_lsn.write() {
            *current = lsn;
        }
    }

    /// Add or update a replica (for primary)
    pub fn update_replica(&self, replica: ReplicaStatus) {
        if let Ok(mut replicas) = self.replicas.write() {
            if let Some(existing) = replicas.iter_mut().find(|r| r.replica_id == replica.replica_id) {
                *existing = replica;
            } else {
                replicas.push(replica);
            }
        }
    }

    /// Remove a replica
    pub fn remove_replica(&self, replica_id: &str) {
        if let Ok(mut replicas) = self.replicas.write() {
            replicas.retain(|r| r.replica_id != replica_id);
        }
    }

    /// Add a replication slot
    pub fn add_slot(&self, slot: ReplicationSlot) {
        if let Ok(mut slots) = self.slots.write() {
            if let Some(existing) = slots.iter_mut().find(|s| s.slot_name == slot.slot_name) {
                *existing = slot;
            } else {
                slots.push(slot);
            }
        }
    }

    /// Remove a replication slot
    pub fn remove_slot(&self, slot_name: &str) {
        if let Ok(mut slots) = self.slots.write() {
            slots.retain(|s| s.slot_name != slot_name);
        }
    }

    /// Record bytes sent (primary)
    pub fn add_bytes_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record bytes received (replica)
    pub fn add_bytes_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get current role
    pub fn get_role(&self) -> ReplicationRole {
        self.role.read()
            .map(|r| *r)
            .unwrap_or(ReplicationRole::Standalone)
    }

    /// Get current state
    pub fn get_state(&self) -> Option<ReplicationState> {
        self.state.read().ok().and_then(|s| *s)
    }

    /// Get current LSN
    pub fn get_lsn(&self) -> String {
        self.current_lsn.read()
            .map(|l| l.clone())
            .unwrap_or_else(|_| "0/0".to_string())
    }

    /// Get all replicas
    pub fn get_replicas(&self) -> Vec<ReplicaStatus> {
        self.replicas.read()
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// Get all slots
    pub fn get_slots(&self) -> Vec<ReplicationSlot> {
        self.slots.read()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Get summary status
    pub fn get_summary(&self) -> ReplicationSummary {
        let role = self.get_role();
        let state = self.get_state();
        let lsn = self.get_lsn();
        let replicas = self.get_replicas();
        let slots = self.get_slots();

        let (primary_host, primary_port) = if role == ReplicationRole::Replica {
            (
                self.primary_host.read().ok().and_then(|h| h.clone()),
                self.primary_port.read().ok().and_then(|p| *p),
            )
        } else {
            (None, None)
        };

        ReplicationSummary {
            role,
            state,
            current_lsn: lsn,
            primary_host,
            primary_port,
            replica_count: replicas.len(),
            slot_count: slots.len(),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            max_replica_lag_ms: replicas.iter().map(|r| r.time_lag_ms).max().unwrap_or(0),
        }
    }
}

impl Default for ReplicationStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// Replication summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationSummary {
    pub role: ReplicationRole,
    pub state: Option<ReplicationState>,
    pub current_lsn: String,
    pub primary_host: Option<String>,
    pub primary_port: Option<u16>,
    pub replica_count: usize,
    pub slot_count: usize,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub max_replica_lag_ms: u64,
}

// ============================================================================
// Global Statistics Collector
// ============================================================================

/// Global statistics collector that combines all stat types
pub struct GlobalStatsCollector {
    /// Database statistics
    pub database_stats: Arc<DatabaseStats>,
    /// Query history tracker
    pub query_history: Arc<QueryHistoryTracker>,
    /// Transaction tracker
    pub transactions: Arc<TransactionTracker>,
    /// Replication status
    pub replication: Arc<ReplicationStatus>,
    /// Statistics collection start time
    pub stats_reset_time: DateTime<Utc>,
}

impl GlobalStatsCollector {
    /// Create new global stats collector
    pub fn new() -> Self {
        Self {
            database_stats: Arc::new(DatabaseStats::new()),
            query_history: Arc::new(QueryHistoryTracker::new(10000)),
            transactions: Arc::new(TransactionTracker::new()),
            replication: Arc::new(ReplicationStatus::new()),
            stats_reset_time: Utc::now(),
        }
    }

    /// Create with custom query history size
    pub fn with_history_size(history_size: usize) -> Self {
        Self {
            database_stats: Arc::new(DatabaseStats::new()),
            query_history: Arc::new(QueryHistoryTracker::new(history_size)),
            transactions: Arc::new(TransactionTracker::new()),
            replication: Arc::new(ReplicationStatus::new()),
            stats_reset_time: Utc::now(),
        }
    }

    /// Get all statistics as a combined snapshot
    pub fn snapshot(&self) -> GlobalStatsSnapshot {
        GlobalStatsSnapshot {
            database: self.database_stats.snapshot(),
            query_history: self.query_history.get_stats(),
            transactions: self.transactions.get_stats(),
            replication: self.replication.get_summary(),
            stats_reset_time: self.stats_reset_time,
            snapshot_time: Utc::now(),
        }
    }

    /// Reset all statistics
    pub fn reset(&mut self) {
        self.database_stats.reset();
        self.query_history.clear();
        self.stats_reset_time = Utc::now();
    }
}

impl Default for GlobalStatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined snapshot of all statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStatsSnapshot {
    pub database: StatsSnapshot,
    pub query_history: QueryHistoryStats,
    pub transactions: TransactionTrackerStats,
    pub replication: ReplicationSummary,
    pub stats_reset_time: DateTime<Utc>,
    pub snapshot_time: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_creation() {
        let stats = DatabaseStats::new();
        assert_eq!(stats.get_commit_count(), 0);
        assert_eq!(stats.get_rollback_count(), 0);
    }

    #[test]
    fn test_increment_commit() {
        let stats = DatabaseStats::new();
        stats.increment_commit();
        stats.increment_commit();
        assert_eq!(stats.get_commit_count(), 2);
    }

    #[test]
    fn test_increment_rollback() {
        let stats = DatabaseStats::new();
        stats.increment_rollback();
        assert_eq!(stats.get_rollback_count(), 1);
    }

    #[test]
    fn test_snapshot() {
        let stats = DatabaseStats::new();
        stats.increment_commit();
        stats.increment_tup_inserted(5);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.xact_commit, 1);
        assert_eq!(snapshot.tup_inserted, 5);
    }

    #[test]
    fn test_reset() {
        let stats = DatabaseStats::new();
        stats.increment_commit();
        stats.increment_rollback();
        stats.reset();

        assert_eq!(stats.get_commit_count(), 0);
        assert_eq!(stats.get_rollback_count(), 0);
    }

    // Query History Tests
    #[test]
    fn test_query_history_start_and_complete() {
        let tracker = QueryHistoryTracker::new(100);

        let query_id = tracker.start_query(
            "SELECT * FROM users".to_string(),
            "admin".to_string(),
            "testdb".to_string(),
        );

        assert!(query_id > 0);

        // Should be in running queries
        let running = tracker.get_running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].query_id, query_id);

        // Complete the query
        tracker.complete_query(query_id, 10, 50);

        // Should no longer be running
        let running = tracker.get_running();
        assert!(running.is_empty());

        // Should be in history
        let history = tracker.get_history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].query_id, query_id);
        assert_eq!(history[0].rows_returned, 10);
        assert_eq!(history[0].rows_examined, 50);
        assert_eq!(history[0].status, QueryStatus::Completed);
    }

    #[test]
    fn test_query_history_fail() {
        let tracker = QueryHistoryTracker::new(100);

        let query_id = tracker.start_query(
            "SELECT * FROM nonexistent".to_string(),
            "admin".to_string(),
            "testdb".to_string(),
        );

        tracker.fail_query(query_id, "Table not found".to_string());

        let history = tracker.get_history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, QueryStatus::Failed);
        assert_eq!(history[0].error_message, Some("Table not found".to_string()));
    }

    #[test]
    fn test_query_history_cancel() {
        let tracker = QueryHistoryTracker::new(100);

        let query_id = tracker.start_query(
            "SELECT * FROM large_table".to_string(),
            "admin".to_string(),
            "testdb".to_string(),
        );

        tracker.cancel_query(query_id);

        let history = tracker.get_history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, QueryStatus::Cancelled);
    }

    #[test]
    fn test_query_history_circular_buffer() {
        let tracker = QueryHistoryTracker::new(3); // Small buffer

        for i in 0..5 {
            let qid = tracker.start_query(
                format!("SELECT {}", i),
                "admin".to_string(),
                "testdb".to_string(),
            );
            tracker.complete_query(qid, i as u64, 0);
        }

        let history = tracker.get_history();
        assert_eq!(history.len(), 3); // Only 3 entries kept
        // Should have queries 2, 3, 4 (oldest dropped)
        assert!(history[0].query_text.contains("2"));
        assert!(history[1].query_text.contains("3"));
        assert!(history[2].query_text.contains("4"));
    }

    #[test]
    fn test_query_history_stats() {
        let tracker = QueryHistoryTracker::new(100);

        // Add some queries with different statuses
        let q1 = tracker.start_query("SELECT 1".to_string(), "admin".to_string(), "db".to_string());
        tracker.complete_query(q1, 1, 1);

        let q2 = tracker.start_query("SELECT 2".to_string(), "admin".to_string(), "db".to_string());
        tracker.complete_query(q2, 1, 1);

        let q3 = tracker.start_query("SELECT bad".to_string(), "admin".to_string(), "db".to_string());
        tracker.fail_query(q3, "error".to_string());

        let stats = tracker.get_stats();
        assert_eq!(stats.total_queries, 3);
        assert_eq!(stats.completed_queries, 2);
        assert_eq!(stats.failed_queries, 1);
        assert_eq!(stats.cancelled_queries, 0);
    }

    #[test]
    fn test_query_type_extraction() {
        let entry = QueryHistoryEntry::new_running(1, "SELECT * FROM t".into(), "u".into(), "d".into());
        assert_eq!(entry.query_type, "SELECT");

        let entry = QueryHistoryEntry::new_running(2, "INSERT INTO t VALUES (1)".into(), "u".into(), "d".into());
        assert_eq!(entry.query_type, "INSERT");

        let entry = QueryHistoryEntry::new_running(3, "  update t set x = 1".into(), "u".into(), "d".into());
        assert_eq!(entry.query_type, "UPDATE");

        let entry = QueryHistoryEntry::new_running(4, "WITH cte AS (SELECT 1) SELECT * FROM cte".into(), "u".into(), "d".into());
        assert_eq!(entry.query_type, "SELECT");
    }

    // Transaction Tracker Tests
    #[test]
    fn test_transaction_start_and_commit() {
        let tracker = TransactionTracker::new();

        let xact_id = tracker.start_transaction(
            "admin".to_string(),
            "testdb".to_string(),
            1234,
        );

        assert!(xact_id > 0);

        // Should be active
        let active = tracker.get_active();
        assert_eq!(active.len(), 1);

        // Commit
        tracker.commit(xact_id);

        // Should no longer be active
        let active = tracker.get_active();
        assert!(active.is_empty());

        let stats = tracker.get_stats();
        assert_eq!(stats.total_started, 1);
        assert_eq!(stats.total_committed, 1);
        assert_eq!(stats.total_rolled_back, 0);
    }

    #[test]
    fn test_transaction_rollback() {
        let tracker = TransactionTracker::new();

        let xact_id = tracker.start_transaction(
            "admin".to_string(),
            "testdb".to_string(),
            1234,
        );

        tracker.rollback(xact_id);

        let stats = tracker.get_stats();
        assert_eq!(stats.total_rolled_back, 1);
    }

    #[test]
    fn test_transaction_state_changes() {
        let tracker = TransactionTracker::new();

        let xact_id = tracker.start_transaction(
            "admin".to_string(),
            "testdb".to_string(),
            1234,
        );

        // Initial state should be Idle
        let tx = tracker.get_transaction(xact_id).unwrap();
        assert_eq!(tx.state, TransactionState::Idle);

        // Set active with query
        tracker.set_active(xact_id, "SELECT * FROM users".to_string());
        let tx = tracker.get_transaction(xact_id).unwrap();
        assert_eq!(tx.state, TransactionState::Active);
        assert_eq!(tx.statement_count, 1);

        // Set idle in transaction
        tracker.set_idle_in_transaction(xact_id);
        let tx = tracker.get_transaction(xact_id).unwrap();
        assert_eq!(tx.state, TransactionState::IdleInTransaction);
    }

    #[test]
    fn test_deadlock_recording() {
        let tracker = TransactionTracker::new();

        tracker.record_deadlock();
        tracker.record_deadlock();

        let stats = tracker.get_stats();
        assert_eq!(stats.total_deadlocks, 2);
    }

    // Replication Status Tests
    #[test]
    fn test_replication_standalone() {
        let repl = ReplicationStatus::new();
        assert_eq!(repl.get_role(), ReplicationRole::Standalone);
        assert!(repl.get_state().is_none());
    }

    #[test]
    fn test_replication_primary() {
        let repl = ReplicationStatus::new();
        repl.set_primary();

        assert_eq!(repl.get_role(), ReplicationRole::Primary);
    }

    #[test]
    fn test_replication_replica() {
        let repl = ReplicationStatus::new();
        repl.set_replica("primary.example.com".to_string(), 5432);

        assert_eq!(repl.get_role(), ReplicationRole::Replica);
        assert_eq!(repl.get_state(), Some(ReplicationState::Initializing));

        repl.set_state(ReplicationState::Streaming);
        assert_eq!(repl.get_state(), Some(ReplicationState::Streaming));
    }

    #[test]
    fn test_replication_slots() {
        let repl = ReplicationStatus::new();
        repl.set_primary();

        let slot = ReplicationSlot {
            slot_name: "my_slot".to_string(),
            slot_type: "physical".to_string(),
            database: None,
            active: true,
            active_pid: Some(1234),
            xmin: None,
            catalog_xmin: None,
            restart_lsn: Some("0/1000".to_string()),
            confirmed_flush_lsn: None,
        };

        repl.add_slot(slot);
        let slots = repl.get_slots();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].slot_name, "my_slot");

        repl.remove_slot("my_slot");
        assert!(repl.get_slots().is_empty());
    }

    #[test]
    fn test_replication_replicas() {
        let repl = ReplicationStatus::new();
        repl.set_primary();

        let replica = ReplicaStatus {
            replica_id: "replica1".to_string(),
            host: "replica1.example.com".to_string(),
            port: 5432,
            state: ReplicationState::Streaming,
            current_lsn: "0/2000".to_string(),
            replay_lsn: "0/1800".to_string(),
            bytes_lag: 512,
            time_lag_ms: 100,
            last_msg_time: Utc::now(),
            application_name: Some("replica1".to_string()),
        };

        repl.update_replica(replica);
        let replicas = repl.get_replicas();
        assert_eq!(replicas.len(), 1);

        repl.remove_replica("replica1");
        assert!(repl.get_replicas().is_empty());
    }

    #[test]
    fn test_replication_summary() {
        let repl = ReplicationStatus::new();
        repl.set_primary();
        repl.set_lsn("0/5000".to_string());
        repl.add_bytes_sent(1000);

        let summary = repl.get_summary();
        assert_eq!(summary.role, ReplicationRole::Primary);
        assert_eq!(summary.current_lsn, "0/5000");
        assert_eq!(summary.bytes_sent, 1000);
    }

    // Global Stats Collector Tests
    #[test]
    fn test_global_stats_collector() {
        let collector = GlobalStatsCollector::new();

        // Use database stats
        collector.database_stats.increment_commit();
        collector.database_stats.increment_tup_inserted(10);

        // Start a query
        let qid = collector.query_history.start_query(
            "SELECT 1".to_string(),
            "admin".to_string(),
            "db".to_string(),
        );
        collector.query_history.complete_query(qid, 1, 1);

        // Get snapshot
        let snapshot = collector.snapshot();
        assert_eq!(snapshot.database.xact_commit, 1);
        assert_eq!(snapshot.database.tup_inserted, 10);
        assert_eq!(snapshot.query_history.total_queries, 1);
    }
}
