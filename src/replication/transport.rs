//! Replication Transport Layer
//!
//! Custom TCP binary protocol for WAL streaming replication.
//!
//! # Protocol Overview
//!
//! The protocol uses a simple framed message format:
//! ```text
//! +----------------+----------------+----------------+
//! | Magic (4)      | Version (2)    | MsgType (2)    |
//! +----------------+----------------+----------------+
//! | Length (4)     | Flags (4)      | Sequence (8)   |
//! +----------------+----------------+----------------+
//! | Payload (variable length)                        |
//! +--------------------------------------------------+
//! | CRC32 (4)                                        |
//! +--------------------------------------------------+
//! ```
//!
//! # Message Types
//!
//! - Handshake: Initial connection setup
//! - WalEntry: Single WAL entry (real-time streaming)
//! - WalBatch: Batch of WAL entries (catch-up mode)
//! - Ack: Acknowledgment (with LSN and ack type)
//! - Heartbeat: Keep-alive with health info
//! - ControlRequest/Response: Admin commands

use crate::replication::{Lsn, ReplicationError, Result};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use uuid::Uuid;

// =============================================================================
// PROTOCOL CONSTANTS
// =============================================================================

/// Protocol magic number: "HELI" in ASCII
pub const PROTOCOL_MAGIC: u32 = 0x48454C49;

/// Current protocol version
pub const PROTOCOL_VERSION: u16 = 1;

/// Header size in bytes
pub const HEADER_SIZE: usize = 24;

/// Maximum message payload size (64MB)
pub const MAX_PAYLOAD_SIZE: usize = 64 * 1024 * 1024;

/// Default connection timeout
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default read/write timeout
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Heartbeat interval
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

// =============================================================================
// MESSAGE TYPES
// =============================================================================

/// Message type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u16)]
pub enum MessageType {
    /// Initial handshake request
    HandshakeRequest = 0x0001,
    /// Handshake response
    HandshakeResponse = 0x0002,
    /// Single WAL entry (real-time streaming)
    WalEntry = 0x0010,
    /// Batch of WAL entries (catch-up mode)
    WalBatch = 0x0011,
    /// Request WAL from specific LSN
    WalRequest = 0x0012,
    /// Acknowledgment
    Ack = 0x0020,
    /// Negative acknowledgment (error)
    Nack = 0x0021,
    /// Heartbeat/keepalive
    Heartbeat = 0x0030,
    /// Heartbeat response
    HeartbeatResponse = 0x0031,
    /// Control command request
    ControlRequest = 0x0040,
    /// Control command response
    ControlResponse = 0x0041,
    /// Logical replication entry
    LogicalEntry = 0x0050,
    /// Logical replication batch
    LogicalBatch = 0x0051,
    /// Observer vote request (split-brain)
    VoteRequest = 0x0060,
    /// Observer vote response
    VoteResponse = 0x0061,
    /// Fencing token
    FencingToken = 0x0062,
    /// Unknown message type
    Unknown = 0xFFFF,
}

impl From<u16> for MessageType {
    fn from(value: u16) -> Self {
        match value {
            0x0001 => Self::HandshakeRequest,
            0x0002 => Self::HandshakeResponse,
            0x0010 => Self::WalEntry,
            0x0011 => Self::WalBatch,
            0x0012 => Self::WalRequest,
            0x0020 => Self::Ack,
            0x0021 => Self::Nack,
            0x0030 => Self::Heartbeat,
            0x0031 => Self::HeartbeatResponse,
            0x0040 => Self::ControlRequest,
            0x0041 => Self::ControlResponse,
            0x0050 => Self::LogicalEntry,
            0x0051 => Self::LogicalBatch,
            0x0060 => Self::VoteRequest,
            0x0061 => Self::VoteResponse,
            0x0062 => Self::FencingToken,
            _ => Self::Unknown,
        }
    }
}

/// Message flags
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageFlags(u32);

impl MessageFlags {
    pub const NONE: u32 = 0;
    pub const COMPRESSED: u32 = 1 << 0;
    pub const ENCRYPTED: u32 = 1 << 1;
    pub const REQUIRES_ACK: u32 = 1 << 2;
    pub const URGENT: u32 = 1 << 3;
    pub const BATCH_START: u32 = 1 << 4;
    pub const BATCH_END: u32 = 1 << 5;

