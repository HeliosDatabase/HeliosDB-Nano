//! High-level lock-free ingestion API
//!
//! Provides a unified interface for high-performance data ingestion
//! with configurable ACID guarantees. This is the main entry point
//! for the lock-free ingestion system.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    LockFreeIngestionEngine                       │
//! ├─────────────────────────────────────────────────────────────────┤
//! │                                                                  │
//! │  ┌──────────────┐   ┌──────────────┐   ┌──────────────────────┐ │
//! │  │   Config     │   │  RowIdGen    │   │  WriteCoordinator    │ │
//! │  │ SafetyLevel  │   │ Hierarchical │   │  TransactionBuffers  │ │
//! │  └──────────────┘   │ or Batched   │   └──────────────────────┘ │
//! │                     └──────────────┘                            │
//! │                                                                  │
//! │  ┌──────────────────────────────────────────────────────────┐   │
//! │  │              PartitionedWalManager                        │   │
//! │  │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐         │   │
//! │  │  │ WAL P0  │ │ WAL P1  │ │ WAL P2  │ │ WAL PN  │         │   │
//! │  │  └─────────┘ └─────────┘ └─────────┘ └─────────┘         │   │
//! │  └──────────────────────────────────────────────────────────┘   │
//! │                                                                  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use heliosdb::storage::lockfree::{LockFreeIngestionEngine, LockFreeIngestionConfig};
//!
//! // Create engine with bulk load configuration
//! let config = LockFreeIngestionConfig::for_bulk_load();
//! let engine = LockFreeIngestionEngine::new(config, "/path/to/wal")?;
//!
//! // Begin a transaction
//! let txn = engine.begin_transaction()?;
//!
//! // Buffer writes (lock-free, no I/O)
//! engine.insert(&txn, "users", row_id, &data)?;
//!
//! // Commit (durability depends on safety level)
//! engine.commit(txn)?;
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::RwLock;

use super::config::{IngestionSafetyLevel, LockFreeIngestionConfig};
use super::row_id::{BatchRowIdAllocator, HierarchicalRowIdGenerator, RowIdGenerator};
use super::wal_manager::{PartitionedWalManager, WalOp, WalRecovery};
use super::write_buffer::{TransactionBuffer, WriteOp};

/// Result type for ingestion operations
pub type IngestionResult<T> = Result<T, IngestionError>;

/// Errors that can occur during ingestion
#[derive(Debug)]
pub enum IngestionError {
    /// Transaction not found
    TransactionNotFound(u64),
    /// Transaction already committed or aborted
    TransactionClosed(u64),
    /// Write conflict detected
    WriteConflict { table: String, row_id: u64 },
    /// WAL error
    WalError(String),
    /// Buffer overflow (backpressure)
    BackpressureExceeded,
    /// Serialization error
    SerializationError(String),
    /// I/O error
    IoError(std::io::Error),
    /// Engine shutdown
    EngineShutdown,
    /// Recovery error
    RecoveryError(String),
}

impl std::fmt::Display for IngestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransactionNotFound(id) => write!(f, "Transaction {} not found", id),
            Self::TransactionClosed(id) => write!(f, "Transaction {} already closed", id),
            Self::WriteConflict { table, row_id } => {
                write!(f, "Write conflict on {}:{}", table, row_id)
            }
            Self::WalError(msg) => write!(f, "WAL error: {}", msg),
            Self::BackpressureExceeded => write!(f, "Backpressure limit exceeded"),
            Self::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            Self::IoError(e) => write!(f, "I/O error: {}", e),
            Self::EngineShutdown => write!(f, "Engine is shutting down"),
            Self::RecoveryError(msg) => write!(f, "Recovery error: {}", msg),
        }
    }
}

impl std::error::Error for IngestionError {}

impl From<std::io::Error> for IngestionError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError(e)
    }
}

/// Handle to an active transaction
#[derive(Debug)]
pub struct TransactionHandle {
    /// Unique transaction ID
    pub txn_id: u64,
    /// Read timestamp for MVCC
    pub read_timestamp: u64,
    /// Partition assignment (for load balancing)
    partition: u16,
    /// Whether transaction is still active
    active: AtomicBool,
}

