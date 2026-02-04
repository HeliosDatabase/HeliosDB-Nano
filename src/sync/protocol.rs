//! Sync Protocol Implementation for HeliosDB-Lite v2.3.0
//!
//! This module implements the replication protocol for client-server synchronization,
//! providing deterministic, idempotent message handling with vector clock-based
//! conflict detection.
//!
//! # Protocol Features
//!
//! - Vector clock-based causality tracking
//! - Idempotent operations for reliable message handling
//! - Batching and pagination for large change sets
//! - Client health monitoring with heartbeat mechanism
//! - Compression support for network efficiency
//! - Protocol versioning for evolution
//!
//! # Message Flow
//!
//! 1. Client Registration: Client → Server (RegisterClient)
//! 2. Pull Changes: Client → Server (PullRequest), Server → Client (PullResponse)
//! 3. Push Changes: Client → Server (PushChanges), Server → Client (PushAck)
//! 4. Heartbeat: Client → Server (Heartbeat)
//!
//! # Example
//!
//! ```rust,ignore
//! use heliosdb_lite::sync::protocol::{SyncProtocol, SyncMessage};
//!
//! let protocol = SyncProtocol::new(change_log, conflict_detector);
//!
//! // Handle client registration
//! let register_msg = SyncMessage::RegisterClient {
//!     client_id: "client-1".to_string(),
//!     last_known_lsn: 0,
//!     vector_clock: VectorClock::new(),
//! };
//! protocol.handle_register(register_msg)?;
//! ```

use super::{
    conflicts::{Conflict, ConflictManager},
    vector_clock::VectorClock,
    Result, SyncError,
};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

/// Protocol version for evolution support
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum message size (1MB for 1000 entries target)
pub const MAX_MESSAGE_SIZE: usize = 1_048_576;

/// Default batch size for pull requests
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// Client heartbeat timeout (60 seconds)
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 60;

/// Protocol message types for client-server synchronization
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncMessage {
    /// Client registers with the server
    RegisterClient {
        /// Protocol version
        version: u32,
        /// Unique client identifier
        client_id: String,
        /// Last known LSN (Log Sequence Number)
        last_known_lsn: u64,
        /// Client's vector clock
        vector_clock: VectorClock,
        /// Optional client metadata
        #[serde(default)]
        metadata: HashMap<String, String>,
    },

    /// Client requests changes from server
    PullRequest {
        /// Message ID for idempotency
        message_id: Uuid,
        /// Client identifier
        client_id: String,
        /// Fetch changes since this LSN
        since_lsn: u64,
        /// Maximum number of entries to return
        max_entries: usize,
        /// Optional continuation token for pagination
        #[serde(default)]
        continuation_token: Option<String>,
    },

    /// Server responds with changes
    PullResponse {
        /// Message ID from request (for correlation)
        request_id: Uuid,
        /// Change entries
        changes: Vec<ChangeEntry>,
        /// Current server LSN
        server_lsn: u64,
        /// More changes available
        has_more: bool,
        /// Continuation token for next request
        #[serde(default)]
        continuation_token: Option<String>,
        /// Server's vector clock
        vector_clock: VectorClock,
    },

    /// Client pushes changes to server
    PushChanges {
        /// Message ID for idempotency
        message_id: Uuid,
        /// Client identifier
        client_id: String,
        /// Changes to apply
        changes: Vec<ChangeEntry>,
        /// Client's vector clock
        vector_clock: VectorClock,
    },

    /// Server acknowledges pushed changes
    PushAck {
        /// Message ID from request (for correlation)
        request_id: Uuid,
        /// Successfully accepted LSNs
        accepted_lsns: Vec<u64>,
        /// Detected conflicts
        conflicts: Vec<ConflictReport>,
        /// Updated server LSN
        server_lsn: u64,
        /// Server's vector clock
        vector_clock: VectorClock,
    },

    /// Client heartbeat
    Heartbeat {
        /// Client identifier
        client_id: String,
        /// Timestamp (milliseconds since epoch)
        timestamp: u64,
        /// Client's current LSN
        current_lsn: u64,
    },

    /// Server error response
    SyncError {
        /// Error code
        code: u32,
        /// Human-readable error message
        message: String,
        /// Optional details
        #[serde(default)]
        details: Option<String>,
    },
}