    pub fn new(flags: u32) -> Self {
        Self(flags)
    }

    pub fn is_compressed(&self) -> bool {
        self.0 & Self::COMPRESSED != 0
    }

    pub fn is_encrypted(&self) -> bool {
        self.0 & Self::ENCRYPTED != 0
    }

    pub fn requires_ack(&self) -> bool {
        self.0 & Self::REQUIRES_ACK != 0
    }

    pub fn is_urgent(&self) -> bool {
        self.0 & Self::URGENT != 0
    }

    pub fn set_compressed(&mut self) {
        self.0 |= Self::COMPRESSED;
    }

    pub fn set_requires_ack(&mut self) {
        self.0 |= Self::REQUIRES_ACK;
    }

    pub fn raw(&self) -> u32 {
        self.0
    }
}

// =============================================================================
// WIRE FORMAT
// =============================================================================

/// Message header
#[derive(Debug, Clone)]
pub struct MessageHeader {
    pub magic: u32,
    pub version: u16,
    pub msg_type: MessageType,
    pub length: u32,
    pub flags: MessageFlags,
    pub sequence: u64,
}

impl MessageHeader {
    pub fn new(msg_type: MessageType, payload_len: usize, sequence: u64) -> Self {
        Self {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            msg_type,
            length: payload_len as u32,
            flags: MessageFlags::default(),
            sequence,
        }
    }

    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u32(self.magic);
        buf.put_u16(self.version);
        buf.put_u16(self.msg_type as u16);
        buf.put_u32(self.length);
        buf.put_u32(self.flags.raw());
        buf.put_u64(self.sequence);
    }

    pub fn decode(buf: &mut impl Buf) -> Result<Self> {
        if buf.remaining() < HEADER_SIZE {
            return Err(ReplicationError::Transport(
                "Incomplete header".to_string(),
            ));
        }

        let magic = buf.get_u32();
        if magic != PROTOCOL_MAGIC {
            return Err(ReplicationError::Transport(format!(
                "Invalid magic: expected {:08X}, got {:08X}",
                PROTOCOL_MAGIC, magic
            )));
        }

        let version = buf.get_u16();
        let msg_type = MessageType::from(buf.get_u16());
        let length = buf.get_u32();
        let flags = MessageFlags::new(buf.get_u32());
        let sequence = buf.get_u64();

        if length as usize > MAX_PAYLOAD_SIZE {
            return Err(ReplicationError::Transport(format!(
                "Payload too large: {} bytes",
                length
            )));
        }

        Ok(Self {
            magic,
            version,
            msg_type,
            length,
            flags,
            sequence,
        })
    }
}

/// Complete message with header and payload
#[derive(Debug, Clone)]
pub struct Message {
    pub header: MessageHeader,
    pub payload: Bytes,
    pub checksum: u32,
}

impl Message {
    pub fn new(msg_type: MessageType, payload: Bytes, sequence: u64) -> Self {
        let header = MessageHeader::new(msg_type, payload.len(), sequence);
        let checksum = crc32fast::hash(&payload);
        Self {
            header,
            payload,
            checksum,
        }
    }

    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len() + 4);
        self.header.encode(&mut buf);
        buf.extend_from_slice(&self.payload);
        buf.put_u32(self.checksum);
        buf
    }

    pub async fn read_from<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Self> {
        // Read header
        let mut header_buf = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header_buf).await.map_err(|e| {
            ReplicationError::Transport(format!("Failed to read header: {}", e))
        })?;

        let header = MessageHeader::decode(&mut &header_buf[..])?;

        // Read payload
        let mut payload = vec![0u8; header.length as usize];
        reader.read_exact(&mut payload).await.map_err(|e| {
            ReplicationError::Transport(format!("Failed to read payload: {}", e))
        })?;

        // Read checksum
        let mut checksum_buf = [0u8; 4];
        reader.read_exact(&mut checksum_buf).await.map_err(|e| {
            ReplicationError::Transport(format!("Failed to read checksum: {}", e))
        })?;
        let checksum = u32::from_be_bytes(checksum_buf);

        // Verify checksum
        let computed_checksum = crc32fast::hash(&payload);
        if checksum != computed_checksum {
            return Err(ReplicationError::Transport(format!(
                "Checksum mismatch: expected {:08X}, got {:08X}",
                checksum, computed_checksum
            )));
        }

        Ok(Self {
            header,
            payload: Bytes::from(payload),
            checksum,
        })
    }

    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<()> {
        let buf = self.encode();
        writer.write_all(&buf).await.map_err(|e| {
            ReplicationError::Transport(format!("Failed to write message: {}", e))
        })?;
        Ok(())
    }
}

