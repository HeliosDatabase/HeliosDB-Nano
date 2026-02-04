//! Switchover Buffer - Query buffering during controlled switchover
//!
//! Buffers write queries during the brief switchover window to ensure
//! zero transaction loss. Queries are replayed to the new primary
//! once switchover completes.
//!
//! ## How it works
//!
//! ```text
//! Normal Operation:
//!   Client → Proxy → Primary
//!
//! During Switchover:
//!   Client → Proxy → Buffer (queued)
//!                      ↓
//!   [Switchover completes]
//!                      ↓
//!            Buffer → New Primary (replayed)
//! ```
//!
//! ## Timeout Behavior
//!
//! If switchover takes longer than `buffer_timeout`, buffered queries
//! will fail with a timeout error rather than blocking indefinitely.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tokio::sync::{broadcast, oneshot, Semaphore};

use super::{Result, ProxyError};

/// Buffer configuration
#[derive(Debug, Clone)]
pub struct BufferConfig {
    /// Maximum time to buffer queries (default: 5s)
    pub buffer_timeout: Duration,
    /// Maximum number of queries to buffer (default: 10000)
    pub max_buffered_queries: usize,
    /// Maximum memory for buffered queries (default: 100MB)
    pub max_buffer_memory: usize,
    /// Whether to allow new queries while draining buffer
    pub allow_queries_during_drain: bool,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            buffer_timeout: Duration::from_secs(5),
            max_buffered_queries: 10000,
            max_buffer_memory: 100 * 1024 * 1024, // 100MB
            allow_queries_during_drain: true,
        }
    }
}

/// A buffered query waiting to be executed
#[derive(Debug)]
pub struct BufferedQuery {
    /// SQL statement
    pub sql: String,
    /// Query parameters
    pub params: Vec<Vec<u8>>,
    /// Time when query was buffered
    pub buffered_at: Instant,
    /// Channel to send result back to client
    pub response_tx: oneshot::Sender<BufferResult>,
    /// Client identifier (for logging/debugging)
    pub client_id: u64,
}

/// Result of a buffered query after replay
#[derive(Debug)]
pub enum BufferResult {
    /// Query executed successfully
    Success,
    /// Query failed with error
    Error(String),
    /// Query timed out while buffered
    Timeout,
    /// Switchover was cancelled/failed
    SwitchoverFailed,
}

/// Buffer state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferState {
    /// Normal operation - no buffering
    Passthrough,
    /// Buffering writes (during switchover)
    Buffering,
    /// Draining buffer to new primary
    Draining,
}

/// Switchover buffer for zero-downtime primary transitions
pub struct SwitchoverBuffer {
    /// Configuration
    config: BufferConfig,
    /// Current state
    state: AtomicU64, // BufferState as u64
    /// Is buffering active
    is_buffering: AtomicBool,
    /// Buffered queries
    buffer: Mutex<VecDeque<BufferedQuery>>,
    /// Current buffer memory usage
    buffer_memory: AtomicU64,
    /// Time when buffering started
    buffering_started: Mutex<Option<Instant>>,
    /// Statistics
    stats: BufferStats,
    /// State change broadcaster
    state_tx: broadcast::Sender<BufferState>,
    /// Semaphore to limit concurrent buffer access
    buffer_semaphore: Semaphore,
}

impl SwitchoverBuffer {
    /// Create a new switchover buffer
    pub fn new(config: BufferConfig) -> Self {
        let (state_tx, _) = broadcast::channel(16);

        Self {
            buffer_semaphore: Semaphore::new(config.max_buffered_queries),
            config,
            state: AtomicU64::new(BufferState::Passthrough as u64),
            is_buffering: AtomicBool::new(false),
            buffer: Mutex::new(VecDeque::new()),
            buffer_memory: AtomicU64::new(0),
            buffering_started: Mutex::new(None),
            stats: BufferStats::default(),
            state_tx,
        }
    }

    /// Check if currently buffering
    pub fn is_buffering(&self) -> bool {
        self.is_buffering.load(Ordering::SeqCst)
    }

    /// Get current state
    pub fn state(&self) -> BufferState {
        match self.state.load(Ordering::SeqCst) {
            0 => BufferState::Passthrough,
            1 => BufferState::Buffering,
            2 => BufferState::Draining,
            _ => BufferState::Passthrough,
        }
    }

    /// Subscribe to state changes
    pub fn subscribe(&self) -> broadcast::Receiver<BufferState> {
        self.state_tx.subscribe()
    }