impl SyncMessage {
    /// Calculate serialized size in bytes
    pub fn size(&self) -> Result<usize> {
        bincode::serialize(self)
            .map(|bytes| bytes.len())
            .map_err(|e| SyncError::Serialization(e.to_string()))
    }

    /// Validate message size
    pub fn validate_size(&self) -> Result<()> {
        let size = self.size()?;
        if size > MAX_MESSAGE_SIZE {
            return Err(SyncError::InvalidMessage(format!(
                "Message size {} exceeds maximum {}",
                size, MAX_MESSAGE_SIZE
            )));
        }
        Ok(())
    }

    /// Get message type name for logging
    pub fn type_name(&self) -> &'static str {
        match self {
            SyncMessage::RegisterClient { .. } => "RegisterClient",
            SyncMessage::PullRequest { .. } => "PullRequest",
            SyncMessage::PullResponse { .. } => "PullResponse",
            SyncMessage::PushChanges { .. } => "PushChanges",
            SyncMessage::PushAck { .. } => "PushAck",
            SyncMessage::Heartbeat { .. } => "Heartbeat",
            SyncMessage::SyncError { .. } => "SyncError",
        }
    }
}

/// Change entry representing a single modification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// Log Sequence Number
    pub lsn: u64,
    /// Table name
    pub table: String,
    /// Operation type
    pub operation: ChangeOperation,
    /// Primary key
    pub key: Vec<u8>,
    /// Changed data (may be compressed)
    pub data: Vec<u8>,
    /// Vector clock at time of change
    pub vector_clock: VectorClock,
    /// Timestamp
    pub timestamp: DateTime<Utc>,
    /// Checksum for integrity
    pub checksum: u32,
    /// Whether data is compressed
    #[serde(default)]
    pub compressed: bool,
}

impl ChangeEntry {
    /// Calculate checksum using data and key
    pub fn calculate_checksum(&self) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.key.hash(&mut hasher);
        self.data.hash(&mut hasher);
        self.lsn.hash(&mut hasher);
        hasher.finish() as u32
    }

    /// Verify checksum integrity
    pub fn verify_checksum(&self) -> bool {
        self.checksum == self.calculate_checksum()
    }

    /// Compress data using zstd
    pub fn compress(&mut self) -> Result<()> {
        if self.compressed {
            return Ok(());
        }

        let compressed = zstd::encode_all(&self.data[..], 3)
            .map_err(|e| SyncError::Serialization(format!("Compression failed: {}", e)))?;

        self.data = compressed;
        self.compressed = true;
        self.checksum = self.calculate_checksum();
        Ok(())
    }

    /// Decompress data
    pub fn decompress(&mut self) -> Result<()> {
        if !self.compressed {
            return Ok(());
        }

        let decompressed = zstd::decode_all(&self.data[..])
            .map_err(|e| SyncError::Serialization(format!("Decompression failed: {}", e)))?;

        self.data = decompressed;
        self.compressed = false;
        self.checksum = self.calculate_checksum();
        Ok(())
    }
}

/// Change operation types
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChangeOperation {
    Insert,
    Update,
    Delete,
}

/// Conflict report in acknowledgment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    /// Change LSN that conflicted
    pub lsn: u64,
    /// Table name
    pub table: String,
    /// Primary key
    pub key: Vec<u8>,
    /// Conflict type
    pub conflict_type: ConflictType,
    /// Description
    pub description: String,
}

/// Conflict types
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConflictType {
    ConcurrentUpdate,
    DeletedOnServer,
    UniqueConstraintViolation,
}

/// Client state tracked by the server
#[derive(Debug, Clone)]
struct ClientState {
    client_id: String,
    last_sync_lsn: u64,
    vector_clock: VectorClock,
    last_heartbeat: SystemTime,
    metadata: HashMap<String, String>,
    /// Track processed message IDs for idempotency
    processed_messages: lru::LruCache<Uuid, SyncMessage>,
}

