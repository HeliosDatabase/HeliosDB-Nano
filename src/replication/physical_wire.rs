//! The replication protocol `NPHYS004`: ONE authenticated connection per
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
//! | `Open` | R→S | snapshot (optionally preferring a leased snapshot id: a resumed download, spec P2.2), resume `{history, after, last write group}` (spec P5.1 §11.F), or fast-forward `{history, after, leaf bytes, preferred snapshot}` (spec P4.1) |
//! | `Manifest` / `ResumeInfo` / `FastForwardInfo` | S→R | the session the source opened |
//! | `ChunkRequest` / `Chunk` | R→S / S→R | one bounded, digested piece of a checkpoint file |
//! | `PollRequest` | R→S | `{session, after, byte limit}` |
//! | `PollStart`, `Batch`*, `PollEnd` | S→R | a contiguous physical interval |
//! | `LeafDigestsRequest` / `LeafDigests` | R→S / S→R | one page of the source's leaf plan ([`LeafDigests`]) |
//! | `LeafRequest` / `LeafReady` | R→S / S→R | one leaf's SST file, built: its size and SHA-256 ([`LeafReady`]) |
//! | `LeafChunkRequest` / `LeafChunk` | R→S / S→R | one bounded, digested piece of a leaf file |
//! | `Heartbeat` | R→S | [`Heartbeat`] = the ACK: applied and durable sequence |
//! | `HeartbeatAck` | S→R | [`HeartbeatAck`]: the source's sequence and clock |
//! | `Fence` | R→S | [`Fence`]: a promoting standby fences this source by a higher epoch (spec P5.1) |
//! | `Fenced` | S→R | [`Fenced`]: the source is fenced, at its final sequence |
//! | `Close` / `Closed` | R→S / S→R | orderly end |
//!
//! Kind 13 (`AuthRejected` of `NPHYS001`) is retired: a wrong token is
//! `Refuse { AuthFailed }`. A peer that speaks `NPHYS001`, `NPHYS002` or
//! `NPHYS003` is answered with `Refuse { Version }` and reported as
//! [`WireError::Version`], naming both.
//!
//! Each session kind has its own requests: a snapshot or resume session
//! sends chunk and poll requests, a fast-forward session (spec P4.1) the
//! three leaf requests; any other request is a protocol error that ends the
//! session. A source offers fast-forward sessions with [`CAP_FAST_FORWARD`].
//!
//! # Epochs and fencing (spec P5.1 §2-§3)
//!
//! Both ends check the dataset history first, then the epoch of a receiver
//! that holds data (a `Hello` with a cursor; a fresh snapshot receiver holds
//! no epoch, takes the source's with its copy, and is never compared):
//!
//! | side | comparison | action |
//! |---|---|---|
//! | source | `hello.epoch < identity.epoch` | `Refuse { EpochStale }`: the receiver follows a superseded epoch; it needs recovery |
//! | source | `hello.epoch > identity.epoch` | this source is the superseded one: it fences itself ([`SourceFence`]), then `Refuse { SourceStale }` (retryable: the receiver keeps retrying, so a corrected `--primary-host` takes effect) |
//! | receiver | `hello_ok.epoch > hello.epoch` | `EpochStale` |
//! | receiver | `hello_ok.epoch < hello.epoch` | `SourceStale` |
//!
//! A source that can be fenced (a primary) offers [`CAP_FENCE`]. A receiver
//! fences it with `Fence { epoch }` after `HelloOk` and before `Open`, or
//! between two requests of a snapshot or resume session (never inside a
//! fast-forward); the source answers `Fenced { epoch, final_sequence }` once
//! it refuses every write, made the earlier ones durable and recorded the
//! fence on disk. The session goes on: a fenced source keeps serving resume
//! sessions — the promoting standby tails it through its final sequence —
//! but bootstraps nobody (spec P5.1 §11.E): a snapshot or fast-forward open
//! is refused `SourceNotServing` ("fenced: bootstrap from the new primary").
//! Spec P5.1 §11.C: a fence must come from a receiver of THIS history that
//! holds data (its `Hello` carries the history and a cursor) and name
//! exactly the epoch after the receiver's (`hello.epoch + 1`, which the
//! handshake made the source's own + 1); anything else is `Refuse {
//! Version }`. A second fence by the promotion that applied the first answers
//! the same `Fenced`; a fence by another one is refused (`Version`, naming
//! the epoch and promotion that fenced it first).
//!
//! # A resume is verified by content (spec P5.1 §11.F, `NPHYS004`)
//!
//! A cursor is a NUMBER in its primary's numbering; after a promotion two
//! primaries of the same history and epoch (two `--force` promotions, say)
//! number different writes alike, and a standby of one could resume from
//! the other at "the same" sequence and silently apply the wrong groups. So
//! a resume carries the receiver's last applied write group — its first
//! sequence and SHA-256, from its durable cursor — and the source re-reads
//! the group ending at `after` (from its WAL, or its archive) before it
//! serves the session: a different digest or boundary is `Refuse { Diverged
//! { at } }` (needs recovery: `recover --from-primary` fast-forwards it), a
//! group no longer held is `WalPruned` as before. A cascading standby
//! verifies in the translated (its primary's) numbering. A cursor at a
//! checkpoint cut names no group (it has applied none since the cut) and is
//! resumed unverified, as before. The `Open` frame changed shape for this,
//! so the protocol is `NPHYS004`: an `NPHYS003` peer is refused by version.
//!
//! # Write groups of any size (spec P4.1c, `NPHYS003`)
//!
//! One write group is one `Batch` frame, never split: up to
//! [`MAX_BATCH_BYTES`] of raw group, the most a `u32` length can say. A
//! `PollRequest`'s byte limit is a coalescing target, not a ceiling: the
//! first batch of a poll may exceed it, a later one may not take the poll's
//! total past it. A payload is read and written in steps of `FRAME_IO_STEP`
//! (1 MiB), each bounded by the frame timeout: an idle timeout, so a large
//! frame from a peer that keeps sending never times out, while a peer that
//! stalls does. `NPHYS002` bounded a batch at 64 MiB and a poll's first batch
//! by its limit; the version changed with the bounds, so the two refuse each
//! other by version, not by a late "invalid length".
//!
//! # Trust
//!
//! Bearer authentication in every transport mode. The stream is TLS
//! ([`super::physical_tls`], spec P2.2: required for a non-loopback peer
//! unless the explicit plaintext opt-in is given) or plaintext on loopback;
//! TLS is transport only, so the token is checked over TLS too, mutual TLS
//! included. Both sides set [`CAP_TLS`] in `Hello` / `HelloOk` when their end
//! of the session is TLS. The token is checked in constant time before any
//! other frame is read; the export factory is never invoked for an
//! unauthenticated, refused or malformed session. Installation verifies every
//! complete file against its manifest digest; the per-chunk digest only
//! checks framing and corruption.

use super::checkpoint_transfer::{CheckpointManifest, TransferLimits};
use super::physical::{PhysicalBatch, PhysicalPoll, MAX_BATCH_BYTES};
use super::physical_tls::{remote_transport_allowed, ReplicationStream, ReplicationTransport};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;
use zeroize::Zeroizing;

/// The protocol this module speaks, as its peers name it. Spec P5.1 §11.F:
/// `NPHYS004` (a resume carries its last write group).
pub const PROTOCOL: &str = "NPHYS004";
const MAGIC: [u8; 8] = *b"NPHYS004";
const VERSION: u16 = 4;
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
/// `Fenced`: the epoch it was fenced by 8, its final sequence 8.
const FENCED_BYTES: usize = 16;
/// A drained payload of a peer this source refuses is at most this large.
const MAX_DRAIN_BYTES: usize = 64 * 1024;

/// The receiver sends `Heartbeat` frames and the source answers them: the
/// acknowledgement channel synchronous replication waits on.
pub const CAP_HEARTBEAT: u64 = 1;
/// Spec P5.1 §3.1: the source can be fenced (`Fence` / `Fenced`): a primary
/// that serves its replication port. Set in `HelloOk` by a source that has a
/// [`SourceFence`]; a cascading standby offers none (it is no primary).
pub const CAP_FENCE: u64 = 1 << 1;
/// Spec P2.2: this side's end of the session is TLS. Set per session, by the
/// receiver in its `Hello` and by the source in its `HelloOk`; a session is on
/// TLS (`/health` `"tls": true`) when both set it.
pub const CAP_TLS: u64 = 1 << 2;
/// Spec P4.1 §3.1: the source serves incremental fast-forward sessions
/// (`OpenRequest::FastForward` and the leaf frames). A receiver does not
/// start one with a source whose `HelloOk` lacks it.
pub const CAP_FAST_FORWARD: u64 = 1 << 3;
/// What this release offers on every session ([`CAP_TLS`] is added per
/// session, by its transport).
pub const CAPABILITIES: u64 = CAP_HEARTBEAT | CAP_FAST_FORWARD;

/// Spec P4.1 §3.1: the bounds of a fast-forward's leaf size (`--leaf-bytes`),
/// checked by both sides.
pub const MIN_LEAF_BYTES: u32 = 64 * 1024;
pub const MAX_LEAF_BYTES: u32 = 256 * 1024 * 1024;
/// At most this many leaves partition a source's keyspace; a larger store is
/// fast-forwarded with larger leaves.
pub const MAX_LEAVES: u32 = 1 << 24;
/// Where the replicated keyspace ends: the last leaf ends here on both sides.
/// No key of the engine's keyspace reaches it.
pub const LEAF_END: [u8; 64] = [0xff; 64];
/// The bound of one `LeafDigests` frame: the source pages its plan.
pub const MAX_LEAF_DIGESTS_BYTES: usize = 1024 * 1024;
/// `LeafDigests` header: snapshot id 16, status 1, leaf count 4, total bytes
/// 8, first leaf 4, entry count 4.
pub const LEAF_DIGESTS_HEADER_BYTES: usize = 37;
/// One `LeafDigests` entry without its start key: key length 2, bytes 8,
/// keys 4, digest 32.
pub const LEAF_ENTRY_META_BYTES: usize = 46;
/// The longest key a leaf may start at: its length is a `u16` on the wire.
pub const MAX_LEAF_START_BYTES: usize = u16::MAX as usize;
/// `FastForwardInfo`: session id 16, snapshot id 16, cut 8.
const FAST_FORWARD_INFO_BYTES: usize = 40;
/// `LeafDigestsRequest`: snapshot id 16, first leaf 4, count 4.
const LEAF_DIGESTS_REQUEST_BYTES: usize = 24;
/// `LeafRequest`: snapshot id 16, leaf 4.
const LEAF_REQUEST_BYTES: usize = 20;
/// `LeafReady`: snapshot id 16, leaf 4, size 8, SHA-256 32.
const LEAF_READY_BYTES: usize = 60;
/// `Open` of a fast-forward: tag 1, history 16, cursor 8, leaf bytes 4, and
/// the preferred snapshot 16 when one is named.
const OPEN_FAST_FORWARD_BYTES: usize = 29;
const OPEN_FAST_FORWARD_OF_BYTES: usize = 45;
/// Spec P5.1 §11.F: `Open` of a resume: tag 1, history 16, cursor 8, a flag
/// 1 and the receiver's last write group — its first sequence 8 and SHA-256
/// 32 (zeros with the flag 0: a cursor at a checkpoint cut).
const OPEN_RESUME_BYTES: usize = 66;
/// How often a client re-asks a source that is still planning its leaves.
const LEAF_PLANNING_RETRY: Duration = Duration::from_millis(300);
/// A client waits for a source's leaf plan at most this many operation
/// deadlines in all.
const LEAF_PLANNING_DEADLINES: u32 = 30;
/// The leaves a server remembers as built for its chunk requests; a client
/// downloads one leaf at a time.
const READY_LEAVES_REMEMBERED: usize = 256;

/// Whether `leaf_bytes` is a leaf size both sides accept.
pub fn valid_leaf_bytes(leaf_bytes: u32) -> bool {
    (MIN_LEAF_BYTES..=MAX_LEAF_BYTES).contains(&leaf_bytes)
}

/// SHA-256 of nothing: the digest of an empty leaf, and the file digest a
/// `LeafReady` of size 0 carries.
pub fn empty_sha256() -> [u8; 32] {
    Sha256::digest(b"").into()
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("physical transfer I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Spec P2.2: the TLS handshake, or a TLS alert, ended the connection (a
    /// wrong CA, an untrusted or misnamed certificate, a missing client
    /// certificate). A transport failure: the data is fine, the receiver
    /// reconnects with backoff.
    #[error("tls: {0}")]
    Tls(String),
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
    /// The peers speak different protocol versions, or a request this source
    /// does not serve (a fence of a source that cannot be fenced, or one that
    /// names no higher epoch than the source's or a fence already applied).
    Version,
    /// The write group starting at source sequence `first` is `bytes` raw
    /// bytes, more than one replication or archive frame can carry
    /// (`MAX_BATCH_BYTES`): it can never be served, so asking again cannot
    /// help. The receiver needs recovery past it (a fast-forward or a full
    /// copy from the primary).
    GroupTooLarge { first: u64, bytes: u64 },
    /// Spec P5.1 §2: the SOURCE is the stale side — it is at epoch `have`,
    /// below the receiver's `want`: a superseded primary (it fenced itself on
    /// this very `Hello`). Retryable: the receiver keeps reconnecting, so
    /// pointing it at the current primary takes effect without a restart.
    SourceStale { have: u64, want: u64 },
    /// Spec P5.1 §11.F: the receiver's last applied write group, which ends
    /// at its cursor `at`, is not this source's group ending there: the
    /// same history and epoch, but another numbering of other writes (the
    /// receiver followed another primary of this epoch — two forced
    /// promotions). Resuming would apply this source's groups onto data they
    /// do not continue; the receiver needs recovery (a fast-forward re-cuts
    /// it at this source's checkpoint).
    Diverged { at: u64 },
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
            Self::GroupTooLarge { .. } => 9,
            Self::SourceStale { .. } => 10,
            Self::Diverged { .. } => 11,
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
            Self::GroupTooLarge { .. } => "GroupTooLarge",
            Self::SourceStale { .. } => "SourceStale",
            Self::Diverged { .. } => "Diverged",
        }
    }

    /// Reconnecting cannot help: the receiver's posture becomes
    /// `needs_recovery` (it stays up, field finding J2).
    pub fn needs_recovery(&self) -> bool {
        matches!(
            self,
            Self::HistoryMismatch
                | Self::Gap { .. }
                | Self::WalPruned { .. }
                | Self::EpochStale { .. }
                | Self::Version
                | Self::GroupTooLarge { .. }
                | Self::Diverged { .. }
        )
    }

    /// The source may serve this receiver later: reconnect with backoff
    /// (posture `reconnecting`). `AuthFailed` is neither: a receiver retries
    /// it a few times (a token file being replaced), then needs recovery.
    /// Spec P5.1: `SourceStale` is retried — the receiver is right, its
    /// configured source is a superseded primary, and the fix (pointing it at
    /// the current primary) is the operator's, taken up at the next attempt.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::TooManySessions | Self::SourceNotServing | Self::SourceStale { .. }
        )
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
            Self::GroupTooLarge { first, bytes } => write!(
                f,
                "write group too large: the group at sequence {first} is {bytes} bytes, more than the \
                 {MAX_BATCH_BYTES} one frame can carry"
            ),
            Self::SourceStale { have, want } => write!(
                f,
                "superseded source: the source is at epoch {have}, the receiver at {want}"
            ),
            Self::Diverged { at } => write!(
                f,
                "diverged: the receiver's write group ending at sequence {at} is not the source's"
            ),
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
            RefuseCode::WalPruned { oldest } | RefuseCode::Diverged { at: oldest } => {
                bytes.extend(oldest.to_be_bytes())
            }
            RefuseCode::EpochStale { have, want } | RefuseCode::SourceStale { have, want } => {
                bytes.extend(have.to_be_bytes());
                bytes.extend(want.to_be_bytes());
            }
            RefuseCode::GroupTooLarge { first, bytes: size } => {
                bytes.extend(first.to_be_bytes());
                bytes.extend(size.to_be_bytes());
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
            9 => (
                RefuseCode::GroupTooLarge {
                    first: u64_at(bytes, 2)?,
                    bytes: u64_at(bytes, 10)?,
                },
                16,
            ),
            10 => (
                RefuseCode::SourceStale {
                    have: u64_at(bytes, 2)?,
                    want: u64_at(bytes, 10)?,
                },
                16,
            ),
            11 => (RefuseCode::Diverged { at: u64_at(bytes, 2)? }, 8),
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

/// Spec P5.1 §3.1, receiver → source: "a standby of this dataset is being
/// promoted to `epoch`: stop writing". Sent by `heliosdb-nano promote` to
/// the primary it follows, after `HelloOk` and before `Open` (or between two
/// requests of a snapshot or resume session). The source answers [`Fenced`]
/// once it is fenced, or refuses (`Version`).
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

/// Spec P5.1 §3.1, source → receiver: this source is fenced by `epoch` —
/// it refuses every write, every write before the fence is durable, and the
/// fence is recorded on its disk — and holds everything through
/// `final_sequence` (its own numbering). A promoting standby tails it through
/// that sequence before it converts its store: nothing the old primary
/// committed is lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fenced {
    /// The epoch the source was fenced by (the first fence's: a repeated
    /// fence by the same node answers the same `Fenced`).
    pub epoch: u64,
    pub final_sequence: u64,
}

impl Fenced {
    fn validate(&self) -> Result<()> {
        if self.epoch < 2 || self.final_sequence > MAX_SOURCE_SEQUENCE {
            return Err(invalid("invalid fence acknowledgement"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(FENCED_BYTES);
        bytes.extend(self.epoch.to_be_bytes());
        bytes.extend(self.final_sequence.to_be_bytes());
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != FENCED_BYTES {
            return Err(invalid("fence acknowledgement has the wrong size"));
        }
        let fenced = Self {
            epoch: u64_at(bytes, 0)?,
            final_sequence: u64_at(bytes, 8)?,
        };
        fenced.validate()?;
        Ok(fenced)
    }
}

/// Whether a source's leaf plan can be served yet (spec P4.1 §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafStatus {
    Ready,
    /// The source still scans its checkpoint: the receiver asks again.
    Planning,
}

impl LeafStatus {
    fn code(self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::Planning => 1,
        }
    }

    fn decode(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Ready),
            1 => Ok(Self::Planning),
            _ => Err(invalid("unknown leaf plan status")),
        }
    }
}

/// One leaf of a source's plan (spec P4.1 §3.2): the keys from `start` up to
/// the next leaf's start (the last one up to [`LEAF_END`]), their key and
/// value bytes, how many, and the SHA-256 of their `len(k) ‖ k ‖ len(v) ‖ v`
/// framing in key order — `physical::range_digest`'s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafEntry {
    pub start: Vec<u8>,
    pub bytes: u64,
    pub keys: u32,
    pub digest: [u8; 32],
}

impl LeafEntry {
    /// The bytes this entry takes in a `LeafDigests` frame.
    pub fn encoded_len(&self) -> usize {
        LEAF_ENTRY_META_BYTES + self.start.len()
    }
}

/// One page of a source's leaf plan, `entries` being leaves `from..`. While
/// the source plans, a page says so and carries nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafDigests {
    pub snapshot_id: Uuid,
    pub status: LeafStatus,
    pub leaf_count: u32,
    pub total_bytes: u64,
    pub from: u32,
    pub entries: Vec<LeafEntry>,
}