    /// Start buffering (called when switchover begins)
    pub fn start_buffering(&self) {
        self.is_buffering.store(true, Ordering::SeqCst);
        self.state.store(BufferState::Buffering as u64, Ordering::SeqCst);
        *self.buffering_started.lock() = Some(Instant::now());

        self.stats.buffering_sessions.fetch_add(1, Ordering::Relaxed);

        let _ = self.state_tx.send(BufferState::Buffering);

        tracing::info!("Switchover buffer: started buffering");
    }

    /// Stop buffering (called when switchover completes or fails)
    pub fn stop_buffering(&self) {
        self.is_buffering.store(false, Ordering::SeqCst);
        self.state.store(BufferState::Draining as u64, Ordering::SeqCst);

        let duration = self.buffering_started.lock()
            .map(|start| start.elapsed())
            .unwrap_or_default();

        let _ = self.state_tx.send(BufferState::Draining);

        tracing::info!(
            "Switchover buffer: stopped buffering after {:?}, {} queries buffered",
            duration,
            self.buffer.lock().len()
        );
    }

    /// Buffer a query (returns receiver for result)
    pub fn buffer_query(
        &self,
        sql: String,
        params: Vec<Vec<u8>>,
        client_id: u64,
    ) -> Result<oneshot::Receiver<BufferResult>> {
        // Check if we should buffer
        if !self.is_buffering() {
            return Err(ProxyError::Internal("Not in buffering mode".to_string()));
        }

        // Check timeout
        if let Some(started) = *self.buffering_started.lock() {
            if started.elapsed() > self.config.buffer_timeout {
                return Err(ProxyError::Timeout("Buffer timeout exceeded".to_string()));
            }
        }

        // Check capacity
        let buffer_len = self.buffer.lock().len();
        if buffer_len >= self.config.max_buffered_queries {
            self.stats.rejected_queries.fetch_add(1, Ordering::Relaxed);
            return Err(ProxyError::PoolExhausted("Buffer full".to_string()));
        }

        // Check memory
        let query_size = sql.len() + params.iter().map(|p| p.len()).sum::<usize>();
        let current_memory = self.buffer_memory.load(Ordering::Relaxed) as usize;
        if current_memory + query_size > self.config.max_buffer_memory {
            self.stats.rejected_queries.fetch_add(1, Ordering::Relaxed);
            return Err(ProxyError::PoolExhausted("Buffer memory exhausted".to_string()));
        }

        // Create response channel
        let (response_tx, response_rx) = oneshot::channel();

        // Create buffered query
        let buffered = BufferedQuery {
            sql,
            params,
            buffered_at: Instant::now(),
            response_tx,
            client_id,
        };

        // Add to buffer
        self.buffer.lock().push_back(buffered);
        self.buffer_memory.fetch_add(query_size as u64, Ordering::Relaxed);
        self.stats.buffered_queries.fetch_add(1, Ordering::Relaxed);

        Ok(response_rx)
    }

    /// Drain buffer and replay queries to new primary
    ///
    /// The `execute_fn` is called for each buffered query to execute it
    /// against the new primary.
    pub async fn drain<F, Fut>(&self, execute_fn: F)
    where
        F: Fn(String, Vec<Vec<u8>>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        tracing::info!("Switchover buffer: draining buffer");

        let queries: Vec<BufferedQuery> = {
            let mut buffer = self.buffer.lock();
            buffer.drain(..).collect()
        };

        self.buffer_memory.store(0, Ordering::Relaxed);

        let total = queries.len();
        let mut success = 0;
        let mut failed = 0;
        let mut timed_out = 0;

        for query in queries {
            // Check if query timed out while buffered
            if query.buffered_at.elapsed() > self.config.buffer_timeout {
                let _ = query.response_tx.send(BufferResult::Timeout);
                timed_out += 1;
                continue;
            }

            // Execute query
            match execute_fn(query.sql, query.params).await {
                Ok(()) => {
                    let _ = query.response_tx.send(BufferResult::Success);
                    success += 1;
                }
                Err(e) => {
                    let _ = query.response_tx.send(BufferResult::Error(e.to_string()));
                    failed += 1;
                }
            }
        }

        self.stats.replayed_queries.fetch_add(success, Ordering::Relaxed);
        self.stats.failed_replays.fetch_add(failed, Ordering::Relaxed);
        self.stats.timed_out_queries.fetch_add(timed_out, Ordering::Relaxed);

        // Return to passthrough mode
        self.state.store(BufferState::Passthrough as u64, Ordering::SeqCst);
        let _ = self.state_tx.send(BufferState::Passthrough);

        tracing::info!(
            "Switchover buffer: drained {} queries (success: {}, failed: {}, timeout: {})",
            total,
            success,
            failed,
            timed_out
        );
    }