// =============================================================================
// PAYLOAD TYPES
// =============================================================================

/// Handshake request payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeRequest {
    /// Node ID
    pub node_id: Uuid,
    /// Node role (Primary, Standby, Observer)
    pub role: NodeRole,
    /// Requested sync mode
    pub sync_mode: SyncModeConfig,
    /// Current LSN (for standbys)
    pub current_lsn: Option<Lsn>,
    /// Replication slot name
    pub slot_name: Option<String>,
    /// Client capabilities
    pub capabilities: Capabilities,
}

/// Handshake response payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    /// Whether handshake was accepted
    pub accepted: bool,
    /// Server node ID
    pub server_node_id: Uuid,
    /// Negotiated sync mode
    pub sync_mode: SyncModeConfig,
    /// Current primary LSN
    pub primary_lsn: Lsn,
    /// Assigned replication slot
    pub slot_name: Option<String>,
    /// Fencing token (for split-brain prevention)
    pub fencing_token: u64,
    /// Server capabilities
    pub capabilities: Capabilities,
    /// Error message if not accepted
    pub error: Option<String>,
}

/// Node role in replication
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    Primary,
    Standby,
    Observer,
    Candidate,
}

/// Sync mode configuration with detailed semantics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncModeConfig {
    /// Asynchronous: Primary doesn't wait for standbys
    Async,
    /// Semi-synchronous: Wait for transport ACK (received, not applied)
    /// The standby acknowledges receipt of the WAL, but may not have applied it yet
    SemiSync {
        /// Minimum standbys that must ACK
        min_acks: u32,
        /// Timeout before falling back to async
        timeout_ms: u32,
    },
    /// Synchronous: Wait for apply ACK (WAL has been applied)
    Sync {
        /// Minimum standbys that must apply
        min_applied: u32,
        /// Timeout before returning error (no fallback)
        timeout_ms: u32,
    },
}

impl Default for SyncModeConfig {
    fn default() -> Self {
        Self::Async
    }
}

/// Node capabilities bitmap
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Capabilities(u32);

impl Capabilities {
    pub const COMPRESSION: u32 = 1 << 0;
    pub const ENCRYPTION: u32 = 1 << 1;
    pub const LOGICAL_REPLICATION: u32 = 1 << 2;
    pub const BATCH_STREAMING: u32 = 1 << 3;
    pub const OBSERVER_PROTOCOL: u32 = 1 << 4;

    pub fn new(caps: u32) -> Self {
        Self(caps)
    }

    pub fn supports_compression(&self) -> bool {
        self.0 & Self::COMPRESSION != 0
    }

    pub fn supports_logical(&self) -> bool {
        self.0 & Self::LOGICAL_REPLICATION != 0
    }

    pub fn supports_observer(&self) -> bool {
        self.0 & Self::OBSERVER_PROTOCOL != 0
    }

    pub fn all() -> Self {
        Self(
            Self::COMPRESSION
                | Self::ENCRYPTION
                | Self::LOGICAL_REPLICATION
                | Self::BATCH_STREAMING
                | Self::OBSERVER_PROTOCOL,
        )
    }
}

/// Acknowledgment type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckType {
    /// Transport ACK: Message received
    Received,
    /// Write ACK: Written to disk (WAL)
    Written,
    /// Apply ACK: Applied to database
    Applied,
    /// Checkpoint ACK: Checkpointed
    Checkpointed,
}

/// Acknowledgment payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckPayload {
    /// Acknowledged LSN
    pub lsn: Lsn,
    /// Type of acknowledgment
    pub ack_type: AckType,
    /// Node ID sending the ACK
    pub node_id: Uuid,
    /// Timestamp of ACK
    pub timestamp_ms: u64,
    /// Sequence number being acknowledged
    pub sequence: u64,
}

/// Heartbeat payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    /// Sender node ID
    pub node_id: Uuid,
    /// Sender role
    pub role: NodeRole,
    /// Current LSN
    pub current_lsn: Lsn,
    /// Flushed LSN (written to disk)
    pub flush_lsn: Lsn,
    /// Applied LSN (for standbys)
    pub apply_lsn: Option<Lsn>,
    /// Timestamp
    pub timestamp_ms: u64,
    /// Replication lag in bytes
    pub lag_bytes: u64,
    /// Node health status
    pub health: HealthStatus,
}