impl ClientState {
    fn new(client_id: String, last_sync_lsn: u64, vector_clock: VectorClock) -> Self {
        Self {
            client_id,
            last_sync_lsn,
            vector_clock,
            last_heartbeat: SystemTime::now(),
            metadata: HashMap::new(),
            processed_messages: lru::LruCache::new(
                std::num::NonZeroUsize::new(100).expect("100 is non-zero"),
            ),
        }
    }

    fn is_healthy(&self) -> bool {
        SystemTime::now()
            .duration_since(self.last_heartbeat)
            .map(|d| d.as_secs() < HEARTBEAT_TIMEOUT_SECS)
            .unwrap_or(false)
    }

    fn update_heartbeat(&mut self) {
        self.last_heartbeat = SystemTime::now();
    }
}

/// Change log interface (abstraction for storage backend)
pub trait ChangeLog: Send + Sync {
    /// Get changes since a given LSN
    fn get_changes_since(&self, lsn: u64, limit: usize) -> Result<Vec<ChangeEntry>>;

    /// Get current LSN
    fn current_lsn(&self) -> Result<u64>;

    /// Append changes to log
    fn append_changes(&self, changes: &[ChangeEntry]) -> Result<Vec<u64>>;
}

/// Conflict detector interface
pub trait ConflictDetector: Send + Sync {
    /// Detect conflicts between local and remote changes
    fn detect_conflicts(
        &self,
        local_clock: &VectorClock,
        remote_changes: &[ChangeEntry],
    ) -> Result<Vec<ConflictReport>>;
}

/// Sync Protocol implementation
pub struct SyncProtocol {
    change_log: Arc<dyn ChangeLog>,
    conflict_detector: Arc<dyn ConflictDetector>,
    registered_clients: Arc<RwLock<HashMap<String, ClientState>>>,
    node_id: Uuid,
}

impl SyncProtocol {
    /// Create a new sync protocol instance
    ///
    /// # Arguments
    ///
    /// * `change_log` - Change log implementation for persistence
    /// * `conflict_detector` - Conflict detection implementation
    ///
    /// # Returns
    ///
    /// New `SyncProtocol` instance
    pub fn new(
        change_log: Arc<dyn ChangeLog>,
        conflict_detector: Arc<dyn ConflictDetector>,
    ) -> Self {
        Self {
            change_log,
            conflict_detector,
            registered_clients: Arc::new(RwLock::new(HashMap::new())),
            node_id: Uuid::new_v4(),
        }
    }

    /// Handle client registration
    ///
    /// This is idempotent - registering the same client multiple times
    /// updates the existing registration.
    ///
    /// # Arguments
    ///
    /// * `msg` - RegisterClient message
    ///
    /// # Returns
    ///
    /// Ok on success, error if validation fails
    pub fn handle_register(&self, msg: SyncMessage) -> Result<()> {
        if let SyncMessage::RegisterClient {
            version,
            client_id,
            last_known_lsn,
            vector_clock,
            metadata,
        } = msg
        {
            // Validate protocol version
            if version != PROTOCOL_VERSION {
                return Err(SyncError::InvalidMessage(format!(
                    "Unsupported protocol version: {}. Expected: {}",
                    version, PROTOCOL_VERSION
                )));
            }

            let mut clients = self.registered_clients.write();
            let mut state = ClientState::new(client_id.clone(), last_known_lsn, vector_clock);
            state.metadata = metadata;
            state.update_heartbeat();

            clients.insert(client_id.clone(), state);

            tracing::info!(
                "Client registered: {} at LSN {}",
                client_id,
                last_known_lsn
            );

            Ok(())
        } else {
            Err(SyncError::InvalidMessage(
                "Expected RegisterClient message".to_string(),
            ))
        }
    }

