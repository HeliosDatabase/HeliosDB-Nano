//! ACID-safe write buffer system
//!
//! Provides lock-free write buffering while maintaining ACID guarantees.
//! Writes are buffered per-transaction and only become durable on commit.
//!
//! # Architecture
//!
//! ```text
//! Transaction 1     Transaction 2     Transaction N
//!      │                 │                 │
//!      ▼                 ▼                 ▼
//! ┌─────────┐       ┌─────────┐       ┌─────────┐
//! │ Buffer 1│       │ Buffer 2│       │ Buffer N│  (Per-transaction)
//! └────┬────┘       └────┬────┘       └────┬────┘
//!      │                 │                 │
//!      └─────────────────┼─────────────────┘
//!                        │
//!                        ▼
//!               ┌────────────────┐
//!               │  Commit Queue  │  (MPSC lock-free)
//!               └───────┬────────┘
//!                       │
//!                       ▼
//!               ┌────────────────┐
//!               │   WAL Writer   │  (Batched fsync)
//!               └───────┬────────┘
//!                       │
//!                       ▼
//!               ┌────────────────┐
//!               │    RocksDB     │  (WriteBatch)
//!               └────────────────┘
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::config::IngestionSafetyLevel;

/// Operation types that can be buffered
#[derive(Debug, Clone)]
pub enum WriteOp {
    /// Insert a new row
    Insert {
        table: String,
        row_id: u64,
        data: Vec<u8>,
    },
    /// Update an existing row
    Update {
        table: String,
        row_id: u64,
        data: Vec<u8>,
    },
    /// Delete a row
    Delete {
        table: String,
        row_id: u64,
    },
}

impl WriteOp {
    /// Get the table name for this operation
    pub fn table(&self) -> &str {
        match self {
            Self::Insert { table, .. } => table,
            Self::Update { table, .. } => table,
            Self::Delete { table, .. } => table,
        }
    }

    /// Get the row ID for this operation
    pub fn row_id(&self) -> u64 {
        match self {
            Self::Insert { row_id, .. } => *row_id,
            Self::Update { row_id, .. } => *row_id,
            Self::Delete { row_id, .. } => *row_id,
        }
    }

    /// Convert to storage key
    pub fn key(&self) -> Vec<u8> {
        format!("data:{}:{}", self.table(), self.row_id()).into_bytes()
    }

    /// Get data bytes (None for Delete)
    pub fn data(&self) -> Option<&[u8]> {
        match self {
            Self::Insert { data, .. } => Some(data),
            Self::Update { data, .. } => Some(data),
            Self::Delete { .. } => None,
        }
    }

    /// Estimated size in bytes
    pub fn size(&self) -> usize {
        match self {
            Self::Insert { table, data, .. } => table.len() + 8 + data.len(),
            Self::Update { table, data, .. } => table.len() + 8 + data.len(),
            Self::Delete { table, .. } => table.len() + 8,
        }
    }
}

/// Transaction write buffer
///
/// Buffers all writes for a single transaction. Writes are only visible
/// to other transactions after commit. This ensures isolation.
#[derive(Debug)]
pub struct TransactionBuffer {
    /// Transaction ID
    pub txn_id: u64,
    /// Start timestamp for MVCC reads
    pub read_timestamp: u64,
    /// Buffered operations (not yet committed)
    operations: Vec<WriteOp>,
    /// Total size of buffered data
    size: usize,
    /// Read set for conflict detection (table -> row_ids)
    read_set: HashMap<String, Vec<u64>>,
    /// Is this transaction read-only?
    read_only: bool,
}

impl TransactionBuffer {
    /// Create a new transaction buffer
    pub fn new(txn_id: u64, read_timestamp: u64) -> Self {
        Self {
            txn_id,
            read_timestamp,
            operations: Vec::with_capacity(64),
            size: 0,
            read_set: HashMap::new(),
            read_only: true,
        }
    }

    /// Buffer an insert operation
    pub fn insert(&mut self, table: String, row_id: u64, data: Vec<u8>) {
        let op = WriteOp::Insert { table, row_id, data };
        self.size += op.size();
        self.operations.push(op);
        self.read_only = false;
    }

