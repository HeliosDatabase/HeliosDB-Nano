//! Protocol integration layer
//!
//! This module provides the infrastructure for integrating multiple database
//! protocols (PostgreSQL, MySQL, etc.) with HeliosDB Lite's storage and query
//! execution engines.
//!
//! # Architecture Overview
//!
//! The protocol integration follows a layered architecture:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │           Protocol Handlers                              │
//! │  (PostgreSQL, MySQL, HTTP/REST - Future)                 │
//! └─────────────────────────────────────────────────────────┘
//!                          │
//!                          ↓
//! ┌─────────────────────────────────────────────────────────┐
//! │              Adapter Layer (Phase 1)                     │
//! │  - StorageAdapter: Bridge to RocksDB engine              │
//! │  - QueryExecutorAdapter: Bridge to SQL executor          │
//! │  - PubSubAdapter: LISTEN/NOTIFY implementation           │
//! │  - ConnectionPool: Connection management                 │
//! └─────────────────────────────────────────────────────────┘
//!                          │
//!                          ↓
//! ┌─────────────────────────────────────────────────────────┐
//! │         HeliosDB Lite Core                               │
//! │  - StorageEngine (RocksDB)                               │
//! │  - SQL Executor                                          │
//! │  - Vector Search                                         │
//! │  - Encryption & Compression                              │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Implementation Phases
//!
//! ## Phase 1: Adapter Layer (Current)
//!
//! - StorageAdapter trait and implementation
//! - QueryExecutorAdapter trait and implementation
//! - PubSubAdapter for LISTEN/NOTIFY
//! - ConnectionPool for connection management
//!
//! ## Phase 2: PostgreSQL Protocol (Future)
//!
//! - Wire protocol implementation
//! - Authentication (MD5, SCRAM-SHA-256)
//! - Extended query protocol
//! - COPY protocol
//! - Streaming replication
//!
//! ## Phase 3: Additional Protocols (Future)
//!
//! - MySQL wire protocol
//! - HTTP/REST API
//! - GraphQL endpoint
//!
//! # Usage Example
//!
//! ```rust,no_run
//! use heliosdb_nano::protocols::adapters::{
//!     LiteStorageAdapter,
//!     LiteQueryExecutorAdapter,
//!     StorageAdapter,
//!     QueryExecutorAdapter,
//! };
//! use heliosdb_nano::{StorageEngine, Config};
//! use std::sync::Arc;
//!
//! // Initialize storage engine
//! let config = Config::in_memory();
//! let engine = Arc::new(StorageEngine::open_in_memory(&config).unwrap());
//!
//! // Create adapters
//! let storage = LiteStorageAdapter::new(Arc::clone(&engine));
//! let executor = LiteQueryExecutorAdapter::new(Arc::clone(&engine));
//!
//! // Use storage adapter
//! storage.put(b"key", b"value").unwrap();
//! let value = storage.get(b"key").unwrap();
//!
//! // Use query executor adapter
//! executor.execute_query("CREATE TABLE users (id INT, name TEXT)").unwrap();
//! let result = executor.execute_query("SELECT * FROM users").unwrap();
//! ```

pub mod adapters;
pub mod oracle;
pub mod server_manager;

// Re-export commonly used types
pub use adapters::{
    StorageAdapter,
    QueryExecutorAdapter,
    PubSubAdapter,
    LiteStorageAdapter,
    LiteQueryExecutorAdapter,
    PubSubManager,
    ConnectionPool,
    PoolConfig,
};

// Re-export Oracle protocol types
pub use oracle::{
    OracleServer,
    OracleServerConfig,
    OracleTranslator,
    OracleProtocolHandler,
};

// Re-export server manager
pub use server_manager::{
    ServerManager,
    ServerManagerConfig,
    ServerHealth,
};