    /// Handle pull request from client
    ///
    /// Idempotent - duplicate requests with same message_id return the same response.
    ///
    /// # Arguments
    ///
    /// * `msg` - PullRequest message
    ///
    /// # Returns
    ///
    /// PullResponse message with changes
    pub fn handle_pull_request(&self, msg: SyncMessage) -> Result<SyncMessage> {
        if let SyncMessage::PullRequest {
            message_id,
            client_id,
            since_lsn,
            max_entries,
            continuation_token,
        } = msg
        {
            // Check for client registration
            let clients = self.registered_clients.read();
            let client = clients
                .get(&client_id)
                .ok_or_else(|| SyncError::InvalidMessage(format!("Client not registered: {}", client_id)))?;

            // Check for duplicate message (idempotency)
            if let Some(cached_response) = client.processed_messages.peek(&message_id) {
                tracing::debug!("Returning cached response for message {}", message_id);
                return Ok(cached_response.clone());
            }
            drop(clients);

            // Validate batch size
            let limit = max_entries.min(DEFAULT_BATCH_SIZE);

            // Calculate offset from continuation token
            let offset = continuation_token
                .as_ref()
                .and_then(|t| t.parse::<u64>().ok())
                .unwrap_or(0);

            // Fetch changes from log
            let mut changes = self.change_log.get_changes_since(since_lsn + offset, limit)?;

            // Compress changes if enabled
            for change in &mut changes {
                if let Err(e) = change.compress() {
                    tracing::warn!("Failed to compress change {}: {}", change.lsn, e);
                }
            }

            let server_lsn = self.change_log.current_lsn()?;
            let has_more = changes.len() == limit;

            // Generate continuation token
            let continuation_token = if has_more {
                Some((offset + changes.len() as u64).to_string())
            } else {
                None
            };

            // Build server vector clock
            let mut vector_clock = VectorClock::new();
            vector_clock.increment(self.node_id);

            let response = SyncMessage::PullResponse {
                request_id: message_id,
                changes,
                server_lsn,
                has_more,
                continuation_token,
                vector_clock,
            };

            // Validate response size
            response.validate_size()?;

            // Cache response for idempotency
            let mut clients = self.registered_clients.write();
            if let Some(client) = clients.get_mut(&client_id) {
                client.processed_messages.put(message_id, response.clone());
            }

            Ok(response)
        } else {
            Err(SyncError::InvalidMessage(
                "Expected PullRequest message".to_string(),
            ))
        }
    }

    /// Handle push changes from client
    ///
    /// Idempotent - duplicate pushes with same message_id are ignored.
    ///
    /// # Arguments
    ///
    /// * `msg` - PushChanges message
    ///
    /// # Returns
    ///
    /// PushAck message with acceptance status and conflicts
    pub fn handle_push_changes(&self, msg: SyncMessage) -> Result<SyncMessage> {
        if let SyncMessage::PushChanges {
            message_id,
            client_id,
            mut changes,
            vector_clock: client_clock,
        } = msg
        {
            // Check for client registration
            let clients = self.registered_clients.read();
            let client = clients
                .get(&client_id)
                .ok_or_else(|| SyncError::InvalidMessage(format!("Client not registered: {}", client_id)))?;

            // Check for duplicate message (idempotency)
            if let Some(cached_response) = client.processed_messages.peek(&message_id) {
                tracing::debug!("Returning cached ack for message {}", message_id);
                return Ok(cached_response.clone());
            }
            drop(clients);

            // Decompress changes
            for change in &mut changes {
                if change.compressed {
                    change.decompress()?;
                }

                // Verify checksum
                if !change.verify_checksum() {
                    tracing::warn!("Checksum verification failed for LSN {}", change.lsn);
                    return Err(SyncError::InvalidMessage(format!(
                        "Checksum mismatch for LSN {}",
                        change.lsn
                    )));
                }
            }

            // Detect conflicts
            let conflicts = self
                .conflict_detector
                .detect_conflicts(&client_clock, &changes)?;

            // Filter out conflicting changes
            let mut accepted_changes = Vec::new();
            let mut accepted_lsns = Vec::new();

            for change in changes {
                let has_conflict = conflicts.iter().any(|c| c.lsn == change.lsn);
                if !has_conflict {
                    accepted_lsns.push(change.lsn);
                    accepted_changes.push(change);
                }
            }

            // Append accepted changes to log
            let new_lsns = self.change_log.append_changes(&accepted_changes)?;
            let server_lsn = self.change_log.current_lsn()?;

            // Update client state
            let mut clients = self.registered_clients.write();
            if let Some(client) = clients.get_mut(&client_id) {
                client.last_sync_lsn = server_lsn;
                client.vector_clock.merge(&client_clock);
                client.update_heartbeat();
            }

            // Build server vector clock
            let mut server_clock = VectorClock::new();
            server_clock.increment(self.node_id);

            let response = SyncMessage::PushAck {
                request_id: message_id,
                accepted_lsns: new_lsns,
                conflicts,
                server_lsn,
                vector_clock: server_clock,
            };

            // Cache response for idempotency
            if let Some(client) = clients.get_mut(&client_id) {
                client.processed_messages.put(message_id, response.clone());
            }

            Ok(response)
        } else {
            Err(SyncError::InvalidMessage(
                "Expected PushChanges message".to_string(),
            ))
        }
    }

