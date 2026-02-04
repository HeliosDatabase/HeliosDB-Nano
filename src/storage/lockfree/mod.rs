//! Lock-free high-performance ingestion subsystem
//!
//! This module provides a complete lock-free data ingestion system with
//! configurable ACID guarantees. It enables linear scaling of write
//! throughput across CPU cores while maintaining transactional safety.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                     Lock-Free Ingestion System                           │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                          │
//! │  User API                                                                │
//! │  ┌─────────────────────────────────────────────────────────────────┐    │
//! │  │               LockFreeIngestionEngine                            │    │
//! │  │  begin_transaction() → insert/update/delete → commit/abort      │    │
//! │  └─────────────────────────────────────────────────────────────────┘    │
//! │                              │                                          │
//! │  ┌───────────────────────────┼───────────────────────────────────┐      │
//! │  │                           ▼                                   │      │
//! │  │  ┌───────────────┐  ┌───────────────┐  ┌──────────────────┐   │      │
//! │  │  │ Config        │  │ RowIdGen      │  │ WriteCoordinator │   │      │
//! │  │  │ SafetyLevel   │  │ Hierarchical  │  │ TxnBuffers       │   │      │
//! │  │  │ - Full        │  │ or Batched    │  │ GroupCommit      │   │      │
//! │  │  │ - Batched     │  │               │  │                  │   │      │
//! │  │  │ - Async       │  │ Lock-free     │  │ Lock-free        │   │      │
//! │  │  │ - Unsafe      │  │ unique IDs    │  │ buffering        │   │      │
//! │  │  └───────────────┘  └───────────────┘  └──────────────────┘   │      │
//! │  │                                                               │      │
//! │  │  ┌────────────────────────────────────────────────────────┐   │      │
//! │  │  │              PartitionedWalManager                      │   │      │
//! │  │  │  ┌────────┐ ┌────────┐ ┌────────┐        ┌────────┐    │   │      │
//! │  │  │  │ WAL P0 │ │ WAL P1 │ │ WAL P2 │  ...   │ WAL Pn │    │   │      │
//! │  │  │  └────────┘ └────────┘ └────────┘        └────────┘    │   │      │
//! │  │  │                                                        │   │      │
//! │  │  │  Two-Phase Commit for cross-partition transactions     │   │      │
//! │  │  │  Parallel fsync for linear I/O scaling                 │   │      │
//! │  │  └────────────────────────────────────────────────────────┘   │      │
//! │  └───────────────────────────────────────────────────────────────┘      │
//! │                                                                          │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Safety Levels
//!
//! The system supports four safety levels that trade durability for performance:
//!
//! | Level    | Durability | Performance | Use Case                    |
//! |----------|------------|-------------|-----------------------------|
//! | Full     | ✓ Zero loss| 1x baseline | Financial, critical data    |
//! | Batched  | ~Up to N   | 3-5x        | High throughput, small loss |
//! | Async    | ~Recent    | 5-10x       | Analytics, logs             |
//! | Unsafe   | ✗ All      | 10-50x      | Bulk load, temp data        |
//!
//! # Key Features
//!
//! - **Lock-free row ID generation**: No coordination between threads
//! - **Per-thread write buffers**: No contention on hot paths
//! - **Partitioned WAL**: Linear I/O scaling with CPU cores
//! - **Group commit**: Amortized fsync cost across transactions
//! - **Two-phase commit**: ACID atomicity for multi-partition writes
//! - **Hierarchical IDs**: Self-describing IDs, no persistence needed
//!
//! # Usage Examples
//!
//! ## Full ACID Mode (Default)
//!
//! ```ignore
//! use heliosdb::storage::lockfree::*;
//!
//! let config = LockFreeIngestionConfig::default();
//! let engine = LockFreeIngestionEngine::new(config, "/path/to/wal")?;
//!
//! let txn = engine.begin_transaction()?;
//! let row_id = engine.generate_row_id("users");
//! engine.insert(&txn, "users", row_id, &serialized_data)?;
//! engine.commit(txn)?; // WAL fsynced before return
//! ```
//!
//! ## Bulk Load Mode (Maximum Performance)
//!
//! ```ignore
//! let config = LockFreeIngestionConfig::for_maximum_performance();
//! let engine = LockFreeIngestionEngine::new(config, "/path/to/wal")?;
//!
//! // Load millions of rows with minimal overhead
//! let result = engine.bulk_insert("events", rows_iterator)?;
//! println!("Loaded {} rows", result.rows_inserted);
//!
//! // Checkpoint before shutdown
//! engine.checkpoint()?;
//! ```
//!
//! ## Batched Mode (Controlled Loss Window)
//!
//! ```ignore
//! let config = LockFreeIngestionConfig {
//!     safety_level: IngestionSafetyLevel::Batched {
//!         batch_size: 1000,
//!         batch_timeout_ms: 100,
//!     },
//!     ..Default::default()
//! };
//! ```
//!
//! # ACID Guarantees
//!
//! The lock-free architecture maintains ACID properties through:
//!
//! - **Atomicity**: Transaction buffers ensure all-or-nothing
//! - **Consistency**: WAL recovery restores consistent state
//! - **Isolation**: MVCC with read timestamps (no read locks needed)
//! - **Durability**: Configurable via safety level
//!
//! Even in "unsafe" mode, Atomicity, Consistency, and Isolation are preserved.
//! Only Durability is configurable.

mod config;
mod ingestion;
mod row_id;
mod wal_manager;
mod write_buffer;

// Re-export configuration types
pub use config::{IngestionSafetyLevel, LockFreeIngestionConfig};

// Re-export row ID generation
pub use row_id::{
    BatchRowIdAllocator, HierarchicalRowIdGenerator, RowIdGenerator,
};

// Re-export write buffer types
pub use write_buffer::{TransactionBuffer, WriteOp};

// Re-export WAL management
pub use wal_manager::{
    PartitionedWalManager, WalOp, WalPartition, WalRecord, WalRecovery,
};

// Re-export high-level API
pub use ingestion::{
    BulkInsertResult, IngestionError, IngestionResult, IngestionStats,
    LockFreeIngestionEngine, RecoveryResult, TransactionHandle,
};