impl LeafDigests {
    /// The answer of a source still planning.
    pub fn planning(snapshot_id: Uuid) -> Self {
        Self {
            snapshot_id,
            status: LeafStatus::Planning,
            leaf_count: 0,
            total_bytes: 0,
            from: 0,
            entries: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.snapshot_id.is_nil() {
            return Err(invalid("a leaf page names no snapshot"));
        }
        if self.status == LeafStatus::Planning {
            if self.leaf_count != 0 || self.total_bytes != 0 || self.from != 0 || !self.entries.is_empty() {
                return Err(invalid("a leaf page of a source still planning carries leaves"));
            }
            return Ok(());
        }
        let available = self
            .leaf_count
            .checked_sub(self.from)
            .ok_or_else(|| invalid("a leaf page starts past the plan"))?;
        if self.leaf_count == 0 || self.leaf_count > MAX_LEAVES || self.entries.len() > available as usize {
            return Err(invalid("a leaf page disagrees with its plan's leaf count"));
        }
        let mut previous: Option<&[u8]> = None;
        for (offset, entry) in self.entries.iter().enumerate() {
            let first = u64::from(self.from) + offset as u64 == 0;
            if entry.start.len() > MAX_LEAF_START_BYTES
                || entry.start.as_slice() >= &LEAF_END[..]
                || first != entry.start.is_empty()
                || entry.bytes > self.total_bytes
                || previous.is_some_and(|previous| previous >= entry.start.as_slice())
            {
                return Err(invalid(
                    "a leaf page's boundaries are out of order, outside the keyspace, or not starting at the empty key",
                ));
            }
            previous = Some(entry.start.as_slice());
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let count = u32::try_from(self.entries.len()).map_err(|_| invalid("too many leaves in one page"))?;
        let size = LEAF_DIGESTS_HEADER_BYTES + self.entries.iter().map(LeafEntry::encoded_len).sum::<usize>();
        if size > MAX_LEAF_DIGESTS_BYTES {
            return Err(invalid("a leaf page exceeds its frame bound"));
        }
        let mut bytes = Vec::with_capacity(size);
        bytes.extend(self.snapshot_id.as_bytes());
        bytes.push(self.status.code());
        bytes.extend(self.leaf_count.to_be_bytes());
        bytes.extend(self.total_bytes.to_be_bytes());
        bytes.extend(self.from.to_be_bytes());
        bytes.extend(count.to_be_bytes());
        for entry in &self.entries {
            bytes.extend((entry.start.len() as u16).to_be_bytes());
            bytes.extend(&entry.start);
            bytes.extend(entry.bytes.to_be_bytes());
            bytes.extend(entry.keys.to_be_bytes());
            bytes.extend(entry.digest);
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if !(LEAF_DIGESTS_HEADER_BYTES..=MAX_LEAF_DIGESTS_BYTES).contains(&bytes.len()) {
            return Err(invalid("a leaf page has the wrong size"));
        }
        let count = u32_at(bytes, 33)? as usize;
        if count > (bytes.len() - LEAF_DIGESTS_HEADER_BYTES) / LEAF_ENTRY_META_BYTES {
            return Err(invalid("a leaf page claims more leaves than it holds"));
        }
        let mut entries = Vec::with_capacity(count);
        let mut offset = LEAF_DIGESTS_HEADER_BYTES;
        for _ in 0..count {
            let length = usize::from(u16_at(bytes, offset)?);
            let start = bytes
                .get(offset + 2..offset + 2 + length)
                .ok_or_else(|| invalid("frame truncated"))?
                .to_vec();
            offset += 2 + length;
            let digest: [u8; 32] = bytes
                .get(offset + 12..offset + 44)
                .and_then(|slice| slice.try_into().ok())
                .ok_or_else(|| invalid("frame truncated"))?;
            entries.push(LeafEntry {
                start,
                bytes: u64_at(bytes, offset)?,
                keys: u32_at(bytes, offset + 8)?,
                digest,
            });
            offset += 44;
        }
        if offset != bytes.len() {
            return Err(invalid("a leaf page's length disagrees with its frame"));
        }
        let page = Self {
            snapshot_id: uuid_at(bytes, 0)?,
            status: LeafStatus::decode(byte_at(bytes, 16)?)?,
            leaf_count: u32_at(bytes, 17)?,
            total_bytes: u64_at(bytes, 21)?,
            from: u32_at(bytes, 29)?,
            entries,
        };
        page.validate()?;
        Ok(page)
    }
}

/// A source's leaf plan in full, as a client fetched it page by page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafPlan {
    pub snapshot_id: Uuid,
    pub total_bytes: u64,
    pub leaves: Vec<LeafEntry>,
}

/// The source built leaf `index`'s SST file: `size` bytes whose SHA-256 is
/// `sha256`. Size 0: the source holds no key in the leaf, and there is no
/// file (the receiver only deletes the range).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafReady {
    pub snapshot_id: Uuid,
    pub index: u32,
    pub size: u64,
    pub sha256: [u8; 32],
}

impl LeafReady {
    fn validate(&self) -> Result<()> {
        if self.snapshot_id.is_nil() || self.index >= MAX_LEAVES || (self.size == 0 && self.sha256 != empty_sha256()) {
            return Err(invalid("invalid leaf file announcement"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(LEAF_READY_BYTES);
        bytes.extend(self.snapshot_id.as_bytes());
        bytes.extend(self.index.to_be_bytes());
        bytes.extend(self.size.to_be_bytes());
        bytes.extend(self.sha256);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != LEAF_READY_BYTES {
            return Err(invalid("a leaf file announcement has the wrong size"));
        }
        let ready = Self {
            snapshot_id: uuid_at(bytes, 0)?,
            index: u32_at(bytes, 16)?,
            size: u64_at(bytes, 20)?,
            sha256: bytes
                .get(28..60)
                .and_then(|slice| slice.try_into().ok())
                .ok_or_else(|| invalid("frame truncated"))?,
        };
        ready.validate()?;
        Ok(ready)
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

/// Spec P5.1 §11.F: a receiver's last applied write group — its first
/// sequence and its SHA-256 (`PhysicalBatch::digest`), as its durable cursor
/// records them. It ends at the cursor. A resume carries it, and the source
/// verifies its own group ending there is this one before it serves the
/// session (`RefuseCode::Diverged` otherwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastGroup {
    pub first_sequence: u64,
    pub digest: [u8; 32],
}

/// Explicit session choice, sent only after authentication. Resume never creates
/// a new checkpoint and pins the receiver's already durable physical history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenRequest {
    Snapshot,
    /// Spec P2.2: a snapshot, preferring the export `snapshot` — the one a
    /// receiver's interrupted download belongs to. A source still leasing it
    /// answers with that export's `Manifest`; otherwise with a fresh one, and
    /// the receiver starts its download over.
    SnapshotOf {
        snapshot: Uuid,
    },
    /// A resume after `after`. Spec P5.1 §11.F: `last` is the receiver's
    /// last applied write group, which ends at `after` — `None` for a cursor
    /// at a checkpoint cut, which has applied no group since.
    Resume {
        history: Uuid,
        after: u64,
        last: Option<LastGroup>,
    },
    /// Spec P4.1 §3.1: an incremental fast-forward of a receiver of
    /// `history` at `after` (`recover --from-primary`): the source serves
    /// its leaf plan of `leaf_bytes` leaves over a checkpoint export —
    /// `prefer`'s while it still leases it (a resumed fast-forward), else a
    /// new one.
    FastForward {
        history: Uuid,
        after: u64,
        leaf_bytes: u32,
        prefer: Option<Uuid>,
    },
}
impl OpenRequest {
    fn encode(self) -> Result<Vec<u8>> {
        match self {
            Self::FastForward {
                history,
                after,
                leaf_bytes,
                prefer,
            } => {
                if history.is_nil()
                    || after > MAX_SOURCE_SEQUENCE
                    || !valid_leaf_bytes(leaf_bytes)
                    || prefer.is_some_and(|snapshot| snapshot.is_nil())
                {
                    return Err(invalid("invalid fast-forward identity/cursor/leaf size"));
                }
                let mut bytes = vec![if prefer.is_some() { 4 } else { 3 }];
                bytes.extend(history.as_bytes());
                bytes.extend(after.to_be_bytes());
                bytes.extend(leaf_bytes.to_be_bytes());
                if let Some(snapshot) = prefer {
                    bytes.extend(snapshot.as_bytes());
                }
                Ok(bytes)
            }
            Self::Snapshot => Ok(vec![0]),
            Self::SnapshotOf { snapshot } => {
                if snapshot.is_nil() {
                    return Err(invalid("invalid preferred snapshot identity"));
                }
                let mut bytes = vec![2];
                bytes.extend(snapshot.as_bytes());
                Ok(bytes)
            }
            Self::Resume { history, after, last } => {
                if history.is_nil()
                    || after > MAX_SOURCE_SEQUENCE
                    || last.is_some_and(|last| last.first_sequence == 0 || last.first_sequence > after)
                {
                    return Err(invalid("invalid resume identity/cursor/last write group"));
                }
                let mut bytes = Vec::with_capacity(OPEN_RESUME_BYTES);
                bytes.push(1);
                bytes.extend(history.as_bytes());
                bytes.extend(after.to_be_bytes());
                bytes.push(u8::from(last.is_some()));
                bytes.extend(last.map_or(0, |last| last.first_sequence).to_be_bytes());
                bytes.extend(last.map_or([0; 32], |last| last.digest));
                Ok(bytes)
            }
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let value = match bytes {
            [0] => Self::Snapshot,
            [2, ..] if bytes.len() == 17 => Self::SnapshotOf {
                snapshot: uuid_at(bytes, 1)?,
            },
            [1, ..] if bytes.len() == OPEN_RESUME_BYTES => {
                let first_sequence = u64_at(bytes, 26)?;
                let digest: [u8; 32] = bytes
                    .get(34..66)
                    .and_then(|digest| digest.try_into().ok())
                    .ok_or_else(|| invalid("frame truncated"))?;
                let last = match byte_at(bytes, 25)? {
                    0 if first_sequence == 0 && digest == [0; 32] => None,
                    1 => Some(LastGroup { first_sequence, digest }),
                    _ => return Err(invalid("malformed resume last write group")),
                };
                Self::Resume {
                    history: uuid_at(bytes, 1)?,
                    after: u64_at(bytes, 17)?,
                    last,
                }
            }
            [3, ..] if bytes.len() == OPEN_FAST_FORWARD_BYTES => Self::FastForward {
                history: uuid_at(bytes, 1)?,
                after: u64_at(bytes, 17)?,
                leaf_bytes: u32_at(bytes, 25)?,
                prefer: None,
            },
            [4, ..] if bytes.len() == OPEN_FAST_FORWARD_OF_BYTES => Self::FastForward {
                history: uuid_at(bytes, 1)?,
                after: u64_at(bytes, 17)?,
                leaf_bytes: u32_at(bytes, 25)?,
                prefer: Some(uuid_at(bytes, 29)?),
            },
            _ => return Err(invalid("invalid session open request")),
        };
        value.encode()?;
        Ok(value)
    }

    /// The session a receiver may open after `hello`: a snapshot without a
    /// cursor, or a resume — or a fast-forward — of exactly the history and
    /// cursor it announced.
    fn agrees_with(self, hello: &Hello) -> bool {
        match self {
            Self::Snapshot | Self::SnapshotOf { .. } => hello.cursor.is_none(),
            Self::Resume { history, after, .. } | Self::FastForward { history, after, .. } => {
                hello.history == Some(history) && hello.cursor == Some(after)
            }
        }
    }

    /// Whether this opens a snapshot (a bootstrap copy) rather than resuming
    /// a durable cursor.
    pub fn is_snapshot(self) -> bool {
        matches!(self, Self::Snapshot | Self::SnapshotOf { .. })
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
    /// Spec P4.1 §3.1: a fast-forward session over the export `snapshot_id`,
    /// cut at the source sequence `sequence`. Its frame (`FastForwardInfo`)
    /// carries the session, the snapshot and the cut; `history` and `after`
    /// are the request's, which the source agreed to.
    FastForward {
        session_id: Uuid,
        snapshot_id: Uuid,
        history: Uuid,
        after: u64,
        sequence: u64,
    },
}
impl SourceSessionInfo {
    pub fn session_id(&self) -> Uuid {
        match self {
            Self::Snapshot(m) => m.snapshot_id,
            Self::Resume { session_id, .. } | Self::FastForward { session_id, .. } => *session_id,
        }
    }
    pub fn history(&self) -> Uuid {
        match self {
            Self::Snapshot(m) => m.history,
            Self::Resume { history, .. } | Self::FastForward { history, .. } => *history,
        }
    }
    pub fn initial_after(&self) -> u64 {
        match self {
            Self::Snapshot(m) => m.checkpoint_sequence,
            Self::Resume { initial_after, .. } => *initial_after,
            Self::FastForward { after, .. } => *after,
        }
    }
    fn initial_barrier(&self) -> u64 {
        match self {
            Self::Snapshot(m) => m.checkpoint_sequence,
            Self::Resume { durable_barrier, .. } => *durable_barrier,
            Self::FastForward { sequence, .. } => *sequence,
        }
    }
    pub fn manifest(&self) -> Option<&CheckpointManifest> {
        match self {
            Self::Snapshot(m) => Some(m),
            Self::Resume { .. } | Self::FastForward { .. } => None,
        }
    }
    /// A fast-forward session's export: what its leaf frames name.
    pub fn fast_forward_snapshot(&self) -> Option<Uuid> {
        match self {
            Self::FastForward { snapshot_id, .. } => Some(*snapshot_id),
            Self::Snapshot(_) | Self::Resume { .. } => None,
        }
    }
    fn validate(&self, request: OpenRequest, limits: &WireLimits) -> Result<()> {
        match (self, request) {
            (Self::Snapshot(m), OpenRequest::Snapshot | OpenRequest::SnapshotOf { .. }) => {
                m.validate(&limits.transfer).map_err(|e| invalid(e.to_string()))
            }
            // A receiver at or past the cut has nothing to fast-forward: the
            // source refuses it (`HistoryMismatch`) rather than open this.
            (
                Self::FastForward {
                    session_id,
                    snapshot_id,
                    history,
                    after,
                    sequence,
                },
                OpenRequest::FastForward {
                    history: expected,
                    after: requested,
                    ..
                },
            ) if !session_id.is_nil()
                && !snapshot_id.is_nil()
                && *history == expected
                && *after == requested
                && requested < *sequence
                && *sequence <= MAX_SOURCE_SEQUENCE =>
            {
                Ok(())
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
                    ..
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
            Self::FastForward {
                session_id,
                snapshot_id,
                sequence,
                ..
            } => {
                let mut bytes = Vec::with_capacity(FAST_FORWARD_INFO_BYTES);
                bytes.extend(session_id.as_bytes());
                bytes.extend(snapshot_id.as_bytes());
                bytes.extend(sequence.to_be_bytes());
                Ok((Kind::FastForwardInfo, bytes))
            }
        }
    }

    /// The `FastForwardInfo` answer to `request` (spec P4.1 §3.1), decoded
    /// and checked against it.
    fn fast_forward_from(payload: &[u8], request: OpenRequest, limits: &WireLimits) -> Result<Self> {
        let OpenRequest::FastForward { history, after, .. } = request else {
            return Err(invalid("a fast-forward answer to another session's open"));
        };
        if payload.len() != FAST_FORWARD_INFO_BYTES {
            return Err(invalid("a fast-forward answer has the wrong size"));
        }
        let info = Self::FastForward {
            session_id: uuid_at(payload, 0)?,
            snapshot_id: uuid_at(payload, 16)?,
            history,
            after,
            sequence: u64_at(payload, 32)?,
        };
        info.validate(request, limits)?;
        Ok(info)
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
    /// Where the last successful [`Self::poll`] read its groups from (spec
    /// P3.2 §1): the store's own WAL, or the WAL archive a source fills a
    /// receiver's gap from. The receiver cannot tell — the frames are the
    /// same — but the source's registry reports it.
    fn poll_origin(&self) -> PollOrigin {
        PollOrigin::Wal
    }
    /// Spec P4.1 §3.1: up to `count` leaves of a fast-forward session's plan
    /// from leaf `from` — or a [`LeafStatus::Planning`] page while the plan
    /// is being built. Paged by the provider; `serve_session` sends at most
    /// [`MAX_LEAF_DIGESTS_BYTES`]. Only a fast-forward session has a plan.
    async fn leaf_digests(&self, _from: u32, _count: u32) -> Result<LeafDigests> {
        Err(invalid("leaf digests require a fast-forward session"))
    }
    /// Build leaf `index`'s SST file (once) and announce it.
    async fn leaf(&self, _index: u32) -> Result<LeafReady> {
        Err(invalid("leaf files require a fast-forward session"))
    }
    /// A piece of a built leaf file.
    async fn read_leaf_chunk(&self, _index: u32, _offset: u64, _length: u32) -> Result<Vec<u8>> {
        Err(invalid("leaf files require a fast-forward session"))
    }
}

/// Where a source read the groups of a poll from (spec P3.2 §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollOrigin {
    /// The store's own RocksDB WAL.
    Wal,
    /// Archived WAL segments: the store's WAL no longer held them.
    Archive,
}

impl PollOrigin {
    /// `"wal"` or `"archive"`, as `/health` reports it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wal => "wal",
            Self::Archive => "archive",
        }
    }
}

/// The most raw bytes of write groups [`PhysicalClient::poll`] hands its sink
/// in one [`BatchSink::apply_many`] (spec P4.1d): a poll's validated batches
/// travel to the sink together, in order, in runs of at most this many bytes
/// — one run for a poll of a serving standby's limit, which is the same size.
/// A single batch larger than this is a run of its own: a write group is
/// never split (spec P4.1c).
pub const APPLY_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// The largest coalescing target a receiver may ask a poll for (its
/// `PollRequest` byte limit; `ReceiverOptions::poll_byte_limit`; `resync
/// --poll-byte-limit`). It bounds how many write groups one poll gathers —
/// what a source reads and holds, and a receiver takes, per round trip — not
/// how large one group may be: the first group of a poll travels whatever its
/// size (spec P4.1c), up to `MAX_BATCH_BYTES`.
pub const MAX_POLL_BYTES: usize = 256 * 1024 * 1024;

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
    /// Spec P4.1d: apply a run of validated, contiguous batches, in order —
    /// a poll's, or a part of it of at most [`APPLY_CHUNK_BYTES`] (a larger
    /// batch alone). Every batch of the run was validated before any of it
    /// is handed over. A sink that applies a run as one unit of work (one
    /// hop to its owner thread, one publication) overrides this; the default
    /// applies each batch through [`Self::apply`] in order, so every other
    /// sink keeps working unchanged. An error leaves the batches before the
    /// failing one applied, as a failed [`Self::apply`] of a later batch did.
    async fn apply_many(&mut self, batches: Vec<PhysicalBatch>) -> Result<()> {
        for batch in batches {
            self.apply(batch).await?;
        }
        Ok(())
    }
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

    /// A poll was served from `origin` (spec P3.2 §1). Does nothing by
    /// default.
    fn served(&mut self, _origin: PollOrigin) {}
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

/// Spec P5.1 §3.2: how a source that can be fenced (a primary) fences
/// itself. The wire calls it for an inbound [`Fence`] and for a `Hello` of a
/// higher epoch; the owner (`replication::fence::PrimaryFence`) closes the
/// write gate, makes every earlier write durable, records the fence on disk,
/// and only then answers. One fence per process: the first one wins.
#[async_trait]
pub trait SourceFence: Send + Sync {
    /// Fence this source by `epoch` on behalf of the receiver `node`.
    /// `Ok(Fenced)` once it is fenced — also for a repeated fence by the same
    /// node, which answers the first one's `Fenced`. `Err` is the refusal the
    /// receiver is sent: a fence of no higher epoch than the source's own, a
    /// fence by another node than the one that fenced it, or a failure to
    /// fence (the reason names it).
    async fn fence(&self, epoch: u64, node: Uuid) -> std::result::Result<Fenced, Refusal>;

    /// Spec P5.1 §11.E: why this source bootstraps nobody any more — it is
    /// fenced (or its fence is under way) — or `None` while it is a primary
    /// that may. A fenced source refuses snapshot and fast-forward opens with
    /// `SourceNotServing` and this reason; it keeps serving resume sessions.
    fn fenced(&self) -> Option<String> {
        None
    }
}

/// Everything a source's sessions share: its identity, its observer, its
/// transport, and (a primary's) its fence.
#[derive(Clone)]
pub struct SourceContext {
    pub identity: SourceIdentity,
    pub observer: Arc<dyn SessionObserver>,
    /// Spec P2.2: the source's listener serves TLS, so every session it hands
    /// [`serve_session`] completed a TLS handshake first; its `HelloOk` then
    /// carries [`CAP_TLS`].
    pub tls: bool,
    /// Spec P5.1: how this source fences itself; `None` for a source that
    /// cannot be fenced (a cascading standby: it is no primary). With one,
    /// every `HelloOk` carries [`CAP_FENCE`].
    pub fence: Option<Arc<dyn SourceFence>>,
}

impl SourceContext {
    /// A plaintext source whose sessions nobody observes, and that cannot be
    /// fenced.
    pub fn unobserved(identity: SourceIdentity) -> Self {
        Self {
            identity,
            observer: Arc::new(NoSessionObserver),
            tls: false,
            fence: None,
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
    // Spec P4.1 §3.1: the fast-forward session.
    FastForwardInfo = 21,
    LeafDigestsRequest = 22,
    LeafDigests = 23,
    LeafRequest = 24,
    LeafReady = 25,
    LeafChunkRequest = 26,
    LeafChunk = 27,
    // 28 is not assigned; spec P5.1 numbers the fence acknowledgement 29.
    Fenced = 29,
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
            21 => Self::FastForwardInfo,
            22 => Self::LeafDigestsRequest,
            23 => Self::LeafDigests,
            24 => Self::LeafRequest,
            25 => Self::LeafReady,
            26 => Self::LeafChunkRequest,
            27 => Self::LeafChunk,
            29 => Self::Fenced,
            _ => return Err(invalid("unknown frame kind")),
        })
    }
    fn valid_size(self, n: usize) -> bool {
        match self {
            Self::Auth => n == TOKEN_BYTES,
            Self::AuthOk | Self::Close | Self::Closed => n == 0,
            Self::Open => {
                n == 1
                    || n == 17
                    || n == OPEN_RESUME_BYTES
                    || n == OPEN_FAST_FORWARD_BYTES
                    || n == OPEN_FAST_FORWARD_OF_BYTES
            }
            Self::FastForwardInfo => n == FAST_FORWARD_INFO_BYTES,
            Self::LeafDigestsRequest => n == LEAF_DIGESTS_REQUEST_BYTES,
            Self::LeafDigests => (LEAF_DIGESTS_HEADER_BYTES..=MAX_LEAF_DIGESTS_BYTES).contains(&n),
            Self::LeafRequest => n == LEAF_REQUEST_BYTES,
            Self::LeafReady => n == LEAF_READY_BYTES,
            Self::LeafChunkRequest => n == 32,
            Self::LeafChunk => (CHUNK_META + 1..=CHUNK_META + MAX_CHUNK_BYTES).contains(&n),
            Self::ResumeInfo => n == 48,
            Self::Manifest => (1..=MAX_MANIFEST_BYTES).contains(&n),
            Self::ChunkRequest => n == 32,
            Self::Chunk => (CHUNK_META + 1..=CHUNK_META + MAX_CHUNK_BYTES).contains(&n),
            Self::PollRequest => n == 28,
            Self::PollStart | Self::PollEnd => n == 20,
            // Spec P4.1c: one write group of any size `u32` can frame
            // ([`MAX_BATCH_BYTES`]), so `read_frame` may allocate up to
            // ~4 GiB for one. The trust boundary: a `Batch` is only ever
            // allowed in a poll response on a session this receiver opened,
            // after authentication, from the source it follows — and it is
            // applied verbatim, so its size is bounded by what that source
            // committed as one write group. No source ever reads one.
            Self::Batch => (BATCH_META + 12..=BATCH_META + MAX_BATCH_BYTES).contains(&n),
            Self::Hello => n == HELLO_BYTES,
            Self::HelloOk => (HELLO_OK_META..=HELLO_OK_META + MAX_ENDPOINT_BYTES).contains(&n),
            Self::Refuse => (4..=REFUSE_META_MAX + MAX_REASON_BYTES).contains(&n),
            Self::Heartbeat => n == HEARTBEAT_BYTES,
            Self::HeartbeatAck => n == HEARTBEAT_ACK_BYTES,
            Self::Fence => n == FENCE_BYTES,
            Self::Fenced => n == FENCED_BYTES,
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

/// Spec P4.1c: a frame's payload is read and written in steps of at most
/// this many bytes, each bounded by the frame timeout. The timeout is an
/// idle one: a peer that keeps a large `Batch` frame coming never times out,
/// however long the whole frame takes, while one that stalls for a frame
/// timeout does.
const FRAME_IO_STEP: usize = 1024 * 1024;

/// `internal-tests` builds only: a receiver that reads a `Batch` frame of at
/// least this many payload bytes SIGKILLs its own process once half of that
/// payload has arrived — a crash with a large write group in flight, exactly
/// instead of by timing (spec P4.1c §3).
pub const KILL_MID_BATCH_ENV: &str = "HELIOSDB_NANO_TEST_KILL_MID_BATCH";

/// Stop part-way through a large `Batch` payload (`internal-tests` only,
/// [`KILL_MID_BATCH_ENV`]): `read` of its `length` payload bytes arrived.
fn kill_mid_batch_hook(kind: Kind, length: usize, read: usize) {
    #[cfg(all(feature = "internal-tests", unix))]
    {
        static KILL_AT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        let threshold = KILL_AT.get_or_init(|| std::env::var(KILL_MID_BATCH_ENV).ok()?.trim().parse().ok());
        if kind == Kind::Batch && threshold.is_some_and(|at| length >= at) && read >= length / 2 {
            tracing::warn!("{KILL_MID_BATCH_ENV}: killing this process {read} bytes into a {length}-byte Batch frame");
            // SAFETY: kill(2) of this very process with a standard signal; no
            // pointers, no preconditions.
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(all(feature = "internal-tests", unix)))]
    let _ = (kind, length, read);
}

async fn read_frame<R: AsyncRead + Unpin>(
    io: &mut R,
    allowed: &[Kind],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Frame> {
    read_frame_within(io, allowed, limits, limits.frame_timeout, shutdown).await
}

/// [`read_frame`], waiting up to `header_wait` for the frame's header to
/// arrive (its payload steps keep the frame timeout). A source waits this
/// long for a receiver's next request: the receiver applies (and fsyncs) a
/// whole poll before it sends one; a receiver for a poll's `PollStart`
/// ([`read_reply_within`]): the source collects the whole poll first.
async fn read_frame_within<R: AsyncRead + Unpin>(
    io: &mut R,
    allowed: &[Kind],
    limits: &WireLimits,
    header_wait: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Frame> {
    let (kind, length) = read_frame_header(io, allowed, header_wait, shutdown).await?;
    let mut payload = vec![0; length];
    read_payload_steps(io, &mut payload, kind, length, 0, limits, shutdown).await?;
    Ok(Frame { kind, payload })
}

/// A frame's header, its kind among `allowed` and its length within the
/// kind's bounds — both checked before anything is allocated for it —
/// waiting up to `wait` for it to arrive.
async fn read_frame_header<R: AsyncRead + Unpin>(
    io: &mut R,
    allowed: &[Kind],
    wait: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(Kind, usize)> {
    bounded(shutdown, wait, async {
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
        Ok((kind, length))
    })
    .await
}

/// Fill `buffer`, the part of a `kind` frame's `length`-byte payload that
/// starts `offset` bytes into it. Spec P4.1c: one deadline per step of the
/// payload, not one for the whole frame (`FRAME_IO_STEP`).
async fn read_payload_steps<R: AsyncRead + Unpin>(
    io: &mut R,
    buffer: &mut [u8],
    kind: Kind,
    length: usize,
    offset: usize,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut read = offset;
    for step in buffer.chunks_mut(FRAME_IO_STEP) {
        bounded(shutdown, limits.frame_timeout, async {
            io.read_exact(step).await?;
            Ok(())
        })
        .await?;
        read += step.len();
        kill_mid_batch_hook(kind, length, read);
    }
    Ok(())
}

/// One `Batch` frame of a poll, as its metadata (`history | first | digest`)
/// and its raw write group in a buffer of its own, so the group — up to
/// gigabytes (spec P4.1c) — is never moved to drop the metadata in front of
/// it. A frame whose raw group is longer than `max_raw` (what is left of the
/// poll's byte limit, for a batch after the first) is refused from its
/// header, before anything is allocated for it.
async fn read_batch_frame<R: AsyncRead + Unpin>(
    io: &mut R,
    max_raw: usize,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<([u8; BATCH_META], Vec<u8>)> {
    let (kind, length) = read_frame_header(io, &[Kind::Batch], limits.frame_timeout, shutdown).await?;
    let raw_length = length
        .checked_sub(BATCH_META)
        .ok_or_else(|| invalid("batch payload truncated"))?;
    if raw_length > max_raw {
        return Err(invalid("physical response history/sequence/byte mismatch"));
    }
    let mut meta = [0_u8; BATCH_META];
    read_payload_steps(io, &mut meta, kind, length, 0, limits, shutdown).await?;
    let mut raw = vec![0; raw_length];
    read_payload_steps(io, &mut raw, kind, length, BATCH_META, limits, shutdown).await?;
    Ok((meta, raw))
}

/// Spec P4.1b: a frame whose payload is at most this large is written as ONE
/// buffer, header and payload together, so a control frame (a `ChunkRequest`,
/// a `LeafChunkRequest`, a `PollRequest`, a heartbeat, a planning re-ask)
/// leaves in one TCP segment and, over TLS, one record. Written as two, the
/// payload of a request would wait under Nagle's algorithm for the ACK of its
/// 16-byte header, which the peer, with nothing to send, delays (up to 40 ms
/// on Linux) — once per round trip of a request/response session. This holds
/// whatever the socket's `TCP_NODELAY` (set by [`accept_replication`] and
/// [`PhysicalClient::connect_replication`]): with it, two writes would still be
/// two segments. A larger payload (a chunk, a batch) follows its header
/// without being copied, in steps of `FRAME_IO_STEP` (spec P4.1c).
const SMALL_FRAME_BYTES: usize = 64 * 1024;

async fn write_frame<W: AsyncWrite + Unpin>(
    io: &mut W,
    kind: Kind,
    payload: &[u8],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    write_frame_parts(io, kind, &[], payload, limits, shutdown).await
}

/// [`write_frame`] of a payload given in two parts, a short `meta` prefix
/// then `body`, sent as one frame without joining them: a `Batch`'s metadata
/// and its raw write group, which may be gigabytes (spec P4.1c), are never
/// copied into one buffer. A small frame is still ONE write (spec P4.1b); a
/// larger one is its header and `meta` in one write, then `body` in steps of
/// `FRAME_IO_STEP`, each bounded by the frame timeout (an idle timeout), then
/// a flush.
async fn write_frame_parts<W: AsyncWrite + Unpin>(
    io: &mut W,
    kind: Kind,
    meta: &[u8],
    body: &[u8],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let length = meta
        .len()
        .checked_add(body.len())
        .filter(|length| kind.valid_size(*length))
        .ok_or_else(|| invalid("outgoing frame exceeds bounds"))?;
    let length_field = u32::try_from(length).map_err(|_| invalid("outgoing frame exceeds bounds"))?;
    let mut header = [0u8; HEADER_BYTES];
    header[..8].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&VERSION.to_be_bytes());
    header[10..12].copy_from_slice(&(kind as u16).to_be_bytes());
    header[12..16].copy_from_slice(&length_field.to_be_bytes());
    if length <= SMALL_FRAME_BYTES {
        return bounded(shutdown, limits.frame_timeout, async {
            // The buffer may hold the token (`Auth`): wiped when dropped, and
            // sized once so it is never reallocated (leaving a copy behind).
            let mut frame: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(HEADER_BYTES + length));
            frame.extend_from_slice(&header);
            frame.extend_from_slice(meta);
            frame.extend_from_slice(body);
            io.write_all(frame.as_slice()).await?;
            io.flush().await?;
            Ok(())
        })
        .await;
    }
    let mut head = Vec::with_capacity(HEADER_BYTES + meta.len());
    head.extend_from_slice(&header);
    head.extend_from_slice(meta);
    bounded(shutdown, limits.frame_timeout, async {
        io.write_all(&head).await?;
        Ok(())
    })
    .await?;
    for step in body.chunks(FRAME_IO_STEP) {
        bounded(shutdown, limits.frame_timeout, async {
            io.write_all(step).await?;
            Ok(())
        })
        .await?;
    }
    bounded(shutdown, limits.frame_timeout, async {
        io.flush().await?;
        Ok(())
    })
    .await
}

/// Read one response of the `expected` kinds. A `Refuse` is the session's
/// end, returned as [`WireError::Refused`]. Spec P5.1: a source never sends a
/// `Fence` (only a receiver fences), so one is the protocol error any other
/// unexpected kind is.
async fn read_reply<R: AsyncRead + Unpin>(
    io: &mut R,
    expected: &[Kind],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Frame> {
    read_reply_within(io, expected, limits, limits.frame_timeout, shutdown).await
}

/// [`read_reply`], waiting up to `header_wait` for the reply's header (its
/// payload steps keep the frame timeout, as [`read_frame_within`]). A
/// receiver waits this long for a reply the source works on before it sends
/// it: a poll's `PollStart` comes after the whole poll was collected.
async fn read_reply_within<R: AsyncRead + Unpin>(
    io: &mut R,
    expected: &[Kind],
    limits: &WireLimits,
    header_wait: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Frame> {
    let mut allowed = expected.to_vec();
    allowed.push(Kind::Refuse);
    let frame = read_frame_within(io, &allowed, limits, header_wait, shutdown).await?;
    match frame.kind {
        Kind::Refuse => Err(WireError::Refused(Refusal::decode(&frame.payload)?)),
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

/// Spec P5.1 §2, the receiver's half of the epoch policy: what a receiver at
/// `epoch` holding data answers a source at `source` — `EpochStale` when the
/// source is AHEAD (a standby was promoted; this receiver needs recovery from
/// it), `SourceStale` when it is BEHIND (it is a superseded primary; the
/// receiver keeps retrying) — or `None` when they agree.
fn receiver_epoch_refusal(epoch: u64, source: u64) -> Option<Refusal> {
    match source.cmp(&epoch) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(Refusal::new(
            RefuseCode::EpochStale {
                have: source,
                want: epoch,
            },
            format!(
                "the source is at epoch {source} of this dataset, this receiver at epoch {epoch}: a standby was \
                 promoted; recover this receiver from the new primary (heliosdb-nano recover --from-primary)"
            ),
        )),
        std::cmp::Ordering::Less => Some(Refusal::new(
            RefuseCode::SourceStale {
                have: source,
                want: epoch,
            },
            format!(
                "the source is a superseded primary at epoch {source}; this receiver follows epoch {epoch} of the \
                 dataset: point it at the current primary"
            ),
        )),
    }
}

/// Spec P5.1 §2, the source's half: the refusal a source at `identity` gives a
/// receiver that holds data at `hello.epoch`, fencing itself first when the
/// receiver is AHEAD — this source is then the superseded one, and the first
/// node of the newer epoch that talks to it stops it writing. `None` when the
/// epochs agree.
async fn source_epoch_refusal(
    source: &SourceContext,
    hello: &Hello,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<Refusal> {
    let ours = source.identity.epoch;
    let theirs = hello.epoch;
    match theirs.cmp(&ours) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => Some(Refusal::new(
            RefuseCode::EpochStale {
                have: ours,
                want: theirs,
            },
            format!(
                "the receiver follows epoch {theirs} of this dataset; this source is at epoch {ours}: run \
                 `heliosdb-nano recover --from-primary` against this source"
            ),
        )),
        std::cmp::Ordering::Greater => {
            let fenced = match &source.fence {
                Some(fence) => {
                    let outcome = bounded(shutdown, limits.operation_timeout, async {
                        Ok(fence.fence(theirs, hello.node_id).await)
                    })
                    .await;
                    match outcome {
                        Ok(Ok(fenced)) => format!(
                            " (this source is fenced now, by epoch {}, at its sequence {})",
                            fenced.epoch, fenced.final_sequence
                        ),
                        // Fenced earlier by another node of a newer epoch, or
                        // the fence failed: the refusal is the same.
                        Ok(Err(refusal)) => format!(" ({})", refusal.reason),
                        Err(error) => format!(" (fencing this source failed: {error})"),
                    }
                }
                None => String::new(),
            };
            Some(Refusal::new(
                RefuseCode::SourceStale {
                    have: ours,
                    want: theirs,
                },
                format!(
                    "this source is a superseded primary at epoch {ours}{fenced}; the receiver follows epoch \
                     {theirs} of the dataset: point it at the current primary"
                ),
            ))
        }
    }
}

/// Spec P5.1 §3.1: answer a receiver's `Fence` (`payload`): `Fenced` once
/// this source is fenced — the session goes on — else the refusal, which
/// ends it. A source without a fence (a cascading standby) and a fence of no
/// higher epoch than the source's are refused `Version`.
async fn answer_fence<I: AsyncWrite + Unpin>(
    io: &mut I,
    source: &SourceContext,
    hello: &Hello,
    payload: &[u8],
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let fence = Fence::decode(payload)?;
    let ours = source.identity.epoch;
    let refusal = match &source.fence {
        None => Refusal::new(
            RefuseCode::Version,
            "this source cannot be fenced: it is not a primary (a cascading standby stops with its own primary)",
        ),
        Some(_) if fence.epoch <= ours => Refusal::new(
            RefuseCode::Version,
            format!(
                "a fence must name a higher epoch than this source's {ours}; it named {}",
                fence.epoch
            ),
        ),
        // Spec P5.1 §11.C: only a standby of THIS dataset that holds data —
        // its `Hello` names the history and a cursor — may fence it, and only
        // by the epoch after its own (the handshake made that the source's
        // own + 1). A fresh receiver, or a fence that skips epochs, would let
        // anything holding the token stop the primary for good.
        Some(_) if hello.history != Some(source.identity.history) || hello.cursor.is_none() => Refusal::new(
            RefuseCode::Version,
            "a fence must come from a standby of this dataset that holds data (a Hello naming the history and a \
             cursor)",
        ),
        Some(_) if Some(fence.epoch) != hello.epoch.checked_add(1) => Refusal::new(
            RefuseCode::Version,
            format!(
                "a fence must name the epoch after the receiver's {} (it named {})",
                hello.epoch, fence.epoch
            ),
        ),
        Some(fencer) => {
            let outcome = bounded(shutdown, limits.operation_timeout, async {
                Ok(fencer.fence(fence.epoch, hello.node_id).await)
            })
            .await?;
            match outcome {
                Ok(fenced) => return write_frame(io, Kind::Fenced, &fenced.encode()?, limits, shutdown).await,
                Err(refusal) => refusal,
            }
        }
    };
    Err(refuse(io, refusal, limits, shutdown).await)
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
            // Every batch after the first fits the limit (12 bytes at least
            // each); the first travels whatever its size, so one batch is
            // always a legal poll, even under a limit below 12 bytes.
            || (self.batches > 1 && self.batches as usize > byte_limit / 12)
        {
            return Err(invalid("inconsistent physical poll summary"));
        }
        Ok(())
    }
}

/// Bind a source's replication listener. Spec P2.2: a non-loopback address
/// (a wildcard included) is refused unless the listener serves TLS (`tls`) or
/// plaintext over the network was explicitly allowed
/// ([`remote_transport_allowed`]).
pub async fn bind_replication(address: SocketAddr, tls: bool, allow_insecure_remote: bool) -> Result<TcpListener> {
    if !remote_transport_allowed(&address, tls, allow_insecure_remote) {
        return Err(invalid(format!(
            "the replication port may bind the non-loopback address {address} only with TLS (--replication-tls) \
             or the plaintext opt-in (--allow-insecure-replication-remote)"
        )));
    }
    Ok(TcpListener::bind(address).await?)
}

/// The source's side of one accepted connection: `TCP_NODELAY` (spec P4.1b),
/// then the TLS handshake when the listener serves TLS (`acceptor`), bounded
/// by the frame deadline and the shutdown watch. A failed handshake is
/// [`WireError::Tls`]: nothing was read from the peer, and no `Refuse` frame
/// can be sent before a handshake.
pub async fn accept_replication(
    stream: TcpStream,
    acceptor: Option<&TlsAcceptor>,
    limits: &WireLimits,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ReplicationStream> {
    // Before the handshake: every answer of this request/response session
    // leaves at once instead of waiting for the receiver's delayed ACK.
    stream.set_nodelay(true)?;
    let Some(acceptor) = acceptor else {
        return Ok(ReplicationStream::Plain(stream));
    };
    let stream = bounded(shutdown, limits.frame_timeout, async {
        acceptor
            .accept(stream)
            .await
            .map_err(|error| WireError::Tls(error.to_string()))
    })
    .await?;
    Ok(ReplicationStream::Tls(Box::new(stream.into())))
}

/// A TLS alert that arrived after the handshake (a mutual-TLS source that
/// refused this receiver's certificate) surfaces as an I/O error carrying a
/// `rustls` error: report it as the TLS failure it is.
fn tls_failure(error: WireError) -> WireError {
    match error {
        WireError::Io(io) if io.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) => {
            WireError::Tls(io.to_string())
        }
        other => other,
    }
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
    // Spec P5.1 §2: a receiver that holds data (a cursor) is at an epoch of
    // its own; a fresh snapshot receiver is not, and takes this source's.
    if hello.cursor.is_some() {
        if let Some(refusal) = source_epoch_refusal(source, &hello, &limits, &mut shutdown).await {
            return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
        }
    }
    // Registered from here until this function returns, however it returns.
    let mut observed = source.observer.admit(peer, &hello);
    let ok = HelloOk {
        node_id: identity.node_id,
        history: identity.history,
        epoch: identity.epoch,
        barrier: source.observer.barrier().min(MAX_SOURCE_SEQUENCE),
        capabilities: CAPABILITIES
            | if source.tls { CAP_TLS } else { 0 }
            | if source.fence.is_some() { CAP_FENCE } else { 0 },
        physical_endpoint: identity.physical_endpoint.clone(),
        pg_port: identity.pg_port,
    };
    write_frame(&mut io, Kind::HelloOk, &ok.encode()?, &limits, &mut shutdown).await?;
    // Spec P5.1 §3.1: a promoting standby may fence this source before it
    // opens a session (or instead of one), as often as it needs to.
    let open = loop {
        let frame = read_frame(&mut io, &[Kind::Open, Kind::Fence, Kind::Close], &limits, &mut shutdown).await?;
        match frame.kind {
            Kind::Close => return close_session(&mut io, &limits, &mut shutdown).await,
            Kind::Fence => answer_fence(&mut io, source, &hello, &frame.payload, &limits, &mut shutdown).await?,
            _ => break frame,
        }
    };
    let open = OpenRequest::decode(&open.payload)?;
    if !open.agrees_with(&hello) {
        return Err(invalid("session open request differs from the receiver's hello"));
    }
    // Spec P5.1 §11.E: a fenced source is a superseded primary — a copy of it
    // (a snapshot, a fast-forward's leaves) would bootstrap a standby of the
    // wrong node. It keeps serving resume sessions: the promoting standby
    // tails it through its final sequence.
    if !matches!(open, OpenRequest::Resume { .. }) {
        if let Some(reason) = source.fence.as_ref().and_then(|fence| fence.fenced()) {
            let refusal = Refusal::new(
                RefuseCode::SourceNotServing,
                format!("this source is a superseded primary, {reason}"),
            );
            return Err(refuse(&mut io, refusal, &limits, &mut shutdown).await);
        }
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
    // Spec P4.1 §3.1: each session kind has its own requests. A fast-forward
    // session reads leaves, never chunks or polls; a snapshot or resume
    // session never reads leaves. Anything else ends the session.
    let fast_forward = info.fast_forward_snapshot();
    let allowed: &[Kind] = if fast_forward.is_some() {
        &[
            Kind::LeafDigestsRequest,
            Kind::LeafRequest,
            Kind::LeafChunkRequest,
            Kind::Heartbeat,
            Kind::Close,
        ]
    } else {
        &[
            Kind::ChunkRequest,
            Kind::PollRequest,
            Kind::Heartbeat,
            Kind::Fence,
            Kind::Close,
        ]
    };
    // The leaf files this session announced, newest last, with their sizes:
    // a leaf chunk request must name one of them.
    let mut ready_leaves: std::collections::VecDeque<(u32, u64)> = std::collections::VecDeque::new();
    // Between two requests the receiver works: it applies (and fsyncs) the
    // whole poll it was just sent — up to its byte limit of write groups, or
    // one larger group — before it sends the next one, and its apply is
    // bounded by its operation timeout. So the wait for a request's header is
    // that plus a frame timeout; every other frame keeps the frame timeout.
    let request_wait = limits.operation_timeout.saturating_add(limits.frame_timeout);
    loop {
        let request = read_frame_within(&mut io, allowed, &limits, request_wait, &mut shutdown).await?;
        match request.kind {
            Kind::LeafDigestsRequest => {
                let snapshot = fast_forward.ok_or_else(|| invalid("leaf digests require a fast-forward session"))?;
                if uuid_at(&request.payload, 0)? != snapshot {
                    return Err(invalid("leaf digests snapshot/session mismatch"));
                }
                let from = u32_at(&request.payload, 16)?;
                let count = u32_at(&request.payload, 20)?;
                if count == 0 || count > MAX_LEAVES || from >= MAX_LEAVES {
                    return Err(invalid("invalid leaf digests request"));
                }
                let page = match bounded(
                    &mut shutdown,
                    limits.operation_timeout,
                    source_session.leaf_digests(from, count),
                )
                .await
                {
                    Ok(page) => page,
                    Err(error) => {
                        return Err(match source_refusal(&error) {
                            Some(refusal) => refuse(&mut io, refusal, &limits, &mut shutdown).await,
                            None => error,
                        })
                    }
                };
                if page.snapshot_id != snapshot
                    || (page.status == LeafStatus::Ready && (page.from != from || page.entries.len() > count as usize))
                {
                    return Err(invalid("the source answered another leaf page than the one asked for"));
                }
                write_frame(&mut io, Kind::LeafDigests, &page.encode()?, &limits, &mut shutdown).await?;
            }
            Kind::LeafRequest => {
                let snapshot = fast_forward.ok_or_else(|| invalid("leaf files require a fast-forward session"))?;
                if uuid_at(&request.payload, 0)? != snapshot {
                    return Err(invalid("leaf snapshot/session mismatch"));
                }
                let index = u32_at(&request.payload, 16)?;
                if index >= MAX_LEAVES {
                    return Err(invalid("invalid leaf request"));
                }
                let ready = match bounded(&mut shutdown, limits.operation_timeout, source_session.leaf(index)).await {
                    Ok(ready) => ready,
                    Err(error) => {
                        return Err(match source_refusal(&error) {
                            Some(refusal) => refuse(&mut io, refusal, &limits, &mut shutdown).await,
                            None => error,
                        })
                    }
                };
                if ready.snapshot_id != snapshot || ready.index != index {
                    return Err(invalid("the source announced another leaf than the one asked for"));
                }
                ready_leaves.retain(|(leaf, _)| *leaf != index);
                ready_leaves.push_back((index, ready.size));
                while ready_leaves.len() > READY_LEAVES_REMEMBERED {
                    ready_leaves.pop_front();
                }
                write_frame(&mut io, Kind::LeafReady, &ready.encode()?, &limits, &mut shutdown).await?;
            }
            Kind::LeafChunkRequest => {
                let snapshot = fast_forward.ok_or_else(|| invalid("leaf files require a fast-forward session"))?;
                if uuid_at(&request.payload, 0)? != snapshot {
                    return Err(invalid("leaf chunk snapshot/session mismatch"));
                }
                let index = u32_at(&request.payload, 16)?;
                let offset = u64_at(&request.payload, 20)?;
                let length = u32_at(&request.payload, 28)?;
                let size = ready_leaves
                    .iter()
                    .find(|(leaf, _)| *leaf == index)
                    .map(|(_, size)| *size)
                    .ok_or_else(|| invalid("a leaf chunk request before its leaf was announced"))?;
                if length == 0
                    || length as usize > limits.transfer.max_chunk_bytes
                    || offset.checked_add(u64::from(length)).is_none_or(|end| end > size)
                {
                    return Err(invalid("leaf chunk request outside the leaf file"));
                }
                let bytes = bounded(
                    &mut shutdown,
                    limits.operation_timeout,
                    source_session.read_leaf_chunk(index, offset, length),
                )
                .await?;
                if bytes.len() != length as usize {
                    return Err(invalid("the source returned a short/oversized leaf chunk"));
                }
                let mut payload = Vec::with_capacity(CHUNK_META + bytes.len());
                payload.extend(snapshot.as_bytes());
                payload.extend(index.to_be_bytes());
                payload.extend(offset.to_be_bytes());
                payload.extend(Sha256::digest(&bytes));
                payload.extend(bytes);
                write_frame(&mut io, Kind::LeafChunk, &payload, &limits, &mut shutdown).await?;
            }
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
                    || byte_limit > MAX_POLL_BYTES
                {
                    return Err(invalid("invalid physical continuation request"));
                }
                // Collecting the poll — reading, validating and digesting its
                // write groups, fsyncing the WAL — happens before `PollStart`
                // is sent and is bounded by the operation timeout. The
                // receiver waits that plus a frame timeout for `PollStart`
                // (`PhysicalClient::poll`), so a source still within this
                // bound is never a deadline on the receiver's side.
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
                observed.served(source_session.poll_origin());
                let count = u32::try_from(poll.batches.len()).map_err(|_| invalid("too many physical batches"))?;
                let summary = PollSummary {
                    through: poll.through,
                    durable_barrier: poll.durable_barrier,
                    batches: count,
                };
                summary.validate_since(after, byte_limit, known_barrier)?;
                let mut cursor = after;
                let mut bytes = 0usize;
                for (index, batch) in poll.batches.iter().enumerate() {
                    bytes = bytes
                        .checked_add(batch.data().len())
                        .ok_or_else(|| invalid("batch byte overflow"))?;
                    // Spec P4.1c: the limit is a coalescing target. The
                    // first batch of a poll may exceed it (a write group is
                    // never split); a later one may not take the total past it.
                    if batch.history() != info.history()
                        || cursor.checked_add(1) != Some(batch.first_sequence())
                        || batch.last_sequence() > summary.through
                        || (bytes > byte_limit && index > 0)
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
                    // The metadata and the raw group go out as one frame,
                    // the group never copied (it may be gigabytes).
                    let mut meta = Vec::with_capacity(BATCH_META);
                    meta.extend(batch.history().as_bytes());
                    meta.extend(batch.first_sequence().to_be_bytes());
                    meta.extend(batch.digest());
                    write_frame_parts(&mut io, Kind::Batch, &meta, batch.data(), &limits, &mut shutdown).await?;
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
            // Spec P5.1 §3.1: between two requests of a snapshot or resume
            // session (a fast-forward session's allowed kinds exclude it).
            // The session goes on: the fenced source keeps serving it.
            Kind::Fence => {
                answer_fence(&mut io, source, &hello, &request.payload, &limits, &mut shutdown).await?;
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
        read_reply(&mut io, &[Kind::AuthOk], &reply_limits, &mut shutdown).await?;
        write_frame(&mut io, Kind::Hello, &hello_bytes, &limits, &mut shutdown).await?;
        let frame = read_reply(&mut io, &[Kind::HelloOk], &reply_limits, &mut shutdown).await?;
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
        // Spec P5.1 §2, the receiver's half of the policy: only a receiver
        // that holds data (a cursor) has an epoch to compare; a fresh
        // snapshot takes the source's with its copy.
        if hello.cursor.is_some() {
            if let Some(refusal) = receiver_epoch_refusal(hello.epoch, hello_ok.epoch) {
                return Err(WireError::Refused(refusal));
            }
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
    /// Spec P2.2: whether this session is on TLS — both ends said so
    /// ([`CAP_TLS`]).
    pub fn tls(&self) -> bool {
        self.hello_ok.capabilities & self.hello.capabilities & CAP_TLS != 0
    }
    /// Spec P4.1: whether the source serves fast-forward sessions
    /// ([`CAP_FAST_FORWARD`] in its `HelloOk`).
    pub fn fast_forward(&self) -> bool {
        self.hello_ok.capabilities & CAP_FAST_FORWARD != 0
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
        self.fetch_manifest_preferring(expected_history, None).await
    }
    /// [`Self::fetch_manifest`], asking for the export `preferred` (spec
    /// P2.2: the snapshot an interrupted download belongs to). The source may
    /// answer with another snapshot — its lease expired — so the caller
    /// compares the manifest it gets with the one it holds.
    pub async fn fetch_manifest_preferring(
        &mut self,
        expected_history: Option<Uuid>,
        preferred: Option<Uuid>,
    ) -> Result<CheckpointManifest> {
        self.check_live()?;
        // Arm BEFORE any await: cancellation/drop can leave a partial frame or
        // durable sink prefix, even when no Result is returned to this caller.
        self.poisoned = true;
        let outcome = async {
            if self.info.is_some() {
                return Err(invalid("manifest already selected for this session"));
            }
            let request = preferred.map_or(OpenRequest::Snapshot, |snapshot| OpenRequest::SnapshotOf { snapshot });
            if !request.agrees_with(&self.hello) {
                return Err(invalid("a snapshot session announces no cursor in its hello"));
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
            let frame = read_reply(&mut self.io, &[Kind::Manifest], &limits, &mut self.shutdown).await?;
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
    /// Select a new wire session for an existing durable physical receiver
    /// at `after`. Spec P5.1 §11.F: `last` is its cursor's last applied write
    /// group (`None` at a checkpoint cut), which the source verifies against
    /// its own before it serves the session.
    pub async fn resume(&mut self, history: Uuid, after: u64, last: Option<LastGroup>) -> Result<SourceSessionInfo> {
        self.check_live()?;
        self.poisoned = true;
        let outcome = async {
            if self.info.is_some() {
                return Err(invalid("session already selected"));
            }
            let request = OpenRequest::Resume { history, after, last };
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
            let frame = read_reply(&mut self.io, &[Kind::ResumeInfo], &limits, &mut self.shutdown).await?;
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
    /// Spec P4.1 §3.1: open a fast-forward session for this receiver of
    /// `history` at `after` (both announced in its `Hello`), with leaves of
    /// `leaf_bytes`, preferring the export `prefer` (a resumed fast-forward).
    /// A source without [`CAP_FAST_FORWARD`] is refused here, before anything
    /// is sent; a receiver at or past the source's cut is refused by the
    /// source (`HistoryMismatch`).
    pub async fn open_fast_forward(
        &mut self,
        history: Uuid,
        after: u64,
        leaf_bytes: u32,
        prefer: Option<Uuid>,
    ) -> Result<SourceSessionInfo> {
        self.check_live()?;
        if !self.fast_forward() {
            return Err(WireError::Refused(Refusal::new(
                RefuseCode::Version,
                "the primary's release has no fast-forward; use --full",
            )));
        }
        self.poisoned = true;
        let outcome = async {
            if self.info.is_some() {
                return Err(invalid("session already selected"));
            }
            let request = OpenRequest::FastForward {
                history,
                after,
                leaf_bytes,
                prefer,
            };
            if !request.agrees_with(&self.hello) {
                return Err(invalid(
                    "a fast-forward session announces its history and cursor in its hello",
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
            let frame = read_reply(&mut self.io, &[Kind::FastForwardInfo], &limits, &mut self.shutdown).await?;
            let info = SourceSessionInfo::fast_forward_from(&frame.payload, request, &self.limits)?;
            if info.history() != self.hello_ok.history {
                return Err(invalid("fast-forward history mismatch"));
            }
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
    /// The open fast-forward session's export.
    fn fast_forward_snapshot(&self) -> Result<Uuid> {
        self.info
            .as_ref()
            .and_then(SourceSessionInfo::fast_forward_snapshot)
            .ok_or_else(|| invalid("leaves require an open fast-forward session"))
    }
    /// One page of the source's leaf plan: up to `count` leaves from `from`,
    /// or a planning page. Answered within the frame deadline.
    pub async fn leaf_digests(&mut self, from: u32, count: u32) -> Result<LeafDigests> {
        self.check_live()?;
        self.poisoned = true;
        let outcome = async {
            let snapshot = self.fast_forward_snapshot()?;
            if count == 0 || count > MAX_LEAVES || from >= MAX_LEAVES {
                return Err(invalid("invalid leaf digests request"));
            }
            let mut request = Vec::with_capacity(LEAF_DIGESTS_REQUEST_BYTES);
            request.extend(snapshot.as_bytes());
            request.extend(from.to_be_bytes());
            request.extend(count.to_be_bytes());
            write_frame(
                &mut self.io,
                Kind::LeafDigestsRequest,
                &request,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let frame = read_reply(&mut self.io, &[Kind::LeafDigests], &self.limits, &mut self.shutdown).await?;
            let page = LeafDigests::decode(&frame.payload)?;
            if page.snapshot_id != snapshot
                || (page.status == LeafStatus::Ready && (page.from != from || page.entries.len() > count as usize))
            {
                return Err(invalid("the source answered another leaf page than the one asked for"));
            }
            Ok(page)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    /// The source's whole leaf plan, page by page (spec P4.1 §3.3.2). While
    /// the source is still planning it is asked again every 300 ms — each
    /// ask a frame of its own, so a long plan never runs into a frame
    /// deadline — for at most `LEAF_PLANNING_DEADLINES` (30) operation
    /// deadlines in all. The pages must make one plan: leaf 0 at the empty
    /// key, boundaries in order across pages, the same leaf count
    /// throughout, the leaves' bytes adding up to the plan's total. Any
    /// failure, the plan's own included, poisons the client.
    pub async fn fetch_leaf_digests(&mut self) -> Result<LeafPlan> {
        let outcome = self.fetch_leaf_plan().await;
        if outcome.is_err() {
            self.poisoned = true;
        }
        outcome
    }
    async fn fetch_leaf_plan(&mut self) -> Result<LeafPlan> {
        let snapshot = self.fast_forward_snapshot()?;
        let planning_deadline = self.limits.operation_timeout.saturating_mul(LEAF_PLANNING_DEADLINES);
        let started = tokio::time::Instant::now();
        let mut leaves: Vec<LeafEntry> = Vec::new();
        let mut plan: Option<(u32, u64)> = None;
        loop {
            let from = u32::try_from(leaves.len()).map_err(|_| invalid("leaf plan overflow"))?;
            // Everything that is left; the source pages it.
            let page = self.leaf_digests(from, MAX_LEAVES.saturating_sub(from).max(1)).await?;
            if page.status == LeafStatus::Planning {
                if plan.is_some() {
                    return Err(invalid(
                        "the source started planning again in the middle of its leaf plan",
                    ));
                }
                if started.elapsed() >= planning_deadline {
                    return Err(WireError::Source(format!(
                        "the source did not finish planning its leaves within {} s ({LEAF_PLANNING_DEADLINES} \
                         operation deadlines); it may be busy: retry later",
                        planning_deadline.as_secs()
                    )));
                }
                let wait = async {
                    tokio::time::sleep(LEAF_PLANNING_RETRY).await;
                    Ok(())
                };
                bounded(&mut self.shutdown, LEAF_PLANNING_RETRY * 2, wait).await?;
                continue;
            }
            let (count, total) = *plan.get_or_insert((page.leaf_count, page.total_bytes));
            if page.leaf_count != count || page.total_bytes != total {
                return Err(invalid("the source's leaf plan changed between pages"));
            }
            if let (Some(last), Some(first)) = (leaves.last(), page.entries.first()) {
                if last.start >= first.start {
                    return Err(invalid("the source's leaf boundaries are out of order across pages"));
                }
            }
            if page.entries.is_empty() && from < count {
                return Err(invalid("the source sent an empty page inside its leaf plan"));
            }
            leaves.extend(page.entries);
            if leaves.len() >= count as usize {
                break;
            }
        }
        let (_, total_bytes) = plan.ok_or_else(|| invalid("an empty leaf plan"))?;
        let mut sum = 0_u64;
        for leaf in &leaves {
            sum = sum
                .checked_add(leaf.bytes)
                .ok_or_else(|| invalid("leaf plan byte overflow"))?;
        }
        if sum != total_bytes || leaves.is_empty() {
            return Err(invalid("the source's leaves do not add up to its plan's total"));
        }
        Ok(LeafPlan {
            snapshot_id: snapshot,
            total_bytes,
            leaves,
        })
    }
    /// Ask the source to build leaf `index`'s SST file; answered once it is
    /// built (within the operation deadline).
    pub async fn request_leaf(&mut self, index: u32) -> Result<LeafReady> {
        self.check_live()?;
        self.poisoned = true;
        let outcome = async {
            let snapshot = self.fast_forward_snapshot()?;
            if index >= MAX_LEAVES {
                return Err(invalid("invalid leaf request"));
            }
            let mut request = Vec::with_capacity(LEAF_REQUEST_BYTES);
            request.extend(snapshot.as_bytes());
            request.extend(index.to_be_bytes());
            write_frame(
                &mut self.io,
                Kind::LeafRequest,
                &request,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let limits = self.reply_limits();
            let frame = read_reply(&mut self.io, &[Kind::LeafReady], &limits, &mut self.shutdown).await?;
            let ready = LeafReady::decode(&frame.payload)?;
            if ready.snapshot_id != snapshot || ready.index != index {
                return Err(invalid("the source announced another leaf than the one asked for"));
            }
            Ok(ready)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    /// `length` bytes of leaf `index`'s file from `offset`, digest-checked
    /// (the whole file's SHA-256 is checked by the caller).
    pub async fn read_leaf_chunk(&mut self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        self.check_live()?;
        self.poisoned = true;
        let outcome = async {
            let snapshot = self.fast_forward_snapshot()?;
            if length == 0
                || length as usize > self.limits.transfer.max_chunk_bytes
                || offset.checked_add(u64::from(length)).is_none()
            {
                return Err(invalid("leaf chunk request outside its bounds"));
            }
            let mut request = Vec::with_capacity(32);
            request.extend(snapshot.as_bytes());
            request.extend(index.to_be_bytes());
            request.extend(offset.to_be_bytes());
            request.extend(length.to_be_bytes());
            write_frame(
                &mut self.io,
                Kind::LeafChunkRequest,
                &request,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let mut frame = read_reply(&mut self.io, &[Kind::LeafChunk], &self.limits, &mut self.shutdown).await?;
            let identity_ok = uuid_at(&frame.payload, 0)? == snapshot
                && u32_at(&frame.payload, 16)? == index
                && u64_at(&frame.payload, 20)? == offset
                && frame.payload.len() == CHUNK_META + length as usize;
            let body = frame
                .payload
                .get(CHUNK_META..)
                .ok_or_else(|| invalid("leaf chunk response truncated"))?;
            let expected = frame
                .payload
                .get(28..60)
                .ok_or_else(|| invalid("leaf chunk response truncated"))?;
            if !identity_ok || Sha256::digest(body).as_slice() != expected {
                return Err(invalid("leaf chunk response identity/length/digest mismatch"));
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
    /// Authenticate over `io` and resume at `after` naming no last write
    /// group (as a cursor at a checkpoint cut does). A real receiver names
    /// its cursor's ([`PhysicalClient::connect_resume_replication`]).
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
        client.resume(history, after, None).await?;
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
            let mut frame = read_reply(&mut self.io, &[Kind::Chunk], &self.limits, &mut self.shutdown).await?;
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
                || byte_limit > MAX_POLL_BYTES
            {
                return Err(invalid("invalid physical continuation request"));
            }
            // MAX_POLL_BYTES is far below u32::MAX.
            let limit_field =
                u32::try_from(byte_limit).map_err(|_| invalid("invalid physical continuation request"))?;
            let mut request = Vec::with_capacity(28);
            request.extend(info.session_id().as_bytes());
            request.extend(after.to_be_bytes());
            request.extend(limit_field.to_be_bytes());
            write_frame(
                &mut self.io,
                Kind::PollRequest,
                &request,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            // The source sends `PollStart` only once it collected the whole
            // poll (up to `byte_limit` of write groups, read, validated and
            // digested, and its WAL fsynced), work it bounds by its operation
            // timeout (`serve_session`, `Kind::PollRequest`). So the wait for
            // `PollStart` is that plus a frame timeout: a frame timeout alone
            // gives up on a source still within its own bound (a poll of many
            // tiny write groups under a write storm). Every other frame of
            // the poll keeps the frame timeout: it follows at once.
            let poll_start_wait = self.limits.operation_timeout.saturating_add(self.limits.frame_timeout);
            let frame = read_reply_within(
                &mut self.io,
                &[Kind::PollStart],
                &self.limits,
                poll_start_wait,
                &mut self.shutdown,
            )
            .await?;
            let summary = PollSummary::decode(&frame.payload)?;
            summary.validate_since(after, byte_limit, self.known_barrier)?;
            sink.begin(summary).await?;
            let mut cursor = after;
            let mut bytes = 0usize;
            // Spec P4.1d: the poll's validated batches, handed to the sink
            // together (`apply_many`) — the whole poll at once, or runs of at
            // most `APPLY_CHUNK_BYTES` in order — instead of one by one. Each
            // batch is validated below before it joins a run, so nothing of a
            // run reaches the sink until all of it is validated.
            let mut run: Vec<PhysicalBatch> = Vec::new();
            let mut run_bytes = 0usize;
            for index in 0..summary.batches {
                // Spec P4.1c: only the poll's first batch may exceed the
                // limit. A later one that would take the poll past it is
                // refused from its frame header, before its payload is
                // allocated or read.
                let max_raw = if index == 0 {
                    usize::MAX
                } else {
                    byte_limit.saturating_sub(bytes)
                };
                let (meta, raw) = read_batch_frame(&mut self.io, max_raw, &self.limits, &mut self.shutdown).await?;
                let history = uuid_at(&meta, 0)?;
                let first = u64_at(&meta, 16)?;
                let digest: [u8; 32] = meta
                    .get(24..56)
                    .and_then(|slice| slice.try_into().ok())
                    .ok_or_else(|| invalid("batch payload truncated"))?;
                bytes = bytes
                    .checked_add(raw.len())
                    .ok_or_else(|| invalid("batch byte overflow"))?;
                if history != info.history()
                    || cursor.checked_add(1) != Some(first)
                    || (bytes > byte_limit && index > 0)
                {
                    return Err(invalid("physical response history/sequence/byte mismatch"));
                }
                let batch = PhysicalBatch::decode(history, first, raw, digest).map_err(|e| invalid(e.to_string()))?;
                if batch.last_sequence() > summary.through {
                    return Err(invalid("batch crosses advertised barrier"));
                }
                cursor = batch.last_sequence();
                let size = batch.data().len();
                if !run.is_empty() && run_bytes.saturating_add(size) > APPLY_CHUNK_BYTES {
                    // The run is full: hand it over before this batch joins
                    // the next one.
                    let full = std::mem::take(&mut run);
                    run_bytes = 0;
                    bounded(&mut self.shutdown, self.limits.operation_timeout, sink.apply_many(full)).await?;
                }
                run_bytes = run_bytes.saturating_add(size);
                run.push(batch);
            }
            if !run.is_empty() {
                bounded(&mut self.shutdown, self.limits.operation_timeout, sink.apply_many(run)).await?;
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
            let frame = read_reply(&mut self.io, &[Kind::HeartbeatAck], &self.limits, &mut self.shutdown).await?;
            HeartbeatAck::decode(&frame.payload)
        }
        .await;
        if outcome.is_ok() {
            self.poisoned = false;
        }
        outcome
    }
    /// Whether the source can be fenced ([`CAP_FENCE`] in its `HelloOk`).
    pub fn fenceable(&self) -> bool {
        self.hello_ok.capabilities & CAP_FENCE != 0
    }
    /// Spec P5.1 §3.1: fence this session's source by `epoch` (a promotion
    /// to it): after `HelloOk`, before or instead of an `Open`, or between
    /// two requests of a snapshot or resume session. `Fenced` once the source
    /// refuses every write and made every earlier one durable; the session
    /// goes on. Waits up to the operation deadline (the source drains its
    /// writes and fsyncs). A source without [`CAP_FENCE`] is refused here,
    /// before anything is sent; a refused fence ends the session
    /// ([`WireError::Refused`]).
    pub async fn fence(&mut self, epoch: u64) -> Result<Fenced> {
        self.check_live()?;
        if !self.fenceable() {
            return Err(WireError::Refused(Refusal::new(
                RefuseCode::Version,
                "the source offers no fencing (CAP_FENCE): it is not a primary of a release that can be fenced",
            )));
        }
        if self
            .info
            .as_ref()
            .and_then(SourceSessionInfo::fast_forward_snapshot)
            .is_some()
        {
            return Err(invalid("a fast-forward session is never fenced"));
        }
        self.poisoned = true;
        let outcome = async {
            write_frame(
                &mut self.io,
                Kind::Fence,
                &Fence { epoch }.encode()?,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let limits = self.reply_limits();
            let frame = read_reply(&mut self.io, &[Kind::Fenced], &limits, &mut self.shutdown).await?;
            let fenced = Fenced::decode(&frame.payload)?;
            if fenced.epoch < epoch {
                return Err(invalid(format!(
                    "the source acknowledged a fence by epoch {}, not the {epoch} asked for",
                    fenced.epoch
                )));
            }
            Ok(fenced)
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
impl PhysicalClient<ReplicationStream> {
    /// [`Self::connect_replication`], then `Open { resume }` at `after`,
    /// naming the cursor's last write group `last` (spec P5.1 §11.F).
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_resume_replication(
        address: SocketAddr,
        transport: &ReplicationTransport,
        token: &TransferToken,
        identity: &ReceiverIdentity,
        limits: WireLimits,
        shutdown: watch::Receiver<bool>,
        history: Uuid,
        after: u64,
        last: Option<LastGroup>,
    ) -> Result<Self> {
        let hello = identity.hello(Some(history), Some(after));
        let mut client = Self::connect_replication(address, transport, token, hello, limits, shutdown).await?;
        client.resume(history, after, last).await?;
        Ok(client)
    }
    /// Connect to a source's replication port over `transport` and
    /// authenticate. Spec P2.2: a non-loopback `address` needs TLS or the
    /// plaintext opt-in; over TLS the source's certificate is verified before
    /// the token is sent, the `Hello` carries [`CAP_TLS`], and a handshake or
    /// alert failure is [`WireError::Tls`]. The socket sets `TCP_NODELAY`
    /// before anything is sent (spec P4.1b).
    pub async fn connect_replication(
        address: SocketAddr,
        transport: &ReplicationTransport,
        token: &TransferToken,
        mut hello: Hello,
        limits: WireLimits,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<Self> {
        if !transport.allows(&address) {
            return Err(invalid(format!(
                "a replication connection to the non-loopback address {address} needs TLS (--primary-tls) or the \
                 plaintext opt-in (--allow-insecure-replication-remote)"
            )));
        }
        limits.validate()?;
        let stream = bounded(&mut shutdown, limits.frame_timeout, async {
            Ok(TcpStream::connect(address).await?)
        })
        .await?;
        // Before the handshake and the token: one request is in flight at a
        // time, so Nagle's algorithm would hold each behind the source's
        // delayed ACK.
        stream.set_nodelay(true)?;
        let Some(tls) = &transport.tls else {
            return Self::authenticate(ReplicationStream::Plain(stream), token, hello, limits, shutdown).await;
        };
        // Files are read per connection: a rotated CA takes effect at the next one.
        let connector = tls.connector().map_err(|error| WireError::Tls(error.to_string()))?;
        let server_name = tls.server_name();
        let stream = bounded(&mut shutdown, limits.frame_timeout, async {
            connector
                .connect(server_name, stream)
                .await
                .map_err(|error| WireError::Tls(error.to_string()))
        })
        .await?;
        hello.capabilities |= CAP_TLS;
        Self::authenticate(
            ReplicationStream::Tls(Box::new(stream.into())),
            token,
            hello,
            limits,
            shutdown,
        )
        .await
        .map_err(tls_failure)
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
                store_sequence: None,
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
            RefuseCode::GroupTooLarge {
                first: 7,
                bytes: u64::MAX,
            },
            // Spec P5.1 §2: id 10.
            RefuseCode::SourceStale { have: 1, want: 2 },
            // Spec P5.1 §11.F: id 11.
            RefuseCode::Diverged { at: u64::MAX },
        ] {
            for reason in ["", "why", &"é".repeat(MAX_REASON_BYTES)] {
                let refusal = Refusal::new(code.clone(), reason);
                assert!(refusal.reason.len() <= MAX_REASON_BYTES);
                let bytes = refusal.encode();
                assert!(Kind::Refuse.valid_size(bytes.len()), "{code:?}");
                assert_eq!(Refusal::decode(&bytes).unwrap(), refusal);
            }
        }
        let stale = Refusal::new(RefuseCode::SourceStale { have: 3, want: 9 }, "").encode();
        assert_eq!(
            u16::from_be_bytes([stale[0], stale[1]]),
            10,
            "SourceStale is refusal code 10"
        );
        let diverged = Refusal::new(RefuseCode::Diverged { at: 41 }, "").encode();
        assert_eq!(
            (u16::from_be_bytes([diverged[0], diverged[1]]), diverged.len()),
            (11, 2 + 8 + 2),
            "Diverged is refusal code 11, its sequence 8 bytes"
        );
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
        // Spec P5.1 §3.1: kind 29, 16 bytes.
        for fenced in [
            Fenced {
                epoch: 2,
                final_sequence: 0,
            },
            Fenced {
                epoch: u64::MAX,
                final_sequence: MAX_SOURCE_SEQUENCE,
            },
        ] {
            let bytes = fenced.encode().unwrap();
            assert_eq!(bytes.len(), 16);
            assert!(Kind::Fenced.valid_size(bytes.len()));
            assert_eq!(Fenced::decode(&bytes).unwrap(), fenced);
        }
        assert_eq!(Kind::Fenced as u16, 29);
        assert_eq!(Kind::decode(29).unwrap(), Kind::Fenced);

        // Spec P4.1: the fast-forward session's frames (kinds 21-27).
        let snapshot = Uuid::new_v4();
        let request = OpenRequest::FastForward {
            history,
            after: 10,
            leaf_bytes: MIN_LEAF_BYTES,
            prefer: None,
        };
        let info = SourceSessionInfo::FastForward {
            session_id: Uuid::new_v4(),
            snapshot_id: snapshot,
            history,
            after: 10,
            sequence: 31_897,
        };
        let (kind, bytes) = info.response().unwrap();
        assert_eq!(kind, Kind::FastForwardInfo);
        assert!(Kind::FastForwardInfo.valid_size(bytes.len()));
        let decoded = SourceSessionInfo::fast_forward_from(&bytes, request, &limits()).unwrap();
        assert!(matches!(
            decoded,
            SourceSessionInfo::FastForward { snapshot_id, history: h, after: 10, sequence: 31_897, .. }
                if snapshot_id == snapshot && h == history
        ));
        assert_eq!(decoded.session_id(), info.session_id());
        assert_eq!(decoded.fast_forward_snapshot(), Some(snapshot));
        assert!(decoded.manifest().is_none());
        let leaf = |start: &[u8], bytes: u64| LeafEntry {
            start: start.to_vec(),
            bytes,
            keys: 3,
            digest: [7; 32],
        };
        for page in [
            LeafDigests::planning(snapshot),
            LeafDigests {
                snapshot_id: snapshot,
                status: LeafStatus::Ready,
                leaf_count: 3,
                total_bytes: 300,
                from: 0,
                entries: vec![leaf(b"", 100), leaf(b"m", 150), leaf(b"t", 50)],
            },
            LeafDigests {
                snapshot_id: snapshot,
                status: LeafStatus::Ready,
                leaf_count: 3,
                total_bytes: 300,
                from: 2,
                entries: vec![leaf(&[0xff; 63], 50)],
            },
            LeafDigests {
                snapshot_id: snapshot,
                status: LeafStatus::Ready,
                leaf_count: 1,
                total_bytes: 0,
                from: 0,
                entries: vec![LeafEntry {
                    start: Vec::new(),
                    bytes: 0,
                    keys: 0,
                    digest: empty_sha256(),
                }],
            },
        ] {
            let bytes = page.encode().unwrap();
            assert!(Kind::LeafDigests.valid_size(bytes.len()));
            assert_eq!(LeafDigests::decode(&bytes).unwrap(), page);
        }
        for ready in [
            LeafReady {
                snapshot_id: snapshot,
                index: 5,
                size: 4 * 1024 * 1024,
                sha256: [9; 32],
            },
            LeafReady {
                snapshot_id: snapshot,
                index: MAX_LEAVES - 1,
                size: 0,
                sha256: empty_sha256(),
            },
        ] {
            let bytes = ready.encode().unwrap();
            assert!(Kind::LeafReady.valid_size(bytes.len()));
            assert_eq!(LeafReady::decode(&bytes).unwrap(), ready);
        }
        for (kind, valid, too_long) in [
            (
                Kind::FastForwardInfo,
                FAST_FORWARD_INFO_BYTES,
                FAST_FORWARD_INFO_BYTES + 1,
            ),
            (
                Kind::LeafDigestsRequest,
                LEAF_DIGESTS_REQUEST_BYTES,
                LEAF_DIGESTS_REQUEST_BYTES + 1,
            ),
            (Kind::LeafDigests, LEAF_DIGESTS_HEADER_BYTES, MAX_LEAF_DIGESTS_BYTES + 1),
            (Kind::LeafRequest, LEAF_REQUEST_BYTES, LEAF_REQUEST_BYTES + 1),
            (Kind::LeafReady, LEAF_READY_BYTES, LEAF_READY_BYTES + 1),
            (Kind::LeafChunkRequest, 32, 33),
            (
                Kind::LeafChunk,
                CHUNK_META + MAX_CHUNK_BYTES,
                CHUNK_META + MAX_CHUNK_BYTES + 1,
            ),
        ] {
            assert!(kind.valid_size(valid), "{kind:?} {valid}");
            assert!(!kind.valid_size(too_long), "{kind:?} {too_long}");
            assert_eq!(Kind::decode(kind as u16).unwrap(), kind);
        }
        assert!(!Kind::LeafChunk.valid_size(CHUNK_META), "a leaf chunk carries bytes");
        assert!(!Kind::LeafDigests.valid_size(LEAF_DIGESTS_HEADER_BYTES - 1));
        assert!(Kind::decode(28).is_err(), "kind 28 is not assigned");
        assert!(Kind::decode(30).is_err(), "the next kind is not assigned yet");
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
        // Spec P5.1: a fence acknowledgement of no promotion (epoch below 2),
        // past the sequence space, or of the wrong size.
        assert!(Fenced {
            epoch: 1,
            final_sequence: 5
        }
        .encode()
        .is_err());
        assert!(Fenced {
            epoch: 2,
            final_sequence: MAX_SOURCE_SEQUENCE + 1
        }
        .encode()
        .is_err());
        assert!(Fenced::decode(&[0; FENCED_BYTES - 1]).is_err());
        let mut bytes = Fenced {
            epoch: 2,
            final_sequence: 5,
        }
        .encode()
        .unwrap();
        bytes[0..8].copy_from_slice(&0_u64.to_be_bytes());
        assert!(Fenced::decode(&bytes).is_err());
        assert!(!Kind::Fenced.valid_size(FENCED_BYTES + 1));

        // Spec P4.1: a fast-forward answer that cannot be this session's: a
        // cut at or before the receiver, a nil session, the wrong size, an
        // answer to another kind of open.
        let snapshot = Uuid::new_v4();
        let request = OpenRequest::FastForward {
            history,
            after: 10,
            leaf_bytes: MIN_LEAF_BYTES,
            prefer: None,
        };
        let answer = |session_id: Uuid, sequence: u64| {
            SourceSessionInfo::FastForward {
                session_id,
                snapshot_id: snapshot,
                history,
                after: 10,
                sequence,
            }
            .response()
            .unwrap()
            .1
        };
        for (bytes, why) in [
            (answer(Uuid::new_v4(), 10), "a cut at the receiver"),
            (answer(Uuid::new_v4(), 9), "a cut behind the receiver"),
            (answer(Uuid::nil(), 11), "a nil session"),
            (
                answer(Uuid::new_v4(), MAX_SOURCE_SEQUENCE + 1),
                "a cut past the sequence space",
            ),
            (answer(Uuid::new_v4(), 11)[..39].to_vec(), "the wrong size"),
        ] {
            assert!(
                SourceSessionInfo::fast_forward_from(&bytes, request, &limits()).is_err(),
                "{why}"
            );
        }
        assert!(SourceSessionInfo::fast_forward_from(
            &answer(Uuid::new_v4(), 11),
            OpenRequest::Resume {
                history,
                after: 10,
                last: None
            },
            &limits()
        )
        .is_err());

        // Leaf pages that cannot be one plan's.
        let leaf = |start: &[u8]| LeafEntry {
            start: start.to_vec(),
            bytes: 1,
            keys: 1,
            digest: [1; 32],
        };
        let ready = |from: u32, entries: Vec<LeafEntry>| LeafDigests {
            snapshot_id: snapshot,
            status: LeafStatus::Ready,
            leaf_count: 3,
            total_bytes: 3,
            from,
            entries,
        };
        for (page, why) in [
            (ready(0, vec![leaf(b"a")]), "leaf 0 starts at the empty key"),
            (ready(1, vec![leaf(b"")]), "only leaf 0 starts at the empty key"),
            (ready(0, vec![leaf(b""), leaf(b"m"), leaf(b"c")]), "boundaries in order"),
            (
                ready(0, vec![leaf(b""), leaf(b"m"), leaf(b"m")]),
                "boundaries strictly in order",
            ),
            (ready(1, vec![leaf(&[0xff; 64])]), "a start at the end of the keyspace"),
            (ready(4, Vec::new()), "a page past the plan"),
            (
                ready(2, vec![leaf(b"m"), leaf(b"t")]),
                "more leaves than the plan holds",
            ),
            (
                LeafDigests {
                    leaf_count: 0,
                    ..ready(0, Vec::new())
                },
                "a ready plan with no leaf",
            ),
            (
                LeafDigests {
                    leaf_count: MAX_LEAVES + 1,
                    ..ready(0, vec![leaf(b"")])
                },
                "more leaves than a plan may have",
            ),
            (
                LeafDigests {
                    status: LeafStatus::Planning,
                    ..ready(0, vec![leaf(b"")])
                },
                "a planning page carries leaves",
            ),
            (
                LeafDigests {
                    snapshot_id: Uuid::nil(),
                    ..LeafDigests::planning(snapshot)
                },
                "a page of no snapshot",
            ),
            (
                ready(1, vec![leaf(&vec![b'k'; MAX_LEAF_START_BYTES + 1])]),
                "a start too long for its length field",
            ),
        ] {
            assert!(page.encode().is_err(), "{why}");
        }
        let good = ready(0, vec![leaf(b""), leaf(b"m")]).encode().unwrap();
        assert!(LeafDigests::decode(&good[..good.len() - 1]).is_err(), "truncated");
        let mut longer = good.clone();
        longer.push(0);
        assert!(LeafDigests::decode(&longer).is_err(), "a trailing byte");
        let mut status = good.clone();
        status[16] = 2;
        assert!(LeafDigests::decode(&status).is_err(), "an unknown status");
        let mut count = good.clone();
        count[33..37].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(
            LeafDigests::decode(&count).is_err(),
            "a count past what the frame holds"
        );
        assert!(LeafDigests::decode(&good[..LEAF_DIGESTS_HEADER_BYTES - 1]).is_err());

        // Leaf announcements that cannot be.
        for ready in [
            LeafReady {
                snapshot_id: snapshot,
                index: 0,
                size: 0,
                sha256: [3; 32],
            },
            LeafReady {
                snapshot_id: snapshot,
                index: MAX_LEAVES,
                size: 1,
                sha256: [3; 32],
            },
            LeafReady {
                snapshot_id: Uuid::nil(),
                index: 0,
                size: 1,
                sha256: [3; 32],
            },
        ] {
            assert!(ready.encode().is_err(), "{ready:?}");
        }
        assert!(LeafReady::decode(&[0; LEAF_READY_BYTES - 1]).is_err());
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
            RefuseCode::GroupTooLarge { first: 1, bytes: 2 },
            RefuseCode::Diverged { at: 7 },
        ] {
            assert!(code.needs_recovery() && !code.retryable(), "{code:?}");
        }
        // Spec P5.1 §2: a superseded source is retried (the receiver is the
        // one that is right; the fix is a `--primary-host` change).
        for code in [
            RefuseCode::TooManySessions,
            RefuseCode::SourceNotServing,
            RefuseCode::SourceStale { have: 1, want: 2 },
        ] {
            assert!(code.retryable() && !code.needs_recovery(), "{code:?}");
        }
        // The tables of spec P5.1 §2, by id: needs_recovery = {2,3,4,5,8,9}
        // and (§11.F) 11, retryable = {6,7,10}, AuthFailed (1) neither.
        let all = [
            RefuseCode::AuthFailed,
            RefuseCode::HistoryMismatch,
            RefuseCode::Gap {
                expected: 1,
                have_from: 2,
                have_to: 3,
            },
            RefuseCode::WalPruned { oldest: 1 },
            RefuseCode::EpochStale { have: 1, want: 2 },
            RefuseCode::TooManySessions,
            RefuseCode::SourceNotServing,
            RefuseCode::Version,
            RefuseCode::GroupTooLarge { first: 1, bytes: 2 },
            RefuseCode::SourceStale { have: 1, want: 2 },
            RefuseCode::Diverged { at: 7 },
        ];
        let needs: Vec<u16> = all
            .iter()
            .filter(|code| code.needs_recovery())
            .map(RefuseCode::id)
            .collect();
        let retry: Vec<u16> = all.iter().filter(|code| code.retryable()).map(RefuseCode::id).collect();
        assert_eq!(needs, vec![2, 3, 4, 5, 8, 9, 11]);
        assert_eq!(retry, vec![6, 7, 10]);
        assert!(!RefuseCode::AuthFailed.retryable() && !RefuseCode::AuthFailed.needs_recovery());
        assert_eq!(RefuseCode::SourceStale { have: 1, want: 2 }.name(), "SourceStale");
        assert_eq!(RefuseCode::Diverged { at: 7 }.name(), "Diverged");
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
        let summary = client.poll(10, MAX_POLL_BYTES, &mut sink).await.unwrap();
        assert_eq!(
            summary,
            PollSummary {
                through: 11,
                durable_barrier: 11,
                batches: 1
            }
        );
        assert_eq!(sink.received, vec![(source.manifest.history, 11, 11)]);
        let empty = client.poll(11, MAX_POLL_BYTES, &mut sink).await.unwrap();
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
            tls: false,
            fence: None,
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
            tls: false,
            fence: None,
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
        client.poll(10, MAX_POLL_BYTES, &mut Sink::default()).await.unwrap();
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
    /// right after its `Hello`, with a typed code, before any export. Spec
    /// P5.1 §2: a receiver behind the source's epoch is `EpochStale`, one
    /// ahead of it `SourceStale` (this source cannot be fenced: no fence).
    #[tokio::test]
    async fn history_and_epoch_mismatches_are_refused_after_hello_before_export() {
        for (case, code) in [
            (0, RefuseCode::HistoryMismatch),
            (1, RefuseCode::SourceStale { have: 1, want: 2 }),
            (2, RefuseCode::EpochStale { have: 3, want: 1 }),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorder = Arc::new(Recorder::default());
            let source = Arc::new(MockSource::new());
            let history = source.manifest.history;
            let (hello, source_epoch) = match case {
                0 => (receiver().hello(Some(Uuid::new_v4()), Some(10)), 1),
                1 => (
                    Hello {
                        epoch: 2,
                        ..receiver().hello(Some(history), Some(10))
                    },
                    1,
                ),
                _ => (receiver().hello(Some(history), Some(10)), 3),
            };
            let context = SourceContext {
                identity: SourceIdentity {
                    epoch: source_epoch,
                    ..identity(history)
                },
                observer: recorder.clone(),
                tls: false,
                fence: None,
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
        let outcome = client.poll(10, MAX_POLL_BYTES, &mut sink).await;
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

    /// A source's fence for the tests: records every call and applies the
    /// rule `replication::fence::PrimaryFence` applies — the first fence wins,
    /// a repeated fence by the same node answers the same `Fenced`, another
    /// node's is refused naming the first.
    struct MockFence {
        calls: Mutex<Vec<(u64, Uuid)>>,
        applied: Mutex<Option<(u64, Uuid)>>,
        final_sequence: u64,
    }
    impl MockFence {
        fn new(final_sequence: u64) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                applied: Mutex::new(None),
                final_sequence,
            })
        }
    }
    #[async_trait]
    impl SourceFence for MockFence {
        async fn fence(&self, epoch: u64, node: Uuid) -> std::result::Result<Fenced, Refusal> {
            self.calls.lock().unwrap().push((epoch, node));
            let mut applied = self.applied.lock().unwrap();
            match *applied {
                Some((by, first)) if first == node => Ok(Fenced {
                    epoch: by,
                    final_sequence: self.final_sequence,
                }),
                Some((by, first)) => Err(Refusal::new(
                    RefuseCode::Version,
                    format!("this primary was already fenced by epoch {by} (node {first})"),
                )),
                None => {
                    *applied = Some((epoch, node));
                    Ok(Fenced {
                        epoch,
                        final_sequence: self.final_sequence,
                    })
                }
            }
        }

        fn fenced(&self) -> Option<String> {
            let applied = *self.applied.lock().unwrap();
            applied.map(|(by, _)| format!("fenced by epoch {by}: bootstrap from the new primary"))
        }
    }
    /// [`server`] for a source at `epoch` that can be fenced by `fence`.
    async fn fenceable_server(
        io: DuplexStream,
        source: Arc<MockSource>,
        fence: Option<Arc<MockFence>>,
        epoch: u64,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let context = SourceContext {
            identity: SourceIdentity {
                epoch,
                ..identity(source.manifest.history)
            },
            observer: Arc::new(NoSessionObserver),
            tls: false,
            fence: fence.map(|fence| fence as Arc<dyn SourceFence>),
        };
        serve_session(io, &token(), limits(), shutdown, &context, None, move |_| async move {
            Ok(source as Arc<dyn PhysicalSource>)
        })
        .await
    }

    /// Spec P5.1 §3.1: a source that cannot be fenced (a cascading standby)
    /// offers no `CAP_FENCE`, and a receiver that fences it anyway — before
    /// its `Open` — is refused `Version`.
    #[tokio::test]
    async fn a_source_without_a_fence_offers_none_and_refuses_one() {
        let source = Arc::new(MockSource::new());
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(256);
        let run = tokio::spawn(fenceable_server(peer, source.clone(), None, 1, rx.clone()));
        let hello = receiver().hello(Some(source.manifest.history), Some(10));
        let mut client = PhysicalClient::authenticate(io, &token(), hello, limits(), rx)
            .await
            .unwrap();
        assert!(!client.fenceable(), "a source without a fence never offers CAP_FENCE");
        assert_eq!(client.hello_ok().capabilities & CAP_FENCE, 0);
        // The client refuses before sending anything...
        assert_eq!(refused(&client.fence(2).await), Some(RefuseCode::Version));
        // ...and a raw fence is refused by the source.
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, peer) = duplex(256);
        let run2 = tokio::spawn(fenceable_server(peer, source.clone(), None, 1, rx.clone()));
        raw_open(&mut io, &fresh(), &mut rx).await;
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
        let refusal = Refusal::decode(&reply.payload).unwrap();
        assert_eq!(refusal.code, RefuseCode::Version);
        assert!(refusal.reason.contains("cannot be fenced"), "{refusal}");
        assert_eq!(refused(&run2.await.unwrap()), Some(RefuseCode::Version));
        drop(client);
        let _ = run.await;
    }

    /// Spec P5.1 §3.1: before `Open` and between two requests of a resume
    /// session a fence is answered `Fenced { epoch, final_sequence }` and the
    /// session goes on (the fenced source keeps serving polls); a repeated
    /// fence by the same node answers the same `Fenced`; a fence that names
    /// no higher epoch than the source's own is refused `Version`.
    #[tokio::test]
    async fn a_fence_is_answered_fenced_and_the_session_goes_on() {
        let source = Arc::new(resume_source(&MockSource::new(), 10));
        let history = source.manifest.history;
        let fence = MockFence::new(77);
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            1,
            rx.clone(),
        ));
        let hello = receiver().hello(Some(history), Some(10));
        let mut client = PhysicalClient::authenticate(io, &token(), hello, limits(), rx)
            .await
            .unwrap();
        assert!(client.fenceable(), "a primary offers CAP_FENCE");
        let fenced = client.fence(2).await.unwrap();
        assert_eq!(
            fenced,
            Fenced {
                epoch: 2,
                final_sequence: 77
            }
        );
        // The session goes on: open it, poll, fence again (idempotent).
        client.resume(history, 10, None).await.unwrap();
        let mut sink = Sink::default();
        let summary = client.poll(10, 1024, &mut sink).await.unwrap();
        assert_eq!(summary.through, 11);
        assert_eq!(sink.received, vec![(history, 11, 11)]);
        assert_eq!(
            client.fence(2).await.unwrap(),
            fenced,
            "a repeated fence answers the same"
        );
        assert_eq!(
            fence.calls.lock().unwrap().as_slice(),
            &[(2, receiver().node_id), (2, receiver().node_id)]
        );
        client.close().await.unwrap();
        run.await.unwrap().unwrap();

        // A fence of no higher epoch than the source's own.
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(MockFence::new(1)),
            3,
            rx.clone(),
        ));
        let hello = Hello {
            epoch: 3,
            ..receiver().hello(Some(history), Some(10))
        };
        let mut client = PhysicalClient::authenticate(io, &token(), hello, limits(), rx)
            .await
            .unwrap();
        let outcome = client.fence(3).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::Version));
        let reason = outcome
            .err()
            .and_then(|error| error.refusal().map(|refusal| refusal.reason.clone()))
            .expect("the session is refused");
        assert!(reason.contains("higher epoch than this source's 3"), "{reason}");
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version));
    }

    /// Spec P5.1 §11.C: a fence is answered only for a standby of this
    /// dataset that holds data (a `Hello` with the history and a cursor) and
    /// only by the epoch after the receiver's; anything else is refused
    /// `Version` before the source's fence is even asked. Spec §11.E: once
    /// fenced, the source refuses to bootstrap anyone (a snapshot open is
    /// `SourceNotServing`, naming the fence) and keeps serving resumes.
    #[tokio::test]
    async fn a_fence_must_come_from_a_standby_of_this_dataset_and_a_fenced_source_bootstraps_nobody() {
        let source = Arc::new(resume_source(&MockSource::new(), 10));
        let history = source.manifest.history;
        let fence = MockFence::new(5);
        let reason_of = |outcome: Result<Fenced>| {
            outcome
                .err()
                .and_then(|error| error.refusal().map(|refusal| refusal.reason.clone()))
                .expect("the fence is refused")
        };

        // A fresh receiver (no history, no cursor).
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            1,
            rx.clone(),
        ));
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx.clone())
            .await
            .unwrap();
        assert!(client.fenceable());
        let reason = reason_of(client.fence(2).await);
        assert!(reason.contains("holds data"), "{reason}");
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version));

        // A fence that skips an epoch.
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            1,
            rx.clone(),
        ));
        let mut client = PhysicalClient::authenticate(
            io,
            &token(),
            receiver().hello(Some(history), Some(10)),
            limits(),
            rx.clone(),
        )
        .await
        .unwrap();
        let reason = reason_of(client.fence(3).await);
        assert!(reason.contains("the epoch after the receiver's 1"), "{reason}");
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version));
        assert!(
            fence.calls.lock().unwrap().is_empty(),
            "neither request reached the source's fence"
        );

        // Fenced: a snapshot open is refused, a resume served.
        *fence.applied.lock().unwrap() = Some((2, Uuid::from_u128(0xF1)));
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            1,
            rx.clone(),
        ));
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx.clone())
            .await
            .unwrap();
        let outcome = client.fetch_manifest(Some(history)).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::SourceNotServing));
        let reason = outcome
            .err()
            .and_then(|error| error.refusal().map(|refusal| refusal.reason.clone()))
            .unwrap_or_default();
        assert!(reason.contains("bootstrap from the new primary"), "{reason}");
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::SourceNotServing));
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            1,
            rx.clone(),
        ));
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, 10)
            .await
            .unwrap();
        let mut sink = Sink::default();
        assert_eq!(client.poll(10, 1024, &mut sink).await.unwrap().through, 11);
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
    }

    /// Spec P5.1 §8 test 7, on the wire: a source fenced by one node refuses
    /// another node's fence of the same epoch, naming the first.
    #[tokio::test]
    async fn a_fence_by_another_node_is_refused_naming_the_first() {
        let source = Arc::new(MockSource::new());
        let history = source.manifest.history;
        let fence = MockFence::new(5);
        let first = Uuid::from_u128(0xF1);
        *fence.applied.lock().unwrap() = Some((2, first));
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(peer, source, Some(fence), 1, rx.clone()));
        let hello = receiver().hello(Some(history), Some(10));
        let mut client = PhysicalClient::authenticate(io, &token(), hello, limits(), rx)
            .await
            .unwrap();
        let outcome = client.fence(2).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::Version));
        let reason = outcome
            .err()
            .and_then(|error| error.refusal().map(|refusal| refusal.reason.clone()))
            .expect("the session is refused");
        assert!(
            reason.contains("already fenced by epoch 2") && reason.contains(&first.to_string()),
            "{reason}"
        );
        assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version));
    }

    /// Spec P5.1 §3.1: only a receiver fences. A `Fence` from a source is the
    /// protocol error any unexpected frame is — no longer read as an epoch.
    #[tokio::test]
    async fn a_fence_from_a_source_is_a_protocol_error() {
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
        assert!(matches!(outcome, Err(WireError::Protocol(_))), "{:?}", outcome.err());
        run.await.unwrap().unwrap();
    }

    /// A raw source at `epoch` for the receiver-side rows of the policy.
    async fn raw_accept_at(
        peer: &mut DuplexStream,
        history: Uuid,
        epoch: u64,
        rx: &mut watch::Receiver<bool>,
    ) -> Result<Hello> {
        read_frame(peer, &[Kind::Auth], &limits(), rx).await?;
        write_frame(peer, Kind::AuthOk, &[], &limits(), rx).await?;
        let hello = Hello::decode(&read_frame(peer, &[Kind::Hello], &limits(), rx).await?.payload)?;
        let ok = HelloOk {
            node_id: Uuid::new_v4(),
            history,
            epoch,
            barrier: 0,
            capabilities: CAPABILITIES,
            physical_endpoint: String::new(),
            pg_port: 0,
        };
        write_frame(peer, Kind::HelloOk, &ok.encode()?, &limits(), rx).await?;
        Ok(hello)
    }

    /// Spec P5.1 §8: the four rows of the epoch policy (§2) on an in-process
    /// source and receiver, and the exemption of a fresh snapshot receiver.
    #[tokio::test]
    async fn the_epoch_policy_table_holds_on_both_sides() {
        let source = Arc::new(MockSource::new());
        let history = source.manifest.history;

        // Source row 1: the receiver is behind → EpochStale, nothing fenced.
        let fence = MockFence::new(9);
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            2,
            rx.clone(),
        ));
        let outcome =
            PhysicalClient::authenticate(io, &token(), receiver().hello(Some(history), Some(10)), limits(), rx).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::EpochStale { have: 2, want: 1 }));
        assert!(RefuseCode::EpochStale { have: 2, want: 1 }.needs_recovery());
        assert_eq!(
            refused(&run.await.unwrap()),
            Some(RefuseCode::EpochStale { have: 2, want: 1 })
        );
        assert!(
            fence.calls.lock().unwrap().is_empty(),
            "a stale receiver never fences its source"
        );

        // Source row 2: the receiver is ahead → the source fences itself by
        // the receiver's epoch on behalf of its node, then SourceStale.
        let fence = MockFence::new(9);
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(4096);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            1,
            rx.clone(),
        ));
        let hello = Hello {
            epoch: 2,
            ..receiver().hello(Some(history), Some(10))
        };
        let outcome = PhysicalClient::authenticate(io, &token(), hello, limits(), rx).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::SourceStale { have: 1, want: 2 }));
        let reason = outcome
            .err()
            .and_then(|error| error.refusal().map(|refusal| refusal.reason.clone()))
            .expect("the session is refused");
        assert!(
            reason.contains("superseded primary at epoch 1") && reason.contains("fenced now"),
            "{reason}"
        );
        assert_eq!(
            refused(&run.await.unwrap()),
            Some(RefuseCode::SourceStale { have: 1, want: 2 })
        );
        assert_eq!(fence.calls.lock().unwrap().as_slice(), &[(2, receiver().node_id)]);