    /// Handle heartbeat from client
    ///
    /// # Arguments
    ///
    /// * `msg` - Heartbeat message
    ///
    /// # Returns
    ///
    /// Ok on success
    pub fn handle_heartbeat(&self, msg: SyncMessage) -> Result<()> {
        if let SyncMessage::Heartbeat {
            client_id,
            current_lsn,
            ..
        } = msg
        {
            let mut clients = self.registered_clients.write();
            if let Some(client) = clients.get_mut(&client_id) {
                client.update_heartbeat();
                client.last_sync_lsn = current_lsn;
                tracing::debug!("Heartbeat received from client: {}", client_id);
                Ok(())
            } else {
                Err(SyncError::InvalidMessage(format!(
                    "Client not registered: {}",
                    client_id
                )))
            }
        } else {
            Err(SyncError::InvalidMessage(
                "Expected Heartbeat message".to_string(),
            ))
        }
    }

    /// Check client health and return list of inactive clients
    ///
    /// # Returns
    ///
    /// Vector of client IDs that have timed out
    pub fn check_client_health(&self) -> Vec<String> {
        let clients = self.registered_clients.read();
        clients
            .iter()
            .filter(|(_, state)| !state.is_healthy())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Evict a client from the registered clients list
    ///
    /// # Arguments
    ///
    /// * `client_id` - Client identifier to evict
    ///
    /// # Returns
    ///
    /// Ok if client was evicted, error if client not found
    pub fn evict_client(&self, client_id: &str) -> Result<()> {
        let mut clients = self.registered_clients.write();
        clients
            .remove(client_id)
            .map(|_| {
                tracing::info!("Client evicted: {}", client_id);
            })
            .ok_or_else(|| {
                SyncError::InvalidMessage(format!("Client not found: {}", client_id))
            })
    }

    /// Get client state for debugging/monitoring
    pub fn get_client_state(&self, client_id: &str) -> Option<(u64, bool)> {
        let clients = self.registered_clients.read();
        clients
            .get(client_id)
            .map(|state| (state.last_sync_lsn, state.is_healthy()))
    }

    /// Get count of registered clients
    pub fn client_count(&self) -> usize {
        self.registered_clients.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock implementations for testing
    struct MockChangeLog {
        changes: RwLock<Vec<ChangeEntry>>,
    }

    impl MockChangeLog {
        fn new() -> Self {
            Self {
                changes: RwLock::new(Vec::new()),
            }
        }
    }

    impl ChangeLog for MockChangeLog {
        fn get_changes_since(&self, lsn: u64, limit: usize) -> Result<Vec<ChangeEntry>> {
            let changes = self.changes.read();
            Ok(changes
                .iter()
                .filter(|c| c.lsn > lsn)
                .take(limit)
                .cloned()
                .collect())
        }

        fn current_lsn(&self) -> Result<u64> {
            let changes = self.changes.read();
            Ok(changes.last().map(|c| c.lsn).unwrap_or(0))
        }

        fn append_changes(&self, changes: &[ChangeEntry]) -> Result<Vec<u64>> {
            let mut log = self.changes.write();
            let lsns: Vec<u64> = changes.iter().map(|c| c.lsn).collect();
            log.extend_from_slice(changes);
            Ok(lsns)
        }
    }

    struct MockConflictDetector;

    impl ConflictDetector for MockConflictDetector {
        fn detect_conflicts(
            &self,
            _local_clock: &VectorClock,
            _remote_changes: &[ChangeEntry],
        ) -> Result<Vec<ConflictReport>> {
            Ok(Vec::new())
        }
    }

    fn create_test_change(lsn: u64) -> ChangeEntry {
        let mut change = ChangeEntry {
            lsn,
            table: "test_table".to_string(),
            operation: ChangeOperation::Insert,
            key: vec![1, 2, 3],
            data: vec![4, 5, 6],
            vector_clock: VectorClock::new(),
            timestamp: Utc::now(),
            checksum: 0,
            compressed: false,
        };
        change.checksum = change.calculate_checksum();
        change
    }

    #[test]
    fn test_protocol_version() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn test_change_entry_checksum() {
        let change = create_test_change(1);
        assert!(change.verify_checksum());

        let mut invalid = change.clone();
        invalid.data = vec![7, 8, 9];
        assert!(!invalid.verify_checksum());
    }

    #[test]
    fn test_change_entry_compression() {
        let mut change = create_test_change(1);
        let original_data = change.data.clone();

        change.compress().expect("Compression failed");
        assert!(change.compressed);
        assert_ne!(change.data, original_data);

        change.decompress().expect("Decompression failed");
        assert!(!change.compressed);
        assert_eq!(change.data, original_data);
    }

    #[test]
    fn test_sync_message_serialization() {
        let msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 42,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };

        let serialized = bincode::serialize(&msg).expect("Serialization failed");
        let deserialized: SyncMessage =
            bincode::deserialize(&serialized).expect("Deserialization failed");

        match deserialized {
            SyncMessage::RegisterClient {
                client_id,
                last_known_lsn,
                ..
            } => {
                assert_eq!(client_id, "test-client");
                assert_eq!(last_known_lsn, 42);
            }
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_handle_register() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        let msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };

        protocol.handle_register(msg).expect("Registration failed");
        assert_eq!(protocol.client_count(), 1);
    }

    #[test]
    fn test_handle_register_wrong_version() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        let msg = SyncMessage::RegisterClient {
            version: 999,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };

        let result = protocol.handle_register(msg);
        assert!(result.is_err());
    }