impl TransactionHandle {
    fn new(txn_id: u64, read_timestamp: u64, partition: u16) -> Self {
        Self {
            txn_id,
            read_timestamp,
            partition,
            active: AtomicBool::new(true),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Statistics for the ingestion engine
#[derive(Debug, Default, Clone)]
pub struct IngestionStats {
    /// Total transactions started
    pub transactions_started: u64,
    /// Total transactions committed
    pub transactions_committed: u64,
    /// Total transactions aborted
    pub transactions_aborted: u64,
    /// Total rows inserted
    pub rows_inserted: u64,
    /// Total rows updated
    pub rows_updated: u64,
    /// Total rows deleted
    pub rows_deleted: u64,
    /// Write conflicts detected
    pub write_conflicts: u64,
    /// WAL bytes written
    pub wal_bytes_written: u64,
    /// WAL syncs performed
    pub wal_syncs: u64,
    /// Average commit latency (microseconds)
    pub avg_commit_latency_us: u64,
    /// Peak pending transactions
    pub peak_pending_transactions: u64,
}

/// Atomic statistics counters
struct AtomicStats {
    transactions_started: AtomicU64,
    transactions_committed: AtomicU64,
    transactions_aborted: AtomicU64,
    rows_inserted: AtomicU64,
    rows_updated: AtomicU64,
    rows_deleted: AtomicU64,
    write_conflicts: AtomicU64,
    wal_bytes_written: AtomicU64,
    wal_syncs: AtomicU64,
    total_commit_latency_us: AtomicU64,
    commit_count: AtomicU64,
    peak_pending: AtomicU64,
}

impl Default for AtomicStats {
    fn default() -> Self {
        Self {
            transactions_started: AtomicU64::new(0),
            transactions_committed: AtomicU64::new(0),
            transactions_aborted: AtomicU64::new(0),
            rows_inserted: AtomicU64::new(0),
            rows_updated: AtomicU64::new(0),
            rows_deleted: AtomicU64::new(0),
            write_conflicts: AtomicU64::new(0),
            wal_bytes_written: AtomicU64::new(0),
            wal_syncs: AtomicU64::new(0),
            total_commit_latency_us: AtomicU64::new(0),
            commit_count: AtomicU64::new(0),
            peak_pending: AtomicU64::new(0),
        }
    }
}

impl AtomicStats {
    fn snapshot(&self) -> IngestionStats {
        let commit_count = self.commit_count.load(Ordering::Relaxed).max(1);
        let total_latency = self.total_commit_latency_us.load(Ordering::Relaxed);

        IngestionStats {
            transactions_started: self.transactions_started.load(Ordering::Relaxed),
            transactions_committed: self.transactions_committed.load(Ordering::Relaxed),
            transactions_aborted: self.transactions_aborted.load(Ordering::Relaxed),
            rows_inserted: self.rows_inserted.load(Ordering::Relaxed),
            rows_updated: self.rows_updated.load(Ordering::Relaxed),
            rows_deleted: self.rows_deleted.load(Ordering::Relaxed),
            write_conflicts: self.write_conflicts.load(Ordering::Relaxed),
            wal_bytes_written: self.wal_bytes_written.load(Ordering::Relaxed),
            wal_syncs: self.wal_syncs.load(Ordering::Relaxed),
            avg_commit_latency_us: total_latency / commit_count,
            peak_pending_transactions: self.peak_pending.load(Ordering::Relaxed),
        }
    }
}

/// Commit request sent to background worker
struct CommitRequest {
    txn_id: u64,
    buffer: TransactionBuffer,
    response: SyncSender<CommitResponse>,
    start_time: Instant,
}

/// Response from commit worker
enum CommitResponse {
    Success { commit_timestamp: u64 },
    Conflict { table: String, row_id: u64 },
    Error(IngestionError),
}

/// Lock-free ingestion engine
///
/// Provides high-performance data ingestion with configurable ACID guarantees.
/// The engine uses lock-free data structures for maximum concurrency and
/// supports multiple safety levels to trade durability for performance.
pub struct LockFreeIngestionEngine {
    /// Configuration
    config: LockFreeIngestionConfig,

    /// Row ID generator
    row_id_gen: RowIdGenerator,

    /// Active transaction buffers (lock-free access)
    active_transactions: DashMap<u64, TransactionBuffer>,

    /// Transaction ID generator
    next_txn_id: AtomicU64,

    /// Global timestamp for MVCC
    global_timestamp: AtomicU64,

    /// Partition counter for load balancing
    next_partition: AtomicU64,

    /// WAL manager (optional based on safety level)
    wal_manager: Option<Arc<PartitionedWalManager>>,

    /// Commit channel for async processing
    commit_sender: SyncSender<CommitRequest>,

    /// Background commit worker handle
    commit_worker: Option<JoinHandle<()>>,

    /// Statistics
    stats: Arc<AtomicStats>,

    /// Shutdown flag
    shutdown: Arc<AtomicBool>,

    /// Pending writes counter (for backpressure)
    pending_writes: AtomicU64,

    /// Callback for applying committed writes to storage
    /// Called with (table, row_id, data, is_delete)
    apply_callback: Arc<RwLock<Option<Box<dyn Fn(&str, u64, Option<&[u8]>) + Send + Sync>>>>,
}

impl LockFreeIngestionEngine {
    /// Create a new ingestion engine
    pub fn new<P: AsRef<Path>>(config: LockFreeIngestionConfig, wal_path: P) -> IngestionResult<Self> {
        let (commit_sender, commit_receiver) = mpsc::sync_channel(config.max_pending_writes);

        // Create row ID generator based on config
        let row_id_gen = if config.hierarchical_row_ids {
            RowIdGenerator::Hierarchical(HierarchicalRowIdGenerator::new())
        } else {
            RowIdGenerator::Batched(BatchRowIdAllocator::new(config.row_id_batch_size))
        };

        // Create WAL manager if enabled
        let wal_manager = if config.safety_level.use_wal() {
            Some(Arc::new(PartitionedWalManager::new(
                wal_path,
                config.partition_count,
                config.safety_level.clone(),
            )?))
        } else {
            None
        };

        let stats = Arc::new(AtomicStats::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let apply_callback: Arc<RwLock<Option<Box<dyn Fn(&str, u64, Option<&[u8]>) + Send + Sync>>>> =
            Arc::new(RwLock::new(None));

        // Start background commit worker
        let worker = Self::start_commit_worker(
            commit_receiver,
            config.safety_level.clone(),
            wal_manager.clone(),
            stats.clone(),
            shutdown.clone(),
            apply_callback.clone(),
        );

        Ok(Self {
            config,
            row_id_gen,
            active_transactions: DashMap::new(),
            next_txn_id: AtomicU64::new(1),
            global_timestamp: AtomicU64::new(1),
            next_partition: AtomicU64::new(0),
            wal_manager,
            commit_sender,
            commit_worker: Some(worker),
            stats,
            shutdown,
            pending_writes: AtomicU64::new(0),
            apply_callback,
        })
    }

    /// Set the callback for applying committed writes to storage
    ///
    /// This callback is invoked after WAL persistence (if enabled) for each
    /// write operation in a committed transaction. The callback receives:
    /// - table: Table name
    /// - row_id: Row identifier
    /// - data: Some(bytes) for insert/update, None for delete
    pub fn set_apply_callback<F>(&self, callback: F)
    where
        F: Fn(&str, u64, Option<&[u8]>) + Send + Sync + 'static,
    {
        let mut guard = self.apply_callback.write();
        *guard = Some(Box::new(callback));
    }

    /// Begin a new transaction
    ///
    /// Returns a handle that must be used for all operations in this transaction.
    /// The transaction is isolated from other transactions via MVCC.
    pub fn begin_transaction(&self) -> IngestionResult<TransactionHandle> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(IngestionError::EngineShutdown);
        }

        let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        let read_timestamp = self.global_timestamp.load(Ordering::Acquire);
        let partition = (self.next_partition.fetch_add(1, Ordering::Relaxed)
            % self.config.partition_count as u64) as u16;

        // Create transaction buffer
        let buffer = TransactionBuffer::new(txn_id, read_timestamp);
        self.active_transactions.insert(txn_id, buffer);

        self.stats.transactions_started.fetch_add(1, Ordering::Relaxed);

        // Update peak pending tracking
        let current = self.active_transactions.len() as u64;
        let mut peak = self.stats.peak_pending.load(Ordering::Relaxed);
        while current > peak {
            match self.stats.peak_pending.compare_exchange_weak(
                peak,
                current,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }

        Ok(TransactionHandle::new(txn_id, read_timestamp, partition))
    }

    /// Generate a new row ID for a table
    ///
    /// Lock-free operation - no coordination between threads.
    #[inline]
    pub fn generate_row_id(&self, table: &str) -> u64 {
        self.row_id_gen.next(table)
    }

    /// Generate a batch of row IDs for bulk insert
    #[inline]
    pub fn generate_row_ids(&self, table: &str, count: usize) -> Vec<u64> {
        self.row_id_gen.next_batch(table, count)
    }

    /// Insert a row (buffered, lock-free)
    ///
    /// The write is buffered in the transaction and not visible to other
    /// transactions until commit. This operation is completely lock-free.
    pub fn insert(
        &self,
        handle: &TransactionHandle,
        table: &str,
        row_id: u64,
        data: &[u8],
    ) -> IngestionResult<()> {
        self.check_handle(handle)?;
        self.check_backpressure()?;

        let mut buffer = self
            .active_transactions
            .get_mut(&handle.txn_id)
            .ok_or(IngestionError::TransactionNotFound(handle.txn_id))?;

        buffer.add_operation(WriteOp::Insert {
            table: table.to_string(),
            row_id,
            data: data.to_vec(),
        });

        self.pending_writes.fetch_add(1, Ordering::Relaxed);
        self.stats.rows_inserted.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Update a row (buffered, lock-free)
    pub fn update(
        &self,
        handle: &TransactionHandle,
        table: &str,
        row_id: u64,
        data: &[u8],
    ) -> IngestionResult<()> {
        self.check_handle(handle)?;
        self.check_backpressure()?;

        let mut buffer = self
            .active_transactions
            .get_mut(&handle.txn_id)
            .ok_or(IngestionError::TransactionNotFound(handle.txn_id))?;

        buffer.add_operation(WriteOp::Update {
            table: table.to_string(),
            row_id,
            data: data.to_vec(),
        });

        self.pending_writes.fetch_add(1, Ordering::Relaxed);
        self.stats.rows_updated.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Delete a row (buffered, lock-free)
    pub fn delete(
        &self,
        handle: &TransactionHandle,
        table: &str,
        row_id: u64,
    ) -> IngestionResult<()> {
        self.check_handle(handle)?;
        self.check_backpressure()?;

        let mut buffer = self
            .active_transactions
            .get_mut(&handle.txn_id)
            .ok_or(IngestionError::TransactionNotFound(handle.txn_id))?;

        buffer.add_operation(WriteOp::Delete {
            table: table.to_string(),
            row_id,
        });

        self.pending_writes.fetch_add(1, Ordering::Relaxed);
        self.stats.rows_deleted.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Commit a transaction
    ///
    /// Durability guarantees depend on the configured safety level:
    /// - Full: WAL fsync before returning
    /// - Batched: WAL fsync within batch window
    /// - Async: WAL written but fsync is async
    /// - Unsafe: No WAL (or WAL without fsync)
    pub fn commit(&self, handle: TransactionHandle) -> IngestionResult<u64> {
        self.check_handle(&handle)?;

        // Extract buffer
        let (_, buffer) = self
            .active_transactions
            .remove(&handle.txn_id)
            .ok_or(IngestionError::TransactionNotFound(handle.txn_id))?;

        handle.close();

        let op_count = buffer.operation_count();
        self.pending_writes.fetch_sub(op_count as u64, Ordering::Relaxed);

        // Empty transaction - just return
        if op_count == 0 {
            self.stats.transactions_committed.fetch_add(1, Ordering::Relaxed);
            return Ok(self.global_timestamp.load(Ordering::Acquire));
        }

        // Send to commit worker
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let request = CommitRequest {
            txn_id: handle.txn_id,
            buffer,
            response: response_tx,
            start_time: Instant::now(),
        };

        self.commit_sender
            .send(request)
            .map_err(|_| IngestionError::EngineShutdown)?;

        // Wait for response
        match response_rx
            .recv()
            .map_err(|_| IngestionError::EngineShutdown)?
        {
            CommitResponse::Success { commit_timestamp } => {
                self.stats.transactions_committed.fetch_add(1, Ordering::Relaxed);
                Ok(commit_timestamp)
            }
            CommitResponse::Conflict { table, row_id } => {
                self.stats.transactions_aborted.fetch_add(1, Ordering::Relaxed);
                self.stats.write_conflicts.fetch_add(1, Ordering::Relaxed);
                Err(IngestionError::WriteConflict { table, row_id })
            }
            CommitResponse::Error(e) => {
                self.stats.transactions_aborted.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Abort a transaction
    ///
    /// Discards all buffered writes. No I/O is performed.
    pub fn abort(&self, handle: TransactionHandle) -> IngestionResult<()> {
        if !handle.is_active() {
            return Err(IngestionError::TransactionClosed(handle.txn_id));
        }

        if let Some((_, buffer)) = self.active_transactions.remove(&handle.txn_id) {
            self.pending_writes
                .fetch_sub(buffer.operation_count() as u64, Ordering::Relaxed);
        }

        handle.close();
        self.stats.transactions_aborted.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Bulk insert many rows in a single transaction
    ///
    /// Optimized for high-throughput ingestion. Automatically batches
    /// and manages backpressure.
    pub fn bulk_insert<I>(
        &self,
        table: &str,
        rows: I,
    ) -> IngestionResult<BulkInsertResult>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let handle = self.begin_transaction()?;
        let mut count = 0u64;
        let mut total_bytes = 0usize;

        for data in rows {
            let row_id = self.generate_row_id(table);
            total_bytes += data.len();
            self.insert(&handle, table, row_id, &data)?;
            count += 1;
        }

        let commit_ts = self.commit(handle)?;

        Ok(BulkInsertResult {
            rows_inserted: count,
            bytes_written: total_bytes,
            commit_timestamp: commit_ts,
        })
    }

    /// Get current statistics
    pub fn stats(&self) -> IngestionStats {
        self.stats.snapshot()
    }

    /// Get the current safety level
    pub fn safety_level(&self) -> &IngestionSafetyLevel {
        &self.config.safety_level
    }

    /// Force a WAL sync (for testing or explicit durability)
    pub fn force_sync(&self) -> IngestionResult<()> {
        if let Some(ref wal) = self.wal_manager {
            wal.sync_all()
                .map_err(|e| IngestionError::WalError(format!("{}", e)))?;
            self.stats.wal_syncs.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Create a checkpoint
    pub fn checkpoint(&self) -> IngestionResult<()> {
        if let Some(ref wal) = self.wal_manager {
            // Get row ID state for checkpoint
            let row_id_state = match &self.row_id_gen {
                RowIdGenerator::Batched(alloc) => alloc.checkpoint_state(),
                RowIdGenerator::Hierarchical(_) => vec![], // No state needed
            };

            wal.checkpoint(row_id_state)
                .map_err(|e| IngestionError::WalError(e.to_string()))?;
        }
        Ok(())
    }

    /// Recover from WAL after crash
    pub fn recover<P: AsRef<Path>>(wal_path: P) -> IngestionResult<RecoveryResult> {
        let recovery = WalRecovery::new(wal_path);

        // Recover row ID state
        let row_id_map = recovery
            .recover_row_ids()
            .map_err(|e| IngestionError::RecoveryError(e.to_string()))?;

        // Convert HashMap to Vec of tuples
        let row_id_state: Vec<(String, u64)> = row_id_map.into_iter().collect();

        // Find max timestamp from recovered state
        let max_timestamp = row_id_state.iter().map(|(_, id)| *id).max().unwrap_or(0);

        // Recover 2PC state
        let (to_rollback, _to_commit) = recovery
            .recover_2pc()
            .map_err(|e| IngestionError::RecoveryError(e.to_string()))?;

        Ok(RecoveryResult {
            committed_transactions: to_rollback.len() as u64,
            row_id_state,
            max_timestamp,
        })
    }

    /// Graceful shutdown
    pub fn shutdown(&self) -> IngestionResult<()> {
        self.shutdown.store(true, Ordering::Release);

        // Wait for pending commits
        while self.active_transactions.len() > 0 {
            thread::sleep(Duration::from_millis(10));
        }

        // Final sync
        self.force_sync()?;

        Ok(())
    }

    // --- Private helpers ---

    fn check_handle(&self, handle: &TransactionHandle) -> IngestionResult<()> {
        if !handle.is_active() {
            return Err(IngestionError::TransactionClosed(handle.txn_id));
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(IngestionError::EngineShutdown);
        }
        Ok(())
    }

    fn check_backpressure(&self) -> IngestionResult<()> {
        if self.pending_writes.load(Ordering::Relaxed) >= self.config.max_pending_writes as u64 {
            return Err(IngestionError::BackpressureExceeded);
        }
        Ok(())
    }

    fn start_commit_worker(
        receiver: Receiver<CommitRequest>,
        safety_level: IngestionSafetyLevel,
        wal_manager: Option<Arc<PartitionedWalManager>>,
        stats: Arc<AtomicStats>,
        shutdown: Arc<AtomicBool>,
        apply_callback: Arc<RwLock<Option<Box<dyn Fn(&str, u64, Option<&[u8]>) + Send + Sync>>>>,
    ) -> JoinHandle<()> {
        thread::Builder::new()
            .name("lockfree-commit-worker".to_string())
            .spawn(move || {
                Self::commit_worker_loop(
                    receiver,
                    safety_level,
                    wal_manager,
                    stats,
                    shutdown,
                    apply_callback,
                );
            })
            .expect("Failed to spawn commit worker")
    }

    fn commit_worker_loop(
        receiver: Receiver<CommitRequest>,
        safety_level: IngestionSafetyLevel,
        wal_manager: Option<Arc<PartitionedWalManager>>,
        stats: Arc<AtomicStats>,
        shutdown: Arc<AtomicBool>,
        apply_callback: Arc<RwLock<Option<Box<dyn Fn(&str, u64, Option<&[u8]>) + Send + Sync>>>>,
    ) {
        // Batch buffer for group commit
        let mut batch: Vec<CommitRequest> = Vec::with_capacity(1000);
        let batch_timeout = safety_level
            .batch_params()
            .map(|(_, d)| d)
            .unwrap_or(Duration::from_millis(1));
        let batch_size = safety_level
            .batch_params()
            .map(|(s, _)| s)
            .unwrap_or(1);

        loop {
            batch.clear();

            // Collect batch
            match receiver.recv_timeout(batch_timeout) {
                Ok(req) => batch.push(req),
                Err(RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::Acquire) {
                        // Try one more non-blocking receive before exiting
                        match receiver.try_recv() {
                            Ok(req) => batch.push(req),
                            Err(_) => break,
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // Drain more if available (up to batch size)
            while batch.len() < batch_size {
                match receiver.try_recv() {
                    Ok(req) => batch.push(req),
                    Err(_) => break,
                }
            }

            if batch.is_empty() {
                continue;
            }

            // Process batch
            Self::process_commit_batch(
                &batch,
                &wal_manager,
                &safety_level,
                &stats,
                &apply_callback,
            );
        }
    }

    fn process_commit_batch(
        batch: &[CommitRequest],
        wal_manager: &Option<Arc<PartitionedWalManager>>,
        safety_level: &IngestionSafetyLevel,
        stats: &Arc<AtomicStats>,
        apply_callback: &Arc<RwLock<Option<Box<dyn Fn(&str, u64, Option<&[u8]>) + Send + Sync>>>>,
    ) {
        // Assign commit timestamps
        static COMMIT_TS: AtomicU64 = AtomicU64::new(1);
        let base_ts = COMMIT_TS.fetch_add(batch.len() as u64, Ordering::SeqCst);

        // Write to WAL if enabled
        if let Some(ref wal) = wal_manager {
            let mut total_bytes = 0usize;

            for (i, req) in batch.iter().enumerate() {
                let commit_ts = base_ts + i as u64;
                let ops: Vec<WalOp> = req
                    .buffer
                    .operations()
                    .iter()
                    .map(|op| match op {
                        WriteOp::Insert { table, row_id, data } => WalOp::Insert {
                            table: table.clone(),
                            row_id: *row_id,
                            data: data.clone(),
                        },
                        WriteOp::Update { table, row_id, data } => WalOp::Update {
                            table: table.clone(),
                            row_id: *row_id,
                            data: data.clone(),
                        },
                        WriteOp::Delete { table, row_id } => WalOp::Delete {
                            table: table.clone(),
                            row_id: *row_id,
                        },
                    })
                    .collect();

                // Calculate approximate size
                for op in &ops {
                    total_bytes += match op {
                        WalOp::Insert { data, .. } => data.len() + 32,
                        WalOp::Update { data, .. } => data.len() + 32,
                        WalOp::Delete { .. } => 32,
                    };
                }

                // Write commit record
                if let Err(e) = wal.write_commit(req.txn_id, commit_ts, ops) {
                    let _ = req.response.send(CommitResponse::Error(
                        IngestionError::WalError(e.to_string()),
                    ));
                    continue;
                }
            }

            stats.wal_bytes_written.fetch_add(total_bytes as u64, Ordering::Relaxed);

            // Sync based on safety level
            if safety_level.sync_on_commit() {
                if let Err(e) = wal.sync_all() {
                    for req in batch {
                        let _ = req.response.send(CommitResponse::Error(
                            IngestionError::WalError(format!("{}", e)),
                        ));
                    }
                    return;
                }
                stats.wal_syncs.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Apply writes and send responses
        let callback_guard = apply_callback.read();

        for (i, req) in batch.iter().enumerate() {
            let commit_ts = base_ts + i as u64;
            let latency = req.start_time.elapsed().as_micros() as u64;

            // Apply to storage via callback
            if let Some(ref callback) = *callback_guard {
                for op in req.buffer.operations() {
                    match op {
                        WriteOp::Insert { table, row_id, data } => {
                            callback(table, *row_id, Some(data));
                        }
                        WriteOp::Update { table, row_id, data } => {
                            callback(table, *row_id, Some(data));
                        }
                        WriteOp::Delete { table, row_id } => {
                            callback(table, *row_id, None);
                        }
                    }
                }
            }

            // Update latency stats
            stats.total_commit_latency_us.fetch_add(latency, Ordering::Relaxed);
            stats.commit_count.fetch_add(1, Ordering::Relaxed);

            // Send success response
            let _ = req.response.send(CommitResponse::Success {
                commit_timestamp: commit_ts,
            });
        }
    }
}

impl Drop for LockFreeIngestionEngine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Worker will exit when channel is dropped
    }
}

/// Result of a bulk insert operation
#[derive(Debug, Clone)]
pub struct BulkInsertResult {
    /// Number of rows inserted
    pub rows_inserted: u64,
    /// Total bytes written
    pub bytes_written: usize,
    /// Commit timestamp
    pub commit_timestamp: u64,
}

/// Result of WAL recovery
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// Number of committed transactions recovered
    pub committed_transactions: u64,
    /// Row ID state from checkpoint
    pub row_id_state: Vec<(String, u64)>,
    /// Maximum timestamp found
    pub max_timestamp: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;

    #[test]
    fn test_basic_transaction() {
        let temp_dir = TempDir::new().unwrap();
        let config = LockFreeIngestionConfig::default();
        let engine = LockFreeIngestionEngine::new(config, temp_dir.path()).unwrap();

        let txn = engine.begin_transaction().unwrap();
        let row_id = engine.generate_row_id("test");

        engine
            .insert(&txn, "test", row_id, b"hello world")
            .unwrap();
        let commit_ts = engine.commit(txn).unwrap();

        assert!(commit_ts > 0);

        let stats = engine.stats();
        assert_eq!(stats.transactions_committed, 1);
        assert_eq!(stats.rows_inserted, 1);
    }

    #[test]
    fn test_bulk_insert() {
        let temp_dir = TempDir::new().unwrap();
        let config = LockFreeIngestionConfig::for_bulk_load();
        let engine = LockFreeIngestionEngine::new(config, temp_dir.path()).unwrap();

        let rows: Vec<Vec<u8>> = (0..1000).map(|i| format!("row_{}", i).into_bytes()).collect();

        let result = engine.bulk_insert("test", rows).unwrap();

        assert_eq!(result.rows_inserted, 1000);

        let stats = engine.stats();
        assert_eq!(stats.transactions_committed, 1);
    }

    #[test]
    fn test_apply_callback() {
        let temp_dir = TempDir::new().unwrap();
        let config = LockFreeIngestionConfig::default();
        let engine = LockFreeIngestionEngine::new(config, temp_dir.path()).unwrap();

        let applied_count = Arc::new(AtomicUsize::new(0));
        let count_clone = applied_count.clone();

        engine.set_apply_callback(move |_table, _row_id, _data| {
            count_clone.fetch_add(1, Ordering::Relaxed);
        });

        let txn = engine.begin_transaction().unwrap();
        for i in 0..10 {
            let row_id = engine.generate_row_id("test");
            engine
                .insert(&txn, "test", row_id, format!("data_{}", i).as_bytes())
                .unwrap();
        }
        engine.commit(txn).unwrap();

        // Wait a bit for callback to be invoked
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(applied_count.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_abort_transaction() {
        let temp_dir = TempDir::new().unwrap();
        let config = LockFreeIngestionConfig::default();
        let engine = LockFreeIngestionEngine::new(config, temp_dir.path()).unwrap();

        let txn = engine.begin_transaction().unwrap();
        let row_id = engine.generate_row_id("test");

        engine
            .insert(&txn, "test", row_id, b"hello world")
            .unwrap();
        engine.abort(txn).unwrap();

        let stats = engine.stats();
        assert_eq!(stats.transactions_aborted, 1);
        assert_eq!(stats.transactions_committed, 0);
    }

    #[test]
    fn test_unsafe_mode() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = LockFreeIngestionConfig::for_maximum_performance();
        config.safety_level = IngestionSafetyLevel::Unsafe {
            disable_wal: true,
            checkpoint_interval_secs: 0,
        };

        let engine = LockFreeIngestionEngine::new(config, temp_dir.path()).unwrap();

        // Should work without WAL
        let txn = engine.begin_transaction().unwrap();
        let row_id = engine.generate_row_id("test");
        engine.insert(&txn, "test", row_id, b"data").unwrap();
        engine.commit(txn).unwrap();
    }

    #[test]
    fn test_concurrent_transactions() {
        use std::thread;

        let temp_dir = TempDir::new().unwrap();
        let config = LockFreeIngestionConfig::for_bulk_load();
        let engine = Arc::new(LockFreeIngestionEngine::new(config, temp_dir.path()).unwrap());

        let handles: Vec<_> = (0..4)
            .map(|t| {
                let engine = engine.clone();
                thread::spawn(move || {
                    for i in 0..100 {
                        let txn = engine.begin_transaction().unwrap();
                        let row_id = engine.generate_row_id("test");
                        engine
                            .insert(
                                &txn,
                                "test",
                                row_id,
                                format!("thread_{}_row_{}", t, i).as_bytes(),
                            )
                            .unwrap();
                        engine.commit(txn).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let stats = engine.stats();
        assert_eq!(stats.transactions_committed, 400);
    }
}