        // Receiver row 1: the source is ahead → EpochStale.
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(4096);
        let mut peer_rx = rx.clone();
        let run = tokio::spawn(async move { raw_accept_at(&mut peer, history, 3, &mut peer_rx).await });
        let outcome = PhysicalClient::authenticate(
            io,
            &token(),
            receiver().hello(Some(history), Some(4)),
            limits(),
            rx.clone(),
        )
        .await;
        assert_eq!(refused(&outcome), Some(RefuseCode::EpochStale { have: 3, want: 1 }));
        run.await.unwrap().unwrap();

        // Receiver row 2: the source is behind → SourceStale (retryable).
        let (io, mut peer) = duplex(4096);
        let mut peer_rx = rx.clone();
        let run = tokio::spawn(async move { raw_accept_at(&mut peer, history, 1, &mut peer_rx).await });
        let hello = Hello {
            epoch: 4,
            ..receiver().hello(Some(history), Some(4))
        };
        let outcome = PhysicalClient::authenticate(io, &token(), hello, limits(), rx.clone()).await;
        assert_eq!(refused(&outcome), Some(RefuseCode::SourceStale { have: 1, want: 4 }));
        assert!(RefuseCode::SourceStale { have: 1, want: 4 }.retryable());
        run.await.unwrap().unwrap();

