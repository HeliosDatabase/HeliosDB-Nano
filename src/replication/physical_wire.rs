//! The replication protocol `NPHYS002`: ONE authenticated connection per
//! receiver carries both the physical data (snapshot chunks, RocksDB write
//! groups) and the control plane — identity, dataset history, epoch,
//! heartbeats and acknowledgements (spec P2.1,
//! `docs/plans/specs/P2_1_single_connection_control_plane.md`).
//!
//! # Why one connection
//!
//! Until P2.1 a standby ran two channels: this one for data and the native v3
//! channel for its handshake, history check, heartbeats and ACKs. Two channels
//! can disagree — a history mismatch only the native handshake could see, a
//! refusal on this one that looked like a dropped socket and was retried
//! forever — and every ACK named a LOGICAL position while the data moved by
//! PHYSICAL sequence. Here every frame of a session is ordered on one socket
//! and names physical sequences only, and a source that will not serve a
//! receiver says why ([`Refusal`]) instead of closing the socket.
//!
//! # Frames
//!
//! A frame is a 16-byte header `MAGIC (8) | VERSION u16 | KIND u16 | LENGTH u32`
//! (big-endian) and a payload whose length every kind bounds BEFORE it is
//! allocated. The session is strictly request/response, driven by the
//! receiver:
//!
//! | Kind | Direction | Payload |
//! |---|---|---|
//! | `Auth` | R→S | the 32-byte token |
//! | `AuthOk` | S→R | — |
//! | `Hello` | R→S | [`Hello`]: node id, role, history, epoch, durable cursor, capabilities, PG port, sync mode |
//! | `HelloOk` | S→R | [`HelloOk`]: node id, history, epoch, barrier, capabilities, replication endpoint, PG port |
//! | `Refuse` | S→R | [`Refusal`]: a [`RefuseCode`] and a reason; the session ends |
//! | `Open` | R→S | snapshot, or resume `{history, after}` |
//! | `Manifest` / `ResumeInfo` | S→R | the session the source opened |
//! | `ChunkRequest` / `Chunk` | R→S / S→R | one bounded, digested piece of a checkpoint file |
//! | `PollRequest` | R→S | `{session, after, byte limit}` |
//! | `PollStart`, `Batch`*, `PollEnd` | S→R | a contiguous physical interval |
//! | `Heartbeat` | R→S | [`Heartbeat`] = the ACK: applied and durable sequence |
//! | `HeartbeatAck` | S→R | [`HeartbeatAck`]: the source's sequence and clock |
//! | `Fence` | either | [`Fence`]: reserved for promotion (P5); refused by both sides now |
//! | `Close` / `Closed` | R→S / S→R | orderly end |
//!
//! Kind 13 (`AuthRejected` of `NPHYS001`) is retired: a wrong token is
//! `Refuse { AuthFailed }`. A peer that speaks `NPHYS001` is answered with
//! `Refuse { Version }` and reported as [`WireError::Version`], naming both.
//!
//! # Trust
//!
//! Plaintext bearer authentication on an explicit loopback socket, behind an
//! authenticated tunnel for remote peers; TLS arrives with P2.2. The token is
//! checked in constant time before any other frame is read; the export
//! factory is never invoked for an unauthenticated, refused or malformed
//! session. Installation verifies every complete file against its manifest
//! digest; the per-chunk digest only checks framing and corruption.

use super::checkpoint_transfer::{CheckpointManifest, TransferLimits};
use super::physical::{PhysicalBatch, PhysicalPoll, MAX_BATCH_BYTES};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use uuid::Uuid;
use zeroize::Zeroizing;

/// The protocol this module speaks, as its peers name it.
pub const PROTOCOL: &str = "NPHYS002";
const MAGIC: [u8; 8] = *b"NPHYS002";
const VERSION: u16 = 2;
/// Every version of this protocol starts its magic with these bytes, so a
/// peer of another version is told so by name instead of "garbage".
const MAGIC_FAMILY: &[u8; 5] = b"NPHYS";
const MAX_SOURCE_SEQUENCE: u64 = (1_u64 << 56) - 1;
const HEADER_BYTES: usize = 16;
const TOKEN_BYTES: usize = 32;
/// Length in bytes of a replication token: 32 raw random bytes.
pub const REPLICATION_TOKEN_BYTES: usize = TOKEN_BYTES;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const CHUNK_META: usize = 16 + 4 + 8 + 32;
const BATCH_META: usize = 16 + 8 + 32;
/// `Hello`: node id 16, role 1, history 16, epoch 8, cursor flag 1 + 8,
/// capabilities 8, PG port 2, sync mode 1.
const HELLO_BYTES: usize = 61;
/// `HelloOk` without its endpoint: node id 16, history 16, epoch 8, barrier
/// 8, capabilities 8, PG port 2, endpoint length 2.
const HELLO_OK_META: usize = 60;
const MAX_ENDPOINT_BYTES: usize = 255;
/// A refusal's reason is for the operator's log and the standby's posture;
/// longer text is cut at a character boundary.
pub const MAX_REASON_BYTES: usize = 1024;
/// `Refuse`: code 2, the largest code's fields (`Gap`: 24), reason length 2.
const REFUSE_META_MAX: usize = 2 + 24 + 2;
const HEARTBEAT_BYTES: usize = 24;
const HEARTBEAT_ACK_BYTES: usize = 16;
const FENCE_BYTES: usize = 8;
/// A drained payload of a peer this source refuses is at most this large.
const MAX_DRAIN_BYTES: usize = 64 * 1024;

/// The receiver sends `Heartbeat` frames and the source answers them: the
/// acknowledgement channel synchronous replication waits on.
pub const CAP_HEARTBEAT: u64 = 1;
/// Reserved for fencing at promotion (P5). Never offered by this release.
pub const CAP_FENCE: u64 = 1 << 1;
/// What this release offers.
pub const CAPABILITIES: u64 = CAP_HEARTBEAT;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("physical transfer I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("physical transfer protocol: {0}")]
    Protocol(String),
    /// The session was refused, by the peer or (on the source) by this node:
    /// the refusal frame was sent and the session ended.
    #[error("physical replication refused: {0}")]
    Refused(Refusal),
    /// The peer speaks another version of this protocol.
    #[error("physical replication protocol version: this node speaks {ours}, the peer {theirs}")]
    Version { ours: &'static str, theirs: String },
    #[error("physical transfer deadline expired")]
    Timeout,
    #[error("physical transfer shutdown requested")]
    Shutdown,
    #[error("physical transfer source: {0}")]
    Source(String),
    #[error("physical transfer session failed; reconnect using durable progress")]
    Poisoned,
}
pub type Result<T> = std::result::Result<T, WireError>;
fn invalid(message: impl Into<String>) -> WireError {
    WireError::Protocol(message.into())
}

impl WireError {
    /// The refusal this error carries, if it is one.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }
}

/// Why a source will not serve a receiver (or a receiver will not follow a
/// source). Spec P2.1 §1: what a receiver does about each is
/// [`RefuseCode::needs_recovery`] / [`RefuseCode::retryable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefuseCode {
    /// The token does not match.
    AuthFailed,
    /// The source is another dataset than the one the receiver follows.
    HistoryMismatch,
    /// The source cannot provide `expected` contiguously: its WAL holds
    /// `have_from..=have_to` around a hole (or the receiver is mid-batch).
    Gap {
        expected: u64,
        have_from: u64,
        have_to: u64,
    },
    /// The WAL the receiver needs is gone: the oldest sequence the source
    /// still retains is `oldest`.
    WalPruned { oldest: u64 },
    /// The source is at epoch `have`, the receiver at `want`.
    EpochStale { have: u64, want: u64 },
    /// The source serves its maximum number of sessions.
    TooManySessions,
    /// The source cannot serve now: shutting down, or failing on its side.
    SourceNotServing,
    /// The peers speak different protocol versions, or a feature this
    /// release does not serve (fencing).
    Version,
}

impl RefuseCode {
    fn id(&self) -> u16 {
        match self {
            Self::AuthFailed => 1,
            Self::HistoryMismatch => 2,
            Self::Gap { .. } => 3,
            Self::WalPruned { .. } => 4,
            Self::EpochStale { .. } => 5,
            Self::TooManySessions => 6,
            Self::SourceNotServing => 7,
            Self::Version => 8,
        }
    }

    /// The code's name, as logs and the posture report it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AuthFailed => "AuthFailed",
            Self::HistoryMismatch => "HistoryMismatch",
            Self::Gap { .. } => "Gap",
            Self::WalPruned { .. } => "WalPruned",
            Self::EpochStale { .. } => "EpochStale",
            Self::TooManySessions => "TooManySessions",
            Self::SourceNotServing => "SourceNotServing",
            Self::Version => "Version",
        }
    }

    /// Reconnecting cannot help: the receiver's posture becomes
    /// `needs_recovery` (it stays up, field finding J2).
    pub fn needs_recovery(&self) -> bool {
        matches!(
            self,
            Self::HistoryMismatch | Self::Gap { .. } | Self::WalPruned { .. } | Self::EpochStale { .. } | Self::Version
        )
    }

    /// The source may serve this receiver later: reconnect with backoff
    /// (posture `reconnecting`). `AuthFailed` is neither: a receiver retries
    /// it a few times (a token file being replaced), then needs recovery.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::TooManySessions | Self::SourceNotServing)
    }
}

impl std::fmt::Display for RefuseCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthFailed => f.write_str("authentication failed"),
            Self::HistoryMismatch => f.write_str("history mismatch"),
            Self::Gap {
                expected,
                have_from,
                have_to,
            } => write!(
                f,
                "WAL gap: the receiver needs sequence {expected}, the source has {have_from}..={have_to}"
            ),
            Self::WalPruned { oldest } => write!(
                f,
                "WAL pruned: the oldest sequence the source still retains is {oldest}"
            ),
            Self::EpochStale { have, want } => {
                write!(f, "stale epoch: the source is at epoch {have}, the receiver at {want}")
            }
            Self::TooManySessions => f.write_str("too many sessions"),
            Self::SourceNotServing => f.write_str("source not serving"),
            Self::Version => f.write_str("protocol version"),
        }
    }
}

/// A [`RefuseCode`] and the reason the refusing side gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefuseCode,
    pub reason: String,
}

impl Refusal {
    /// `reason` is cut to [`MAX_REASON_BYTES`] at a character boundary.
    pub fn new(code: RefuseCode, reason: impl Into<String>) -> Self {
        let mut reason = reason.into();
        if reason.len() > MAX_REASON_BYTES {
            let mut end = MAX_REASON_BYTES;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        Self { code, reason }
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(REFUSE_META_MAX + self.reason.len());
        bytes.extend(self.code.id().to_be_bytes());
        match &self.code {
            RefuseCode::Gap {
                expected,
                have_from,
                have_to,
            } => {
                bytes.extend(expected.to_be_bytes());
                bytes.extend(have_from.to_be_bytes());
                bytes.extend(have_to.to_be_bytes());
            }
            RefuseCode::WalPruned { oldest } => bytes.extend(oldest.to_be_bytes()),
            RefuseCode::EpochStale { have, want } => {
                bytes.extend(have.to_be_bytes());
                bytes.extend(want.to_be_bytes());
            }
            _ => {}
        }
        // `new` bounds the reason; a struct literal may not have.
        let reason = Self::new(self.code.clone(), self.reason.clone()).reason;
        bytes.extend((reason.len() as u16).to_be_bytes());
        bytes.extend(reason.as_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let id = u16_at(bytes, 0)?;
        let (code, fields) = match id {
            1 => (RefuseCode::AuthFailed, 0),
            2 => (RefuseCode::HistoryMismatch, 0),
            3 => (
                RefuseCode::Gap {
                    expected: u64_at(bytes, 2)?,
                    have_from: u64_at(bytes, 10)?,
                    have_to: u64_at(bytes, 18)?,
                },
                24,
            ),
            4 => (
                RefuseCode::WalPruned {
                    oldest: u64_at(bytes, 2)?,
                },
                8,
            ),
            5 => (
                RefuseCode::EpochStale {
                    have: u64_at(bytes, 2)?,
                    want: u64_at(bytes, 10)?,
                },
                16,
            ),
            6 => (RefuseCode::TooManySessions, 0),
            7 => (RefuseCode::SourceNotServing, 0),
            8 => (RefuseCode::Version, 0),
            _ => return Err(invalid("unknown refusal code")),
        };
        let length = usize::from(u16_at(bytes, 2 + fields)?);
        let start = 4 + fields;
        if length > MAX_REASON_BYTES || bytes.len() != start + length {
            return Err(invalid("refusal reason length disagrees with its frame"));
        }
        let reason = bytes.get(start..).ok_or_else(|| invalid("frame truncated"))?;
        let reason = std::str::from_utf8(reason)
            .map_err(|_| invalid("refusal reason is not UTF-8"))?
            .to_string();
        Ok(Self { code, reason })
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.reason.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.reason)
        }
    }
}

/// What a receiver is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRole {
    /// A serving physical standby (`start --replication-role standby`).
    Standby,
    /// The offline `resync` receiver: never serves SQL.
    Resync,
}

impl PeerRole {
    fn code(self) -> u8 {
        match self {
            Self::Standby => 1,
            Self::Resync => 2,
        }
    }

    fn decode(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::Standby),
            2 => Ok(Self::Resync),
            _ => Err(invalid("unknown receiver role")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standby => "standby",
            Self::Resync => "resync",
        }
    }
}

/// The sync mode a receiver was configured with: informational on the source
/// (its own `[replication] sync_mode` decides what a commit waits for).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireSyncMode {
    Async,
    SemiSync,
    Sync,
}

impl WireSyncMode {
    fn code(self) -> u8 {
        match self {
            Self::Async => 0,
            Self::SemiSync => 1,
            Self::Sync => 2,
        }
    }

    fn decode(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Async),
            1 => Ok(Self::SemiSync),
            2 => Ok(Self::Sync),
            _ => Err(invalid("unknown sync mode")),
        }
    }

    /// `async`, `semi-sync` (also `semisync`) or `sync`; anything else is async.
    pub fn from_config(mode: &str) -> Self {
        match mode.to_ascii_lowercase().as_str() {
            "semi-sync" | "semisync" => Self::SemiSync,
            "sync" => Self::Sync,
            _ => Self::Async,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Async => "async",
            Self::SemiSync => "semi-sync",
            Self::Sync => "sync",
        }
    }
}