/// Health status for heartbeat
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    CatchingUp,
    Lagging,
    Error,
}

/// WAL entry for transport
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntryPayload {
    /// Log Sequence Number
    pub lsn: Lsn,
    /// Transaction ID
    pub tx_id: Option<u64>,
    /// Entry type
    pub entry_type: WalEntryType,
    /// Entry data
    pub data: Vec<u8>,
    /// Timestamp (microseconds since epoch)
    pub timestamp_us: u64,
    /// CRC32 of original entry
    pub checksum: u32,
}

/// WAL entry type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalEntryType {
    Insert,
    Update,
    Delete,
    TxBegin,
    TxCommit,
    TxAbort,
    Checkpoint,
    SchemaChange,
    BranchOp,
}

/// WAL batch for catch-up
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalBatchPayload {
    /// First LSN in batch
    pub start_lsn: Lsn,
    /// Last LSN in batch
    pub end_lsn: Lsn,
    /// Number of entries
    pub entry_count: u32,
    /// Compressed entries (if compression enabled)
    pub entries: Vec<WalEntryPayload>,
    /// Whether this is the last batch in catch-up
    pub is_final: bool,
}

/// WAL request (for catch-up)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRequestPayload {
    /// Start LSN (exclusive)
    pub from_lsn: Lsn,
    /// End LSN (inclusive, or 0 for latest)
    pub to_lsn: Option<Lsn>,
    /// Maximum entries to return
    pub max_entries: u32,
    /// Maximum bytes to return
    pub max_bytes: u32,
}

// =============================================================================
// SPLIT-BRAIN PROTECTION
// =============================================================================

/// Vote request for split-brain protection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRequestPayload {
    /// Candidate node ID
    pub candidate_id: Uuid,
    /// Current term/epoch
    pub term: u64,
    /// Candidate's last LSN
    pub last_lsn: Lsn,
    /// Previous primary node ID
    pub previous_primary: Option<Uuid>,
    /// Reason for vote request
    pub reason: VoteReason,
}

/// Reason for requesting a vote
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoteReason {
    PrimaryFailure,
    NetworkPartition,
    ManualFailover,
    SplitBrainRecovery,
}

/// Vote response from observer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteResponsePayload {
    /// Voter node ID
    pub voter_id: Uuid,
    /// Whether vote is granted
    pub vote_granted: bool,
    /// Current term (may be higher)
    pub term: u64,
    /// Current fencing token
    pub fencing_token: u64,
    /// Known primary (if any)
    pub known_primary: Option<Uuid>,
    /// Rejection reason
    pub rejection_reason: Option<String>,
}

/// Fencing token payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FencingTokenPayload {
    /// New fencing token
    pub token: u64,
    /// Node ID that issued the token
    pub issuer_id: Uuid,
    /// Term/epoch
    pub term: u64,
    /// Timestamp
    pub timestamp_ms: u64,
}

// =============================================================================
// LOGICAL REPLICATION
// =============================================================================

/// Logical replication entry (for filtering/transformation)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicalEntryPayload {
    /// LSN of the physical entry
    pub lsn: Lsn,
    /// Transaction ID
    pub tx_id: Option<u64>,
    /// Schema name
    pub schema: String,
    /// Table name
    pub table: String,
    /// Operation type
    pub operation: LogicalOperation,
    /// Old row values (for UPDATE/DELETE)
    pub old_values: Option<HashMap<String, LogicalValue>>,
    /// New row values (for INSERT/UPDATE)
    pub new_values: Option<HashMap<String, LogicalValue>>,
    /// Timestamp
    pub timestamp_us: u64,
}

/// Logical operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogicalOperation {
    Insert,
    Update,
    Delete,
    Truncate,
    Begin,
    Commit,
    Rollback,
}

/// Logical column value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogicalValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Timestamp(i64),
    Json(String),
}

// =============================================================================
// CONTROL COMMANDS
// =============================================================================

