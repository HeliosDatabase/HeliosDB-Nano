//! Query cancellation support
//!
//! Provides cooperative query cancellation using tokens that can be
//! checked at various points during query execution.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use chrono::{DateTime, Utc};

use crate::{Result, Error};

// =============================================================================
// CancellationToken - Thread-safe cancellation signal
// =============================================================================

/// A cooperative cancellation token
///
/// Executors should periodically check `is_cancelled()` and return early
/// if the query has been cancelled.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    /// Whether cancellation has been requested
    cancelled: Arc<AtomicBool>,
    /// Reason for cancellation (if any)
    reason: Arc<RwLock<Option<String>>>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Create a new cancellation token
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            reason: Arc::new(RwLock::new(None)),
        }
    }

    /// Check if cancellation has been requested
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Request cancellation
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Request cancellation with a reason
    pub fn cancel_with_reason(&self, reason: impl Into<String>) {
        if let Ok(mut r) = self.reason.write() {
            *r = Some(reason.into());
        }
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Get the cancellation reason (if any)
    pub fn cancellation_reason(&self) -> Option<String> {
        self.reason.read().ok().and_then(|r| r.clone())
    }

    /// Check if cancelled and return an error if so
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            let reason = self.cancellation_reason()
                .unwrap_or_else(|| "Query cancelled".to_string());
            Err(Error::QueryCancelled(reason))
        } else {
            Ok(())
        }
    }

    /// Create a child token that is cancelled when either parent or child is cancelled
    pub fn child(&self) -> CancellationToken {
        // Child shares the same cancelled flag but can have its own reason
        Self {
            cancelled: self.cancelled.clone(),
            reason: Arc::new(RwLock::new(None)),
        }
    }
}

// =============================================================================
// RunningQuery - Metadata for a running query
// =============================================================================

/// State of a running query
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryState {
    /// Query is being parsed/planned
    Planning,
    /// Query is executing
    Executing,
    /// Query is cancelled but still cleaning up
    Cancelling,
    /// Query completed successfully
    Completed,
    /// Query failed with an error
    Failed,
    /// Query was cancelled
    Cancelled,
}

impl std::fmt::Display for QueryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryState::Planning => write!(f, "planning"),
            QueryState::Executing => write!(f, "executing"),
            QueryState::Cancelling => write!(f, "cancelling"),
            QueryState::Completed => write!(f, "completed"),
            QueryState::Failed => write!(f, "failed"),
            QueryState::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Information about a running query
#[derive(Debug, Clone)]
pub struct RunningQuery {
    /// Unique query ID
    pub query_id: u64,
    /// SQL text (possibly truncated)
    pub sql: String,
    /// Session/user who submitted the query
    pub session_id: Option<u64>,
    /// User name
    pub user_name: String,
    /// Database name
    pub database: String,
    /// Query state
    pub state: QueryState,
    /// When the query started
    pub started_at: DateTime<Utc>,
    /// Elapsed time
    pub elapsed: Duration,
    /// Rows processed so far
    pub rows_processed: u64,
    /// Whether the query can be cancelled
    pub cancellable: bool,
    /// Cancellation token (for internal use)
    #[doc(hidden)]
    pub cancel_token: CancellationToken,
}

impl RunningQuery {
    /// Update elapsed time
    pub fn update_elapsed(&mut self) {
        let now = Utc::now();
        self.elapsed = (now - self.started_at)
            .to_std()
            .unwrap_or(Duration::ZERO);
    }
}

// =============================================================================
// QueryRegistry - Track all running queries
// =============================================================================

/// Registry of all running queries in the system
///
/// Provides thread-safe tracking and cancellation of queries.
#[derive(Debug)]
pub struct QueryRegistry {
    /// Running queries indexed by query_id
    queries: RwLock<HashMap<u64, RunningQuery>>,
    /// Next query ID
    next_id: AtomicU64,
    /// Maximum number of queries to track (prevents memory exhaustion)
    max_tracked: usize,
    /// Default query timeout
    default_timeout: Option<Duration>,
}