/// The receiver's identity, sent right after `Auth`. The source checks the
/// history and epoch before anything else is opened, and registers the
/// receiver (the primary's standby registry) from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub node_id: Uuid,
    pub role: PeerRole,
    /// The dataset the receiver follows; `None` while it has no copy yet.
    pub history: Option<Uuid>,
    pub epoch: u64,
    /// The receiver's durable cursor; `None` for a fresh snapshot.
    pub cursor: Option<u64>,
    pub capabilities: u64,
    /// The receiver's PostgreSQL port (0 when it serves none).
    pub pg_port: u16,
    pub sync_mode: WireSyncMode,
}

impl Hello {
    fn validate(&self) -> Result<()> {
        if self.node_id.is_nil()
            || self.epoch == 0
            || self.history.is_some_and(|history| history.is_nil())
            || (self.cursor.is_some() && self.history.is_none())
            || self.cursor.is_some_and(|cursor| cursor > MAX_SOURCE_SEQUENCE)
        {
            return Err(invalid("invalid receiver identity"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(HELLO_BYTES);
        bytes.extend(self.node_id.as_bytes());
        bytes.push(self.role.code());
        bytes.extend(self.history.unwrap_or_else(Uuid::nil).as_bytes());
        bytes.extend(self.epoch.to_be_bytes());
        bytes.push(u8::from(self.cursor.is_some()));
        bytes.extend(self.cursor.unwrap_or(0).to_be_bytes());
        bytes.extend(self.capabilities.to_be_bytes());
        bytes.extend(self.pg_port.to_be_bytes());
        bytes.push(self.sync_mode.code());
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HELLO_BYTES {
            return Err(invalid("receiver identity has the wrong size"));
        }
        let history = uuid_at(bytes, 17)?;
        let cursor = match (byte_at(bytes, 41)?, u64_at(bytes, 42)?) {
            (0, 0) => None,
            (1, cursor) => Some(cursor),
            _ => return Err(invalid("malformed receiver cursor")),
        };
        let hello = Self {
            node_id: uuid_at(bytes, 0)?,
            role: PeerRole::decode(byte_at(bytes, 16)?)?,
            history: (!history.is_nil()).then_some(history),
            epoch: u64_at(bytes, 33)?,
            cursor,
            capabilities: u64_at(bytes, 50)?,
            pg_port: u16_at(bytes, 58)?,
            sync_mode: WireSyncMode::decode(byte_at(bytes, 60)?)?,
        };
        hello.validate()?;
        Ok(hello)
    }
}

/// The source's answer to an admitted `Hello`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloOk {
    pub node_id: Uuid,
    pub history: Uuid,
    pub epoch: u64,
    /// The source's written physical sequence when it answered: a position,
    /// not a durable barrier (a poll's barrier is the durable one).
    pub barrier: u64,
    pub capabilities: u64,
    /// Where the source's replication port listens, as it names it.
    pub physical_endpoint: String,
    /// The source's PostgreSQL port.
    pub pg_port: u16,
}

impl HelloOk {
    fn validate(&self) -> Result<()> {
        if self.node_id.is_nil()
            || self.history.is_nil()
            || self.epoch == 0
            || self.barrier > MAX_SOURCE_SEQUENCE
            || self.physical_endpoint.len() > MAX_ENDPOINT_BYTES
        {
            return Err(invalid("invalid source identity"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(HELLO_OK_META + self.physical_endpoint.len());
        bytes.extend(self.node_id.as_bytes());
        bytes.extend(self.history.as_bytes());
        bytes.extend(self.epoch.to_be_bytes());
        bytes.extend(self.barrier.to_be_bytes());
        bytes.extend(self.capabilities.to_be_bytes());
        bytes.extend(self.pg_port.to_be_bytes());
        bytes.extend((self.physical_endpoint.len() as u16).to_be_bytes());
        bytes.extend(self.physical_endpoint.as_bytes());
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let length = usize::from(u16_at(bytes, 58)?);
        if bytes.len() != HELLO_OK_META + length {
            return Err(invalid("source identity length disagrees with its frame"));
        }
        let endpoint = bytes.get(HELLO_OK_META..).ok_or_else(|| invalid("frame truncated"))?;
        let endpoint = std::str::from_utf8(endpoint)
            .map_err(|_| invalid("source endpoint is not UTF-8"))?
            .to_string();
        let ok = Self {
            node_id: uuid_at(bytes, 0)?,
            history: uuid_at(bytes, 16)?,
            epoch: u64_at(bytes, 32)?,
            barrier: u64_at(bytes, 40)?,
            capabilities: u64_at(bytes, 48)?,
            pg_port: u16_at(bytes, 56)?,
            physical_endpoint: endpoint,
        };
        ok.validate()?;
        Ok(ok)
    }
}

/// Receiver → source, every heartbeat interval and whenever the durable
/// sequence advanced after an apply. It is also THE acknowledgement — one
/// frame, two triggers: synchronous replication waits on `durable_sequence`
/// (semi-sync) or `applied_sequence` (sync).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    /// The source sequence the receiver has applied (visible to its readers).
    pub applied_sequence: u64,
    /// The source sequence the receiver has fsynced. Never above `applied_sequence`.
    pub durable_sequence: u64,
    /// The receiver's wall clock, milliseconds since the Unix epoch.
    pub received_at: u64,
}

/// The acknowledgement is the heartbeat (spec P2.1 §1: one type, two triggers).
pub type Ack = Heartbeat;

impl Heartbeat {
    fn validate(&self) -> Result<()> {
        if self.durable_sequence > self.applied_sequence || self.applied_sequence > MAX_SOURCE_SEQUENCE {
            return Err(invalid("heartbeat durable sequence above its applied sequence"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(HEARTBEAT_BYTES);
        bytes.extend(self.applied_sequence.to_be_bytes());
        bytes.extend(self.durable_sequence.to_be_bytes());
        bytes.extend(self.received_at.to_be_bytes());
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HEARTBEAT_BYTES {
            return Err(invalid("heartbeat has the wrong size"));
        }
        let heartbeat = Self {
            applied_sequence: u64_at(bytes, 0)?,
            durable_sequence: u64_at(bytes, 8)?,
            received_at: u64_at(bytes, 16)?,
        };
        heartbeat.validate()?;
        Ok(heartbeat)
    }
}

/// The source's answer to a heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartbeatAck {
    /// The source's written physical sequence (not a durable barrier).
    pub barrier: u64,
    /// The source's wall clock, milliseconds since the Unix epoch.
    pub now: u64,
}

impl HeartbeatAck {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEARTBEAT_ACK_BYTES);
        bytes.extend(self.barrier.to_be_bytes());
        bytes.extend(self.now.to_be_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HEARTBEAT_ACK_BYTES {
            return Err(invalid("heartbeat acknowledgement has the wrong size"));
        }
        let ack = Self {
            barrier: u64_at(bytes, 0)?,
            now: u64_at(bytes, 8)?,
        };
        if ack.barrier > MAX_SOURCE_SEQUENCE {
            return Err(invalid("heartbeat acknowledgement barrier overflow"));
        }
        Ok(ack)
    }
}

/// Reserved for promotion (P5): "everything below this epoch is fenced".
/// Defined now so its frame has a kind and a codec; refused by both sides in
/// this release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub epoch: u64,
}

impl Fence {
    fn encode(&self) -> Result<Vec<u8>> {
        if self.epoch == 0 {
            return Err(invalid("fence epoch zero"));
        }
        Ok(self.epoch.to_be_bytes().to_vec())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != FENCE_BYTES {
            return Err(invalid("fence has the wrong size"));
        }
        let epoch = u64_at(bytes, 0)?;
        if epoch == 0 {
            return Err(invalid("fence epoch zero"));
        }
        Ok(Self { epoch })
    }
}

/// Explicit 256-bit fixture/operator token. Never Debug/Display or serialized.
/// Operators must generate random bytes; all-zero placeholders are refused.
#[derive(Clone)]
pub struct TransferToken(Zeroizing<[u8; TOKEN_BYTES]>);
impl TransferToken {
    pub fn new(bytes: [u8; TOKEN_BYTES]) -> Result<Self> {
        if bytes == [0; TOKEN_BYTES] {
            return Err(invalid("zero authentication token"));
        }
        Ok(Self(Zeroizing::new(bytes)))
    }
    /// Constant-time check of a supplied token. The length must match exactly;
    /// the comparison itself is `subtle::ConstantTimeEq` in every feature
    /// combination (no crypto feature selection can downgrade it).
    pub fn accepts(&self, supplied: &[u8]) -> bool {
        supplied.len() == TOKEN_BYTES && bool::from(self.0[..].ct_eq(supplied))
    }

    /// The raw 32 token bytes, for presenting the token in a handshake payload.
    /// Never log or Debug these bytes.
    pub fn wire_bytes(&self) -> [u8; TOKEN_BYTES] {
        *self.0
    }
}

impl std::fmt::Debug for TransferToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately redacted: the token is a bearer secret.
        f.write_str("TransferToken(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WireLimits {
    pub frame_timeout: Duration,
    pub operation_timeout: Duration,
    pub transfer: TransferLimits,
}
impl Default for WireLimits {
    fn default() -> Self {
        Self {
            frame_timeout: Duration::from_secs(30),
            operation_timeout: Duration::from_secs(120),
            transfer: TransferLimits::default(),
        }
    }
}
impl WireLimits {
    fn validate(&self) -> Result<()> {
        if self.frame_timeout.is_zero()
            || self.operation_timeout.is_zero()
            || self.frame_timeout > Duration::from_secs(3600)
            || self.operation_timeout > Duration::from_secs(3600)
            || self.transfer.max_files == 0
            || self.transfer.max_files > 2048
            || self.transfer.max_total_bytes == 0
            || self.transfer.max_total_bytes > 1024 * 1024 * 1024 * 1024
            || self.transfer.max_file_bytes == 0
            || self.transfer.max_file_bytes > self.transfer.max_total_bytes
            || self.transfer.max_chunk_bytes == 0
            || self.transfer.max_chunk_bytes > MAX_CHUNK_BYTES
        {
            return Err(invalid("invalid transfer deadlines/chunk bound"));
        }
        Ok(())
    }
}

/// Explicit session choice, sent only after authentication. Resume never creates
/// a new checkpoint and pins the receiver's already durable physical history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenRequest {
    Snapshot,
    Resume { history: Uuid, after: u64 },
}
impl OpenRequest {
    fn encode(self) -> Result<Vec<u8>> {
        match self {
            Self::Snapshot => Ok(vec![0]),
            Self::Resume { history, after } => {
                if history.is_nil() || after > MAX_SOURCE_SEQUENCE {
                    return Err(invalid("invalid resume identity/cursor"));
                }
                let mut bytes = vec![1];
                bytes.extend(history.as_bytes());
                bytes.extend(after.to_be_bytes());
                Ok(bytes)
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let value = match bytes {
            [0] => Self::Snapshot,
            [1, ..] if bytes.len() == 25 => Self::Resume {
                history: uuid_at(bytes, 1)?,
                after: u64_at(bytes, 17)?,
            },
            _ => return Err(invalid("invalid session open request")),
        };
        value.encode()?;
        Ok(value)
    }

    /// The session a receiver may open after `hello`: a snapshot without a
    /// cursor, or a resume of exactly the history and cursor it announced.
    fn agrees_with(self, hello: &Hello) -> bool {
        match self {
            Self::Snapshot => hello.cursor.is_none(),
            Self::Resume { history, after } => hello.history == Some(history) && hello.cursor == Some(after),
        }
    }
}

/// The factory must bind this to the actual source's durable history and storage
/// identity. A Resume factory verifies its cursor against retained physical WAL
/// and a freshly durable barrier; it must not create an unrelated checkpoint.
#[derive(Clone, Debug)]
pub enum SourceSessionInfo {
    Snapshot(CheckpointManifest),
    Resume {
        session_id: Uuid,
        history: Uuid,
        initial_after: u64,
        durable_barrier: u64,
    },
}
impl SourceSessionInfo {
    pub fn session_id(&self) -> Uuid {
        match self {
            Self::Snapshot(m) => m.snapshot_id,
            Self::Resume { session_id, .. } => *session_id,
        }
    }
    pub fn history(&self) -> Uuid {
        match self {
            Self::Snapshot(m) => m.history,
            Self::Resume { history, .. } => *history,
        }
    }
    pub fn initial_after(&self) -> u64 {
        match self {
            Self::Snapshot(m) => m.checkpoint_sequence,
            Self::Resume { initial_after, .. } => *initial_after,
        }
    }
    fn initial_barrier(&self) -> u64 {
        match self {
            Self::Snapshot(m) => m.checkpoint_sequence,
            Self::Resume { durable_barrier, .. } => *durable_barrier,
        }
    }
    pub fn manifest(&self) -> Option<&CheckpointManifest> {
        match self {
            Self::Snapshot(m) => Some(m),
            Self::Resume { .. } => None,
        }
    }
    fn validate(&self, request: OpenRequest, limits: &WireLimits) -> Result<()> {
        match (self, request) {
            (Self::Snapshot(m), OpenRequest::Snapshot) => {
                m.validate(&limits.transfer).map_err(|e| invalid(e.to_string()))
            }
            (
                Self::Resume {
                    session_id,
                    history,
                    initial_after,
                    durable_barrier,
                },
                OpenRequest::Resume {
                    history: expected,
                    after,
                },
            ) if !session_id.is_nil()
                && !history.is_nil()
                && *history == expected
                && *initial_after == after
                && after <= *durable_barrier
                && *durable_barrier <= MAX_SOURCE_SEQUENCE =>
            {
                Ok(())
            }
            _ => Err(invalid("source session identity/cursor/mode mismatch")),
        }
    }
    fn response(&self) -> Result<(Kind, Vec<u8>)> {
        match self {
            Self::Snapshot(m) => Ok((
                Kind::Manifest,
                serde_json::to_vec(m).map_err(|e| invalid(e.to_string()))?,
            )),
            Self::Resume {
                session_id,
                history,
                initial_after,
                durable_barrier,
            } => {
                let mut bytes = session_id.as_bytes().to_vec();
                bytes.extend(history.as_bytes());
                bytes.extend(initial_after.to_be_bytes());
                bytes.extend(durable_barrier.to_be_bytes());
                Ok((Kind::ResumeInfo, bytes))
            }
        }
    }
}

/// Provider methods must be cancellation-safe and own all export resources.
/// Blocking filesystem work should run in an owned spawn_blocking task; a timed
/// out task can finish cleanup but must never publish a receiver or mutate users.
/// A provider that will not serve returns [`WireError::Refused`]; the session
/// then sends that refusal to the receiver before it ends.
#[async_trait]
pub trait PhysicalSource: Send + Sync {
    fn session_info(&self) -> &SourceSessionInfo;
    async fn read_chunk(&self, file_index: u32, offset: u64, length: u32) -> Result<Vec<u8>>;
    async fn poll(&self, after: u64, byte_limit: usize) -> Result<PhysicalPoll>;
}

/// Apply each validated physical batch with its progress (the cursor in the
/// same write) before returning; a sink may defer the fsync of a poll's
/// batches to the poll's end, which its owner then performs before it reports
/// or acknowledges the cursor. Any error/cancellation poisons the wire
/// session. Serving/publication is a distinct owner protocol; receiving
/// PollEnd alone never means SQL-ready.
#[async_trait]
pub trait BatchSink: Send {
    /// One poll's validated summary, before its first batch: the source's
    /// durable barrier is known from here on, even when the batches take long
    /// to apply. Does nothing by default.
    async fn begin(&mut self, _summary: PollSummary) -> Result<()> {
        Ok(())
    }
    async fn apply(&mut self, batch: PhysicalBatch) -> Result<()>;
}

/// What a source tells every receiver it admits (`HelloOk`), and checks a
/// `Hello` against.
#[derive(Clone, Debug)]
pub struct SourceIdentity {
    pub node_id: Uuid,
    pub history: Uuid,
    pub epoch: u64,
    /// The replication port's address, as the source names it.
    pub physical_endpoint: String,
    pub pg_port: u16,
}

/// The source owner's view of the sessions it serves — on a primary, its
/// standby registry, which the commit path's synchronous-replication wait
/// reads (spec P2.1 §2).
pub trait SessionObserver: Send + Sync {
    /// The source's written physical sequence (`HelloOk`, `HeartbeatAck`).
    fn barrier(&self) -> u64;
    /// A receiver passed authentication and its `Hello` was accepted. The
    /// returned session lives exactly as long as the wire session.
    fn admit(&self, peer: Option<SocketAddr>, hello: &Hello) -> Box<dyn ObservedSession>;
}

/// One admitted wire session, as its source owner tracks it. Dropped when the
/// session ends, however it ends.
pub trait ObservedSession: Send {
    /// A heartbeat (= ACK) arrived. Its sequences were checked against what
    /// this session delivered; the observer keeps the maxima, so a late or
    /// stale heartbeat can never move a recorded position back.
    fn heartbeat(&mut self, heartbeat: &Heartbeat);
}

/// A source without a registry: a test fixture, or an export nobody waits on.
pub struct NoSessionObserver;

struct UnobservedSession;
impl ObservedSession for UnobservedSession {
    fn heartbeat(&mut self, _heartbeat: &Heartbeat) {}
}
impl SessionObserver for NoSessionObserver {
    fn barrier(&self) -> u64 {
        0
    }
    fn admit(&self, _peer: Option<SocketAddr>, _hello: &Hello) -> Box<dyn ObservedSession> {
        Box::new(UnobservedSession)
    }
}

/// Everything a source's sessions share: its identity and its observer.
#[derive(Clone)]
pub struct SourceContext {
    pub identity: SourceIdentity,
    pub observer: Arc<dyn SessionObserver>,
}

impl SourceContext {
    /// A source whose sessions nobody observes.
    pub fn unobserved(identity: SourceIdentity) -> Self {
        Self {
            identity,
            observer: Arc::new(NoSessionObserver),
        }
    }
}

/// What a receiver announces about itself in every `Hello`; the history and
/// cursor are its session's.
#[derive(Clone, Debug)]
pub struct ReceiverIdentity {
    pub node_id: Uuid,
    pub role: PeerRole,
    pub epoch: u64,
    pub pg_port: u16,
    pub sync_mode: WireSyncMode,
}

impl ReceiverIdentity {
    pub fn hello(&self, history: Option<Uuid>, cursor: Option<u64>) -> Hello {
        Hello {
            node_id: self.node_id,
            role: self.role,
            history,
            epoch: self.epoch,
            cursor,
            capabilities: CAPABILITIES,
            pg_port: self.pg_port,
            sync_mode: self.sync_mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum Kind {
    Auth = 1,
    AuthOk = 2,
    Open = 3,
    Manifest = 4,
    ChunkRequest = 5,
    Chunk = 6,
    PollRequest = 7,
    PollStart = 8,
    Batch = 9,
    PollEnd = 10,
    Close = 11,
    Closed = 12,
    // 13 was NPHYS001's AuthRejected: a wrong token is `Refuse { AuthFailed }`.
    ResumeInfo = 14,
    Hello = 15,
    HelloOk = 16,
    Refuse = 17,
    Heartbeat = 18,
    HeartbeatAck = 19,
    Fence = 20,
}
impl Kind {
    fn decode(value: u16) -> Result<Self> {
        Ok(match value {
            1 => Self::Auth,
            2 => Self::AuthOk,
            3 => Self::Open,
            4 => Self::Manifest,
            5 => Self::ChunkRequest,
            6 => Self::Chunk,
            7 => Self::PollRequest,
            8 => Self::PollStart,
            9 => Self::Batch,
            10 => Self::PollEnd,
            11 => Self::Close,
            12 => Self::Closed,
            14 => Self::ResumeInfo,
            15 => Self::Hello,
            16 => Self::HelloOk,
            17 => Self::Refuse,
            18 => Self::Heartbeat,
            19 => Self::HeartbeatAck,
            20 => Self::Fence,
            _ => return Err(invalid("unknown frame kind")),
        })
    }
    fn valid_size(self, n: usize) -> bool {
        match self {
            Self::Auth => n == TOKEN_BYTES,
            Self::AuthOk | Self::Close | Self::Closed => n == 0,
            Self::Open => n == 1 || n == 25,
            Self::ResumeInfo => n == 48,
            Self::Manifest => (1..=MAX_MANIFEST_BYTES).contains(&n),
            Self::ChunkRequest => n == 32,
            Self::Chunk => (CHUNK_META + 1..=CHUNK_META + MAX_CHUNK_BYTES).contains(&n),
            Self::PollRequest => n == 28,
            Self::PollStart | Self::PollEnd => n == 20,
            Self::Batch => (BATCH_META + 12..=BATCH_META + MAX_BATCH_BYTES).contains(&n),
            Self::Hello => n == HELLO_BYTES,
            Self::HelloOk => (HELLO_OK_META..=HELLO_OK_META + MAX_ENDPOINT_BYTES).contains(&n),
            Self::Refuse => (4..=REFUSE_META_MAX + MAX_REASON_BYTES).contains(&n),
            Self::Heartbeat => n == HEARTBEAT_BYTES,
            Self::HeartbeatAck => n == HEARTBEAT_ACK_BYTES,
            Self::Fence => n == FENCE_BYTES,
        }
    }
}
struct Frame {
    kind: Kind,
    payload: Vec<u8>,
}
async fn cancelled(shutdown: &mut watch::Receiver<bool>) {
    loop {
        let stopped = *shutdown.borrow_and_update();
        if stopped || shutdown.changed().await.is_err() {
            return;
        }
    }
}
async fn bounded<T>(
    shutdown: &mut watch::Receiver<bool>,
    duration: Duration,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! { biased;
        ()=cancelled(shutdown)=>Err(WireError::Shutdown),
        result=tokio::time::timeout(duration,future)=>result.map_err(|_|WireError::Timeout)?,
    }
}

/// What a header that is not this protocol's most likely is, so an operator
/// who pointed a standby at the wrong port is told which one to use.
fn foreign_peer(header: &[u8; HEADER_BYTES]) -> &'static str {
    if header.starts_with(b"HTTP") {
        "an HTTP server answered: --primary-host must name the primary's --replication-port, not its HTTP port"
    } else if header.starts_with(b"HELI") {
        "the peer speaks the native v3 replication protocol of 4.41.x, removed in this release: upgrade it"
    } else if matches!(header[0], b'E' | b'R' | b'S' | b'N') && header[1..4] == [0, 0, 0] {
        "a PostgreSQL server answered: --primary-host must name the primary's --replication-port, not its \
         PostgreSQL port"
    } else {
        "the peer is not a replication port"
    }
}

/// Check a header's magic and version: another version of this protocol is
/// [`WireError::Version`], naming both; anything else is a protocol error
/// that says what the peer looks like.
fn check_header(header: &[u8; HEADER_BYTES]) -> Result<()> {
    if header[..8] == MAGIC && u16::from_be_bytes([header[8], header[9]]) == VERSION {
        return Ok(());
    }
    if header.starts_with(MAGIC_FAMILY) {
        let theirs = String::from_utf8_lossy(&header[..8]).into_owned();
        return Err(WireError::Version {
            ours: PROTOCOL,
            theirs: if header[..8] == MAGIC {
                format!("{theirs} v{}", u16::from_be_bytes([header[8], header[9]]))
            } else {
                theirs
            },
        });
    }
    Err(invalid(format!(
        "wrong physical transfer magic/version: {}",
        foreign_peer(header)
    )))
}

async fn read_frame<R: AsyncRead + Unpin>(
    io: &mut R,
    allowed: &[Kind],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Frame> {
    bounded(shutdown, limits.frame_timeout, async {
        let mut header = [0u8; HEADER_BYTES];
        io.read_exact(&mut header).await?;
        if let Err(error) = check_header(&header) {
            if matches!(error, WireError::Version { .. }) {
                // Consume the other version's (small) payload, so the refusal
                // this side may send next is not lost to a reset on unread data.
                let length = u32::from_be_bytes([header[12], header[13], header[14], header[15]]) as usize;
                if length <= MAX_DRAIN_BYTES {
                    let mut payload = Zeroizing::new(vec![0; length]);
                    let _ = io.read_exact(payload.as_mut_slice()).await;
                }
            }
            return Err(error);
        }
        let kind = Kind::decode(u16::from_be_bytes([header[10], header[11]]))?;
        let length = u32::from_be_bytes([header[12], header[13], header[14], header[15]]) as usize;
        // Both state and per-kind bounds checked BEFORE any payload allocation.
        if !allowed.contains(&kind) || !kind.valid_size(length) {
            return Err(invalid("unexpected frame or invalid length"));
        }
        let mut payload = vec![0; length];
        io.read_exact(&mut payload).await?;
        Ok(Frame { kind, payload })
    })
    .await
}
async fn write_frame<W: AsyncWrite + Unpin>(
    io: &mut W,
    kind: Kind,
    payload: &[u8],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    if !kind.valid_size(payload.len()) {
        return Err(invalid("outgoing frame exceeds bounds"));
    }
    bounded(shutdown, limits.frame_timeout, async {
        let mut header = [0u8; HEADER_BYTES];
        header[..8].copy_from_slice(&MAGIC);
        header[8..10].copy_from_slice(&VERSION.to_be_bytes());
        header[10..12].copy_from_slice(&(kind as u16).to_be_bytes());
        header[12..16].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        io.write_all(&header).await?;
        io.write_all(payload).await?;
        io.flush().await?;
        Ok(())
    })
    .await
}

/// Read one response of the `expected` kinds. A `Refuse` is the session's
/// end, returned as [`WireError::Refused`]; a `Fence` (promotion, P5) is
/// refused by this receiver as a stale epoch against `epoch`, its own.
async fn read_reply<R: AsyncRead + Unpin>(
    io: &mut R,
    expected: &[Kind],
    epoch: u64,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Frame> {
    let mut allowed = expected.to_vec();
    allowed.extend([Kind::Refuse, Kind::Fence]);
    let frame = read_frame(io, &allowed, limits, shutdown).await?;
    match frame.kind {
        Kind::Refuse => Err(WireError::Refused(Refusal::decode(&frame.payload)?)),
        Kind::Fence => {
            let fence = Fence::decode(&frame.payload)?;
            Err(WireError::Refused(Refusal::new(
                RefuseCode::EpochStale {
                    have: fence.epoch,
                    want: epoch,
                },
                "the source sent a fence; this receiver does not follow fencing until promotion support (P5)",
            )))
        }
        _ => Ok(frame),
    }
}

/// Send `refusal` (best effort: the peer may already be gone), end the
/// session, and return the refusal as this side's error.
async fn refuse<I: AsyncWrite + Unpin>(
    io: &mut I,
    refusal: Refusal,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> WireError {
    let _ = write_frame(io, Kind::Refuse, &refusal.encode(), limits, shutdown).await;
    let _ = bounded(shutdown, limits.frame_timeout, async {
        io.shutdown().await?;
        Ok(())
    })
    .await;
    WireError::Refused(refusal)
}

/// Answer a receiver's `Close` and end the session.
async fn close_session<I: AsyncWrite + Unpin>(
    io: &mut I,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    write_frame(io, Kind::Closed, &[], limits, shutdown).await?;
    bounded(shutdown, limits.frame_timeout, async {
        io.shutdown().await?;
        Ok(())
    })
    .await
}

/// A source failure, as the refusal its receiver is sent: a provider's own
/// refusal as it is, anything else "not serving" (the receiver retries).
/// `None` for a deadline or shutdown, which end the session silently.
fn source_refusal(error: &WireError) -> Option<Refusal> {
    match error {
        WireError::Refused(refusal) => Some(refusal.clone()),
        WireError::Timeout | WireError::Shutdown => None,
        other => Some(Refusal::new(RefuseCode::SourceNotServing, other.to_string())),
    }
}

fn byte_at(bytes: &[u8], offset: usize) -> Result<u8> {
    bytes.get(offset).copied().ok_or_else(|| invalid("frame truncated"))
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let end = offset.checked_add(2).ok_or_else(|| invalid("frame offset overflow"))?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("frame truncated"))?
        .try_into()
        .map_err(|_| invalid("frame truncated"))?;
    Ok(u16::from_be_bytes(raw))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).ok_or_else(|| invalid("frame offset overflow"))?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("frame truncated"))?
        .try_into()
        .map_err(|_| invalid("frame truncated"))?;
    Ok(u32::from_be_bytes(raw))
}
fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset.checked_add(8).ok_or_else(|| invalid("frame offset overflow"))?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("frame truncated"))?
        .try_into()
        .map_err(|_| invalid("frame truncated"))?;
    Ok(u64::from_be_bytes(raw))
}
fn uuid_at(bytes: &[u8], offset: usize) -> Result<Uuid> {
    let end = offset.checked_add(16).ok_or_else(|| invalid("frame offset overflow"))?;
    let raw: [u8; 16] = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("frame truncated"))?
        .try_into()
        .map_err(|_| invalid("frame truncated"))?;
    Ok(Uuid::from_bytes(raw))
}
fn chunk_bounds(
    manifest: &CheckpointManifest,
    index: u32,
    offset: u64,
    length: u32,
    limits: &WireLimits,
) -> Result<()> {
    let file = manifest
        .files
        .get(index as usize)
        .ok_or_else(|| invalid("unknown manifest file index"))?;
    if length == 0
        || length as usize > limits.transfer.max_chunk_bytes
        || offset.checked_add(u64::from(length)).is_none_or(|end| end > file.size)
    {
        return Err(invalid("chunk request outside immutable manifest"));
    }
    Ok(())
}

/// Milliseconds since the Unix epoch (0 before it).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollSummary {
    pub through: u64,
    pub durable_barrier: u64,
    pub batches: u32,
}
impl PollSummary {
    fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20);
        bytes.extend(self.through.to_be_bytes());
        bytes.extend(self.durable_barrier.to_be_bytes());
        bytes.extend(self.batches.to_be_bytes());
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            through: u64_at(bytes, 0)?,
            durable_barrier: u64_at(bytes, 8)?,
            batches: u32_at(bytes, 16)?,
        })
    }
    fn validate_since(self, after: u64, byte_limit: usize, known_barrier: u64) -> Result<()> {
        self.validate(after, byte_limit)?;
        if self.durable_barrier < known_barrier {
            return Err(invalid("source durable barrier regressed within session"));
        }
        Ok(())
    }
    fn validate(self, after: u64, byte_limit: usize) -> Result<()> {
        if self.through < after
            || self.through > self.durable_barrier
            || self.durable_barrier > MAX_SOURCE_SEQUENCE
            || (self.batches == 0 && self.through != self.durable_barrier)
            || (self.batches == 0) != (self.through == after)
            || self.batches as usize > byte_limit / 12
        {
            return Err(invalid("inconsistent physical poll summary"));
        }
        Ok(())
    }
}

