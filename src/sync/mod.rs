//! Cloud Synchronization Module
//!
//! Implements the Embedded-Cloud sync protocol for bidirectional
//! synchronization between embedded HeliosDB Lite instances and
//! cloud HeliosDB servers.
//!
//! # Features
//!
//! - <1s sync latency for datasets <10MB
//! - Vector clock-based conflict resolution
//! - Offline queue for disconnected operation
//! - Delta-based incremental sync
//! - End-to-end encryption
//! - Automatic conflict resolution (>95%)

pub mod auth;
pub mod change_log;
pub mod client;
pub mod conflict;
pub mod conflicts; // Legacy compatibility - deprecated
pub mod delta;
pub mod delta_applicator;
pub mod http_server; // HTTP/REST API server for v2.3.0
pub mod message;
pub mod offline_queue;
pub mod protocol;
pub mod server;
pub mod vector_clock;

// Test utilities module - only compiled in test/bench mode
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub use auth::{Authorizer, Claims, JwtManager, TokenPair};
// Export change_log types with aliases to avoid naming conflicts
pub use change_log::{
    ChangeLog as ChangeLogImpl,
    ChangeEntry as ChangeLogEntry,
    ChangeType,
    ChangeLogStats,
    QueryOptions,
};
pub use client::SyncClient;

// V2.3.0 Enhanced Conflict Detection System
pub use conflict::{
    ChangeEntry as ConflictChangeEntry,
    ChangeOperation as ConflictChangeOperation,
    Conflict as ConflictV2,
    ConflictDetector,
    ConflictError,
    ConflictReport as ConflictReportV2,
    ConflictResolution as ConflictResolutionV2,
    ConflictStats,
    ConflictType as ConflictTypeV2,
};

// Legacy Conflict System (for backward compatibility with client/server)
pub use conflicts::{
    Conflict as ConflictLegacy,
    ConflictManager,
    ConflictResolution,
    ConflictType as ConflictTypeLegacy,
};
pub use delta::{Delta, DeltaCompressor, DeltaError, DeltaMetadata, DeltaOp};
pub use delta_applicator::{
    ApplyResult, BatchApplyResult, ChangeEntry as DeltaChangeEntry, DeltaApplicator, DeltaStats,
};
pub use http_server::HttpSyncServer;
pub use message::{
    Acknowledgment, BatchDelta, MessageType, Operation, RowDelta, RowId, SyncMode, SyncRequest,
    SyncResponse,
};
pub use offline_queue::OfflineQueue;
pub use protocol::{
    ChangeLog as ChangeLogTrait, SyncMessage, SyncProtocol, PROTOCOL_VERSION,
    ChangeEntry as ProtocolChangeEntry, ChangeOperation as ProtocolChangeOp,
    ConflictReport as ProtocolConflictReport, ConflictType as ProtocolConflictType,
};
pub use server::SyncServer;
pub use vector_clock::VectorClock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

/// Sync result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub version: u64,
    pub synced_rows: usize,
    pub conflicts: usize,
    pub duration_ms: u64,
}

/// Sync configuration
#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub server_url: String,
    pub client_id: Uuid,
    pub sync_interval: std::time::Duration,
    pub retry_interval: std::time::Duration,
    pub max_batch_size: usize,
    pub enable_compression: bool,
    pub enable_e2e_encryption: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            server_url: "https://sync.heliosdb.io".to_string(),
            client_id: Uuid::new_v4(),
            sync_interval: std::time::Duration::from_secs(30),
            retry_interval: std::time::Duration::from_secs(5),
            max_batch_size: 1000,
            enable_compression: true,
            enable_e2e_encryption: false,
        }
    }
}

/// Sync errors
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Conflict resolution failed: {0}")]
    ConflictResolution(String),

    #[error("Authentication failed")]
    Authentication,

    #[error("Queue full")]
    QueueFull,

    #[error("Invalid message: {0}")]
    InvalidMessage(String),

    #[error("Storage error: {0}")]
    Storage(String),
}

pub type Result<T> = std::result::Result<T, SyncError>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_config_default() {
        let config = SyncConfig::default();
        assert_eq!(config.server_url, "https://sync.heliosdb.io");
        assert!(config.enable_compression);
    }
}