        // A fresh snapshot receiver holds no epoch: admitted by a source of
        // any epoch, and it accepts the source's.
        let (io, peer) = duplex(4096);
        let fence = MockFence::new(9);
        let run = tokio::spawn(fenceable_server(
            peer,
            source.clone(),
            Some(fence.clone()),
            5,
            rx.clone(),
        ));
        let client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx.clone())
            .await
            .unwrap();
        assert_eq!(client.hello_ok().epoch, 5);
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
        assert!(fence.calls.lock().unwrap().is_empty());
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
    /// Spec P4.1c: so is an `NPHYS002` receiver, whose batch bounds differ.
    /// Spec P5.1 §11.F: and an `NPHYS003` one, whose resume carries no last
    /// write group.
    #[tokio::test]
    async fn previous_version_is_refused_by_name_and_other_magic_before_export() {
        assert_eq!((PROTOCOL, &MAGIC, VERSION), ("NPHYS004", b"NPHYS004", 4));
        for (old_magic, old_version) in [(*b"NPHYS001", 1_u16), (*b"NPHYS002", 2), (*b"NPHYS003", 3)] {
            let old_name = String::from_utf8_lossy(&old_magic).into_owned();
            let (_shutdown, mut rx) = watch::channel(false);
            let (mut io, peer) = duplex(256);
            let calls = Arc::new(AtomicUsize::new(0));
            let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx.clone()));
            let mut old = old_magic.to_vec();
            old.extend(old_version.to_be_bytes());
            old.extend(1_u16.to_be_bytes()); // the old version's Auth
            old.extend(32_u32.to_be_bytes());
            old.extend([7; 32]);
            io.write_all(&old).await.unwrap();
            let reply = read_frame(&mut io, &[Kind::Refuse], &limits(), &mut rx).await.unwrap();
            let refusal = Refusal::decode(&reply.payload).unwrap();
            assert_eq!(refusal.code, RefuseCode::Version, "{old_name}");
            assert!(
                refusal.reason.contains(&old_name) && refusal.reason.contains("NPHYS004"),
                "{refusal}"
            );
            assert_eq!(refused(&run.await.unwrap()), Some(RefuseCode::Version), "{old_name}");
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{old_name}");
        }

        // The same version number under another magic, the previous
        // version, and a bumped version under this magic.
        for (magic, version) in [
            (*b"NPHYS001", VERSION),
            (*b"NPHYS002", 2),
            (*b"NPHYS003", 3),
            (MAGIC, 9),
        ] {
            let mut bytes = magic.to_vec();
            bytes.extend(version.to_be_bytes());
            bytes.extend(1_u16.to_be_bytes());
            bytes.extend(0_u32.to_be_bytes());
            let mut header = [0; HEADER_BYTES];
            header.copy_from_slice(&bytes);
            let error = check_header(&header).unwrap_err();
            assert!(matches!(error, WireError::Version { .. }), "{error}");
            assert!(error.to_string().contains("NPHYS004"), "{error}");
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
        // Review P4.1d: one batch is a legal poll under any limit, even one
        // below the 12 bytes of a batch header (`resync --poll-byte-limit 1`);
        // a second batch still needs room for its header.
        for limit in [1, 11] {
            assert!(PollSummary {
                through: 11,
                durable_barrier: 11,
                batches: 1
            }
            .validate(10, limit)
            .is_ok());
            assert!(PollSummary {
                through: 12,
                durable_barrier: 12,
                batches: 2
            }
            .validate(10, limit)
            .is_err());
        }
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
                    client.poll(10, MAX_POLL_BYTES, &mut sink).await,
                    Err(WireError::Protocol(_))
                ),
                "{fault}"
            );
            assert!(sink.received.is_empty(), "{fault}");
            assert!(matches!(
                client.poll(10, MAX_POLL_BYTES, &mut sink).await,
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
            client.poll(10, MAX_POLL_BYTES, &mut FailedSink).await,
            Err(WireError::Source(_))
        ));
        assert!(matches!(
            client.poll(10, MAX_POLL_BYTES, &mut Sink::default()).await,
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
                    assert_eq!(
                        request,
                        OpenRequest::Resume {
                            history,
                            after: 10,
                            last: None
                        }
                    );
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
        assert_eq!(client.poll(10, MAX_POLL_BYTES, &mut sink).await.unwrap().through, 11);
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
        let durable = client.poll(10, MAX_POLL_BYTES, &mut sink).await.unwrap().through;
        drop(client); // transport interruption after the sink owns sequence 11
        assert!(run.await.unwrap().is_err());
        let resumed = Arc::new(resume_source(&source, durable));
        let (io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, resumed, Arc::new(AtomicUsize::new(0)), rx.clone()));
        let mut client = PhysicalClient::connect_resume(io, &token(), &receiver(), limits(), rx, history, durable)
            .await
            .unwrap();
        let summary = client.poll(durable, MAX_POLL_BYTES, &mut sink).await.unwrap();
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
                    OpenRequest::Resume {
                        history,
                        after: 10,
                        last: None
                    }
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
            (fresh(), vec![0; OPEN_RESUME_BYTES]),
            // A resume of a nil history.
            (fresh(), {
                let mut bytes = vec![1];
                bytes.extend([0; 16]);
                bytes.extend(10_u64.to_be_bytes());
                bytes.extend([0; 41]);
                bytes
            }),
            // Spec P5.1 §11.F: a last write group flag that is neither 0 nor
            // 1, a group named with flag 0, and one past the cursor.
            (receiver().hello(Some(history), Some(10)), {
                let mut bytes = OpenRequest::Resume {
                    history,
                    after: 10,
                    last: None,
                }
                .encode()
                .unwrap();
                bytes[25] = 2;
                bytes
            }),
            (receiver().hello(Some(history), Some(10)), {
                let mut bytes = OpenRequest::Resume {
                    history,
                    after: 10,
                    last: None,
                }
                .encode()
                .unwrap();
                bytes[26..34].copy_from_slice(&9_u64.to_be_bytes());
                bytes
            }),
            (receiver().hello(Some(history), Some(10)), {
                let mut bytes = OpenRequest::Resume {
                    history,
                    after: 10,
                    last: Some(LastGroup {
                        first_sequence: 10,
                        digest: [7; 32],
                    }),
                }
                .encode()
                .unwrap();
                bytes[26..34].copy_from_slice(&11_u64.to_be_bytes());
                bytes
            }),
            // A well-formed open that disagrees with the hello: a resume of a
            // fresh receiver, and a snapshot of a resuming one.
            (
                fresh(),
                OpenRequest::Resume {
                    history,
                    after: 10,
                    last: None,
                }
                .encode()
                .unwrap(),
            ),
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
        assert!(client.poll(10, MAX_POLL_BYTES, &mut sink).await.is_err());
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
            client.poll(10, MAX_POLL_BYTES, &mut sink).await,
            Err(WireError::Protocol(_))
        ));
        assert!(sink.received.is_empty());
        assert!(matches!(
            client.poll(10, MAX_POLL_BYTES, &mut sink).await,
            Err(WireError::Poisoned)
        ));
        run.await.unwrap().unwrap();
    }

    // --- spec P2.2 ---------------------------------------------------------

    /// `CAP_TLS` is set by each side for its own end of the session; the
    /// session counts as TLS only when both set it.
    #[tokio::test]
    async fn cap_tls_is_set_per_side_and_a_session_is_tls_only_when_both_set_it() {
        for (source_tls, receiver_tls) in [(true, true), (true, false), (false, true), (false, false)] {
            let source = Arc::new(MockSource::new());
            let context = SourceContext {
                identity: identity(source.manifest.history),
                observer: Arc::new(NoSessionObserver),
                tls: source_tls,
                fence: None,
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
            let hello = Hello {
                capabilities: if receiver_tls {
                    CAPABILITIES | CAP_TLS
                } else {
                    CAPABILITIES
                },
                ..fresh()
            };
            let client = PhysicalClient::authenticate(client_io, &token(), hello, limits(), rx)
                .await
                .unwrap();
            assert_eq!(
                client.hello_ok().capabilities & CAP_TLS != 0,
                source_tls,
                "HelloOk carries CAP_TLS exactly when the source's end is TLS"
            );
            assert_eq!(client.tls(), source_tls && receiver_tls, "{source_tls} {receiver_tls}");
            assert!(client.heartbeats(), "CAP_TLS leaves the other capabilities alone");
            client.close().await.unwrap();
            run.await.unwrap().unwrap();
        }
        assert_eq!(CAP_TLS, 1 << 2);
        assert_eq!(
            CAPABILITIES & CAP_TLS,
            0,
            "CAP_TLS is per session, never offered blindly"
        );
    }

    /// Spec P2.2: `Open` may name the snapshot an interrupted download
    /// belongs to; it is a snapshot request in every other respect.
    #[tokio::test]
    async fn an_open_may_prefer_a_snapshot_and_is_served_as_a_snapshot() {
        let preferred = Uuid::new_v4();
        let request = OpenRequest::SnapshotOf { snapshot: preferred };
        let bytes = request.encode().unwrap();
        assert!(Kind::Open.valid_size(bytes.len()));
        assert_eq!(OpenRequest::decode(&bytes).unwrap(), request);
        assert!(request.is_snapshot() && OpenRequest::Snapshot.is_snapshot());
        assert!(!OpenRequest::Resume {
            history: preferred,
            after: 1,
            last: None
        }
        .is_snapshot());
        // Spec P5.1 §11.F (`NPHYS004`): a resume carries its last write
        // group, or none at a checkpoint cut; both round-trip at 66 bytes.
        for last in [
            None,
            Some(LastGroup {
                first_sequence: 8,
                digest: [9; 32],
            }),
        ] {
            let resume = OpenRequest::Resume {
                history: preferred,
                after: 10,
                last,
            };
            let bytes = resume.encode().unwrap();
            assert_eq!(bytes.len(), OPEN_RESUME_BYTES);
            assert!(Kind::Open.valid_size(bytes.len()));
            assert_eq!(OpenRequest::decode(&bytes).unwrap(), resume);
        }
        assert!(
            !Kind::Open.valid_size(25),
            "NPHYS003's resume is no frame of this protocol"
        );
        for first_sequence in [0, 11] {
            assert!(OpenRequest::Resume {
                history: preferred,
                after: 10,
                last: Some(LastGroup {
                    first_sequence,
                    digest: [0; 32],
                }),
            }
            .encode()
            .is_err());
        }
        assert!(OpenRequest::SnapshotOf { snapshot: Uuid::nil() }.encode().is_err());
        let mut nil = vec![2];
        nil.extend([0; 16]);
        assert!(OpenRequest::decode(&nil).is_err());
        assert!(request.agrees_with(&fresh()));
        assert!(!request.agrees_with(&receiver().hello(Some(preferred), Some(3))));

        // Spec P4.1: a fast-forward open, with and without a preferred
        // snapshot (29 and 45 bytes), is no snapshot, and agrees only with a
        // hello that announced its history and cursor.
        let history = Uuid::new_v4();
        for (prefer, size) in [(None, 29), (Some(preferred), 45)] {
            let open = OpenRequest::FastForward {
                history,
                after: 3,
                leaf_bytes: 4 * 1024 * 1024,
                prefer,
            };
            let bytes = open.encode().unwrap();
            assert_eq!(bytes.len(), size);
            assert!(Kind::Open.valid_size(bytes.len()));
            assert_eq!(OpenRequest::decode(&bytes).unwrap(), open);
            assert!(!open.is_snapshot());
            assert!(open.agrees_with(&receiver().hello(Some(history), Some(3))));
            assert!(!open.agrees_with(&receiver().hello(Some(history), Some(4))));
            assert!(!open.agrees_with(&fresh()));
        }
        for leaf_bytes in [MIN_LEAF_BYTES - 1, MAX_LEAF_BYTES + 1] {
            let open = OpenRequest::FastForward {
                history,
                after: 3,
                leaf_bytes,
                prefer: None,
            };
            assert!(open.encode().is_err(), "{leaf_bytes}");
        }
        assert!(OpenRequest::FastForward {
            history: Uuid::nil(),
            after: 3,
            leaf_bytes: MIN_LEAF_BYTES,
            prefer: None
        }
        .encode()
        .is_err());
        assert!(OpenRequest::FastForward {
            history,
            after: 3,
            leaf_bytes: MIN_LEAF_BYTES,
            prefer: Some(Uuid::nil())
        }
        .encode()
        .is_err());
        let mut mislabelled = OpenRequest::FastForward {
            history,
            after: 3,
            leaf_bytes: MIN_LEAF_BYTES,
            prefer: Some(preferred),
        }
        .encode()
        .unwrap();
        mislabelled[0] = 3;
        assert!(OpenRequest::decode(&mislabelled).is_err(), "tag 3 is the 29-byte form");

        let source = Arc::new(MockSource::new());
        let history = source.manifest.history;
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(256);
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
                move |open| async move {
                    assert_eq!(open, OpenRequest::SnapshotOf { snapshot: preferred });
                    Ok(provider as Arc<dyn PhysicalSource>)
                },
            )
            .await
        });
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        let manifest = client
            .fetch_manifest_preferring(Some(history), Some(preferred))
            .await
            .unwrap();
        // The source answered with ITS export (the preferred one had expired):
        // the receiver compares and starts over.
        assert_eq!(manifest.snapshot_id, source.manifest.snapshot_id);
        assert_eq!(client.read_chunk(0, 0, 1).await.unwrap(), source.data[0][..1].to_vec());
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
    }

    /// Spec P2.2: the replication listener binds a non-loopback address only
    /// with TLS or the plaintext opt-in; the refusal binds nothing.
    #[tokio::test]
    async fn a_plaintext_replication_listener_stays_on_loopback() {
        for refused in ["0.0.0.0:0", "192.0.2.1:0"] {
            let address: SocketAddr = refused.parse().unwrap();
            assert!(
                matches!(
                    bind_replication(address, false, false).await,
                    Err(WireError::Protocol(_))
                ),
                "{refused}"
            );
        }
        let listener = bind_replication("127.0.0.1:0".parse().unwrap(), false, false)
            .await
            .unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        let wildcard = bind_replication("0.0.0.0:0".parse().unwrap(), false, true)
            .await
            .unwrap();
        assert!(wildcard.local_addr().unwrap().ip().is_unspecified());
    }

    /// A rustls error inside an I/O error is a TLS failure; anything else is
    /// left as it is.
    #[test]
    fn tls_alerts_after_the_handshake_are_reported_as_tls() {
        let alert = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(rustls::AlertDescription::CertificateRequired),
        );
        let mapped = tls_failure(WireError::Io(alert));
        assert!(matches!(&mapped, WireError::Tls(_)), "{mapped}");
        assert!(mapped.to_string().starts_with("tls: "), "{mapped}");
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(matches!(tls_failure(WireError::Io(reset)), WireError::Io(_)));
    }

    /// A refused plaintext connection to a non-loopback address is refused
    /// before any socket is opened.
    #[tokio::test]
    async fn a_plaintext_connection_to_a_remote_primary_is_refused_before_connecting() {
        let (_shutdown, rx) = watch::channel(false);
        let outcome = PhysicalClient::connect_replication(
            "192.0.2.1:5433".parse().unwrap(),
            &ReplicationTransport::default(),
            &token(),
            fresh(),
            limits(),
            rx,
        )
        .await;
        assert!(
            matches!(&outcome, Err(WireError::Protocol(reason)) if reason.contains("--primary-tls")),
            "{:?}",
            outcome.err()
        );
    }

    // --- spec P4.1: the fast-forward session --------------------------------

    /// A fast-forward provider: a plan of `leaves`, answered "planning" for
    /// the first `planning` asks and in pages of at most `page` leaves; leaf
    /// 1 is delete-only, every other leaf's file is `file`.
    struct LeafSource {
        info: SourceSessionInfo,
        leaves: Vec<LeafEntry>,
        total: u64,
        planning: AtomicUsize,
        page: u32,
        file: Vec<u8>,
        chunks: AtomicUsize,
    }
    impl LeafSource {
        fn new(history: Uuid, after: u64, sequence: u64) -> Self {
            let leaves: Vec<LeafEntry> = [&b""[..], b"g", b"p", b"w"]
                .iter()
                .enumerate()
                .map(|(index, start)| LeafEntry {
                    start: start.to_vec(),
                    bytes: if index == 1 { 0 } else { 100 },
                    keys: if index == 1 { 0 } else { 2 },
                    digest: [index as u8; 32],
                })
                .collect();
            Self {
                info: SourceSessionInfo::FastForward {
                    session_id: Uuid::new_v4(),
                    snapshot_id: Uuid::new_v4(),
                    history,
                    after,
                    sequence,
                },
                total: leaves.iter().map(|leaf| leaf.bytes).sum(),
                leaves,
                planning: AtomicUsize::new(1),
                page: 3,
                file: (0..=255_u8).cycle().take(5000).collect(),
                chunks: AtomicUsize::new(0),
            }
        }
        fn snapshot(&self) -> Uuid {
            self.info.fast_forward_snapshot().unwrap()
        }
    }
    #[async_trait]
    impl PhysicalSource for LeafSource {
        fn session_info(&self) -> &SourceSessionInfo {
            &self.info
        }
        async fn read_chunk(&self, _: u32, _: u64, _: u32) -> Result<Vec<u8>> {
            panic!("a fast-forward session reads no checkpoint chunks")
        }
        async fn poll(&self, _: u64, _: usize) -> Result<PhysicalPoll> {
            panic!("a fast-forward session never polls")
        }
        async fn leaf_digests(&self, from: u32, count: u32) -> Result<LeafDigests> {
            if self
                .planning
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
                .is_ok()
            {
                return Ok(LeafDigests::planning(self.snapshot()));
            }
            let end = (from + count.min(self.page)).min(self.leaves.len() as u32);
            Ok(LeafDigests {
                snapshot_id: self.snapshot(),
                status: LeafStatus::Ready,
                leaf_count: self.leaves.len() as u32,
                total_bytes: self.total,
                from,
                entries: self.leaves[from as usize..end as usize].to_vec(),
            })
        }
        async fn leaf(&self, index: u32) -> Result<LeafReady> {
            let (size, sha256) = if index == 1 {
                (0, empty_sha256())
            } else {
                (self.file.len() as u64, Sha256::digest(&self.file).into())
            };
            Ok(LeafReady {
                snapshot_id: self.snapshot(),
                index,
                size,
                sha256,
            })
        }
        async fn read_leaf_chunk(&self, _index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
            self.chunks.fetch_add(1, Ordering::SeqCst);
            Ok(self.file[offset as usize..offset as usize + length as usize].to_vec())
        }
    }

    fn serve_leaves(
        io: DuplexStream,
        source: Arc<LeafSource>,
        history: Uuid,
        rx: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let context = SourceContext::unobserved(identity(history));
            serve_session(io, &token(), limits(), rx, &context, None, move |open| async move {
                assert!(matches!(open, OpenRequest::FastForward { .. }), "{open:?}");
                Ok(source as Arc<dyn PhysicalSource>)
            })
            .await
        })
    }

    /// Spec P4.1 §3.1: one fast-forward session opens at the source's cut,
    /// waits out the planning, pages through the plan, and fetches a leaf's
    /// file (and a delete-only leaf's announcement) — the plan and the file
    /// arrive exactly as the source holds them.
    #[tokio::test]
    async fn a_fast_forward_session_pages_the_plan_and_fetches_leaf_files() {
        let history = Uuid::new_v4();
        let source = Arc::new(LeafSource::new(history, 10, 20));
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(64 * 1024);
        let run = serve_leaves(peer, source.clone(), history, rx.clone());
        let mut client =
            PhysicalClient::authenticate(io, &token(), receiver().hello(Some(history), Some(10)), limits(), rx)
                .await
                .unwrap();
        assert!(client.fast_forward(), "this release offers fast-forward sessions");
        let info = client
            .open_fast_forward(history, 10, MIN_LEAF_BYTES, None)
            .await
            .unwrap();
        assert!(matches!(
            info,
            SourceSessionInfo::FastForward {
                sequence: 20,
                after: 10,
                ..
            }
        ));
        let plan = client.fetch_leaf_digests().await.unwrap();
        assert_eq!(plan.snapshot_id, source.snapshot());
        assert_eq!(
            plan.leaves, source.leaves,
            "paged in pages of 3, after one planning answer"
        );
        assert_eq!(plan.total_bytes, 300);
        let empty = client.request_leaf(1).await.unwrap();
        assert_eq!(empty.size, 0, "a delete-only leaf has no file");
        let ready = client.request_leaf(2).await.unwrap();
        assert_eq!(ready.size, source.file.len() as u64);
        let mut file = Vec::new();
        while (file.len() as u64) < ready.size {
            let length = (ready.size - file.len() as u64).min(1024) as u32;
            file.extend(client.read_leaf_chunk(2, file.len() as u64, length).await.unwrap());
        }
        assert_eq!(file, source.file);
        assert_eq!(<[u8; 32]>::from(Sha256::digest(&file)), ready.sha256);
        // Heartbeats keep a session alive while the receiver works locally.
        client
            .heartbeat(Heartbeat {
                applied_sequence: 10,
                durable_sequence: 10,
                received_at: now_millis(),
            })
            .await
            .unwrap();
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
    }

    /// Spec P4.1 §3.1: a chunk of a leaf the session never announced, or
    /// past its file, ends the session before the provider reads anything.
    #[tokio::test]
    async fn a_leaf_chunk_outside_an_announced_leaf_file_ends_the_session() {
        for (announce, offset, length) in [(false, 0_u64, 1_u32), (true, 4999, 2), (true, 0, 0)] {
            let history = Uuid::new_v4();
            let source = Arc::new(LeafSource::new(history, 10, 20));
            let (_shutdown, mut rx) = watch::channel(false);
            let (mut io, peer) = duplex(64 * 1024);
            let run = serve_leaves(peer, source.clone(), history, rx.clone());
            raw_open(&mut io, &receiver().hello(Some(history), Some(10)), &mut rx).await;
            let open = OpenRequest::FastForward {
                history,
                after: 10,
                leaf_bytes: MIN_LEAF_BYTES,
                prefer: None,
            };
            write_frame(&mut io, Kind::Open, &open.encode().unwrap(), &limits(), &mut rx)
                .await
                .unwrap();
            read_frame(&mut io, &[Kind::FastForwardInfo], &limits(), &mut rx)
                .await
                .unwrap();
            if announce {
                let mut request = source.snapshot().as_bytes().to_vec();
                request.extend(2_u32.to_be_bytes());
                write_frame(&mut io, Kind::LeafRequest, &request, &limits(), &mut rx)
                    .await
                    .unwrap();
                read_frame(&mut io, &[Kind::LeafReady], &limits(), &mut rx)
                    .await
                    .unwrap();
            }
            let mut request = source.snapshot().as_bytes().to_vec();
            request.extend(2_u32.to_be_bytes());
            request.extend(offset.to_be_bytes());
            request.extend(length.to_be_bytes());
            write_frame(&mut io, Kind::LeafChunkRequest, &request, &limits(), &mut rx)
                .await
                .unwrap();
            assert!(
                matches!(run.await.unwrap(), Err(WireError::Protocol(_))),
                "{announce} {offset} {length}"
            );
            assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
        }
    }

    /// Spec P4.1 §3.1: each session kind accepts its own requests only: a
    /// fast-forward session ends on a chunk or poll request, a snapshot
    /// session on any leaf request — as protocol errors, before any provider
    /// call.
    #[tokio::test]
    async fn each_session_kind_accepts_only_its_own_requests() {
        for (kind, size) in [
            (Kind::ChunkRequest, 32),
            (Kind::PollRequest, 28),
            (Kind::Fence, FENCE_BYTES),
        ] {
            let history = Uuid::new_v4();
            let source = Arc::new(LeafSource::new(history, 10, 20));
            let (_shutdown, mut rx) = watch::channel(false);
            let (mut io, peer) = duplex(64 * 1024);
            let run = serve_leaves(peer, source.clone(), history, rx.clone());
            raw_open(&mut io, &receiver().hello(Some(history), Some(10)), &mut rx).await;
            let open = OpenRequest::FastForward {
                history,
                after: 10,
                leaf_bytes: MIN_LEAF_BYTES,
                prefer: None,
            };
            write_frame(&mut io, Kind::Open, &open.encode().unwrap(), &limits(), &mut rx)
                .await
                .unwrap();
            read_frame(&mut io, &[Kind::FastForwardInfo], &limits(), &mut rx)
                .await
                .unwrap();
            // The header is refused before its payload is read.
            io.write_all(&header(kind, size as u32)).await.unwrap();
            assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))), "{kind:?}");
        }
        for (kind, size) in [
            (Kind::LeafDigestsRequest, LEAF_DIGESTS_REQUEST_BYTES),
            (Kind::LeafRequest, LEAF_REQUEST_BYTES),
            (Kind::LeafChunkRequest, 32),
        ] {
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
            io.write_all(&header(kind, size as u32)).await.unwrap();
            assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))), "{kind:?}");
            assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
        }
    }

    /// Spec P4.1 §3.1: a receiver does not open a fast-forward with a source
    /// whose `HelloOk` lacks [`CAP_FAST_FORWARD`]: refused before anything is
    /// sent, naming `--full`.
    #[tokio::test]
    async fn a_source_without_the_capability_is_never_asked_for_a_fast_forward() {
        let history = Uuid::new_v4();
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(256);
        let mut peer_rx = rx.clone();
        let run = tokio::spawn(async move {
            read_frame(&mut peer, &[Kind::Auth], &limits(), &mut peer_rx).await?;
            write_frame(&mut peer, Kind::AuthOk, &[], &limits(), &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::Hello], &limits(), &mut peer_rx).await?;
            let ok = HelloOk {
                node_id: Uuid::new_v4(),
                history,
                epoch: 1,
                barrier: 0,
                capabilities: CAP_HEARTBEAT,
                physical_endpoint: String::new(),
                pg_port: 0,
            };
            write_frame(&mut peer, Kind::HelloOk, &ok.encode()?, &limits(), &mut peer_rx).await?;
            // Nothing more arrives: the receiver refused on its own.
            Ok::<_, WireError>(
                read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_rx)
                    .await
                    .is_err(),
            )
        });
        let mut client =
            PhysicalClient::authenticate(io, &token(), receiver().hello(Some(history), Some(10)), limits(), rx)
                .await
                .unwrap();
        assert!(!client.fast_forward());
        let outcome = client.open_fast_forward(history, 10, MIN_LEAF_BYTES, None).await;
        assert!(
            matches!(&outcome, Err(WireError::Refused(refusal)) if refusal.reason.contains("use --full")),
            "{:?}",
            outcome.err()
        );
        drop(client);
        assert!(run.await.unwrap().unwrap(), "no open was sent");
    }

    // --- spec P4.1b: TCP_NODELAY and one-write frames -----------------------

    /// A writer that takes every buffer whole and counts the writes and
    /// flushes it is given: one `write_all` is exactly one `poll_write`.
    #[derive(Default)]
    struct WriteRecorder {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
    }
    impl AsyncWrite for WriteRecorder {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            this.writes += 1;
            this.bytes.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.get_mut().flushes += 1;
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Spec P4.1b §2.2: a frame whose payload is at most `SMALL_FRAME_BYTES`
    /// reaches the transport in ONE write; a larger one as its header, then
    /// its payload — spec P4.1c: in steps of at most `FRAME_IO_STEP`. The
    /// bytes are the same either way (the header, then the payload), one
    /// flush ends every frame, and they decode to the same frame.
    #[tokio::test]
    async fn a_small_frame_is_one_write_and_a_large_one_its_header_then_steps() {
        let (_shutdown, mut rx) = watch::channel(false);
        for (kind, length, writes) in [
            (Kind::Close, 0, 1),
            (Kind::ChunkRequest, 32, 1),
            (Kind::Chunk, SMALL_FRAME_BYTES, 1),
            (Kind::Chunk, SMALL_FRAME_BYTES + 1, 2),
            (Kind::Chunk, FRAME_IO_STEP, 2),
            (Kind::Chunk, CHUNK_META + MAX_CHUNK_BYTES, 3),
            (Kind::Batch, 3 * FRAME_IO_STEP + 5, 5),
        ] {
            let payload: Vec<u8> = (0..=u8::MAX).cycle().take(length).collect();
            let mut recorder = WriteRecorder::default();
            write_frame(&mut recorder, kind, &payload, &limits(), &mut rx)
                .await
                .unwrap();
            assert_eq!(recorder.writes, writes, "{kind:?} with {length} payload bytes");
            assert_eq!(recorder.flushes, 1, "{kind:?}: every frame is flushed");
            let mut wire = header(kind, u32::try_from(length).unwrap());
            wire.extend(&payload);
            assert_eq!(recorder.bytes, wire, "{kind:?}: the bytes on the wire are unchanged");
            let mut reader = recorder.bytes.as_slice();
            let frame = read_frame(&mut reader, &[kind], &limits(), &mut rx).await.unwrap();
            assert_eq!(frame.kind, kind);
            assert_eq!(frame.payload, payload, "{kind:?}");
            assert!(reader.is_empty(), "{kind:?}: one frame, nothing after it");
        }
    }

    /// Spec P4.1b §2.1: both ends of a real loopback session set
    /// `TCP_NODELAY` before anything is sent — the source in
    /// [`accept_replication`], the receiver in
    /// [`PhysicalClient::connect_replication`]. Asserted on the `TcpStream`
    /// each side's plaintext `ReplicationStream` wraps, after the receiver
    /// authenticated; the TLS branch of both functions hands the same socket,
    /// after the same call, to its handshake (this module has no TLS
    /// fixture).
    #[tokio::test]
    async fn both_ends_of_a_replication_session_set_nodelay() {
        let listener = bind_replication("127.0.0.1:0".parse().unwrap(), false, false)
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let source = Arc::new(MockSource::new());
        let history = source.manifest.history;
        let (_shutdown, rx) = watch::channel(false);
        let mut server_rx = rx.clone();
        let run = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let stream = accept_replication(socket, None, &limits(), &mut server_rx).await?;
            let ReplicationStream::Plain(accepted) = &stream else {
                panic!("a listener without an acceptor serves plaintext");
            };
            let nodelay = accepted.nodelay()?;
            let context = SourceContext::unobserved(identity(history));
            serve_session(
                stream,
                &token(),
                limits(),
                server_rx,
                &context,
                None,
                move |_| async move { Ok(source as Arc<dyn PhysicalSource>) },
            )
            .await?;
            Ok::<_, WireError>(nodelay)
        });
        let client = PhysicalClient::connect_replication(
            address,
            &ReplicationTransport::default(),
            &token(),
            fresh(),
            limits(),
            rx,
        )
        .await
        .unwrap();
        let ReplicationStream::Plain(connected) = &client.io else {
            panic!("a loopback transport without TLS is plaintext");
        };
        assert!(connected.nodelay().unwrap(), "the receiver's end sets TCP_NODELAY");
        assert!(!client.tls());
        client.close().await.unwrap();
        assert!(run.await.unwrap().unwrap(), "the source's end sets TCP_NODELAY");
    }

    // --- spec P4.1c: write groups of any size -------------------------------

    /// A frame timeout of 1 s, the spec's idle-timeout fixture.
    fn idle_limits() -> WireLimits {
        WireLimits {
            frame_timeout: Duration::from_secs(1),
            ..limits()
        }
    }

    /// A peer sending one `Batch` frame of `steps` × `FRAME_IO_STEP` payload
    /// bytes, one step every `every`, and once, before step `at`, pausing
    /// `stall` longer.
    fn slow_batch_peer(
        mut peer: DuplexStream,
        steps: usize,
        every: Duration,
        stall: Option<(usize, Duration)>,
    ) -> tokio::task::JoinHandle<std::io::Result<()>> {
        tokio::spawn(async move {
            peer.write_all(&header(Kind::Batch, u32::try_from(steps * FRAME_IO_STEP).unwrap()))
                .await?;
            let step = vec![0x5A_u8; FRAME_IO_STEP];
            for index in 0..steps {
                if let Some((at, pause)) = stall {
                    if index == at {
                        tokio::time::sleep(pause).await;
                    }
                }
                tokio::time::sleep(every).await;
                peer.write_all(&step).await?;
            }
            peer.flush().await
        })
    }

    /// Spec P4.1c §3: a payload is read in steps, each bounded by the frame
    /// timeout — an idle timeout. A 100 MiB `Batch` frame from a peer that
    /// writes 1 MiB every 200 ms takes twenty frame timeouts of 1 s in all and
    /// is read whole. (Paused time: the 20 s pass at once, timers in order.)
    #[tokio::test(start_paused = true)]
    async fn a_large_frame_from_a_peer_that_keeps_sending_never_times_out() {
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, peer) = duplex(256 * 1024);
        let started = tokio::time::Instant::now();
        let feeder = slow_batch_peer(peer, 100, Duration::from_millis(200), None);
        let frame = read_frame(&mut io, &[Kind::Batch], &idle_limits(), &mut rx)
            .await
            .unwrap();
        assert_eq!(frame.kind, Kind::Batch);
        assert_eq!(frame.payload.len(), 100 * FRAME_IO_STEP);
        assert_eq!(
            (frame.payload.first(), frame.payload.last()),
            (Some(&0x5A), Some(&0x5A))
        );
        assert!(
            started.elapsed() >= Duration::from_secs(20),
            "the frame took {:?}, twenty frame timeouts",
            started.elapsed()
        );
        feeder.await.unwrap().unwrap();
    }

    /// Spec P4.1c §3: a peer that stops mid-payload for 2 s fails the read
    /// with the timeout (1 s): the step it stalled in ran out.
    #[tokio::test(start_paused = true)]
    async fn a_large_frame_from_a_peer_that_stalls_mid_payload_times_out() {
        let (_shutdown, mut rx) = watch::channel(false);
        let (mut io, peer) = duplex(256 * 1024);
        let feeder = slow_batch_peer(peer, 100, Duration::ZERO, Some((50, Duration::from_secs(2))));
        let outcome = read_frame(&mut io, &[Kind::Batch], &idle_limits(), &mut rx).await;
        assert!(matches!(outcome, Err(WireError::Timeout)), "{:?}", outcome.err());
        drop(io);
        assert!(feeder.await.unwrap().is_err(), "the reader is gone");
    }

    /// Spec P4.1c: writing is stepped the same way. A 100 MiB `Batch` frame
    /// to a peer that reads 1 MiB every 200 ms is written whole; to a peer
    /// that stops reading for 2 s half-way, the write fails with the timeout.
    #[tokio::test(start_paused = true)]
    async fn a_large_frame_is_written_to_a_slow_reader_and_times_out_on_a_stalled_one() {
        let payload = vec![0x5A_u8; 100 * FRAME_IO_STEP];
        for stall in [false, true] {
            let (_shutdown, mut rx) = watch::channel(false);
            let (mut io, mut peer) = duplex(64 * 1024);
            let reader = tokio::spawn(async move {
                let mut header = [0_u8; HEADER_BYTES];
                peer.read_exact(&mut header).await?;
                let mut step = vec![0_u8; FRAME_IO_STEP];
                for index in 0..100 {
                    let pause = if stall && index == 50 {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_millis(200)
                    };
                    tokio::time::sleep(pause).await;
                    peer.read_exact(&mut step).await?;
                }
                Ok::<_, std::io::Error>(())
            });
            let outcome = write_frame(&mut io, Kind::Batch, &payload, &idle_limits(), &mut rx).await;
            if stall {
                assert!(matches!(outcome, Err(WireError::Timeout)), "{:?}", outcome.err());
                drop(io);
                assert!(reader.await.unwrap().is_err(), "the writer is gone");
            } else {
                outcome.unwrap();
                reader.await.unwrap().unwrap();
            }
        }
    }

    /// A write group of one put of `value_bytes`, at source sequence `first`.
    fn sized_batch(history: Uuid, first: u64, value_bytes: usize) -> PhysicalBatch {
        let mut native = rocksdb::WriteBatch::default();
        native.put(format!("k{first}"), vec![7_u8; value_bytes]);
        let mut raw = native.data().to_vec();
        raw[..8].copy_from_slice(&first.to_le_bytes());
        let digest = Sha256::digest(&raw).into();
        PhysicalBatch::decode(history, first, raw, digest).unwrap()
    }

    /// [`MockSource`]'s snapshot session (cut 10), whose poll at the cut
    /// returns `batches`.
    struct BatchesSource {
        inner: MockSource,
        batches: Vec<PhysicalBatch>,
    }
    #[async_trait]
    impl PhysicalSource for BatchesSource {
        fn session_info(&self) -> &SourceSessionInfo {
            &self.inner.info
        }
        async fn read_chunk(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
            self.inner.read_chunk(index, offset, length).await
        }
        async fn poll(&self, after: u64, _limit: usize) -> Result<PhysicalPoll> {
            let barrier = self.batches.last().map_or(after, PhysicalBatch::last_sequence);
            Ok(PhysicalPoll {
                batches: if after == 10 { self.batches.clone() } else { Vec::new() },
                through: if after == 10 { barrier } else { after },
                durable_barrier: barrier,
            })
        }
    }

    /// Spec P4.1c §3: a poll's byte limit is a coalescing target on the
    /// wire. A source serves a first batch above the receiver's limit and
    /// the receiver applies it; a second batch that takes the poll past the
    /// limit is refused by the source before anything of the poll is sent.
    #[tokio::test]
    async fn a_first_batch_above_the_poll_limit_is_served_and_a_second_crossing_it_is_refused() {
        const LIMIT: usize = 1024;
        for crossing in [false, true] {
            let inner = MockSource::new();
            let history = inner.manifest.history;
            let batches = if crossing {
                vec![sized_batch(history, 11, 64), sized_batch(history, 12, 2 * LIMIT)]
            } else {
                vec![sized_batch(history, 11, 2 * LIMIT)]
            };
            assert!(batches.last().unwrap().data().len() > LIMIT, "precondition");
            let source = Arc::new(BatchesSource { inner, batches });
            let (_shutdown, rx) = watch::channel(false);
            let (client_io, server_io) = duplex(64 * 1024);
            let server_rx = rx.clone();
            let run = tokio::spawn(async move {
                let context = SourceContext::unobserved(identity(history));
                serve_session(
                    server_io,
                    &token(),
                    limits(),
                    server_rx,
                    &context,
                    None,
                    move |_| async move { Ok(source as Arc<dyn PhysicalSource>) },
                )
                .await
            });
            let mut client = PhysicalClient::authenticate(client_io, &token(), fresh(), limits(), rx)
                .await
                .unwrap();
            client.fetch_manifest(Some(history)).await.unwrap();
            let mut sink = Sink::default();
            let outcome = client.poll(10, LIMIT, &mut sink).await;
            if crossing {
                assert!(outcome.is_err(), "{crossing}");
                assert!(sink.received.is_empty(), "nothing of the poll was sent");
                let served = run.await.unwrap();
                assert!(
                    matches!(&served, Err(WireError::Protocol(reason)) if reason.contains("oversized")),
                    "{:?}",
                    served.err()
                );
            } else {
                let summary = outcome.unwrap();
                assert_eq!((summary.through, summary.batches), (11, 1));
                assert_eq!(sink.received, vec![(history, 11, 11)]);
                client.close().await.unwrap();
                run.await.unwrap().unwrap();
            }
        }
    }

    /// Spec P4.1c §3: the receiver checks the same rule on its own. From a
    /// source that sends a first batch above the limit, it applies it; a
    /// second batch that takes a poll past the limit is refused before the
    /// sink sees it — and (spec P4.1d) so is the valid batch before it in the
    /// same poll, the poll's batches reaching the sink together — and the
    /// session is poisoned.
    #[tokio::test]
    async fn a_receiver_accepts_a_first_batch_above_its_limit_and_refuses_a_second_crossing_it() {
        const LIMIT: usize = 1024;
        let source = MockSource::new();
        let history = source.manifest.history;
        let manifest = serde_json::to_vec(&source.manifest).unwrap();
        let batches = vec![
            sized_batch(history, 11, 2 * LIMIT),
            sized_batch(history, 12, 64),
            sized_batch(history, 13, 2 * LIMIT),
        ];
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(64 * 1024);
        let mut peer_rx = rx.clone();
        let run = tokio::spawn(async move {
            raw_accept(&mut peer, history, &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_rx).await?;
            write_frame(&mut peer, Kind::Manifest, &manifest, &limits(), &mut peer_rx).await?;
            let polls = [
                (
                    PollSummary {
                        through: 11,
                        durable_barrier: 13,
                        batches: 1,
                    },
                    &batches[..1],
                ),
                (
                    PollSummary {
                        through: 13,
                        durable_barrier: 13,
                        batches: 2,
                    },
                    &batches[1..],
                ),
            ];
            for (summary, sent) in polls {
                read_frame(&mut peer, &[Kind::PollRequest], &limits(), &mut peer_rx).await?;
                write_frame(&mut peer, Kind::PollStart, &summary.encode(), &limits(), &mut peer_rx).await?;
                for batch in sent {
                    let mut payload = batch.history().as_bytes().to_vec();
                    payload.extend(batch.first_sequence().to_be_bytes());
                    payload.extend(batch.digest());
                    payload.extend(batch.data());
                    write_frame(&mut peer, Kind::Batch, &payload, &limits(), &mut peer_rx).await?;
                }
                write_frame(&mut peer, Kind::PollEnd, &summary.encode(), &limits(), &mut peer_rx).await?;
            }
            Ok::<_, WireError>(())
        });
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        client.fetch_manifest(Some(history)).await.unwrap();
        let mut sink = Sink::default();
        let first = client.poll(10, LIMIT, &mut sink).await.unwrap();
        assert_eq!((first.through, first.batches), (11, 1), "a first batch above the limit");
        assert_eq!(sink.received, vec![(history, 11, 11)]);
        assert!(matches!(
            client.poll(11, LIMIT, &mut sink).await,
            Err(WireError::Protocol(_))
        ));
        // Spec P4.1d: a poll's batches reach the sink as one run once all of
        // it validated, so neither the crossing batch nor the valid one before
        // it in the same poll was handed over.
        assert_eq!(
            sink.received,
            vec![(history, 11, 11)],
            "nothing of the refused poll reached the sink"
        );
        assert!(matches!(
            client.poll(11, LIMIT, &mut sink).await,
            Err(WireError::Poisoned)
        ));
        drop(client);
        let _ = run.await.unwrap();
    }

    // --- review of spec P4.1d -------------------------------------------------

    /// A source waits for a receiver's next request as long as the receiver
    /// may take to apply the poll it was sent — its operation timeout — plus
    /// a frame timeout (`limits()`: 3 s + 2 s), not one frame timeout: a
    /// receiver that sends its next poll 4 s after the last one (applying a
    /// large write group) is still served. One that waits 6 s is not.
    /// (Paused time: the waits pass at once, timers in order.)
    #[tokio::test(start_paused = true)]
    async fn a_source_waits_for_the_next_request_while_the_receiver_applies() {
        for (pause, served) in [(Duration::from_secs(4), true), (Duration::from_secs(6), false)] {
            let source = Arc::new(MockSource::new());
            let history = source.manifest.history;
            let (_shutdown, rx) = watch::channel(false);
            let (io, peer) = duplex(64 * 1024);
            let run = tokio::spawn(server(peer, source, Arc::new(AtomicUsize::new(0)), rx.clone()));
            let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
                .await
                .unwrap();
            client.fetch_manifest(Some(history)).await.unwrap();
            let mut sink = Sink::default();
            assert_eq!(client.poll(10, MAX_POLL_BYTES, &mut sink).await.unwrap().through, 11);
            // The receiver applies the poll (and fsyncs it) before it asks again.
            tokio::time::sleep(pause).await;
            let next = client.poll(11, MAX_POLL_BYTES, &mut sink).await;
            if served {
                assert_eq!(next.unwrap().batches, 0, "{pause:?}");
                client.close().await.unwrap();
                run.await.unwrap().unwrap();
            } else {
                assert!(next.is_err(), "{pause:?}");
                assert!(
                    matches!(run.await.unwrap(), Err(WireError::Timeout)),
                    "the source gave up waiting after {pause:?}"
                );
            }
        }
    }

    /// [`MockSource`] whose poll takes `delay` to collect — as a source
    /// reading, digesting and fsyncing a storm of tiny write groups does
    /// before it sends `PollStart`.
    struct SlowPoll {
        inner: MockSource,
        delay: Duration,
    }
    #[async_trait]
    impl PhysicalSource for SlowPoll {
        fn session_info(&self) -> &SourceSessionInfo {
            &self.inner.info
        }
        async fn read_chunk(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
            self.inner.read_chunk(index, offset, length).await
        }
        async fn poll(&self, after: u64, limit: usize) -> Result<PhysicalPoll> {
            tokio::time::sleep(self.delay).await;
            self.inner.poll(after, limit).await
        }
    }

    /// The mirror of the test above: a receiver waits for `PollStart` as long
    /// as the source may take to collect the poll — the source's operation
    /// timeout — plus a frame timeout (`limits()`: 3 s + 2 s), not one frame
    /// timeout. A source that answers after 2.5 s (past the frame timeout,
    /// within its own 3 s bound) is not a deadline: its poll is applied. A
    /// peer that never answers is given up on after the 5 s, not the 2 s.
    /// (Paused time: the waits pass at once, timers in order.)
    #[tokio::test(start_paused = true)]
    async fn a_receiver_waits_for_poll_start_while_the_source_collects_the_poll() {
        let inner = MockSource::new();
        let history = inner.manifest.history;
        let delay = Duration::from_millis(2500);
        assert!(
            delay > limits().frame_timeout && delay < limits().operation_timeout,
            "precondition"
        );
        let source = Arc::new(SlowPoll { inner, delay });
        let (_shutdown, rx) = watch::channel(false);
        let (io, peer) = duplex(64 * 1024);
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
                move |_| async move { Ok(source as Arc<dyn PhysicalSource>) },
            )
            .await
        });
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx.clone())
            .await
            .unwrap();
        client.fetch_manifest(Some(history)).await.unwrap();
        let mut sink = Sink::default();
        let started = tokio::time::Instant::now();
        let summary = client.poll(10, MAX_POLL_BYTES, &mut sink).await.unwrap();
        assert!(started.elapsed() >= delay, "the poll took {:?}", started.elapsed());
        assert_eq!((summary.through, summary.batches), (11, 1));
        assert_eq!(sink.received, vec![(history, 11, 11)]);
        client.close().await.unwrap();
        run.await.unwrap().unwrap();

        // A peer that reads the request and never answers it.
        let silent = MockSource::new();
        let history = silent.manifest.history;
        let manifest = serde_json::to_vec(&silent.manifest).unwrap();
        let (io, mut peer) = duplex(64 * 1024);
        let mut peer_rx = rx.clone();
        let stalled = tokio::spawn(async move {
            raw_accept(&mut peer, history, &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_rx).await?;
            write_frame(&mut peer, Kind::Manifest, &manifest, &limits(), &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::PollRequest], &limits(), &mut peer_rx).await?;
            // Hold the connection open, sending nothing more.
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<_, WireError>(())
        });
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        client.fetch_manifest(Some(history)).await.unwrap();
        let started = tokio::time::Instant::now();
        let outcome = client.poll(10, MAX_POLL_BYTES, &mut Sink::default()).await;
        assert!(matches!(outcome, Err(WireError::Timeout)), "{:?}", outcome.err());
        let waited = started.elapsed();
        let deadline = limits().operation_timeout + limits().frame_timeout;
        assert!(waited >= deadline, "gave up after {waited:?}, before {deadline:?}");
        drop(client);
        stalled.abort();
    }

    /// Review P4.1d: a poll's second `Batch` frame whose header announces
    /// more than what is left of the poll's limit is refused from its header,
    /// before its payload is allocated or awaited — here a header announcing
    /// 3 GiB that no payload follows: refused at once (a protocol error),
    /// not after a frame timeout.
    #[tokio::test]
    async fn a_later_batch_header_past_the_poll_limit_is_refused_before_its_payload() {
        const LIMIT: usize = 1024;
        let source = MockSource::new();
        let history = source.manifest.history;
        let manifest = serde_json::to_vec(&source.manifest).unwrap();
        let first = sized_batch(history, 11, 64);
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(64 * 1024);
        let mut peer_rx = rx.clone();
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let run = tokio::spawn(async move {
            raw_accept(&mut peer, history, &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_rx).await?;
            write_frame(&mut peer, Kind::Manifest, &manifest, &limits(), &mut peer_rx).await?;
            read_frame(&mut peer, &[Kind::PollRequest], &limits(), &mut peer_rx).await?;
            let summary = PollSummary {
                through: 12,
                durable_barrier: 12,
                batches: 2,
            };
            write_frame(&mut peer, Kind::PollStart, &summary.encode(), &limits(), &mut peer_rx).await?;
            let mut payload = first.history().as_bytes().to_vec();
            payload.extend(first.first_sequence().to_be_bytes());
            payload.extend(first.digest());
            payload.extend(first.data());
            write_frame(&mut peer, Kind::Batch, &payload, &limits(), &mut peer_rx).await?;
            let announced = u32::try_from(BATCH_META + 3 * 1024 * 1024 * 1024).unwrap();
            peer.write_all(&header(Kind::Batch, announced)).await?;
            peer.flush().await?;
            let _ = sent_tx.send(());
            // Hold the connection open, sending nothing more.
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<_, WireError>(())
        });
        let mut client = PhysicalClient::authenticate(io, &token(), fresh(), limits(), rx)
            .await
            .unwrap();
        client.fetch_manifest(Some(history)).await.unwrap();
        let mut sink = Sink::default();
        let started = tokio::time::Instant::now();
        let outcome = client.poll(10, LIMIT, &mut sink).await;
        assert!(
            matches!(&outcome, Err(WireError::Protocol(_))),
            "refused from the header: {:?}",
            outcome.err()
        );
        assert!(
            started.elapsed() < limits().frame_timeout,
            "not after waiting for a payload"
        );
        assert!(sink.received.is_empty(), "nothing of the poll reached the sink");
        sent_rx.await.unwrap();
        drop(client);
        run.abort();
    }
}