/// Explicit opt-in only. Non-loopback binding requires a separate reviewed TLS
/// transport (P2.2); this plaintext listener never accepts a wildcard/remote bind.
pub async fn bind_loopback(address: SocketAddr) -> Result<TcpListener> {
    if !address.ip().is_loopback() {
        return Err(invalid("physical transfer requires loopback plus authenticated tunnel"));
    }
    Ok(TcpListener::bind(address).await?)
}

/// Answer a connection the source does not admit — it already serves its
/// maximum number of sessions — with `refusal` instead of leaving it in the
/// backlog: read its first frame (bounded, so the reply is not lost to a
/// reset on unread data), send the refusal, end. A peer of another protocol
/// version is told that instead. Never reads past the first frame.
pub async fn refuse_session<I>(
    mut io: I,
    refusal: Refusal,
    limits: WireLimits,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send,
{
    limits.validate()?;
    let refusal = match read_frame(&mut io, &[Kind::Auth], &limits, &mut shutdown).await {
        Ok(frame) => {
            drop(Zeroizing::new(frame.payload));
            refusal
        }
        Err(WireError::Version { theirs, .. }) => Refusal::new(
            RefuseCode::Version,
            format!("this source speaks {PROTOCOL}; the receiver spoke {theirs}"),
        ),
        Err(error) => return Err(error),
    };
    Err(refuse(&mut io, refusal, &limits, &mut shutdown).await)
}

/// Owns exactly this accepted socket and one lazily created export. No task is
/// detached here. Any session failure drops the socket/provider; the source DB
/// and other sessions continue under their owner's normal lifecycle. A
/// refusal is sent to the receiver before the session ends.
pub async fn serve_session<I, F, Fut>(
    mut io: I,
    token: &TransferToken,
    limits: WireLimits,
    mut shutdown: watch::Receiver<bool>,
    source: &SourceContext,
    peer: Option<SocketAddr>,
    factory: F,
) -> Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send,
    F: FnOnce(OpenRequest) -> Fut,
    Fut: Future<Output = Result<Arc<dyn PhysicalSource>>>,
{
    limits.validate()?;
    let identity = &source.identity;
    let auth = match read_frame(&mut io, &[Kind::Auth], &limits, &mut shutdown).await {
        Ok(frame) => frame,
        Err(WireError::Version { theirs, .. }) => {
            let refusal = Refusal::new(
                RefuseCode::Version,
                format!("this source speaks {PROTOCOL}; the receiver spoke {theirs}: run the same release on both"),
            );
            return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
        }
        Err(error) => return Err(error),
    };
    let supplied = Zeroizing::new(auth.payload);
    let authorized = token.accepts(&supplied);
    drop(supplied);
    if !authorized {
        let refusal = Refusal::new(
            RefuseCode::AuthFailed,
            "the replication token does not match this source's",
        );
        return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
    }
    write_frame(&mut io, Kind::AuthOk, &[], &limits, &mut shutdown).await?;
    let hello = read_frame(&mut io, &[Kind::Hello, Kind::Close], &limits, &mut shutdown).await?;
    if hello.kind == Kind::Close {
        return close_session(&mut io, &limits, &mut shutdown).await;
    }
    let hello = Hello::decode(&hello.payload)?;
    if let Some(history) = hello.history.filter(|history| *history != identity.history) {
        let refusal = Refusal::new(
            RefuseCode::HistoryMismatch,
            format!(
                "this source serves dataset history {}; the receiver follows {history}",
                identity.history
            ),
        );
        return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
    }
    if hello.epoch != identity.epoch {
        let refusal = Refusal::new(
            RefuseCode::EpochStale {
                have: identity.epoch,
                want: hello.epoch,
            },
            "the receiver followed another epoch of this dataset",
        );
        return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
    }
    // Registered from here until this function returns, however it returns.
    let mut observed = source.observer.admit(peer, &hello);
    let ok = HelloOk {
        node_id: identity.node_id,
        history: identity.history,
        epoch: identity.epoch,
        barrier: source.observer.barrier().min(MAX_SOURCE_SEQUENCE),
        capabilities: CAPABILITIES,
        physical_endpoint: identity.physical_endpoint.clone(),
        pg_port: identity.pg_port,
    };
    write_frame(&mut io, Kind::HelloOk, &ok.encode()?, &limits, &mut shutdown).await?;
    let open = read_frame(&mut io, &[Kind::Open, Kind::Close], &limits, &mut shutdown).await?;
    if open.kind == Kind::Close {
        return close_session(&mut io, &limits, &mut shutdown).await;
    }
    let open = OpenRequest::decode(&open.payload)?;
    if !open.agrees_with(&hello) {
        return Err(invalid("session open request differs from the receiver's hello"));
    }
    // Factory is NEVER invoked for unauthenticated/refused/malformed sessions.
    // Resume providers validate existing source identity/cursor without
    // checkpointing; a provider that will not serve says why.
    let source_session = match bounded(&mut shutdown, limits.operation_timeout, async { factory(open).await }).await {
        Ok(session) => session,
        Err(error) => {
            return Err(match source_refusal(&error) {
                Some(refusal) => refuse(&mut io, refusal, &limits, &mut shutdown).await,
                None => error,
            })
        }
    };
    let info = source_session.session_info().clone();
    info.validate(open, &limits)?;
    let mut known_barrier = info.initial_barrier();
    // What this session has delivered: a heartbeat may not claim more.
    let mut delivered = info.initial_after();
    let (kind, bytes) = info.response()?;
    write_frame(&mut io, kind, &bytes, &limits, &mut shutdown).await?;
    loop {
        let request = read_frame(
            &mut io,
            &[
                Kind::ChunkRequest,
                Kind::PollRequest,
                Kind::Heartbeat,
                Kind::Fence,
                Kind::Close,
            ],
            &limits,
            &mut shutdown,
        )
        .await?;
        match request.kind {
            Kind::ChunkRequest => {
                let manifest = info.manifest().ok_or_else(|| invalid("chunks require snapshot mode"))?;
                if uuid_at(&request.payload, 0)? != manifest.snapshot_id {
                    return Err(invalid("chunk snapshot/session mismatch"));
                }
                let index = u32_at(&request.payload, 16)?;
                let offset = u64_at(&request.payload, 20)?;
                let length = u32_at(&request.payload, 28)?;
                chunk_bounds(manifest, index, offset, length, &limits)?;
                let bytes = bounded(
                    &mut shutdown,
                    limits.operation_timeout,
                    source_session.read_chunk(index, offset, length),
                )
                .await?;
                if bytes.len() != length as usize {
                    return Err(invalid("export returned a short/oversized chunk"));
                }
                let mut payload = Vec::with_capacity(CHUNK_META + bytes.len());
                payload.extend(manifest.snapshot_id.as_bytes());
                payload.extend(index.to_be_bytes());
                payload.extend(offset.to_be_bytes());
                payload.extend(Sha256::digest(&bytes));
                payload.extend(bytes);
                write_frame(&mut io, Kind::Chunk, &payload, &limits, &mut shutdown).await?;
            }
            Kind::PollRequest => {
                if uuid_at(&request.payload, 0)? != info.session_id() {
                    return Err(invalid("poll snapshot/session mismatch"));
                }
                let after = u64_at(&request.payload, 16)?;
                let byte_limit = u32_at(&request.payload, 24)? as usize;
                if after < info.initial_after()
                    || after > MAX_SOURCE_SEQUENCE
                    || byte_limit == 0
                    || byte_limit > MAX_BATCH_BYTES
                {
                    return Err(invalid("invalid physical continuation request"));
                }
                let poll = match bounded(
                    &mut shutdown,
                    limits.operation_timeout,
                    source_session.poll(after, byte_limit),
                )
                .await
                {
                    Ok(poll) => poll,
                    Err(error) => {
                        return Err(match source_refusal(&error) {
                            Some(refusal) => refuse(&mut io, refusal, &limits, &mut shutdown).await,
                            None => error,
                        })
                    }
                };
                let count = u32::try_from(poll.batches.len()).map_err(|_| invalid("too many physical batches"))?;
                let summary = PollSummary {
                    through: poll.through,
                    durable_barrier: poll.durable_barrier,
                    batches: count,
                };
                summary.validate_since(after, byte_limit, known_barrier)?;
                let mut cursor = after;
                let mut bytes = 0usize;
                for batch in &poll.batches {
                    bytes = bytes
                        .checked_add(batch.data().len())
                        .ok_or_else(|| invalid("batch byte overflow"))?;
                    if batch.history() != info.history()
                        || cursor.checked_add(1) != Some(batch.first_sequence())
                        || batch.last_sequence() > summary.through
                        || bytes > byte_limit
                    {
                        return Err(invalid("source returned noncontiguous/oversized physical interval"));
                    }
                    cursor = batch.last_sequence();
                }
                if cursor != summary.through {
                    return Err(invalid("source returned incomplete physical interval"));
                }
                let summary_bytes = summary.encode();
                write_frame(&mut io, Kind::PollStart, &summary_bytes, &limits, &mut shutdown).await?;
                for batch in poll.batches {
                    let mut payload = Vec::with_capacity(BATCH_META + batch.data().len());
                    payload.extend(batch.history().as_bytes());
                    payload.extend(batch.first_sequence().to_be_bytes());
                    payload.extend(batch.digest());
                    payload.extend(batch.data());
                    write_frame(&mut io, Kind::Batch, &payload, &limits, &mut shutdown).await?;
                }
                write_frame(&mut io, Kind::PollEnd, &summary_bytes, &limits, &mut shutdown).await?;
                known_barrier = summary.durable_barrier;
                delivered = delivered.max(summary.through);
            }
            Kind::Heartbeat => {
                let heartbeat = Heartbeat::decode(&request.payload)?;
                // A receiver cannot have applied what this session never sent
                // it: an acknowledgement past that is a broken peer, and
                // recording it would release synchronous commits nothing holds.
                if heartbeat.applied_sequence > delivered {
                    return Err(invalid("heartbeat acknowledges sequences this session never delivered"));
                }
                observed.heartbeat(&heartbeat);
                let ack = HeartbeatAck {
                    barrier: source.observer.barrier().min(MAX_SOURCE_SEQUENCE),
                    now: now_millis(),
                };
                write_frame(&mut io, Kind::HeartbeatAck, &ack.encode(), &limits, &mut shutdown).await?;
            }
            Kind::Fence => {
                let fence = Fence::decode(&request.payload)?;
                let refusal = Refusal::new(
                    RefuseCode::Version,
                    format!(
                        "fencing (epoch {}) is not served by this source until promotion support (P5)",
                        fence.epoch
                    ),
                );
                return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
            }
            Kind::Close => return close_session(&mut io, &limits, &mut shutdown).await,
            _ => return Err(invalid("unexpected request state")),
        }
    }
}

/// Authenticated, ordered client. Any failed operation makes reuse impossible;
/// reconnect with the receiver's durable cursor. No automatic logical fallback.
pub struct PhysicalClient<I> {
    io: I,
    limits: WireLimits,
    shutdown: watch::Receiver<bool>,
    hello: Hello,
    hello_ok: HelloOk,
    info: Option<SourceSessionInfo>,
    known_barrier: u64,
    poisoned: bool,
}
impl<I: AsyncRead + AsyncWrite + Unpin + Send> PhysicalClient<I> {
    /// `Auth`, then `Hello`: an authenticated session whose source accepted
    /// this receiver's identity, history and epoch. A refusal is
    /// [`WireError::Refused`].
    pub async fn authenticate(
        mut io: I,
        token: &TransferToken,
        hello: Hello,
        limits: WireLimits,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<Self> {
        limits.validate()?;
        let hello_bytes = hello.encode()?;
        let reply_limits = WireLimits {
            frame_timeout: limits.operation_timeout,
            ..limits
        };
        write_frame(&mut io, Kind::Auth, &token.0[..], &limits, &mut shutdown).await?;
        read_reply(&mut io, &[Kind::AuthOk], hello.epoch, &reply_limits, &mut shutdown).await?;
        write_frame(&mut io, Kind::Hello, &hello_bytes, &limits, &mut shutdown).await?;
        let frame = read_reply(&mut io, &[Kind::HelloOk], hello.epoch, &reply_limits, &mut shutdown).await?;
        let hello_ok = HelloOk::decode(&frame.payload)?;
        // The source checked both; a source that did not is refused here.
        if let Some(history) = hello.history.filter(|history| *history != hello_ok.history) {
            return Err(WireError::Refused(Refusal::new(
                RefuseCode::HistoryMismatch,
                format!(
                    "the source serves dataset history {}; this receiver follows {history}",
                    hello_ok.history
                ),
            )));
        }
        if hello_ok.epoch != hello.epoch {
            return Err(WireError::Refused(Refusal::new(
                RefuseCode::EpochStale {
                    have: hello_ok.epoch,
                    want: hello.epoch,
                },
                "the source is at another epoch of this dataset",
            )));
        }
        Ok(Self {
            io,
            limits,
            shutdown,
            hello,
            hello_ok,
            info: None,
            known_barrier: 0,
            poisoned: false,
        })
    }
    /// The source's answer to this session's `Hello`.
    pub fn hello_ok(&self) -> &HelloOk {
        &self.hello_ok
    }
    /// Whether the source answers heartbeats (and so receives ACKs).
    pub fn heartbeats(&self) -> bool {
        self.hello_ok.capabilities & self.hello.capabilities & CAP_HEARTBEAT != 0
    }
    pub fn manifest(&self) -> Option<&CheckpointManifest> {
        self.info.as_ref().and_then(SourceSessionInfo::manifest)
    }
    pub fn session_info(&self) -> Option<&SourceSessionInfo> {
        self.info.as_ref()
    }
    fn check_live(&self) -> Result<()> {
        if self.poisoned {
            Err(WireError::Poisoned)
        } else {
            Ok(())
        }
    }
    fn reply_limits(&self) -> WireLimits {
        WireLimits {
            frame_timeout: self.limits.operation_timeout,
            ..self.limits
        }
    }
    pub async fn fetch_manifest(&mut self, expected_history: Option<Uuid>) -> Result<CheckpointManifest> {
        self.check_live()?;
        // Arm BEFORE any await: cancellation/drop can leave a partial frame or
        // durable sink prefix, even when no Result is returned to this caller.
        self.poisoned = true;
        let outcome = async {
            if self.info.is_some() {
                return Err(invalid("manifest already selected for this session"));
            }
            if !OpenRequest::Snapshot.agrees_with(&self.hello) {
                return Err(invalid("a snapshot session announces no cursor in its hello"));
            }
            write_frame(
                &mut self.io,
                Kind::Open,
                &OpenRequest::Snapshot.encode()?,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let limits = self.reply_limits();
            let frame = read_reply(
                &mut self.io,
                &[Kind::Manifest],
                self.hello.epoch,
                &limits,
                &mut self.shutdown,
            )
            .await?;
            let manifest = CheckpointManifest::decode(&frame.payload, &self.limits.transfer)
                .map_err(|e| invalid(e.to_string()))?;
            if expected_history.is_some_and(|expected| expected != manifest.history)
                || manifest.history != self.hello_ok.history
            {
                return Err(invalid("manifest history mismatch"));
            }
            self.known_barrier = manifest.checkpoint_sequence;
            self.info = Some(SourceSessionInfo::Snapshot(manifest.clone()));
            Ok(manifest)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    /// Select a new wire session for an existing durable physical receiver.
    pub async fn resume(&mut self, history: Uuid, after: u64) -> Result<SourceSessionInfo> {
        self.check_live()?;
        self.poisoned = true;
        let outcome = async {
            if self.info.is_some() {
                return Err(invalid("session already selected"));
            }
            let request = OpenRequest::Resume { history, after };
            if !request.agrees_with(&self.hello) {
                return Err(invalid(
                    "a resumed session announces its history and cursor in its hello",
                ));
            }
            write_frame(
                &mut self.io,
                Kind::Open,
                &request.encode()?,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let limits = self.reply_limits();
            let frame = read_reply(
                &mut self.io,
                &[Kind::ResumeInfo],
                self.hello.epoch,
                &limits,
                &mut self.shutdown,
            )
            .await?;
            let info = SourceSessionInfo::Resume {
                session_id: uuid_at(&frame.payload, 0)?,
                history: uuid_at(&frame.payload, 16)?,
                initial_after: u64_at(&frame.payload, 32)?,
                durable_barrier: u64_at(&frame.payload, 40)?,
            };
            info.validate(request, &self.limits)?;
            self.known_barrier = info.initial_barrier();
            self.info = Some(info.clone());
            Ok(info)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    pub async fn connect_resume(
        io: I,
        token: &TransferToken,
        identity: &ReceiverIdentity,
        limits: WireLimits,
        shutdown: watch::Receiver<bool>,
        history: Uuid,
        after: u64,
    ) -> Result<Self> {
        let hello = identity.hello(Some(history), Some(after));
        let mut client = Self::authenticate(io, token, hello, limits, shutdown).await?;
        client.resume(history, after).await?;
        Ok(client)
    }
    pub async fn read_chunk(&mut self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        self.check_live()?;
        // Arm BEFORE any await: cancellation/drop can leave a partial frame or
        // durable sink prefix, even when no Result is returned to this caller.
        self.poisoned = true;
        let outcome = async {
            let manifest = self
                .manifest()
                .ok_or_else(|| invalid("snapshot manifest required before chunk"))?
                .clone();
            chunk_bounds(&manifest, index, offset, length, &self.limits)?;
            let mut request = Vec::with_capacity(32);
            request.extend(manifest.snapshot_id.as_bytes());
            request.extend(index.to_be_bytes());
            request.extend(offset.to_be_bytes());
            request.extend(length.to_be_bytes());
            write_frame(
                &mut self.io,
                Kind::ChunkRequest,
                &request,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let mut frame = read_reply(
                &mut self.io,
                &[Kind::Chunk],
                self.hello.epoch,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let identity_ok = uuid_at(&frame.payload, 0)? == manifest.snapshot_id
                && u32_at(&frame.payload, 16)? == index
                && u64_at(&frame.payload, 20)? == offset
                && frame.payload.len() == CHUNK_META + length as usize;
            let body = frame
                .payload
                .get(CHUNK_META..)
                .ok_or_else(|| invalid("chunk response truncated"))?;
            let expected = frame
                .payload
                .get(28..60)
                .ok_or_else(|| invalid("chunk response truncated"))?;
            if !identity_ok || Sha256::digest(body).as_slice() != expected {
                return Err(invalid("chunk response identity/length/digest mismatch"));
            }
            frame.payload.drain(..CHUNK_META);
            Ok(frame.payload)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    pub async fn poll(&mut self, after: u64, byte_limit: usize, sink: &mut dyn BatchSink) -> Result<PollSummary> {
        self.check_live()?;
        // Arm BEFORE any await: cancellation/drop can leave a partial frame or
        // durable sink prefix, even when no Result is returned to this caller.
        self.poisoned = true;
        let outcome = async {
            let info = self
                .info
                .as_ref()
                .ok_or_else(|| invalid("open session required before continuation"))?
                .clone();
            if after < info.initial_after()
                || after > MAX_SOURCE_SEQUENCE
                || byte_limit == 0
                || byte_limit > MAX_BATCH_BYTES
            {
                return Err(invalid("invalid physical continuation request"));
            }
            let mut request = Vec::with_capacity(28);
            request.extend(info.session_id().as_bytes());
            request.extend(after.to_be_bytes());
            request.extend((byte_limit as u32).to_be_bytes());
            write_frame(
                &mut self.io,
                Kind::PollRequest,
                &request,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let frame = read_reply(
                &mut self.io,
                &[Kind::PollStart],
                self.hello.epoch,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let summary = PollSummary::decode(&frame.payload)?;
            summary.validate_since(after, byte_limit, self.known_barrier)?;
            sink.begin(summary).await?;
            let mut cursor = after;
            let mut bytes = 0usize;
            for _ in 0..summary.batches {
                let mut frame = read_frame(&mut self.io, &[Kind::Batch], &self.limits, &mut self.shutdown).await?;
                let history = uuid_at(&frame.payload, 0)?;
                let first = u64_at(&frame.payload, 16)?;
                let digest: [u8; 32] = frame
                    .payload
                    .get(24..56)
                    .and_then(|slice| slice.try_into().ok())
                    .ok_or_else(|| invalid("batch payload truncated"))?;
                frame.payload.drain(..BATCH_META);
                bytes = bytes
                    .checked_add(frame.payload.len())
                    .ok_or_else(|| invalid("batch byte overflow"))?;
                if history != info.history() || cursor.checked_add(1) != Some(first) || bytes > byte_limit {
                    return Err(invalid("physical response history/sequence/byte mismatch"));
                }
                let batch =
                    PhysicalBatch::decode(history, first, frame.payload, digest).map_err(|e| invalid(e.to_string()))?;
                if batch.last_sequence() > summary.through {
                    return Err(invalid("batch crosses advertised barrier"));
                }
                cursor = batch.last_sequence();
                bounded(&mut self.shutdown, self.limits.operation_timeout, sink.apply(batch)).await?;
            }
            let end = read_frame(&mut self.io, &[Kind::PollEnd], &self.limits, &mut self.shutdown).await?;
            if PollSummary::decode(&end.payload)? != summary || cursor != summary.through {
                return Err(invalid("incomplete physical poll response"));
            }
            self.known_barrier = summary.durable_barrier;
            Ok(summary)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    /// Report this receiver's applied and durable sequences (the ACK) and
    /// learn the source's position and clock. Requires an open session.
    pub async fn heartbeat(&mut self, heartbeat: Heartbeat) -> Result<HeartbeatAck> {
        self.check_live()?;
        self.poisoned = true;
        let outcome = async {
            if self.info.is_none() {
                return Err(invalid("open session required before a heartbeat"));
            }
            write_frame(
                &mut self.io,
                Kind::Heartbeat,
                &heartbeat.encode()?,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let frame = read_reply(
                &mut self.io,
                &[Kind::HeartbeatAck],
                self.hello.epoch,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            HeartbeatAck::decode(&frame.payload)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    pub async fn close(mut self) -> Result<()> {
        self.check_live()?;
        write_frame(&mut self.io, Kind::Close, &[], &self.limits, &mut self.shutdown).await?;
        read_frame(&mut self.io, &[Kind::Closed], &self.limits, &mut self.shutdown).await?;
        bounded(&mut self.shutdown, self.limits.frame_timeout, async {
            self.io.shutdown().await?;
            Ok(())
        })
        .await
    }
}
impl PhysicalClient<TcpStream> {
    pub async fn connect_resume_loopback(
        address: SocketAddr,
        token: &TransferToken,
        identity: &ReceiverIdentity,
        limits: WireLimits,
        shutdown: watch::Receiver<bool>,
        history: Uuid,
        after: u64,
    ) -> Result<Self> {
        let hello = identity.hello(Some(history), Some(after));
        let mut client = Self::connect_loopback(address, token, hello, limits, shutdown).await?;
        client.resume(history, after).await?;
        Ok(client)
    }
    pub async fn connect_loopback(
        address: SocketAddr,
        token: &TransferToken,
        hello: Hello,
        limits: WireLimits,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<Self> {
        if !address.ip().is_loopback() {
            return Err(invalid("plaintext physical transfer requires a loopback tunnel"));
        }
        limits.validate()?;
        let stream = bounded(&mut shutdown, limits.frame_timeout, async {
            Ok(TcpStream::connect(address).await?)
        })
        .await?;
        Self::authenticate(stream, token, hello, limits, shutdown).await
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::ignored_unit_patterns
)]
mod tests {
    use super::super::checkpoint_transfer::CheckpointFile;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::io::{duplex, DuplexStream};

    fn limits() -> WireLimits {
        WireLimits {
            frame_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(3),
            ..Default::default()
        }
    }
    fn token() -> TransferToken {
        TransferToken::new([7; 32]).unwrap()
    }
    fn receiver() -> ReceiverIdentity {
        ReceiverIdentity {
            node_id: Uuid::from_u128(0xA11CE),
            role: PeerRole::Standby,
            epoch: 1,
            pg_port: 5432,
            sync_mode: WireSyncMode::Async,
        }
    }
    /// The `Hello` of a fresh snapshot session.
    fn fresh() -> Hello {
        receiver().hello(None, None)
    }
    fn identity(history: Uuid) -> SourceIdentity {
        SourceIdentity {
            node_id: Uuid::from_u128(0xB0B),
            history,
            epoch: 1,
            physical_endpoint: "127.0.0.1:5433".into(),
            pg_port: 5432,
        }
    }

    /// Records what the source owner sees: admitted hellos, heartbeats, and
    /// the sessions still alive.
    #[derive(Default)]
    struct Recorder {
        hellos: Mutex<Vec<Hello>>,
        heartbeats: Arc<Mutex<Vec<Heartbeat>>>,
        live: Arc<AtomicUsize>,
    }
    struct RecordedSession {
        heartbeats: Arc<Mutex<Vec<Heartbeat>>>,
        live: Arc<AtomicUsize>,
    }
    impl ObservedSession for RecordedSession {
        fn heartbeat(&mut self, heartbeat: &Heartbeat) {
            self.heartbeats.lock().unwrap().push(*heartbeat);
        }
    }
    impl Drop for RecordedSession {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl SessionObserver for Recorder {
        fn barrier(&self) -> u64 {
            42
        }
        fn admit(&self, _peer: Option<SocketAddr>, hello: &Hello) -> Box<dyn ObservedSession> {
            self.hellos.lock().unwrap().push(hello.clone());
            self.live.fetch_add(1, Ordering::SeqCst);
            Box::new(RecordedSession {
                heartbeats: self.heartbeats.clone(),
                live: self.live.clone(),
            })
        }
    }

    /// C2: the token's decimal ("171") and hex ("ab") renderings never appear,
    /// and the redaction marker does.
    #[test]
    fn transfer_token_debug_is_redacted() {
        let token = TransferToken::new([0xAB; 32]).unwrap();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("171"), "decimal token byte leaked: {rendered}");
        let lowered = rendered.to_lowercase();
        assert!(!lowered.contains(&"ab".repeat(32)), "hex token leaked: {rendered}");
        assert!(
            !lowered.contains(&vec!["ab"; 32].join(", ")),
            "hex token leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "missing redaction marker: {rendered}");
    }
    fn header(kind: Kind, length: u32) -> Vec<u8> {
        let mut result = MAGIC.to_vec();
        result.extend(VERSION.to_be_bytes());
        result.extend((kind as u16).to_be_bytes());
        result.extend(length.to_be_bytes());
        result
    }
    struct MockSource {
        manifest: CheckpointManifest,
        info: SourceSessionInfo,
        data: Vec<Vec<u8>>,
        batch: PhysicalBatch,
        chunks: AtomicUsize,
        polls: AtomicUsize,
    }
    impl MockSource {
        fn new() -> Self {
            let history = Uuid::new_v4();
            let identity = Uuid::new_v4().to_string();
            let data = vec![
                b"MANIFEST-000001\n".to_vec(),
                identity.as_bytes().to_vec(),
                b"mock manifest content".to_vec(),
            ];
            let names = ["CURRENT", "IDENTITY", "MANIFEST-000001"];
            let files = data
                .iter()
                .zip(names)
                .map(|(bytes, name)| CheckpointFile {
                    name: name.into(),
                    size: bytes.len() as u64,
                    sha256: Sha256::digest(bytes).into(),
                })
                .collect();
            let manifest = CheckpointManifest {
                format: 1,
                snapshot_id: Uuid::new_v4(),
                history,
                checkpoint_sequence: 10,
                storage_identity: identity,
                files,
                total_bytes: data.iter().map(|v| v.len() as u64).sum(),
            };
            let mut native = rocksdb::WriteBatch::default();
            native.put(b"after", b"continued");
            let mut raw = native.data().to_vec();
            raw[..8].copy_from_slice(&11_u64.to_le_bytes());
            let digest = Sha256::digest(&raw).into();
            let batch = PhysicalBatch::decode(history, 11, raw, digest).unwrap();
            Self {
                info: SourceSessionInfo::Snapshot(manifest.clone()),
                manifest,
                data,
                batch,
                chunks: AtomicUsize::new(0),
                polls: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl PhysicalSource for MockSource {
        fn session_info(&self) -> &SourceSessionInfo {
            &self.info
        }
        async fn read_chunk(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
            self.chunks.fetch_add(1, Ordering::SeqCst);
            Ok(self.data[index as usize][offset as usize..offset as usize + length as usize].to_vec())
        }
        async fn poll(&self, after: u64, _limit: usize) -> Result<PhysicalPoll> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(PhysicalPoll {
                batches: if after == 10 { vec![self.batch.clone()] } else { vec![] },
                through: 11,
                durable_barrier: 11,
            })
        }
    }
    #[derive(Default)]
    struct Sink {
        received: Vec<(Uuid, u64, u64)>,
    }
    #[async_trait]
    impl BatchSink for Sink {
        async fn apply(&mut self, batch: PhysicalBatch) -> Result<()> {
            self.received
                .push((batch.history(), batch.first_sequence(), batch.last_sequence()));
            Ok(())
        }
    }
    async fn server(
        io: DuplexStream,
        source: Arc<MockSource>,
        calls: Arc<AtomicUsize>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let context = SourceContext::unobserved(identity(source.manifest.history));
        serve_session(io, &token(), limits(), shutdown, &context, None, move |_| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(source as Arc<dyn PhysicalSource>)
        })
        .await
    }
    /// A raw receiver's opening: `Auth`, `AuthOk`, `Hello`, `HelloOk`.
    async fn raw_open(io: &mut DuplexStream, hello: &Hello, rx: &mut watch::Receiver<bool>) {
        write_frame(io, Kind::Auth, &token().0[..], &limits(), rx)
            .await
            .unwrap();
        read_frame(io, &[Kind::AuthOk], &limits(), rx).await.unwrap();
        write_frame(io, Kind::Hello, &hello.encode().unwrap(), &limits(), rx)
            .await
            .unwrap();
        read_frame(io, &[Kind::HelloOk], &limits(), rx).await.unwrap();
    }
    /// A raw source's opening: reads `Auth` and `Hello`, answers both for `history`.
    async fn raw_accept(peer: &mut DuplexStream, history: Uuid, rx: &mut watch::Receiver<bool>) -> Result<Hello> {
        read_frame(peer, &[Kind::Auth], &limits(), rx).await?;
        write_frame(peer, Kind::AuthOk, &[], &limits(), rx).await?;
        let hello = Hello::decode(&read_frame(peer, &[Kind::Hello], &limits(), rx).await?.payload)?;
        let ok = HelloOk {
            node_id: Uuid::new_v4(),
            history,
            epoch: 1,
            barrier: 0,
            capabilities: CAPABILITIES,
            physical_endpoint: String::new(),
            pg_port: 0,
        };
        write_frame(peer, Kind::HelloOk, &ok.encode()?, &limits(), rx).await?;
        Ok(hello)
    }
    fn refused<T>(result: &Result<T>) -> Option<RefuseCode> {
        match result {
            Err(WireError::Refused(refusal)) => Some(refusal.code.clone()),
            _ => None,
        }
    }

    // --- the control frames' codecs ---------------------------------------

    #[test]
    fn every_control_frame_round_trips_through_its_codec() {
        let history = Uuid::new_v4();
        for hello in [
            fresh(),
            receiver().hello(Some(history), Some(0)),
            Hello {
                role: PeerRole::Resync,
                sync_mode: WireSyncMode::Sync,
                capabilities: u64::MAX,
                pg_port: 0,
                ..receiver().hello(Some(history), Some(MAX_SOURCE_SEQUENCE))
            },
            Hello {
                sync_mode: WireSyncMode::SemiSync,
                ..receiver().hello(Some(history), None)
            },
        ] {
            let bytes = hello.encode().unwrap();
            assert!(Kind::Hello.valid_size(bytes.len()));
            assert_eq!(Hello::decode(&bytes).unwrap(), hello);
        }
        for endpoint in ["", "127.0.0.1:5433", &"x".repeat(MAX_ENDPOINT_BYTES)] {
            let ok = HelloOk {
                node_id: Uuid::new_v4(),
                history,
                epoch: 7,
                barrier: 123,
                capabilities: CAPABILITIES,
                physical_endpoint: endpoint.to_string(),
                pg_port: 27721,
            };
            let bytes = ok.encode().unwrap();
            assert!(Kind::HelloOk.valid_size(bytes.len()));
            assert_eq!(HelloOk::decode(&bytes).unwrap(), ok);
        }
        for code in [
            RefuseCode::AuthFailed,
            RefuseCode::HistoryMismatch,
            RefuseCode::Gap {
                expected: 5,
                have_from: 9,
                have_to: 12,
            },
            RefuseCode::WalPruned { oldest: 99 },
            RefuseCode::EpochStale { have: 2, want: 3 },
            RefuseCode::TooManySessions,
            RefuseCode::SourceNotServing,
            RefuseCode::Version,
        ] {
            for reason in ["", "why", &"é".repeat(MAX_REASON_BYTES)] {
                let refusal = Refusal::new(code.clone(), reason);
                assert!(refusal.reason.len() <= MAX_REASON_BYTES);
                let bytes = refusal.encode();
                assert!(Kind::Refuse.valid_size(bytes.len()), "{code:?}");
                assert_eq!(Refusal::decode(&bytes).unwrap(), refusal);
            }
        }
        let heartbeat = Heartbeat {
            applied_sequence: 10,
            durable_sequence: 9,
            received_at: now_millis(),
        };
        assert_eq!(Heartbeat::decode(&heartbeat.encode().unwrap()).unwrap(), heartbeat);
        let ack = HeartbeatAck { barrier: 11, now: 12 };
        assert_eq!(HeartbeatAck::decode(&ack.encode()).unwrap(), ack);
        let fence = Fence { epoch: 2 };
        assert_eq!(Fence::decode(&fence.encode().unwrap()).unwrap(), fence);
    }

    #[test]
    fn malformed_control_frames_are_refused_by_their_codecs() {
        let history = Uuid::new_v4();
        // Identities that cannot be: a nil node, epoch zero, a cursor of no
        // history, a cursor past the sequence space.
        for hello in [
            Hello {
                node_id: Uuid::nil(),
                ..fresh()
            },
            Hello { epoch: 0, ..fresh() },
            Hello {
                cursor: Some(3),
                ..fresh()
            },
            receiver().hello(Some(history), Some(MAX_SOURCE_SEQUENCE + 1)),
            Hello {
                history: Some(Uuid::nil()),
                ..fresh()
            },
        ] {
            assert!(hello.encode().is_err(), "{hello:?}");
        }
        let mut bytes = fresh().encode().unwrap();
        bytes[16] = 9; // unknown role
        assert!(Hello::decode(&bytes).is_err());
        let mut bytes = fresh().encode().unwrap();
        bytes[60] = 3; // unknown sync mode
        assert!(Hello::decode(&bytes).is_err());
        let mut bytes = fresh().encode().unwrap();
        bytes[49] = 1; // no cursor, but a cursor value
        assert!(Hello::decode(&bytes).is_err());
        assert!(Hello::decode(&[0; HELLO_BYTES - 1]).is_err());

        let ok = HelloOk {
            node_id: Uuid::new_v4(),
            history,
            epoch: 1,
            barrier: 0,
            capabilities: 0,
            physical_endpoint: "a".into(),
            pg_port: 0,
        };
        let mut bytes = ok.encode().unwrap();
        bytes.push(b'b'); // endpoint length disagrees
        assert!(HelloOk::decode(&bytes).is_err());
        assert!(HelloOk {
            history: Uuid::nil(),
            ..ok.clone()
        }
        .encode()
        .is_err());
        assert!(HelloOk {
            physical_endpoint: "x".repeat(MAX_ENDPOINT_BYTES + 1),
            ..ok
        }
        .encode()
        .is_err());

        let mut bytes = Refusal::new(RefuseCode::Version, "v").encode();
        bytes[0..2].copy_from_slice(&99_u16.to_be_bytes());
        assert!(Refusal::decode(&bytes).is_err(), "unknown code");
        let mut bytes = Refusal::new(RefuseCode::WalPruned { oldest: 1 }, "abc").encode();
        bytes.pop();
        assert!(Refusal::decode(&bytes).is_err(), "truncated reason");
        let mut bytes = Refusal::new(RefuseCode::HistoryMismatch, "ab").encode();
        let last = bytes.len() - 1;
        bytes[last] = 0xff;
        assert!(Refusal::decode(&bytes).is_err(), "reason not UTF-8");

        assert!(Heartbeat {
            applied_sequence: 1,
            durable_sequence: 2,
            received_at: 0,
        }
        .encode()
        .is_err());
        assert!(Heartbeat::decode(&[0; HEARTBEAT_BYTES + 1]).is_err());
        assert!(Fence { epoch: 0 }.encode().is_err());
        assert!(Fence::decode(&0_u64.to_be_bytes()).is_err());
    }

    #[test]
    fn refusal_codes_say_whether_reconnecting_can_help() {
        for code in [
            RefuseCode::HistoryMismatch,
            RefuseCode::Gap {
                expected: 1,
                have_from: 2,
                have_to: 3,
            },
            RefuseCode::WalPruned { oldest: 1 },
            RefuseCode::EpochStale { have: 1, want: 2 },
            RefuseCode::Version,
        ] {
            assert!(code.needs_recovery() && !code.retryable(), "{code:?}");
        }
        for code in [RefuseCode::TooManySessions, RefuseCode::SourceNotServing] {
            assert!(code.retryable() && !code.needs_recovery(), "{code:?}");
        }
        assert!(!RefuseCode::AuthFailed.retryable() && !RefuseCode::AuthFailed.needs_recovery());
        assert!(Refusal::new(RefuseCode::HistoryMismatch, "x")
            .to_string()
            .contains("history mismatch"));
    }

    // --- sessions --------------------------------------------------------

    #[tokio::test]
    async fn authenticated_snapshot_chunks_and_physical_continuation_share_one_session() {
        let source = Arc::new(MockSource::new());
        source.manifest.validate(&limits().transfer).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (_shutdown, rx) = watch::channel(false);
        let (client_io, server_io) = duplex(128);
        let run = tokio::spawn(server(server_io, source.clone(), calls.clone(), rx.clone()));
        let mut client = PhysicalClient::authenticate(client_io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "auth alone must not export");
        assert_eq!(client.hello_ok().history, source.manifest.history);
        assert!(client.heartbeats());
        let manifest = client.fetch_manifest(Some(source.manifest.history)).await.unwrap();
        assert_eq!(manifest.snapshot_id, source.manifest.snapshot_id);
        for (index, bytes) in source.data.iter().enumerate() {
            assert_eq!(
                client.read_chunk(index as u32, 0, bytes.len() as u32).await.unwrap(),
                *bytes
            );
        }
        let mut sink = Sink::default();
        let summary = client.poll(10, MAX_BATCH_BYTES, &mut sink).await.unwrap();
        assert_eq!(
            summary,
            PollSummary {
                through: 11,
                durable_barrier: 11,
                batches: 1
            }
        );
        assert_eq!(sink.received, vec![(source.manifest.history, 11, 11)]);
        let empty = client.poll(11, MAX_BATCH_BYTES, &mut sink).await.unwrap();
        assert_eq!(empty.batches, 0);
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(source.polls.load(Ordering::SeqCst), 2);
    }

    /// Spec P2.1: the acknowledgement travels on the data session. The
    /// source records what the receiver applied and answers with its own
    /// position; a heartbeat may not claim more than the session delivered.
    #[tokio::test]
    async fn heartbeats_reach_the_observer_and_cannot_claim_undelivered_sequences() {
        let source = Arc::new(MockSource::new());
        let recorder = Arc::new(Recorder::default());
        let context = SourceContext {
            identity: identity(source.manifest.history),
            observer: recorder.clone(),
        };
        let (_shutdown, rx) = watch::channel(false);
        let (client_io, server_io) = duplex(256);
        let provider = source.clone();
        let server_rx = rx.clone();
        let run = tokio::spawn(async move {
            serve_session(
                server_io,
                &token(),
                limits(),
                server_rx,
                &context,
                None,
                move |_| async move { Ok(provider as Arc<dyn PhysicalSource>) },
            )
            .await
        });
        let mut client = PhysicalClient::authenticate(client_io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        assert_eq!(client.hello_ok().barrier, 42, "HelloOk carries the observer's barrier");
        assert!(
            client
                .heartbeat(Heartbeat {
                    applied_sequence: 10,
                    durable_sequence: 10,
                    received_at: 1
                })
                .await
                .is_err(),
            "a heartbeat needs an open session"
        );
        drop(client);
        assert!(run.await.unwrap().is_err());
        assert_eq!(recorder.hellos.lock().unwrap().as_slice(), &[fresh()]);
        assert_eq!(recorder.live.load(Ordering::SeqCst), 0, "the session left the registry");

        let context = SourceContext {
            identity: identity(source.manifest.history),
            observer: recorder.clone(),
        };
        let (_shutdown, rx) = watch::channel(false);
        let (client_io, server_io) = duplex(256);
        let provider = source.clone();
        let server_rx = rx.clone();
        let run = tokio::spawn(async move {
            serve_session(
                server_io,
                &token(),
                limits(),
                server_rx,
                &context,
                None,
                move |_| async move { Ok(provider as Arc<dyn PhysicalSource>) },
            )
            .await
        });
        let mut client = PhysicalClient::authenticate(client_io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        client.fetch_manifest(None).await.unwrap();
        // The checkpoint (10) was delivered by the manifest itself.
        let at_checkpoint = Heartbeat {
            applied_sequence: 10,
            durable_sequence: 10,
            received_at: 1,
        };
        let ack = client.heartbeat(at_checkpoint).await.unwrap();
        assert_eq!(ack.barrier, 42);
        assert!(ack.now > 0);
        client.poll(10, MAX_BATCH_BYTES, &mut Sink::default()).await.unwrap();
        let after_poll = Heartbeat {
            applied_sequence: 11,
            durable_sequence: 11,
            received_at: 2,
        };
        client.heartbeat(after_poll).await.unwrap();
        assert_eq!(recorder.live.load(Ordering::SeqCst), 1);
        // 12 was never delivered: the source ends the session.
        assert!(client
            .heartbeat(Heartbeat {
                applied_sequence: 12,
                durable_sequence: 11,
                received_at: 3,
            })
            .await
            .is_err());
        assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
        assert_eq!(
            recorder.heartbeats.lock().unwrap().as_slice(),
            &[at_checkpoint, after_poll]
        );
        assert_eq!(recorder.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn wrong_token_is_refused_with_auth_failed_and_never_invokes_export_factory() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_shutdown, rx) = watch::channel(false);
        let (client_io, server_io) = duplex(128);
        let run = tokio::spawn(server(
            server_io,
            Arc::new(MockSource::new()),
            calls.clone(),
            rx.clone(),
        ));
        let outcome =
            PhysicalClient::authenticate(client_io, &TransferToken::new([8; 32]).unwrap(), fresh(), limits(), rx).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::AuthFailed));
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::AuthFailed));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// Spec P2.1: a source of another dataset (or epoch) refuses the receiver
    /// right after its `Hello`, with a typed code, before any export.
    #[tokio::test]
    async fn history_and_epoch_mismatches_are_refused_after_hello_before_export() {
        for (hello, code) in [
            (
                receiver().hello(Some(Uuid::new_v4()), Some(10)),
                RefuseCode::HistoryMismatch,
            ),
            (
                Hello { epoch: 2, ..fresh() },
                RefuseCode::EpochStale { have: 1, want: 2 },
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorder = Arc::new(Recorder::default());
            let source = Arc::new(MockSource::new());
            let context = SourceContext {
                identity: identity(source.manifest.history),
                observer: recorder.clone(),
            };
            let (_shutdown, rx) = watch::channel(false);
            let (client_io, server_io) = duplex(256);
            let factory_calls = calls.clone();
            let server_rx = rx.clone();
            let run = tokio::spawn(async move {
                serve_session(
                    server_io,
                    &token(),
                    limits(),
                    server_rx,
                    &context,
                    None,
                    move |_| async move {
                        factory_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(source as Arc<dyn PhysicalSource>)
                    },
                )
                .await
            });
            let outcome = PhysicalClient::authenticate(client_io, &token(), hello, limits(), rx).await;
            assert_eq!(refused(&outcome), Some(code.clone()));
            assert_eq!(refused(&run.await.unwrap()), Some(code));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(
                recorder.hellos.lock().unwrap().is_empty(),
                "a refused receiver is never registered"
            );
        }
    }

    /// A provider's refusal (the WAL the receiver needs is gone) reaches the
    /// receiver as that code, not as a dropped socket; any other provider
    /// failure is "not serving", which the receiver retries.
    #[tokio::test]
    async fn provider_refusals_and_failures_are_sent_as_refusals() {
        for (failure, code) in [
            (
                WireError::Refused(Refusal::new(RefuseCode::WalPruned { oldest: 77 }, "pruned")),
                RefuseCode::WalPruned { oldest: 77 },
            ),
            (WireError::Source("export failed".into()), RefuseCode::SourceNotServing),
        ] {
            let history = Uuid::new_v4();
            let context = SourceContext::unobserved(identity(history));
            let (_shutdown, rx) = watch::channel(false);
            let (io, peer) = duplex(256);
            let server_rx = rx.clone();
            let run = tokio::spawn(async move {
                serve_session(
                    peer,
                    &token(),
                    limits(),
                    server_rx,
                    &context,
                    None,
                    move |_| async move { Err::<Arc<dyn PhysicalSource>, _>(failure) },
                )
                .await
            });
            let outcome = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10).await;
            assert_eq!(refused(&outcome), Some(code.clone()));
            assert_eq!(refused(&run.await.unwrap()), Some(code));
        }
    }

    struct RefusingPoll {
        info: SourceSessionInfo,
    }
    #[async_trait]
    impl PhysicalSource for RefusingPoll {
        fn session_info(&self) -> &SourceSessionInfo {
            &self.info
        }
        async fn read_chunk(&self, _: u32, _: u64, _: u32) -> Result<Vec<u8>> {
            panic!("resume must not read chunks")
        }
        async fn poll(&self, after: u64, _: usize) -> Result<PhysicalPoll> {
            Err(WireError::Refused(Refusal::new(
                RefuseCode::Gap {
                    expected: after + 1,
                    have_from: after + 3,
                    have_to: after + 9,
                },
                "hole",
            )))
        }
    }

    #[tokio::test]
    async fn a_poll_refusal_replaces_the_poll_response() {
        let history = Uuid::new_v4();
        let context = SourceContext::unobserved(identity(history));
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(256);
        let server_rx = rx.clone();
        let run = tokio::spawn(async move {
            serve_session(
                peer,
                &token(),
                limits(),
                server_rx,
                &context,
                None,
                move |_| async move {
                    Ok(Arc::new(RefusingPoll {
                        info: SourceSessionInfo::Resume {
                            session_id: Uuid::new_v4(),
                            history,
                            initial_after: 10,
                            durable_barrier: 20,
                        },
                    }) as Arc<dyn PhysicalSource>)
                },
            )
            .await
        });
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
            .await
            .unwrap();
        let mut sink = Sink::default();
        let outcome = client.poll(10, MAX_BATCH_BYTES, &mut sink).await;
        let code = RefuseCode::Gap {
            expected: 11,
            have_from: 13,
            have_to: 19,
        };
        assert_eq!(refused(&outcome), Some(code.clone()));
        assert!(sink.received.is_empty());
        assert_eq!(refused(&run.await.unwrap()), Some(code));
    }

    #[tokio::test]
    async fn over_capacity_connections_are_refused_with_too_many_sessions() {
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(256);
        let server_rx = rx.clone();
        let run = tokio::spawn(async move {
            refuse_session(
                peer,
                Refusal::new(RefuseCode::TooManySessions, "2 sessions"),
                limits(),
                server_rx,
            )
            .await
        });
        let outcome = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::TooManySessions));
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::TooManySessions));
    }

    /// Fencing arrives with promotion (P5): the frame exists, both sides refuse it.
    #[tokio::test]
    async fn fence_is_refused_by_source_and_receiver() {
        // A receiver that sends one is refused with `Version`.
        let source = Arc::new(MockSource::new());
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, peer) = duplex(256);
        let run = tokio::spawn(server(peer, source.clone(), Arc::new(AtomicUsize::new(0)), rx.clone()));
        raw_open(&mut io, &fresh(), &mut rx).await;
        write_frame(&mut io, Kind::Open, &[0], &limits(), &mut rx)
            .await
            .unwrap();
        read_frame(&mut io, &[Kind::Manifest], &limits(), &mut rx)
            .await
            .unwrap();
        write_frame(
            &mut io,
            Kind::Fence,
            &Fence { epoch: 2 }.encode().unwrap(),
            &limits(),
            &mut rx,
        )
        .await
        .unwrap();
        let reply = read_frame(&mut io, &[Kind::Refuse], &limits(), &mut rx).await.unwrap();
        assert_eq!(Refusal::decode(&reply.payload).unwrap().code, RefuseCode::Version);
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version));

        // A source that sends one is refused as a stale epoch.
        let history = Uuid::new_v4();
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(256);
        let mut peer_rx = rx.clone();
        let run = tokio::spawn(async move {
            read_frame(&mut peer, &[Kind::Auth], &limits(), &mut peer_rx).await?;
            write_frame(
                &mut peer,
                Kind::Fence,
                &Fence { epoch: 5 }.encode()?,
                &limits(),
                &mut peer_rx,
            )
            .await
        });
        let outcome =
            PhysicalClient::authenticate(io, &token(), receiver().hello(Some(history), Some(1)), limits(), rx).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::EpochStale { have: 5, want: 1 }));
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_or_pre_auth_data_frames_refuse_before_payload_allocation_or_export() {
        for (kind, length) in [
            (Kind::Auth, u32::MAX),
            (Kind::Open, 0),
            (Kind::Hello, HELLO_BYTES as u32),
            (Kind::Batch, (MAX_BATCH_BYTES + BATCH_META) as u32),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let (_shutdown, rx) = watch::channel(false);
            let (mut io, peer) = duplex(128);
            let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx));
            io.write_all(&header(kind, length)).await.unwrap(); // no payload: a read would hang/fail instead of protocol refusal
            assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }

    /// Spec P2.1: an `NPHYS001` receiver is answered with `Refuse { Version }`
    /// (in this protocol's framing) and the source reports the refusal; any
    /// other magic is a protocol error that says what the peer looks like.
    #[tokio::test]
    async fn previous_version_is_refused_by_name_and_other_magic_before_export() {
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, peer) = duplex(256);
        let calls = Arc::new(AtomicUsize::new(0));
        let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx.clone()));
        let mut old = b"NPHYS001".to_vec();
        old.extend(1_u16.to_be_bytes());
        old.extend(1_u16.to_be_bytes()); // NPHYS001 Auth
        old.extend(32_u32.to_be_bytes());
        old.extend([7; 32]);
        io.write_all(&old).await.unwrap();
        let reply = read_frame(&mut io, &[Kind::Refuse], &limits(), &mut rx).await.unwrap();
        let refusal = Refusal::decode(&reply.payload).unwrap();
        assert_eq!(refusal.code, RefuseCode::Version);
        assert!(
            refusal.reason.contains("NPHYS001") && refusal.reason.contains("NPHYS002"),
            "{refusal}"
        );
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // The same version number under another magic, and a bumped version
        // under this magic.
        for (magic, version) in [(*b"NPHYS001", VERSION), (MAGIC, 9)] {
            let mut bytes = magic.to_vec();
            bytes.extend(version.to_be_bytes());
            bytes.extend(1_u16.to_be_bytes());
            bytes.extend(0_u32.to_be_bytes());
            let mut header = [0; HEADER_BYTES];
            header.copy_from_slice(&bytes);
            let error = check_header(&header).unwrap_err();
            assert!(matches!(error, WireError::Version { .. }), "{error}");
            assert!(error.to_string().contains("NPHYS002"), "{error}");
        }
        for (garbage, hint) in [
            (b"HTTP/1.1 400 Bad R".to_vec(), "HTTP"),
            (
                [
                    b'E', 0, 0, 0, 80, b'S', b'F', b'A', b'T', b'A', b'L', 0, b'M', b'x', 0, 0,
                ]
                .to_vec(),
                "PostgreSQL",
            ),
            (b"HELI\0\0\0\x03\0\0\0\0\0\0\0\0".to_vec(), "native v3"),
        ] {
            let mut header = [0; HEADER_BYTES];
            header.copy_from_slice(&garbage[..HEADER_BYTES]);
            let error = check_header(&header).unwrap_err().to_string();
            assert!(error.contains(hint), "{error}");
        }

        let (_shutdown, rx) = watch::channel(false);
        let (mut io, peer) = duplex(128);
        let calls = Arc::new(AtomicUsize::new(0));
        let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx));
        let mut bytes = header(Kind::Auth, 32);
        bytes[0] ^= 255;
        io.write_all(&bytes).await.unwrap();
        assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn partial_auth_payload_times_out_without_export() {
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, mut peer) = duplex(128);
        io.write_all(&header(Kind::Auth, 32)).await.unwrap();
        let mut short = limits();
        short.frame_timeout = Duration::from_millis(20);
        assert!(matches!(
            read_frame(&mut peer, &[Kind::Auth], &short, &mut rx).await,
            Err(WireError::Timeout)
        ));
    }

    #[tokio::test]
    async fn shutdown_cancels_own_idle_session_without_export() {
        let (shutdown, rx) = watch::channel(false);
        let (_io, peer) = duplex(128);
        let calls = Arc::new(AtomicUsize::new(0));
        let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx));
        shutdown.send(true).unwrap();
        assert!(matches!(run.await.unwrap(), Err(WireError::Shutdown)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn client_bad_chunk_index_or_overflow_poison_session_without_provider_read() {
        for (index, offset, length) in [
            (99, 0, 1),
            (0, u64::MAX, 2),
            (0, 0, 0),
            (0, 0, MAX_CHUNK_BYTES as u32 + 1),
        ] {
            let source = Arc::new(MockSource::new());
            let (_shutdown, rx) = watch::channel(false);
            let (io, peer) = duplex(128);
            let run = tokio::spawn(server(peer, source.clone(), Arc::new(AtomicUsize::new(0)), rx.clone()));
            let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
                .await
                .unwrap();
            client.fetch_manifest(None).await.unwrap();
            assert!(matches!(
                client.read_chunk(index, offset, length).await,
                Err(WireError::Protocol(_))
            ));
            assert!(matches!(client.read_chunk(0, 0, 1).await, Err(WireError::Poisoned)));
            drop(client);
            assert!(run.await.unwrap().is_err());
            assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn server_validates_raw_chunk_request_before_provider_access() {
        let source = Arc::new(MockSource::new());
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, source.clone(), Arc::new(AtomicUsize::new(0)), rx.clone()));
        raw_open(&mut io, &fresh(), &mut rx).await;
        write_frame(&mut io, Kind::Open, &[0], &limits(), &mut rx)
            .await
            .unwrap();
        read_frame(&mut io, &[Kind::Manifest], &limits(), &mut rx)
            .await
            .unwrap();
        let mut payload = source.manifest.snapshot_id.as_bytes().to_vec();
        payload.extend(0_u32.to_be_bytes());
        payload.extend(u64::MAX.to_be_bytes());
        payload.extend(2_u32.to_be_bytes());
        write_frame(&mut io, Kind::ChunkRequest, &payload, &limits(), &mut rx)
            .await
            .unwrap();
        assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
        assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn summaries_cannot_hide_missing_intervals_or_sequence_overflow() {
        assert!(PollSummary {
            through: 10,
            durable_barrier: 11,
            batches: 0
        }
        .validate(10, 1024)
        .is_err());
        assert!(PollSummary {
            through: 11,
            durable_barrier: 10,
            batches: 1
        }
        .validate(10, 1024)
        .is_err());
        assert!(PollSummary {
            through: 11,
            durable_barrier: 11,
            batches: 1000
        }
        .validate(10, 12)
        .is_err());
        assert!(PollSummary {
            through: u64::MAX,
            durable_barrier: u64::MAX,
            batches: 0
        }
        .validate(u64::MAX, 1024)
        .is_err());
        assert!(PollSummary {
            through: 10,
            durable_barrier: 10,
            batches: 0
        }
        .validate(10, 1024)
        .is_ok());
        assert!(TransferToken::new([0; 32]).is_err());
    }

    #[tokio::test]
    async fn untrusted_batch_history_sequence_and_checksum_are_rejected_before_sink() {
        for fault in ["history", "sequence", "digest"] {
            let source = Arc::new(MockSource::new());
            let (_shutdown, rx) = watch::channel(false);
            let (io, mut peer) = duplex(128);
            let peer_source = source.clone();
            let mut peer_shutdown = rx.clone();
            let run = tokio::spawn(async move {
                raw_accept(&mut peer, peer_source.manifest.history, &mut peer_shutdown).await?;
                read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_shutdown).await?;
                let manifest = serde_json::to_vec(&peer_source.manifest).unwrap();
                write_frame(&mut peer, Kind::Manifest, &manifest, &limits(), &mut peer_shutdown).await?;
                read_frame(&mut peer, &[Kind::PollRequest], &limits(), &mut peer_shutdown).await?;
                let summary = PollSummary {
                    through: 11,
                    durable_barrier: 11,
                    batches: 1,
                };
                write_frame(
                    &mut peer,
                    Kind::PollStart,
                    &summary.encode(),
                    &limits(),
                    &mut peer_shutdown,
                )
                .await?;
                let mut payload = peer_source.manifest.history.as_bytes().to_vec();
                payload.extend(11_u64.to_be_bytes());
                payload.extend(peer_source.batch.digest());
                payload.extend(peer_source.batch.data());
                match fault {
                    "history" => payload[..16].copy_from_slice(Uuid::new_v4().as_bytes()),
                    "sequence" => payload[16..24].copy_from_slice(&12_u64.to_be_bytes()),
                    "digest" => payload[24] ^= 255,
                    _ => unreachable!(),
                }
                write_frame(&mut peer, Kind::Batch, &payload, &limits(), &mut peer_shutdown).await
            });
            let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
                .await
                .unwrap();
            client.fetch_manifest(Some(source.manifest.history)).await.unwrap();
            let mut sink = Sink::default();
            assert!(
                matches!(
                    client.poll(10, MAX_BATCH_BYTES, &mut sink).await,
                    Err(WireError::Protocol(_))
                ),
                "{fault}"
            );
            assert!(sink.received.is_empty(), "{fault}");
            assert!(matches!(
                client.poll(10, MAX_BATCH_BYTES, &mut sink).await,
                Err(WireError::Poisoned)
            ));
            drop(client);
            run.await.unwrap().unwrap();
        }
    }

    struct FailedSink;
    #[async_trait]
    impl BatchSink for FailedSink {
        async fn apply(&mut self, _batch: PhysicalBatch) -> Result<()> {
            Err(WireError::Source("injected durable apply failure".into()))
        }
    }

    #[tokio::test]
    async fn sink_failure_prevents_session_reuse_or_successful_poll_completion() {
        let source = Arc::new(MockSource::new());
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, source, Arc::new(AtomicUsize::new(0)), rx.clone()));
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        client.fetch_manifest(None).await.unwrap();
        assert!(matches!(
            client.poll(10, MAX_BATCH_BYTES, &mut FailedSink).await,
            Err(WireError::Source(_))
        ));
        assert!(matches!(
            client.poll(10, MAX_BATCH_BYTES, &mut Sink::default()).await,
            Err(WireError::Poisoned)
        ));
        drop(client);
        assert!(run.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn cancellation_of_inflight_operation_poisoning_survives_future_drop() {
        let history = Uuid::new_v4();
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(128);
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut peer_shutdown = rx.clone();
        let run = tokio::spawn(async move {
            raw_accept(&mut peer, history, &mut peer_shutdown).await?;
            read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_shutdown).await?;
            // Deliberately stop inside a response frame, after the client has
            // sent a request and started awaiting a response.
            peer.write_all(&header(Kind::Manifest, 100)).await?;
            peer.write_all(b"{").await?;
            observed_tx.send(()).unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), release_rx).await;
            Ok::<(), WireError>(())
        });
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        let mut operation = Box::pin(client.fetch_manifest(None));
        tokio::select! {
            result = &mut operation => panic!("operation finished instead of waiting for frame tail: {result:?}"),
            seen = observed_rx => seen.unwrap(),
        }
        drop(operation);
        assert!(matches!(client.fetch_manifest(None).await, Err(WireError::Poisoned)));
        let _ = release_tx.send(());
        drop(client);
        run.await.unwrap().unwrap();
    }
    fn resume_source(original: &MockSource, after: u64) -> MockSource {
        let mut manifest = original.manifest.clone();
        // A new checkpoint would now be ahead of this existing receiver. Resume
        // must not expose/use that unrelated cut as the minimum poll cursor.
        manifest.checkpoint_sequence = 100;
        MockSource {
            info: SourceSessionInfo::Resume {
                session_id: Uuid::new_v4(),
                history: manifest.history,
                initial_after: after,
                durable_barrier: 11,
            },
            manifest,
            data: original.data.clone(),
            batch: original.batch.clone(),
            chunks: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
        }
    }

    #[tokio::test]
    async fn resume_older_cursor_does_not_request_or_depend_on_newer_checkpoint() {
        let source = Arc::new(resume_source(&MockSource::new(), 10));
        let history = source.manifest.history;
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(128);
        let provider = source.clone();
        let server_rx = rx.clone();
        let run = tokio::spawn(async move {
            let context = SourceContext::unobserved(identity(history));
            serve_session(
                peer,
                &token(),
                limits(),
                server_rx,
                &context,
                None,
                move |request| async move {
                    assert_eq!(request, OpenRequest::Resume { history, after: 10 });
                    Ok(provider as Arc<dyn PhysicalSource>)
                },
            )
            .await
        });
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
            .await
            .unwrap();
        assert!(client.manifest().is_none());
        let mut sink = Sink::default();
        assert_eq!(client.poll(10, MAX_BATCH_BYTES, &mut sink).await.unwrap().through, 11);
        assert_eq!(sink.received, vec![(history, 11, 11)]);
        assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn disconnected_snapshot_session_reconnects_at_durable_prefix_without_replaying_it() {
        let source = Arc::new(MockSource::new());
        let history = source.manifest.history;
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, source.clone(), Arc::new(AtomicUsize::new(0)), rx.clone()));
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx.clone())
            .await
            .unwrap();
        client.fetch_manifest(Some(history)).await.unwrap();
        let mut sink = Sink::default();
        let durable = client.poll(10, MAX_BATCH_BYTES, &mut sink).await.unwrap().through;
        drop(client); // transport interruption after the sink owns sequence 11
        assert!(run.await.unwrap().is_err());
        let resumed = Arc::new(resume_source(&source, durable));
        let (io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, resumed, Arc::new(AtomicUsize::new(0)), rx.clone()));
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, durable)
            .await
            .unwrap();
        let summary = client.poll(durable, MAX_BATCH_BYTES, &mut sink).await.unwrap();
        assert_eq!(summary.batches, 0);
        assert_eq!(sink.received, vec![(history, 11, 11)]);
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn source_resume_mode_history_cursor_and_future_barrier_mismatches_fail_before_poll() {
        for fault in ["mode", "history", "cursor", "future"] {
            let original = MockSource::new();
            let history = original.manifest.history;
            let mut source = resume_source(&original, 10);
            source.info = match fault {
                "mode" => SourceSessionInfo::Snapshot(original.manifest.clone()),
                "history" => SourceSessionInfo::Resume {
                    session_id: Uuid::new_v4(),
                    history: Uuid::new_v4(),
                    initial_after: 10,
                    durable_barrier: 11,
                },
                "cursor" => SourceSessionInfo::Resume {
                    session_id: Uuid::new_v4(),
                    history,
                    initial_after: 9,
                    durable_barrier: 11,
                },
                "future" => SourceSessionInfo::Resume {
                    session_id: Uuid::new_v4(),
                    history,
                    initial_after: 10,
                    durable_barrier: 9,
                },
                _ => unreachable!(),
            };
            let source = Arc::new(source);
            let (_shutdown, rx) = watch::channel(false);
            let (io, peer) = duplex(128);
            let run = tokio::spawn(server(peer, source.clone(), Arc::new(AtomicUsize::new(0)), rx.clone()));
            assert!(
                PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
                    .await
                    .is_err(),
                "{fault}"
            );
            assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))), "{fault}");
            assert_eq!(source.polls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn malicious_resume_response_is_independently_validated_by_client() {
        for fault in ["history", "cursor", "future", "nil"] {
            let history = Uuid::new_v4();
            let (_shutdown, rx) = watch::channel(false);
            let (io, mut peer) = duplex(128);
            let mut peer_shutdown = rx.clone();
            let run = tokio::spawn(async move {
                raw_accept(&mut peer, history, &mut peer_shutdown).await?;
                let open = read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_shutdown).await?;
                assert_eq!(
                    OpenRequest::decode(&open.payload)?,
                    OpenRequest::Resume { history, after: 10 }
                );
                let info = SourceSessionInfo::Resume {
                    session_id: if fault == "nil" { Uuid::nil() } else { Uuid::new_v4() },
                    history: if fault == "history" { Uuid::new_v4() } else { history },
                    initial_after: if fault == "cursor" { 9 } else { 10 },
                    durable_barrier: if fault == "future" { 9 } else { 11 },
                };
                let (kind, bytes) = info.response()?;
                write_frame(&mut peer, kind, &bytes, &limits(), &mut peer_shutdown).await
            });
            assert!(
                matches!(
                    PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10).await,
                    Err(WireError::Protocol(_))
                ),
                "{fault}"
            );
            run.await.unwrap().unwrap();
        }
    }

    /// The client refuses a source that answered its `Hello` with another
    /// history, even when the source itself did not.
    #[tokio::test]
    async fn client_refuses_a_hello_ok_of_another_history() {
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(256);
        let mut peer_shutdown = rx.clone();
        let run = tokio::spawn(async move { raw_accept(&mut peer, Uuid::new_v4(), &mut peer_shutdown).await });
        let outcome = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, Uuid::new_v4(), 10).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::HistoryMismatch));
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn resume_cannot_request_snapshot_chunks_and_auth_only_can_close_without_factory() {
        let original = MockSource::new();
        let history = original.manifest.history;
        let source = Arc::new(resume_source(&original, 10));
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(128);
        let calls = Arc::new(AtomicUsize::new(0));
        let run = tokio::spawn(server(peer, source.clone(), calls.clone(), rx.clone()));
        let client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx.clone())
            .await
            .unwrap();
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let (io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, source.clone(), calls, rx.clone()));
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
            .await
            .unwrap();
        assert!(matches!(client.read_chunk(0, 0, 1).await, Err(WireError::Protocol(_))));
        assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
        drop(client);
        assert!(run.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn malformed_open_after_auth_is_refused_before_factory() {
        let history = Uuid::new_v4();
        for (hello, payload) in [
            (fresh(), vec![2]),
            (fresh(), vec![0; 25]),
            (fresh(), {
                let mut bytes = vec![1];
                bytes.extend([0; 16]);
                bytes.extend(10_u64.to_be_bytes());
                bytes
            }),
            // A well-formed open that disagrees with the hello: a resume of a
            // fresh receiver, and a snapshot of a resuming one.
            (fresh(), OpenRequest::Resume { history, after: 10 }.encode().unwrap()),
            (receiver().hello(Some(history), Some(10)), vec![0]),
        ] {
            let (_shutdown, mut rx) = watch::channel(false);
            let (mut io, peer) = duplex(128);
            let calls = Arc::new(AtomicUsize::new(0));
            let source = Arc::new(MockSource::new());
            let context = SourceContext::unobserved(identity(history));
            let factory_calls = calls.clone();
            let server_rx = rx.clone();
            let run = tokio::spawn(async move {
                serve_session(
                    peer,
                    &token(),
                    limits(),
                    server_rx,
                    &context,
                    None,
                    move |_| async move {
                        factory_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(source as Arc<dyn PhysicalSource>)
                    },
                )
                .await
            });
            raw_open(&mut io, &hello, &mut rx).await;
            write_frame(&mut io, Kind::Open, &payload, &limits(), &mut rx)
                .await
                .unwrap();
            assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
    struct RegressingSource {
        info: SourceSessionInfo,
    }
    #[async_trait]
    impl PhysicalSource for RegressingSource {
        fn session_info(&self) -> &SourceSessionInfo {
            &self.info
        }
        async fn read_chunk(&self, _: u32, _: u64, _: u32) -> Result<Vec<u8>> {
            panic!("resume must not read chunks")
        }
        async fn poll(&self, after: u64, _: usize) -> Result<PhysicalPoll> {
            Ok(PhysicalPoll {
                batches: vec![],
                through: after,
                durable_barrier: after,
            })
        }
    }
    #[tokio::test]
    async fn source_barrier_cannot_regress_below_resume_or_previous_poll_observation() {
        let history = Uuid::new_v4();
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(128);
        let source = Arc::new(RegressingSource {
            info: SourceSessionInfo::Resume {
                session_id: Uuid::new_v4(),
                history,
                initial_after: 10,
                durable_barrier: 20,
            },
        });
        let peer_rx = rx.clone();
        let run = tokio::spawn(async move {
            let context = SourceContext::unobserved(identity(history));
            serve_session(peer, &token(), limits(), peer_rx, &context, None, move |_| async move {
                Ok(source as Arc<dyn PhysicalSource>)
            })
            .await
        });
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
            .await
            .unwrap();
        let mut sink = Sink::default();
        assert!(client.poll(10, MAX_BATCH_BYTES, &mut sink).await.is_err());
        assert!(sink.received.is_empty());
        assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
        // A repeated earlier request may not lower a barrier already learned
        // by a successful poll, even when local summary arithmetic is valid.
        assert!(PollSummary {
            through: 10,
            durable_barrier: 10,
            batches: 0
        }
        .validate_since(10, 1024, 11)
        .is_err());
    }
    #[tokio::test]
    async fn peer_cannot_report_false_caught_up_below_opened_resume_barrier() {
        let history = Uuid::new_v4();
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(128);
        let mut peer_rx = rx.clone();
        let run = tokio::spawn(async move {
            raw_accept(&mut peer, history, &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_rx).await?;
            let info = SourceSessionInfo::Resume {
                session_id: Uuid::new_v4(),
                history,
                initial_after: 10,
                durable_barrier: 20,
            };
            let (kind, bytes) = info.response()?;
            write_frame(&mut peer, kind, &bytes, &limits(), &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::PollRequest], &limits(), &mut peer_rx).await?;
            write_frame(
                &mut peer,
                Kind::PollStart,
                &PollSummary {
                    through: 10,
                    durable_barrier: 10,
                    batches: 0,
                }
                .encode(),
                &limits(),
                &mut peer_rx,
            )
            .await
        });
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
            .await
            .unwrap();
        let mut sink = Sink::default();
        assert!(matches!(
            client.poll(10, MAX_BATCH_BYTES, &mut sink).await,
            Err(WireError::Protocol(_))
        ));
        assert!(sink.received.is_empty());
        assert!(matches!(
            client.poll(10, MAX_BATCH_BYTES, &mut sink).await,
            Err(WireError::Poisoned)
        ));
        run.await.unwrap().unwrap();
    }
}