    #[test]
    fn test_handle_pull_request() {
        let change_log = Arc::new(MockChangeLog::new());

        // Add test data
        {
            let mut changes = change_log.changes.write();
            changes.push(create_test_change(1));
            changes.push(create_test_change(2));
        }

        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client first
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");

        // Pull request
        let pull_msg = SyncMessage::PullRequest {
            message_id: Uuid::new_v4(),
            client_id: "test-client".to_string(),
            since_lsn: 0,
            max_entries: 10,
            continuation_token: None,
        };

        let response = protocol
            .handle_pull_request(pull_msg)
            .expect("Pull request failed");

        match response {
            SyncMessage::PullResponse {
                changes,
                server_lsn,
                has_more,
                ..
            } => {
                assert_eq!(changes.len(), 2);
                assert_eq!(server_lsn, 2);
                assert!(!has_more);
            }
            _ => panic!("Wrong response type"),
        }
    }

    #[test]
    fn test_handle_push_changes() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");

        // Push changes
        let changes = vec![create_test_change(1), create_test_change(2)];
        let push_msg = SyncMessage::PushChanges {
            message_id: Uuid::new_v4(),
            client_id: "test-client".to_string(),
            changes,
            vector_clock: VectorClock::new(),
        };

        let response = protocol
            .handle_push_changes(push_msg)
            .expect("Push failed");