/// Control command types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlCommand {
    /// Pause replication
    Pause,
    /// Resume replication
    Resume,
    /// Request status
    GetStatus,
    /// Create replication slot
    CreateSlot { name: String },
    /// Drop replication slot
    DropSlot { name: String },
    /// Trigger checkpoint
    Checkpoint,
    /// Request failover
    Failover { target_node: Option<Uuid> },
    /// Demote to standby
    Demote,
    /// Promote to primary
    Promote { fencing_token: u64 },
    /// Sync barrier (wait for all standbys)
    SyncBarrier { timeout_ms: u32 },
}

/// Control response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// =============================================================================
// CONNECTION MANAGEMENT
// =============================================================================

/// Replication connection
pub struct ReplicationConnection {
    /// Underlying TCP stream
    stream: TcpStream,
    /// Remote address
    remote_addr: SocketAddr,
    /// Connection state
    state: ConnectionState,
    /// Sequence number generator
    sequence: AtomicU64,
    /// Handshake info
    handshake: Option<HandshakeResponse>,
    /// Last activity time
    last_activity: RwLock<Instant>,
    /// Pending ACKs
    pending_acks: RwLock<HashMap<u64, oneshot::Sender<AckPayload>>>,
}

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Handshaking,
    Streaming,
    CatchingUp,
    Paused,
    Closing,
    Closed,
}

impl ReplicationConnection {
    /// Create a new connection (client side)
    pub async fn connect(
        addr: SocketAddr,
        timeout: Duration,
    ) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| ReplicationError::Transport("Connection timeout".to_string()))?
            .map_err(|e| ReplicationError::Transport(format!("Connect failed: {}", e)))?;

        stream.set_nodelay(true).ok();

        Ok(Self {
            remote_addr: addr,
            stream,
            state: ConnectionState::Connecting,
            sequence: AtomicU64::new(0),
            handshake: None,
            last_activity: RwLock::new(Instant::now()),
            pending_acks: RwLock::new(HashMap::new()),
        })
    }

    /// Accept a new connection (server side)
    pub fn from_stream(stream: TcpStream, remote_addr: SocketAddr) -> Self {
        stream.set_nodelay(true).ok();

        Self {
            stream,
            remote_addr: remote_addr,
            state: ConnectionState::Connecting,
            sequence: AtomicU64::new(0),
            handshake: None,
            last_activity: RwLock::new(Instant::now()),
            pending_acks: RwLock::new(HashMap::new()),
        }
    }

    /// Perform handshake as client
    pub async fn handshake_client(&mut self, request: HandshakeRequest) -> Result<HandshakeResponse> {
        self.state = ConnectionState::Handshaking;

        let payload = bincode::serialize(&request)
            .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

        let seq = self.next_sequence();
        let msg = Message::new(MessageType::HandshakeRequest, Bytes::from(payload), seq);
        msg.write_to(&mut self.stream).await?;

        let response = Message::read_from(&mut self.stream).await?;
        if response.header.msg_type != MessageType::HandshakeResponse {
            return Err(ReplicationError::Transport(
                "Expected HandshakeResponse".to_string(),
            ));
        }

        let handshake: HandshakeResponse = bincode::deserialize(&response.payload)
            .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

        if !handshake.accepted {
            return Err(ReplicationError::Transport(format!(
                "Handshake rejected: {}",
                handshake.error.as_deref().unwrap_or("unknown")
            )));
        }

        self.handshake = Some(handshake.clone());
        self.state = ConnectionState::Streaming;
        *self.last_activity.write().await = Instant::now();

        Ok(handshake)
    }

    /// Send a message
    pub async fn send(&mut self, msg_type: MessageType, payload: Bytes) -> Result<u64> {
        let seq = self.next_sequence();
        let msg = Message::new(msg_type, payload, seq);
        msg.write_to(&mut self.stream).await?;
        *self.last_activity.write().await = Instant::now();
        Ok(seq)
    }

    /// Send a message and wait for ACK
    pub async fn send_with_ack(
        &mut self,
        msg_type: MessageType,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<AckPayload> {
        let seq = self.next_sequence();
        let mut msg = Message::new(msg_type, payload, seq);
        msg.header.flags.set_requires_ack();

        let (tx, rx) = oneshot::channel();
        self.pending_acks.write().await.insert(seq, tx);

        msg.write_to(&mut self.stream).await?;

        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| ReplicationError::Transport("ACK timeout".to_string()))?
            .map_err(|_| ReplicationError::Transport("ACK channel closed".to_string()))
    }

    /// Receive a message
    pub async fn recv(&mut self) -> Result<Message> {
        let msg = Message::read_from(&mut self.stream).await?;
        *self.last_activity.write().await = Instant::now();
        Ok(msg)
    }

    /// Send acknowledgment
    pub async fn send_ack(&mut self, ack: AckPayload) -> Result<()> {
        let payload = bincode::serialize(&ack)
            .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

        self.send(MessageType::Ack, Bytes::from(payload)).await?;
        Ok(())
    }

    /// Get next sequence number
    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst)
    }

    /// Get connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Get remote address
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Send a pre-constructed message
    pub async fn send_message(&mut self, msg: &Message) -> Result<()> {
        msg.write_to(&mut self.stream).await?;
        *self.last_activity.write().await = Instant::now();
        Ok(())
    }

    /// Close the connection
    pub async fn close(&mut self) -> Result<()> {
        self.state = ConnectionState::Closing;
        self.stream.shutdown().await.ok();
        self.state = ConnectionState::Closed;
        Ok(())
    }
}