    /// Buffer an update operation
    pub fn update(&mut self, table: String, row_id: u64, data: Vec<u8>) {
        let op = WriteOp::Update { table, row_id, data };
        self.size += op.size();
        self.operations.push(op);
        self.read_only = false;
    }

    /// Buffer a delete operation
    pub fn delete(&mut self, table: String, row_id: u64) {
        let op = WriteOp::Delete { table, row_id };
        self.size += op.size();
        self.operations.push(op);
        self.read_only = false;
    }

    /// Record a read for conflict detection
    pub fn record_read(&mut self, table: &str, row_id: u64) {
        self.read_set
            .entry(table.to_string())
            .or_insert_with(Vec::new)
            .push(row_id);
    }

    /// Get all buffered operations
    pub fn operations(&self) -> &[WriteOp] {
        &self.operations
    }

    /// Take ownership of operations (consumes buffer)
    pub fn take_operations(self) -> Vec<WriteOp> {
        self.operations
    }

    /// Add a generic write operation
    pub fn add_operation(&mut self, op: WriteOp) {
        self.size += op.size();
        self.operations.push(op);
        self.read_only = false;
    }

    /// Get the number of operations buffered
    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    /// Get total buffered size
    pub fn size(&self) -> usize {
        self.size
    }

    /// Check if transaction is read-only
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Number of operations
    pub fn len(&self) -> usize {
        self.operations.len()
    }
}

/// Commit request sent to the write coordinator
#[derive(Debug)]
pub struct CommitRequest {
    /// Transaction ID
    pub txn_id: u64,
    /// Operations to commit
    pub operations: Vec<WriteOp>,
    /// Response channel
    pub response: Option<tokio::sync::oneshot::Sender<CommitResult>>,
}

/// Result of a commit operation
#[derive(Debug, Clone)]
pub enum CommitResult {
    /// Commit succeeded
    Success {
        /// Commit timestamp
        commit_timestamp: u64,
        /// Number of operations committed
        ops_count: usize,
    },
    /// Commit failed
    Failed {
        /// Error message
        error: String,
    },
}

/// Write coordinator that batches commits for efficiency
///
/// This is the central component that ensures durability while
/// allowing lock-free write buffering.
pub struct WriteCoordinator {
    /// Commit request queue (MPSC)
    commit_sender: SyncSender<CommitRequest>,
    commit_receiver: Mutex<Option<Receiver<CommitRequest>>>,

    /// Safety level configuration
    safety_level: IngestionSafetyLevel,

    /// Next transaction ID
    next_txn_id: AtomicU64,

    /// Current timestamp for MVCC
    current_timestamp: AtomicU64,

    /// Running flag
    running: AtomicBool,

    /// Statistics
    commits_total: AtomicU64,
    ops_total: AtomicU64,
    bytes_total: AtomicU64,
}

impl WriteCoordinator {
    /// Create a new write coordinator
    pub fn new(safety_level: IngestionSafetyLevel, queue_size: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(queue_size);

        Self {
            commit_sender: sender,
            commit_receiver: Mutex::new(Some(receiver)),
            safety_level,
            next_txn_id: AtomicU64::new(1),
            current_timestamp: AtomicU64::new(1),
            running: AtomicBool::new(true),
            commits_total: AtomicU64::new(0),
            ops_total: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
        }
    }

    /// Begin a new transaction
    pub fn begin_transaction(&self) -> TransactionBuffer {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        let read_timestamp = self.current_timestamp.load(Ordering::Acquire);
        TransactionBuffer::new(txn_id, read_timestamp)
    }