        match response {
            SyncMessage::PushAck {
                accepted_lsns,
                conflicts,
                ..
            } => {
                assert_eq!(accepted_lsns.len(), 2);
                assert_eq!(conflicts.len(), 0);
            }
            _ => panic!("Wrong response type"),
        }
    }

    #[test]
    fn test_idempotent_pull_request() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");

        let message_id = Uuid::new_v4();

        // First pull request
        let pull_msg1 = SyncMessage::PullRequest {
            message_id,
            client_id: "test-client".to_string(),
            since_lsn: 0,
            max_entries: 10,
            continuation_token: None,
        };
        let response1 = protocol.handle_pull_request(pull_msg1).expect("Pull 1 failed");

        // Duplicate pull request (same message_id)
        let pull_msg2 = SyncMessage::PullRequest {
            message_id,
            client_id: "test-client".to_string(),
            since_lsn: 0,
            max_entries: 10,
            continuation_token: None,
        };
        let response2 = protocol.handle_pull_request(pull_msg2).expect("Pull 2 failed");

        // Responses should be identical
        let size1 = bincode::serialize(&response1).expect("Serialization failed").len();
        let size2 = bincode::serialize(&response2).expect("Serialization failed").len();
        assert_eq!(size1, size2);
    }

    #[test]
    fn test_handle_heartbeat() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");

        // Send heartbeat
        let heartbeat_msg = SyncMessage::Heartbeat {
            client_id: "test-client".to_string(),
            timestamp: 1234567890,
            current_lsn: 10,
        };

        protocol
            .handle_heartbeat(heartbeat_msg)
            .expect("Heartbeat failed");

        // Check client state
        let (lsn, healthy) = protocol
            .get_client_state("test-client")
            .expect("Client not found");
        assert_eq!(lsn, 10);
        assert!(healthy);
    }

    #[test]
    fn test_client_health_check() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");

        // Client should be healthy
        let inactive = protocol.check_client_health();
        assert_eq!(inactive.len(), 0);
    }

    #[test]
    fn test_evict_client() {
        let change_log = Arc::new(MockChangeLog::new());
        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");
        assert_eq!(protocol.client_count(), 1);

        // Evict client
        protocol
            .evict_client("test-client")
            .expect("Eviction failed");
        assert_eq!(protocol.client_count(), 0);

        // Evicting again should fail
        let result = protocol.evict_client("test-client");
        assert!(result.is_err());
    }

    #[test]
    fn test_pagination() {
        let change_log = Arc::new(MockChangeLog::new());

        // Add 15 test changes
        {
            let mut changes = change_log.changes.write();
            for i in 1..=15 {
                changes.push(create_test_change(i));
            }
        }

        let conflict_detector = Arc::new(MockConflictDetector);
        let protocol = SyncProtocol::new(change_log, conflict_detector);

        // Register client
        let register_msg = SyncMessage::RegisterClient {
            version: PROTOCOL_VERSION,
            client_id: "test-client".to_string(),
            last_known_lsn: 0,
            vector_clock: VectorClock::new(),
            metadata: HashMap::new(),
        };
        protocol.handle_register(register_msg).expect("Registration failed");

        // First page
        let pull_msg1 = SyncMessage::PullRequest {
            message_id: Uuid::new_v4(),
            client_id: "test-client".to_string(),
            since_lsn: 0,
            max_entries: 10,
            continuation_token: None,
        };
        let response1 = protocol.handle_pull_request(pull_msg1).expect("Pull failed");

        match response1 {
            SyncMessage::PullResponse {
                changes,
                has_more,
                continuation_token,
                ..
            } => {
                assert_eq!(changes.len(), 10);
                assert!(has_more);
                assert!(continuation_token.is_some());

                // Second page
                let pull_msg2 = SyncMessage::PullRequest {
                    message_id: Uuid::new_v4(),
                    client_id: "test-client".to_string(),
                    since_lsn: 0,
                    max_entries: 10,
                    continuation_token,
                };
                let response2 = protocol.handle_pull_request(pull_msg2).expect("Pull failed");

                match response2 {
                    SyncMessage::PullResponse {
                        changes: changes2,
                        has_more: has_more2,
                        ..
                    } => {
                        assert_eq!(changes2.len(), 5);
                        assert!(!has_more2);
                    }
                    _ => panic!("Wrong response type"),
                }
            }
            _ => panic!("Wrong response type"),
        }
    }
}
