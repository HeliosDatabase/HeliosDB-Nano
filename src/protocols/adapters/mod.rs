//! Adapter layer for protocol integration
//!
//! This module provides trait-based adapters that bridge HeliosDB Full's
//! interfaces to HeliosDB Lite's implementations. These adapters enable
//! protocol handlers (PostgreSQL, MySQL, etc.) to work with Lite's storage
//! and query execution engines.
//!
//! # Architecture
//!
//! The adapter layer follows the Adapter design pattern, providing:
//!
//! - **Storage Adapter**: Bridges Full's LsmStorageEngine to Lite's RocksDB engine
//! - **Query Executor Adapter**: Bridges Full's QueryExecutor to Lite's SQL executor
//! - **Pub/Sub Adapter**: Implements PostgreSQL LISTEN/NOTIFY mechanism
//! - **Connection Pool**: Manages database connections efficiently
//!
//! # Usage
//!
//! ```rust,no_run
//! use heliosdb_lite::protocols::adapters::{
//!     LiteStorageAdapter,
//!     LiteQueryExecutorAdapter,
//!     PubSubManager,
//!     ConnectionPool,
//!     PoolConfig,
//! };
//! use heliosdb_lite::{StorageEngine, Config, Result};
//! use std::sync::Arc;
//!
//! # fn main() -> Result<()> {
//! // Create storage engine
//! let config = Config::in_memory();
//! let engine = Arc::new(StorageEngine::open_in_memory(&config)?);
//!
//! // Create storage adapter
//! let storage_adapter = LiteStorageAdapter::new(Arc::clone(&engine));
//!
//! // Create query executor adapter
//! let executor_adapter = LiteQueryExecutorAdapter::new(Arc::clone(&engine));
//!
//! // Create pub/sub manager
//! let pubsub = PubSubManager::new();
//!
//! // Create connection pool
//! let pool_config = PoolConfig::new()
//!     .with_min_size(5)
//!     .with_max_size(20);
//! let pool = ConnectionPool::new(pool_config)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Thread Safety
//!
//! All adapters are thread-safe and can be shared across multiple threads
//! using `Arc`. The storage engine and connection pool use internal locking
//! to ensure safe concurrent access.

mod storage;
mod executor;
mod pubsub;
mod pool;

pub use storage::{
    StorageAdapter,
    TransactionAdapter,
    LiteStorageAdapter,
};

pub use executor::{
    QueryExecutorAdapter,
    LiteQueryExecutorAdapter,
    QueryResult,
    PreparedStatement,
};

pub use pubsub::{
    PubSubAdapter,
    PubSubManager,
    Notification,
    Subscription,
    SubscriptionHandle,
};

pub use pool::{
    ConnectionPool,
    PoolConfig,
    PooledConnection,
    PoolStats,
};
