//! Sync protocol message types

use super::vector_clock::VectorClock;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Message type identifier
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    SyncRequest = 0x01,
    SyncResponse = 0x02,
    RowDelta = 0x03,
    Acknowledgment = 0x04,
    Heartbeat = 0x05,
    ConflictNotification = 0x06,
    Error = 0xFF,
}

/// Sync mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SyncMode {
    Incremental, // Only changes since last_sync_version
    Full,        // Complete state transfer (recovery)
}

/// Row identifier (primary key bytes)
pub type RowId = Vec<u8>;

/// Operation type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Operation {
    Insert,
    Update { columns: Vec<String> }, // Only changed columns
    Delete,
}

/// Sync request message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    /// Unique client identifier
    pub client_id: Uuid,

    /// Last version successfully synced
    pub last_sync_version: u64,

    /// List of tables with local changes
    pub changed_tables: Vec<String>,

    /// Number of pending changes
    pub pending_changes: u32,

    /// Client vector clock
    pub vector_clock: VectorClock,

    /// Sync mode (full or incremental)
    pub sync_mode: SyncMode,
}

/// Sync response message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Current server version
    pub server_version: u64,

    /// Delta changes since client's last_sync_version
    pub delta: Vec<RowDelta>,

    /// Detected conflicts (if any)
    pub conflicts: Vec<super::conflicts::Conflict>,

    /// Next sync token (for resumable sync)
    pub continuation_token: Option<String>,

    /// Server vector clock
    pub vector_clock: VectorClock,
}

/// Row delta (single row change)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowDelta {
    /// Table name
    pub table: String,

    /// Operation type
    pub operation: Operation,

    /// Row identifier
    pub row_id: RowId,

    /// Changed data (compressed)
    pub data: Vec<u8>,

    /// Vector clock for this change
    pub vector_clock: VectorClock,

    /// Timestamp of change
    pub timestamp: DateTime<Utc>,

    /// Checksum for integrity
    pub checksum: u32,
}

impl RowDelta {
    /// Calculate checksum
    pub fn calculate_checksum(&self) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.data.hash(&mut hasher);
        hasher.finish() as u32
    }

    /// Verify checksum
    pub fn verify_checksum(&self) -> bool {
        self.checksum == self.calculate_checksum()
    }
}

/// Batch delta (multiple row changes)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDelta {
    pub batch_id: Uuid,
    pub deltas: Vec<RowDelta>,
    pub compressed: bool, // Use zstd compression
}

/// Acknowledgment message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acknowledgment {
    /// Version after applying changes
    pub new_version: u64,

    /// Successfully applied changes
    pub applied_count: u32,

    /// Failed changes (with reasons)
    pub failed: Vec<FailedChange>,

    /// Updated vector clock
    pub vector_clock: VectorClock,
}

/// Failed change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedChange {
    pub row_id: RowId,
    pub reason: String,
    pub conflict: Option<super::conflicts::Conflict>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_row_delta_checksum() {
        let delta = RowDelta {
            table: "users".to_string(),
            operation: Operation::Insert,
            row_id: vec![1, 2, 3],
            data: vec![4, 5, 6],
            vector_clock: VectorClock::default(),
            timestamp: Utc::now(),
            checksum: 0,
        };

        let checksum = delta.calculate_checksum();
        assert!(checksum > 0);

        let mut delta = delta;
        delta.checksum = checksum;
        assert!(delta.verify_checksum());
    }

    #[test]
    fn test_sync_request_serialization() {
        let request = SyncRequest {
            client_id: Uuid::new_v4(),
            last_sync_version: 42,
            changed_tables: vec!["users".to_string(), "orders".to_string()],
            pending_changes: 10,
            vector_clock: VectorClock::default(),
            sync_mode: SyncMode::Incremental,
        };

        let bytes = bincode::serialize(&request).unwrap();
        let deserialized: SyncRequest = bincode::deserialize(&bytes).unwrap();

        assert_eq!(request.client_id, deserialized.client_id);
        assert_eq!(request.last_sync_version, deserialized.last_sync_version);
    }
}