impl Default for QueryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryRegistry {
    /// Create a new query registry
    pub fn new() -> Self {
        Self {
            queries: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            max_tracked: 10000,
            default_timeout: None,
        }
    }

    /// Create a query registry with a default timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            queries: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            max_tracked: 10000,
            default_timeout: Some(timeout),
        }
    }

    /// Set the maximum number of tracked queries
    pub fn set_max_tracked(&mut self, max: usize) {
        self.max_tracked = max;
    }

    /// Register a new query and get a cancellation token
    pub fn register_query(
        &self,
        sql: &str,
        user_name: &str,
        database: &str,
        session_id: Option<u64>,
    ) -> (u64, CancellationToken) {
        let query_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let cancel_token = CancellationToken::new();

        // Truncate SQL for storage (keep first 1000 chars)
        let truncated_sql = if sql.len() > 1000 {
            format!("{}...", &sql[..1000])
        } else {
            sql.to_string()
        };

        let query = RunningQuery {
            query_id,
            sql: truncated_sql,
            session_id,
            user_name: user_name.to_string(),
            database: database.to_string(),
            state: QueryState::Planning,
            started_at: Utc::now(),
            elapsed: Duration::ZERO,
            rows_processed: 0,
            cancellable: true,
            cancel_token: cancel_token.clone(),
        };

        if let Ok(mut queries) = self.queries.write() {
            // Clean up old completed queries if we're at capacity
            if queries.len() >= self.max_tracked {
                let completed: Vec<u64> = queries
                    .iter()
                    .filter(|(_, q)| matches!(
                        q.state,
                        QueryState::Completed | QueryState::Failed | QueryState::Cancelled
                    ))
                    .map(|(id, _)| *id)
                    .collect();

                for id in completed.into_iter().take(queries.len() / 4) {
                    queries.remove(&id);
                }
            }
            queries.insert(query_id, query);
        }

        (query_id, cancel_token)
    }

    /// Update query state
    pub fn update_state(&self, query_id: u64, state: QueryState) {
        if let Ok(mut queries) = self.queries.write() {
            if let Some(query) = queries.get_mut(&query_id) {
                query.state = state;
                query.update_elapsed();
            }
        }
    }

    /// Update rows processed
    pub fn update_rows_processed(&self, query_id: u64, rows: u64) {
        if let Ok(mut queries) = self.queries.write() {
            if let Some(query) = queries.get_mut(&query_id) {
                query.rows_processed = rows;
            }
        }
    }

    /// Mark query as completed
    pub fn complete_query(&self, query_id: u64) {
        self.update_state(query_id, QueryState::Completed);
    }

    /// Mark query as failed
    pub fn fail_query(&self, query_id: u64) {
        self.update_state(query_id, QueryState::Failed);
    }

    /// Unregister a query (removes it from tracking)
    pub fn unregister_query(&self, query_id: u64) {
        if let Ok(mut queries) = self.queries.write() {
            queries.remove(&query_id);
        }
    }

    /// Cancel a specific query by ID
    pub fn cancel_query(&self, query_id: u64) -> Result<bool> {
        if let Ok(mut queries) = self.queries.write() {
            if let Some(query) = queries.get_mut(&query_id) {
                if !query.cancellable {
                    return Err(Error::Generic(format!(
                        "Query {} cannot be cancelled",
                        query_id
                    )));
                }

                query.cancel_token.cancel_with_reason("Cancelled by user request");
                query.state = QueryState::Cancelling;
                return Ok(true);
            }
        }
        Ok(false) // Query not found
    }

    /// Cancel a query with a specific reason
    pub fn cancel_query_with_reason(&self, query_id: u64, reason: &str) -> Result<bool> {
        if let Ok(mut queries) = self.queries.write() {
            if let Some(query) = queries.get_mut(&query_id) {
                if !query.cancellable {
                    return Err(Error::Generic(format!(
                        "Query {} cannot be cancelled",
                        query_id
                    )));
                }

                query.cancel_token.cancel_with_reason(reason);
                query.state = QueryState::Cancelling;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Cancel all queries for a specific session
    pub fn cancel_session_queries(&self, session_id: u64) -> usize {
        let mut cancelled = 0;
        if let Ok(mut queries) = self.queries.write() {
            for query in queries.values_mut() {
                if query.session_id == Some(session_id) && query.cancellable {
                    if matches!(query.state, QueryState::Planning | QueryState::Executing) {
                        query.cancel_token.cancel_with_reason("Session terminated");
                        query.state = QueryState::Cancelling;
                        cancelled += 1;
                    }
                }
            }
        }
        cancelled
    }

    /// Cancel all queries that have exceeded their timeout
    pub fn cancel_timed_out_queries(&self, timeout: Duration) -> usize {
        let mut cancelled = 0;
        if let Ok(mut queries) = self.queries.write() {
            for query in queries.values_mut() {
                if query.cancellable
                    && matches!(query.state, QueryState::Planning | QueryState::Executing)
                {
                    query.update_elapsed();
                    if query.elapsed > timeout {
                        query.cancel_token.cancel_with_reason(format!(
                            "Query timeout exceeded ({:.1}s)",
                            timeout.as_secs_f64()
                        ));
                        query.state = QueryState::Cancelling;
                        cancelled += 1;
                    }
                }
            }
        }
        cancelled
    }

    /// Get information about a specific query
    pub fn get_query(&self, query_id: u64) -> Option<RunningQuery> {
        self.queries.read().ok()?.get(&query_id).cloned()
    }

    /// List all running queries
    pub fn list_running_queries(&self) -> Vec<RunningQuery> {
        if let Ok(queries) = self.queries.read() {
            queries
                .values()
                .filter(|q| matches!(q.state, QueryState::Planning | QueryState::Executing | QueryState::Cancelling))
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// List all queries (including completed)
    pub fn list_all_queries(&self) -> Vec<RunningQuery> {
        if let Ok(queries) = self.queries.read() {
            queries.values().cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// Get count of running queries
    pub fn running_count(&self) -> usize {
        if let Ok(queries) = self.queries.read() {
            queries
                .values()
                .filter(|q| matches!(q.state, QueryState::Planning | QueryState::Executing))
                .count()
        } else {
            0
        }
    }

    /// Get count of running queries for a specific user
    pub fn user_running_count(&self, user_name: &str) -> usize {
        if let Ok(queries) = self.queries.read() {
            queries
                .values()
                .filter(|q| {
                    q.user_name == user_name
                        && matches!(q.state, QueryState::Planning | QueryState::Executing)
                })
                .count()
        } else {
            0
        }
    }

    /// Clean up old completed queries
    pub fn cleanup_completed(&self, max_age: Duration) {
        if let Ok(mut queries) = self.queries.write() {
            let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::hours(1));
            queries.retain(|_, q| {
                // Keep running queries
                if matches!(q.state, QueryState::Planning | QueryState::Executing | QueryState::Cancelling) {
                    return true;
                }
                // Keep recent completed queries
                q.started_at > cutoff
            });
        }
    }

    /// Get default timeout
    pub fn default_timeout(&self) -> Option<Duration> {
        self.default_timeout
    }
}

// =============================================================================
// QueryGuard - RAII guard for automatic query cleanup
// =============================================================================

/// RAII guard that automatically unregisters a query when dropped
pub struct QueryGuard<'a> {
    registry: &'a QueryRegistry,
    query_id: u64,
    auto_cleanup: bool,
}

impl<'a> QueryGuard<'a> {
    /// Create a new query guard
    pub fn new(registry: &'a QueryRegistry, query_id: u64) -> Self {
        Self {
            registry,
            query_id,
            auto_cleanup: true,
        }
    }

    /// Get the query ID
    pub fn query_id(&self) -> u64 {
        self.query_id
    }

    /// Disable automatic cleanup on drop
    pub fn disable_cleanup(&mut self) {
        self.auto_cleanup = false;
    }

    /// Mark the query as completed
    pub fn complete(mut self) {
        self.registry.complete_query(self.query_id);
        self.auto_cleanup = false;
    }

    /// Mark the query as failed
    pub fn fail(mut self) {
        self.registry.fail_query(self.query_id);
        self.auto_cleanup = false;
    }
}

impl Drop for QueryGuard<'_> {
    fn drop(&mut self) {
        if self.auto_cleanup {
            // If we're dropping without explicit completion, assume failure
            self.registry.fail_query(self.query_id);
        }
    }
}

// =============================================================================
// Timeout background task
// =============================================================================

/// Start a background task that cancels queries exceeding their timeout
pub fn start_timeout_checker(registry: Arc<QueryRegistry>, check_interval: Duration, timeout: Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(check_interval);
        loop {
            interval.tick().await;
            let cancelled = registry.cancel_timed_out_queries(timeout);
            if cancelled > 0 {
                tracing::info!("Cancelled {} timed out queries", cancelled);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_cancellation_token() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());

        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_with_reason() {
        let token = CancellationToken::new();
        token.cancel_with_reason("Timeout exceeded");

        assert!(token.is_cancelled());
        assert_eq!(token.cancellation_reason(), Some("Timeout exceeded".to_string()));
    }

    #[test]
    fn test_cancellation_token_check() {
        let token = CancellationToken::new();
        assert!(token.check().is_ok());

        token.cancel();
        assert!(token.check().is_err());
    }

    #[test]
    fn test_child_token() {
        let parent = CancellationToken::new();
        let child = parent.child();

        assert!(!child.is_cancelled());

        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn test_query_registry() {
        let registry = QueryRegistry::new();

        let (id1, token1) = registry.register_query("SELECT 1", "alice", "test", Some(1));
        let (id2, _token2) = registry.register_query("SELECT 2", "bob", "test", Some(2));

        assert_eq!(registry.running_count(), 2);

        // Cancel first query
        assert!(registry.cancel_query(id1).unwrap());
        assert!(token1.is_cancelled());

        // Complete second query
        registry.complete_query(id2);
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn test_cancel_session_queries() {
        let registry = QueryRegistry::new();

        registry.register_query("SELECT 1", "alice", "test", Some(1));
        registry.register_query("SELECT 2", "alice", "test", Some(1));
        registry.register_query("SELECT 3", "bob", "test", Some(2));

        let cancelled = registry.cancel_session_queries(1);
        assert_eq!(cancelled, 2);
    }

    #[test]
    fn test_list_running_queries() {
        let registry = QueryRegistry::new();

        let (id1, _) = registry.register_query("SELECT 1", "alice", "test", None);
        registry.register_query("SELECT 2", "bob", "test", None);

        registry.complete_query(id1);

        let running = registry.list_running_queries();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].user_name, "bob");
    }

    #[test]
    fn test_query_guard() {
        let registry = QueryRegistry::new();
        let (id, _) = registry.register_query("SELECT 1", "alice", "test", None);

        {
            let guard = QueryGuard::new(&registry, id);
            // Guard dropped here, query should be marked as failed
            drop(guard);
        }

        let query = registry.get_query(id).unwrap();
        assert_eq!(query.state, QueryState::Failed);
    }

    #[test]
    fn test_query_guard_complete() {
        let registry = QueryRegistry::new();
        let (id, _) = registry.register_query("SELECT 1", "alice", "test", None);

        let guard = QueryGuard::new(&registry, id);
        guard.complete();

        let query = registry.get_query(id).unwrap();
        assert_eq!(query.state, QueryState::Completed);
    }
}