    /// Fail all buffered queries (called if switchover fails)
    pub fn fail_all(&self, error: &str) {
        let queries: Vec<BufferedQuery> = {
            let mut buffer = self.buffer.lock();
            buffer.drain(..).collect()
        };

        self.buffer_memory.store(0, Ordering::Relaxed);

        for query in queries {
            let _ = query.response_tx.send(BufferResult::SwitchoverFailed);
        }

        self.stats.failed_replays.fetch_add(queries.len() as u64, Ordering::Relaxed);

        // Return to passthrough mode
        self.state.store(BufferState::Passthrough as u64, Ordering::SeqCst);
        let _ = self.state_tx.send(BufferState::Passthrough);

        tracing::warn!(
            "Switchover buffer: failed {} queries due to: {}",
            queries.len(),
            error
        );
    }

    /// Get current buffer length
    pub fn len(&self) -> usize {
        self.buffer.lock().len()
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.buffer.lock().is_empty()
    }

    /// Get buffer statistics
    pub fn stats(&self) -> BufferStatsSnapshot {
        BufferStatsSnapshot {
            buffering_sessions: self.stats.buffering_sessions.load(Ordering::Relaxed),
            buffered_queries: self.stats.buffered_queries.load(Ordering::Relaxed),
            replayed_queries: self.stats.replayed_queries.load(Ordering::Relaxed),
            failed_replays: self.stats.failed_replays.load(Ordering::Relaxed),
            timed_out_queries: self.stats.timed_out_queries.load(Ordering::Relaxed),
            rejected_queries: self.stats.rejected_queries.load(Ordering::Relaxed),
            current_buffer_size: self.buffer.lock().len(),
            current_memory_usage: self.buffer_memory.load(Ordering::Relaxed) as usize,
        }
    }
}

/// Internal statistics (atomic counters)
#[derive(Default)]
struct BufferStats {
    buffering_sessions: AtomicU64,
    buffered_queries: AtomicU64,
    replayed_queries: AtomicU64,
    failed_replays: AtomicU64,
    timed_out_queries: AtomicU64,
    rejected_queries: AtomicU64,
}

/// Statistics snapshot
#[derive(Debug, Clone)]
pub struct BufferStatsSnapshot {
    pub buffering_sessions: u64,
    pub buffered_queries: u64,
    pub replayed_queries: u64,
    pub failed_replays: u64,
    pub timed_out_queries: u64,
    pub rejected_queries: u64,
    pub current_buffer_size: usize,
    pub current_memory_usage: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_state_transitions() {
        let buffer = SwitchoverBuffer::new(BufferConfig::default());

        assert_eq!(buffer.state(), BufferState::Passthrough);
        assert!(!buffer.is_buffering());

        buffer.start_buffering();
        assert_eq!(buffer.state(), BufferState::Buffering);
        assert!(buffer.is_buffering());

        buffer.stop_buffering();
        assert_eq!(buffer.state(), BufferState::Draining);
        assert!(!buffer.is_buffering());
    }

    #[tokio::test]
    async fn test_buffer_query() {
        let buffer = SwitchoverBuffer::new(BufferConfig::default());

        // Can't buffer when not in buffering mode
        let result = buffer.buffer_query("SELECT 1".to_string(), vec![], 1);
        assert!(result.is_err());

        // Start buffering
        buffer.start_buffering();

        // Now can buffer
        let rx = buffer.buffer_query("INSERT INTO t VALUES (1)".to_string(), vec![], 1).unwrap();
        assert_eq!(buffer.len(), 1);

        // Drain buffer
        buffer.drain(|_sql, _params| async { Ok(()) }).await;

        // Check result
        let result = rx.await.unwrap();
        assert!(matches!(result, BufferResult::Success));
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_buffer_limits() {
        let config = BufferConfig {
            max_buffered_queries: 2,
            ..Default::default()
        };
        let buffer = SwitchoverBuffer::new(config);
        buffer.start_buffering();

        // Buffer up to limit
        let _ = buffer.buffer_query("Q1".to_string(), vec![], 1).unwrap();
        let _ = buffer.buffer_query("Q2".to_string(), vec![], 2).unwrap();

        // Third should fail
        let result = buffer.buffer_query("Q3".to_string(), vec![], 3);
        assert!(result.is_err());
    }
}