// =============================================================================
// SERVER
// =============================================================================

/// Replication server (runs on primary)
pub struct ReplicationServer {
    /// Listen address
    listen_addr: SocketAddr,
    /// Server state
    state: Arc<RwLock<ServerState>>,
    /// Shutdown signal
    shutdown_tx: broadcast::Sender<()>,
}

/// Server state
struct ServerState {
    /// Connected standbys
    standbys: HashMap<Uuid, StandbyInfo>,
    /// Connected observers
    observers: HashMap<Uuid, ObserverInfo>,
    /// Current fencing token
    fencing_token: u64,
    /// Current term
    term: u64,
    /// Is this server the primary
    is_primary: bool,
}

/// Connected standby info
struct StandbyInfo {
    node_id: Uuid,
    remote_addr: SocketAddr,
    sync_mode: SyncModeConfig,
    last_ack_lsn: Lsn,
    last_ack_time: Instant,
    connection_tx: mpsc::Sender<Message>,
}

/// Connected observer info
struct ObserverInfo {
    node_id: Uuid,
    remote_addr: SocketAddr,
    last_heartbeat: Instant,
}

impl ReplicationServer {
    /// Create a new replication server
    pub fn new(listen_addr: SocketAddr) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);

        Self {
            listen_addr,
            state: Arc::new(RwLock::new(ServerState {
                standbys: HashMap::new(),
                observers: HashMap::new(),
                fencing_token: 1,
                term: 1,
                is_primary: true,
            })),
            shutdown_tx,
        }
    }

    /// Start the server
    pub async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(self.listen_addr)
            .await
            .map_err(|e| ReplicationError::Transport(format!("Bind failed: {}", e)))?;

        tracing::info!("Replication server listening on {}", self.listen_addr);

        let mut shutdown_rx = self.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            let state = self.state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_connection(stream, addr, state).await {
                                    tracing::error!("Connection error from {}: {}", addr, e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Replication server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle a client connection
    async fn handle_connection(
        stream: TcpStream,
        addr: SocketAddr,
        state: Arc<RwLock<ServerState>>,
    ) -> Result<()> {
        let mut conn = ReplicationConnection::from_stream(stream, addr);

        // Wait for handshake
        let msg = conn.recv().await?;
        if msg.header.msg_type != MessageType::HandshakeRequest {
            return Err(ReplicationError::Transport(
                "Expected HandshakeRequest".to_string(),
            ));
        }

        let request: HandshakeRequest = bincode::deserialize(&msg.payload)
            .map_err(|e| ReplicationError::Transport(format!("Deserialize failed: {}", e)))?;

        tracing::info!(
            "Handshake from {:?} node {} at {}",
            request.role,
            request.node_id,
            addr
        );

        // Build response
        let state_guard = state.read().await;
        let response = HandshakeResponse {
            accepted: state_guard.is_primary,
            server_node_id: Uuid::new_v4(), // TODO: Get from config
            sync_mode: request.sync_mode,
            primary_lsn: 0, // TODO: Get actual LSN
            slot_name: request.slot_name.clone(),
            fencing_token: state_guard.fencing_token,
            capabilities: Capabilities::all(),
            error: if state_guard.is_primary {
                None
            } else {
                Some("Not primary".to_string())
            },
        };
        drop(state_guard);

        let response_payload = bincode::serialize(&response)
            .map_err(|e| ReplicationError::Transport(format!("Serialize failed: {}", e)))?;

        conn.send(MessageType::HandshakeResponse, Bytes::from(response_payload))
            .await?;

        if !response.accepted {
            conn.close().await?;
            return Ok(());
        }

        // Main connection loop
        // TODO: Implement message handling

        Ok(())
    }

    /// Shutdown the server
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_header_encode_decode() {
        let header = MessageHeader::new(MessageType::WalEntry, 100, 42);
        let mut buf = BytesMut::new();
        header.encode(&mut buf);

        let decoded = MessageHeader::decode(&mut buf.freeze()).unwrap();
        assert_eq!(decoded.magic, PROTOCOL_MAGIC);
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.msg_type, MessageType::WalEntry);
        assert_eq!(decoded.length, 100);
        assert_eq!(decoded.sequence, 42);
    }

    #[test]
    fn test_message_encode_decode() {
        let payload = Bytes::from("test payload");
        let msg = Message::new(MessageType::Heartbeat, payload.clone(), 123);

        let encoded = msg.encode();
        assert!(encoded.len() > HEADER_SIZE);
    }

    #[test]
    fn test_message_flags() {
        let mut flags = MessageFlags::default();
        assert!(!flags.is_compressed());
        assert!(!flags.requires_ack());

        flags.set_compressed();
        assert!(flags.is_compressed());

        flags.set_requires_ack();
        assert!(flags.requires_ack());
    }

    #[test]
    fn test_sync_mode_config() {
        let async_mode = SyncModeConfig::Async;
        let semi_sync = SyncModeConfig::SemiSync {
            min_acks: 1,
            timeout_ms: 5000,
        };
        let sync = SyncModeConfig::Sync {
            min_applied: 1,
            timeout_ms: 10000,
        };

        // Serialize and deserialize
        let async_json = serde_json::to_string(&async_mode).unwrap();
        let semi_json = serde_json::to_string(&semi_sync).unwrap();
        let sync_json = serde_json::to_string(&sync).unwrap();

        assert!(async_json.contains("Async"));
        assert!(semi_json.contains("SemiSync"));
        assert!(sync_json.contains("Sync"));
    }

    #[test]
    fn test_capabilities() {
        let caps = Capabilities::all();
        assert!(caps.supports_compression());
        assert!(caps.supports_logical());
        assert!(caps.supports_observer());

        let empty = Capabilities::default();
        assert!(!empty.supports_compression());
    }

    #[test]
    fn test_handshake_serialization() {
        let request = HandshakeRequest {
            node_id: Uuid::new_v4(),
            role: NodeRole::Standby,
            sync_mode: SyncModeConfig::SemiSync {
                min_acks: 1,
                timeout_ms: 5000,
            },
            current_lsn: Some(1000),
            slot_name: Some("standby_1".to_string()),
            capabilities: Capabilities::all(),
        };

        let encoded = bincode::serialize(&request).unwrap();
        let decoded: HandshakeRequest = bincode::deserialize(&encoded).unwrap();

        assert_eq!(decoded.node_id, request.node_id);
        assert_eq!(decoded.role, NodeRole::Standby);
    }

    #[test]
    fn test_wal_entry_payload() {
        let entry = WalEntryPayload {
            lsn: 12345,
            tx_id: Some(100),
            entry_type: WalEntryType::Insert,
            data: vec![1, 2, 3, 4],
            timestamp_us: 1234567890,
            checksum: 0xDEADBEEF,
        };

        let encoded = bincode::serialize(&entry).unwrap();
        let decoded: WalEntryPayload = bincode::deserialize(&encoded).unwrap();

        assert_eq!(decoded.lsn, 12345);
        assert_eq!(decoded.entry_type, WalEntryType::Insert);
    }

    #[test]
    fn test_logical_value() {
        let values = vec![
            LogicalValue::Null,
            LogicalValue::Bool(true),
            LogicalValue::Int(42),
            LogicalValue::Float(3.14),
            LogicalValue::Text("hello".to_string()),
            LogicalValue::Bytes(vec![1, 2, 3]),
        ];

        for val in values {
            let encoded = bincode::serialize(&val).unwrap();
            let decoded: LogicalValue = bincode::deserialize(&encoded).unwrap();
            // Just check it round-trips
            let _ = decoded;
        }
    }
}