    /// Submit a transaction for commit (non-blocking)
    pub fn submit_commit(&self, buffer: TransactionBuffer) -> Result<(), String> {
        if buffer.is_read_only() {
            // Read-only transactions don't need to go through commit
            return Ok(());
        }

        let request = CommitRequest {
            txn_id: buffer.txn_id,
            operations: buffer.take_operations(),
            response: None,
        };

        match self.commit_sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err("Commit queue full - backpressure".to_string())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err("Write coordinator shut down".to_string())
            }
        }
    }

    /// Submit and wait for commit result
    pub async fn commit_and_wait(&self, buffer: TransactionBuffer) -> CommitResult {
        if buffer.is_read_only() {
            return CommitResult::Success {
                commit_timestamp: buffer.read_timestamp,
                ops_count: 0,
            };
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = CommitRequest {
            txn_id: buffer.txn_id,
            operations: buffer.take_operations(),
            response: Some(tx),
        };

        if self.commit_sender.send(request).is_err() {
            return CommitResult::Failed {
                error: "Write coordinator shut down".to_string(),
            };
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => CommitResult::Failed {
                error: "Response channel closed".to_string(),
            },
        }
    }

    /// Take the commit receiver for the background worker (can only be called once)
    pub fn take_receiver(&self) -> Option<Receiver<CommitRequest>> {
        self.commit_receiver.lock().take()
    }

    /// Advance the current timestamp
    pub fn advance_timestamp(&self) -> u64 {
        self.current_timestamp.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Get current timestamp
    pub fn current_timestamp(&self) -> u64 {
        self.current_timestamp.load(Ordering::Acquire)
    }

    /// Check if running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Shutdown the coordinator
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Record commit statistics
    pub fn record_commit(&self, ops_count: usize, bytes: usize) {
        self.commits_total.fetch_add(1, Ordering::Relaxed);
        self.ops_total.fetch_add(ops_count as u64, Ordering::Relaxed);
        self.bytes_total.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Get statistics
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.commits_total.load(Ordering::Relaxed),
            self.ops_total.load(Ordering::Relaxed),
            self.bytes_total.load(Ordering::Relaxed),
        )
    }
}

/// Batched commit worker that processes commits according to safety level
pub struct BatchedCommitWorker {
    /// Commit receiver
    receiver: Receiver<CommitRequest>,

    /// Safety level
    safety_level: IngestionSafetyLevel,

    /// Pending commits for batching
    pending: Vec<CommitRequest>,

    /// Total bytes in pending batch
    pending_bytes: usize,

    /// Last flush time
    last_flush: Instant,

    /// Coordinator reference for stats
    coordinator: Arc<WriteCoordinator>,
}

impl BatchedCommitWorker {
    /// Create a new worker
    ///
    /// Panics if the receiver has already been taken by another worker.
    pub fn new(coordinator: Arc<WriteCoordinator>) -> Self {
        #[allow(clippy::expect_used)]
        let receiver = coordinator
            .take_receiver()
            .expect("Receiver already taken by another worker");
        Self {
            receiver,
            safety_level: coordinator.safety_level.clone(),
            pending: Vec::with_capacity(1024),
            pending_bytes: 0,
            last_flush: Instant::now(),
            coordinator,
        }
    }

    /// Should we flush the current batch?
    fn should_flush(&self) -> bool {
        if self.pending.is_empty() {
            return false;
        }

        match &self.safety_level {
            IngestionSafetyLevel::Full => true, // Always flush immediately

            IngestionSafetyLevel::Batched { batch_size, batch_timeout_ms } => {
                self.pending.len() >= *batch_size
                    || self.last_flush.elapsed() >= Duration::from_millis(*batch_timeout_ms)
            }

            IngestionSafetyLevel::Async { sync_interval_ms } => {
                self.last_flush.elapsed() >= Duration::from_millis(*sync_interval_ms)
            }

            IngestionSafetyLevel::Unsafe { .. } => {
                // Flush when buffer gets large enough
                self.pending_bytes >= 1024 * 1024 // 1MB
            }
        }
    }

    /// Process commits (call this in a loop)
    pub fn process_batch<F>(&mut self, flush_fn: F) -> Result<usize, String>
    where
        F: FnOnce(&[CommitRequest], bool) -> Result<u64, String>,
    {
        // Receive new commits (with timeout for batching)
        let timeout = match &self.safety_level {
            IngestionSafetyLevel::Full => Duration::from_millis(0),
            IngestionSafetyLevel::Batched { batch_timeout_ms, .. } => {
                Duration::from_millis(*batch_timeout_ms)
            }
            IngestionSafetyLevel::Async { sync_interval_ms } => {
                Duration::from_millis(*sync_interval_ms)
            }
            IngestionSafetyLevel::Unsafe { .. } => Duration::from_millis(100),
        };

        // Try to receive commits
        match self.receiver.recv_timeout(timeout) {
            Ok(request) => {
                self.pending_bytes += request.operations.iter().map(|op| op.size()).sum::<usize>();
                self.pending.push(request);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Coordinator disconnected".to_string());
            }
        }

        // Drain any additional pending commits (non-blocking)
        while let Ok(request) = self.receiver.try_recv() {
            self.pending_bytes += request.operations.iter().map(|op| op.size()).sum::<usize>();
            self.pending.push(request);

            // Don't accumulate too many
            if self.pending.len() >= 10000 {
                break;
            }
        }

        // Check if we should flush
        if self.should_flush() {
            let sync = self.safety_level.sync_on_commit();
            let commit_ts = flush_fn(&self.pending, sync)?;

            // Send responses
            let count = self.pending.len();
            for request in self.pending.drain(..) {
                if let Some(response) = request.response {
                    let _ = response.send(CommitResult::Success {
                        commit_timestamp: commit_ts,
                        ops_count: request.operations.len(),
                    });
                }
                self.coordinator.record_commit(
                    request.operations.len(),
                    request.operations.iter().map(|op| op.size()).sum(),
                );
            }

            self.pending_bytes = 0;
            self.last_flush = Instant::now();
            return Ok(count);
        }

        Ok(0)
    }
}

/// Lock-free write buffer pool for high-throughput scenarios
///
/// Pre-allocates buffers to avoid allocation overhead during ingestion.
pub struct WriteBufferPool {
    /// Available buffers
    available: crossbeam::queue::ArrayQueue<TransactionBuffer>,
    /// Pool size
    pool_size: usize,
    /// Next transaction ID
    next_txn_id: AtomicU64,
    /// Current read timestamp
    current_timestamp: AtomicU64,
}

impl WriteBufferPool {
    /// Create a new buffer pool
    pub fn new(pool_size: usize) -> Self {
        let queue = crossbeam::queue::ArrayQueue::new(pool_size);

        // Pre-allocate buffers
        for i in 0..pool_size {
            let buffer = TransactionBuffer::new(0, 0);
            let _ = queue.push(buffer);
        }

        Self {
            available: queue,
            pool_size,
            next_txn_id: AtomicU64::new(1),
            current_timestamp: AtomicU64::new(1),
        }
    }

    /// Acquire a buffer from the pool
    pub fn acquire(&self) -> TransactionBuffer {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        let read_ts = self.current_timestamp.load(Ordering::Acquire);

        // Try to get from pool
        if let Some(mut buffer) = self.available.pop() {
            buffer.txn_id = txn_id;
            buffer.read_timestamp = read_ts;
            buffer.operations.clear();
            buffer.size = 0;
            buffer.read_set.clear();
            buffer.read_only = true;
            return buffer;
        }

        // Pool exhausted - allocate new
        TransactionBuffer::new(txn_id, read_ts)
    }

    /// Return a buffer to the pool
    pub fn release(&self, buffer: TransactionBuffer) {
        // Only return if pool isn't full
        let _ = self.available.push(buffer);
    }

    /// Advance timestamp
    pub fn advance_timestamp(&self) -> u64 {
        self.current_timestamp.fetch_add(1, Ordering::AcqRel) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_buffer() {
        let mut buffer = TransactionBuffer::new(1, 100);
        assert!(buffer.is_read_only());
        assert!(buffer.is_empty());

        buffer.insert("test".to_string(), 1, vec![1, 2, 3]);
        assert!(!buffer.is_read_only());
        assert_eq!(buffer.len(), 1);

        buffer.update("test".to_string(), 1, vec![4, 5, 6]);
        assert_eq!(buffer.len(), 2);

        buffer.delete("test".to_string(), 2);
        assert_eq!(buffer.len(), 3);
    }

    #[test]
    fn test_write_coordinator() {
        let coord = WriteCoordinator::new(IngestionSafetyLevel::Full, 1000);

        let buffer1 = coord.begin_transaction();
        let buffer2 = coord.begin_transaction();

        assert_ne!(buffer1.txn_id, buffer2.txn_id);
        assert!(buffer2.txn_id > buffer1.txn_id);
    }

    #[test]
    fn test_buffer_pool() {
        let pool = WriteBufferPool::new(10);

        let b1 = pool.acquire();
        let b2 = pool.acquire();

        assert_ne!(b1.txn_id, b2.txn_id);

        pool.release(b1);
        let b3 = pool.acquire();

        // b3 should reuse b1's buffer
        assert!(b3.is_empty());
    }
}
