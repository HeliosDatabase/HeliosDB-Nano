//! EXTERNAL DRAFT: authenticated snapshot chunks and continuing physical WAL.
//!
//! Integrate beside checkpoint_transfer and physical only after independent
//! review. Requires direct `subtle = "2"` (already transitively locked), plus
//! existing async-trait, tokio, serde_json, sha2, uuid, zeroize, thiserror.
//! Plaintext bearer authentication is restricted to an explicit loopback socket
//! behind an authenticated tunnel. It is NOT remote TLS or server authentication.
//! No listener is enabled by default. The caller owns admission/concurrency caps,
//! source WAL retention lease, immutable checkpoint provider and publication.
//! Installation must verify every complete file against its manifest digest;
//! the per-chunk digest only checks framing/corruption, not server authenticity.

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

const MAGIC: [u8; 8] = *b"NPHYS001";
const VERSION: u16 = 1;
const MAX_SOURCE_SEQUENCE: u64 = (1_u64 << 56) - 1;
const HEADER_BYTES: usize = 16;
const TOKEN_BYTES: usize = 32;
/// Length in bytes of a replication token: 32 raw random bytes.
pub const REPLICATION_TOKEN_BYTES: usize = TOKEN_BYTES;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const CHUNK_META: usize = 16 + 4 + 8 + 32;
const BATCH_META: usize = 16 + 8 + 32;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("physical transfer I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("physical transfer protocol: {0}")]
    Protocol(String),
    #[error("physical transfer authentication refused")]
    Authentication,
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
                history: uuid_at(bytes, 1),
                after: u64_at(bytes, 17),
            },
            _ => return Err(invalid("invalid session open request")),
        };
        value.encode()?;
        Ok(value)
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
#[async_trait]
pub trait PhysicalSource: Send + Sync {
    fn session_info(&self) -> &SourceSessionInfo;
    async fn read_chunk(&self, file_index: u32, offset: u64, length: u32) -> Result<Vec<u8>>;
    async fn poll(&self, after: u64, byte_limit: usize) -> Result<PhysicalPoll>;
}

/// Apply each validated physical batch with durable progress before returning.
/// Any error/cancellation poisons the wire session. Serving/publication is a
/// distinct owner protocol; receiving PollEnd alone never means SQL-ready.
#[async_trait]
pub trait BatchSink: Send {
    async fn apply(&mut self, batch: PhysicalBatch) -> Result<()>;
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
    AuthRejected = 13,
    ResumeInfo = 14,
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
            13 => Self::AuthRejected,
            14 => Self::ResumeInfo,
            _ => return Err(invalid("unknown frame kind")),
        })
    }
    fn valid_size(self, n: usize) -> bool {
        match self {
            Self::Auth => n == TOKEN_BYTES,
            Self::AuthOk | Self::Close | Self::Closed | Self::AuthRejected => n == 0,
            Self::Open => n == 1 || n == 25,
            Self::ResumeInfo => n == 48,
            Self::Manifest => (1..=MAX_MANIFEST_BYTES).contains(&n),
            Self::ChunkRequest => n == 32,
            Self::Chunk => (CHUNK_META + 1..=CHUNK_META + MAX_CHUNK_BYTES).contains(&n),
            Self::PollRequest => n == 28,
            Self::PollStart | Self::PollEnd => n == 20,
            Self::Batch => (BATCH_META + 12..=BATCH_META + MAX_BATCH_BYTES).contains(&n),
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
        _=cancelled(shutdown)=>Err(WireError::Shutdown),
        result=tokio::time::timeout(duration,future)=>result.map_err(|_|WireError::Timeout)?,
    }
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
        if header[..8] != MAGIC || u16::from_be_bytes(header[8..10].try_into().unwrap()) != VERSION {
            return Err(invalid("wrong physical transfer magic/version"));
        }
        let kind = Kind::decode(u16::from_be_bytes(header[10..12].try_into().unwrap()))?;
        let length = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
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
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn uuid_at(bytes: &[u8], offset: usize) -> Uuid {
    Uuid::from_bytes(bytes[offset..offset + 16].try_into().unwrap())
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
    fn decode(bytes: &[u8]) -> Self {
        Self {
            through: u64_at(bytes, 0),
            durable_barrier: u64_at(bytes, 8),
            batches: u32_at(bytes, 16),
        }
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
/// transport; this plaintext listener never accepts a wildcard/remote bind.
pub async fn bind_loopback(address: SocketAddr) -> Result<TcpListener> {
    if !address.ip().is_loopback() {
        return Err(invalid("physical transfer requires loopback plus authenticated tunnel"));
    }
    Ok(TcpListener::bind(address).await?)
}

/// Owns exactly this accepted socket and one lazily created export. No task is
/// detached here. Any session failure drops the socket/provider; the source DB
/// and other sessions continue under their owner's normal lifecycle.
pub async fn serve_session<I, F, Fut>(
    mut io: I,
    token: &TransferToken,
    limits: WireLimits,
    mut shutdown: watch::Receiver<bool>,
    factory: F,
) -> Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send,
    F: FnOnce(OpenRequest) -> Fut,
    Fut: Future<Output = Result<Arc<dyn PhysicalSource>>>,
{
    limits.validate()?;
    let auth = read_frame(&mut io, &[Kind::Auth], &limits, &mut shutdown).await?;
    let supplied = Zeroizing::new(auth.payload);
    let authorized = token.accepts(&supplied);
    drop(supplied);
    if !authorized {
        write_frame(&mut io, Kind::AuthRejected, &[], &limits, &mut shutdown).await?;
        return Err(WireError::Authentication);
    }
    write_frame(&mut io, Kind::AuthOk, &[], &limits, &mut shutdown).await?;
    let open = read_frame(&mut io, &[Kind::Open, Kind::Close], &limits, &mut shutdown).await?;
    if open.kind == Kind::Close {
        write_frame(&mut io, Kind::Closed, &[], &limits, &mut shutdown).await?;
        return bounded(&mut shutdown, limits.frame_timeout, async {
            io.shutdown().await?;
            Ok(())
        })
        .await;
    }
    let open = OpenRequest::decode(&open.payload)?;
    // Factory is NEVER invoked for unauthenticated/malformed sessions. Resume
    // providers validate existing source identity/cursor without checkpointing.
    let source = bounded(&mut shutdown, limits.operation_timeout, async { factory(open).await }).await?;
    let info = source.session_info().clone();
    info.validate(open, &limits)?;
    let mut known_barrier = info.initial_barrier();
    let (kind, bytes) = info.response()?;
    write_frame(&mut io, kind, &bytes, &limits, &mut shutdown).await?;
    loop {
        let request = read_frame(
            &mut io,
            &[Kind::ChunkRequest, Kind::PollRequest, Kind::Close],
            &limits,
            &mut shutdown,
        )
        .await?;
        match request.kind {
            Kind::ChunkRequest => {
                let manifest = info.manifest().ok_or_else(|| invalid("chunks require snapshot mode"))?;
                if uuid_at(&request.payload, 0) != manifest.snapshot_id {
                    return Err(invalid("chunk snapshot/session mismatch"));
                }
                let index = u32_at(&request.payload, 16);
                let offset = u64_at(&request.payload, 20);
                let length = u32_at(&request.payload, 28);
                chunk_bounds(manifest, index, offset, length, &limits)?;
                let bytes = bounded(
                    &mut shutdown,
                    limits.operation_timeout,
                    source.read_chunk(index, offset, length),
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
                if uuid_at(&request.payload, 0) != info.session_id() {
                    return Err(invalid("poll snapshot/session mismatch"));
                }
                let after = u64_at(&request.payload, 16);
                let byte_limit = u32_at(&request.payload, 24) as usize;
                if after < info.initial_after()
                    || after > MAX_SOURCE_SEQUENCE
                    || byte_limit == 0
                    || byte_limit > MAX_BATCH_BYTES
                {
                    return Err(invalid("invalid physical continuation request"));
                }
                let poll = bounded(&mut shutdown, limits.operation_timeout, source.poll(after, byte_limit)).await?;
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
            }
            Kind::Close => {
                write_frame(&mut io, Kind::Closed, &[], &limits, &mut shutdown).await?;
                return bounded(&mut shutdown, limits.frame_timeout, async {
                    io.shutdown().await?;
                    Ok(())
                })
                .await;
            }
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
    info: Option<SourceSessionInfo>,
    known_barrier: u64,
    poisoned: bool,
}
impl<I: AsyncRead + AsyncWrite + Unpin + Send> PhysicalClient<I> {
    pub async fn authenticate(
        mut io: I,
        token: &TransferToken,
        limits: WireLimits,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<Self> {
        limits.validate()?;
        write_frame(&mut io, Kind::Auth, &token.0[..], &limits, &mut shutdown).await?;
        let response = read_frame(
            &mut io,
            &[Kind::AuthOk, Kind::AuthRejected],
            &WireLimits {
                frame_timeout: limits.operation_timeout,
                ..limits
            },
            &mut shutdown,
        )
        .await?;
        if response.kind == Kind::AuthRejected {
            return Err(WireError::Authentication);
        }
        Ok(Self {
            io,
            limits,
            shutdown,
            info: None,
            known_barrier: 0,
            poisoned: false,
        })
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
    pub async fn fetch_manifest(&mut self, expected_history: Option<Uuid>) -> Result<CheckpointManifest> {
        self.check_live()?;
        // Arm BEFORE any await: cancellation/drop can leave a partial frame or
        // durable sink prefix, even when no Result is returned to this caller.
        self.poisoned = true;
        let outcome = async {
            if self.info.is_some() {
                return Err(invalid("manifest already selected for this session"));
            }
            write_frame(
                &mut self.io,
                Kind::Open,
                &OpenRequest::Snapshot.encode()?,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let frame = read_frame(
                &mut self.io,
                &[Kind::Manifest],
                &WireLimits {
                    frame_timeout: self.limits.operation_timeout,
                    ..self.limits
                },
                &mut self.shutdown,
            )
            .await?;
            let manifest = CheckpointManifest::decode(&frame.payload, &self.limits.transfer)
                .map_err(|e| invalid(e.to_string()))?;
            if expected_history.is_some_and(|expected| expected != manifest.history) {
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
            write_frame(
                &mut self.io,
                Kind::Open,
                &request.encode()?,
                &self.limits,
                &mut self.shutdown,
            )
            .await?;
            let frame = read_frame(
                &mut self.io,
                &[Kind::ResumeInfo],
                &WireLimits {
                    frame_timeout: self.limits.operation_timeout,
                    ..self.limits
                },
                &mut self.shutdown,
            )
            .await?;
            let info = SourceSessionInfo::Resume {
                session_id: uuid_at(&frame.payload, 0),
                history: uuid_at(&frame.payload, 16),
                initial_after: u64_at(&frame.payload, 32),
                durable_barrier: u64_at(&frame.payload, 40),
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
        limits: WireLimits,
        shutdown: watch::Receiver<bool>,
        history: Uuid,
        after: u64,
    ) -> Result<Self> {
        let mut client = Self::authenticate(io, token, limits, shutdown).await?;
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
            let mut frame = read_frame(&mut self.io, &[Kind::Chunk], &self.limits, &mut self.shutdown).await?;
            if uuid_at(&frame.payload, 0) != manifest.snapshot_id
                || u32_at(&frame.payload, 16) != index
                || u64_at(&frame.payload, 20) != offset
                || frame.payload.len() != CHUNK_META + length as usize
                || Sha256::digest(&frame.payload[CHUNK_META..]).as_slice() != &frame.payload[28..60]
            {
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
            let frame = read_frame(&mut self.io, &[Kind::PollStart], &self.limits, &mut self.shutdown).await?;
            let summary = PollSummary::decode(&frame.payload);
            summary.validate_since(after, byte_limit, self.known_barrier)?;
            let mut cursor = after;
            let mut bytes = 0usize;
            for _ in 0..summary.batches {
                let mut frame = read_frame(&mut self.io, &[Kind::Batch], &self.limits, &mut self.shutdown).await?;
                let history = uuid_at(&frame.payload, 0);
                let first = u64_at(&frame.payload, 16);
                let digest: [u8; 32] = frame.payload[24..56].try_into().unwrap();
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
            if PollSummary::decode(&end.payload) != summary || cursor != summary.through {
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
        limits: WireLimits,
        shutdown: watch::Receiver<bool>,
        history: Uuid,
        after: u64,
    ) -> Result<Self> {
        let mut client = Self::connect_loopback(address, token, limits, shutdown).await?;
        client.resume(history, after).await?;
        Ok(client)
    }
    pub async fn connect_loopback(
        address: SocketAddr,
        token: &TransferToken,
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
        Self::authenticate(stream, token, limits, shutdown).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::checkpoint_transfer::CheckpointFile;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    /// C2: the token's decimal ("171") and hex ("ab") renderings never appear,
    /// and the redaction marker does.
    #[test]
    fn transfer_token_debug_is_redacted() {
        let token = TransferToken::new([0xAB; 32]).unwrap();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("171"), "decimal token byte leaked: {rendered}");
        let lowered = rendered.to_lowercase();
        assert!(!lowered.contains(&"ab".repeat(32)), "hex token leaked: {rendered}");
        assert!(!lowered.contains(&vec!["ab"; 32].join(", ")), "hex token leaked: {rendered}");
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
        serve_session(io, &token(), limits(), shutdown, move |_| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(source as Arc<dyn PhysicalSource>)
        })
        .await
    }

    #[tokio::test]
    async fn authenticated_snapshot_chunks_and_physical_continuation_share_one_session() {
        let source = Arc::new(MockSource::new());
        source.manifest.validate(&limits().transfer).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (_shutdown, rx) = watch::channel(false);
        let (client_io, server_io) = duplex(128);
        let run = tokio::spawn(server(server_io, source.clone(), calls.clone(), rx.clone()));
        let mut client = PhysicalClient::authenticate(client_io, &token(), limits(), rx)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "auth alone must not export");
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

    #[tokio::test]
    async fn wrong_token_never_invokes_export_factory() {
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
            PhysicalClient::authenticate(client_io, &TransferToken::new([8; 32]).unwrap(), limits(), rx).await;
        assert!(matches!(outcome, Err(WireError::Authentication)));
        assert!(matches!(run.await.unwrap(), Err(WireError::Authentication)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn malformed_or_pre_auth_data_frames_refuse_before_payload_allocation_or_export() {
        for (kind, length) in [
            (Kind::Auth, u32::MAX),
            (Kind::Open, 0),
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

    #[tokio::test]
    async fn old_version_and_magic_are_refused_before_export() {
        for version in [true, false] {
            let (_shutdown, rx) = watch::channel(false);
            let (mut io, peer) = duplex(128);
            let calls = Arc::new(AtomicUsize::new(0));
            let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx));
            let mut bytes = header(Kind::Auth, 32);
            if version {
                bytes[8..10].copy_from_slice(&0_u16.to_be_bytes());
            } else {
                bytes[0] ^= 255;
            }
            io.write_all(&bytes).await.unwrap();
            assert!(matches!(run.await.unwrap(), Err(WireError::Protocol(_))));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
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
            let mut client = PhysicalClient::authenticate(io, &token(), limits(), rx).await.unwrap();
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
        write_frame(&mut io, Kind::Auth, &token().0[..], &limits(), &mut rx)
            .await
            .unwrap();
        read_frame(&mut io, &[Kind::AuthOk], &limits(), &mut rx).await.unwrap();
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
                read_frame(&mut peer, &[Kind::Auth], &limits(), &mut peer_shutdown).await?;
                write_frame(&mut peer, Kind::AuthOk, &[], &limits(), &mut peer_shutdown).await?;
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
            let mut client = PhysicalClient::authenticate(io, &token(), limits(), rx).await.unwrap();
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
        let mut client = PhysicalClient::authenticate(io, &token(), limits(), rx).await.unwrap();
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
        let (_shutdown, rx) = watch::channel(false);
        let (io, mut peer) = duplex(128);
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut peer_shutdown = rx.clone();
        let run = tokio::spawn(async move {
            read_frame(&mut peer, &[Kind::Auth], &limits(), &mut peer_shutdown).await?;
            write_frame(&mut peer, Kind::AuthOk, &[], &limits(), &mut peer_shutdown).await?;
            read_frame(&mut peer, &[Kind::Open], &limits(), &mut peer_shutdown).await?;
            // Deliberately stop inside a response frame, after the client has
            // sent a request and started awaiting a response.
            peer.write_all(&header(Kind::Manifest, 100)).await?;
            peer.write_all(b"{").await?;
            observed_tx.send(()).unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), release_rx).await;
            Ok::<(), WireError>(())
        });
        let mut client = PhysicalClient::authenticate(io, &token(), limits(), rx).await.unwrap();
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
            serve_session(peer, &token(), limits(), server_rx, move |request| async move {
                assert_eq!(request, OpenRequest::Resume { history, after: 10 });
                Ok(provider as Arc<dyn PhysicalSource>)
            })
            .await
        });
        let mut client = PhysicalClient::connect_resume(io, &token(), limits(), rx, history, 10)
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
        let mut client = PhysicalClient::authenticate(io, &token(), limits(), rx.clone())
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
        let mut client = PhysicalClient::connect_resume(io, &token(), limits(), rx, history, durable)
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
                PhysicalClient::connect_resume(io, &token(), limits(), rx, history, 10)
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
                read_frame(&mut peer, &[Kind::Auth], &limits(), &mut peer_shutdown).await?;
                write_frame(&mut peer, Kind::AuthOk, &[], &limits(), &mut peer_shutdown).await?;
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
                    PhysicalClient::connect_resume(io, &token(), limits(), rx, history, 10).await,
                    Err(WireError::Protocol(_))
                ),
                "{fault}"
            );
            run.await.unwrap().unwrap();
        }
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
        let client = PhysicalClient::authenticate(io, &token(), limits(), rx.clone())
            .await
            .unwrap();
        client.close().await.unwrap();
        run.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let (io, peer) = duplex(128);
        let run = tokio::spawn(server(peer, source.clone(), calls, rx.clone()));
        let mut client = PhysicalClient::connect_resume(io, &token(), limits(), rx, history, 10)
            .await
            .unwrap();
        assert!(matches!(client.read_chunk(0, 0, 1).await, Err(WireError::Protocol(_))));
        assert_eq!(source.chunks.load(Ordering::SeqCst), 0);
        drop(client);
        assert!(run.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn malformed_open_after_auth_is_refused_before_factory() {
        for payload in [vec![2], vec![0; 25], {
            let mut bytes = vec![1];
            bytes.extend([0; 16]);
            bytes.extend(10_u64.to_be_bytes());
            bytes
        }] {
            let (_shutdown, mut rx) = watch::channel(false);
            let (mut io, peer) = duplex(128);
            let calls = Arc::new(AtomicUsize::new(0));
            let run = tokio::spawn(server(peer, Arc::new(MockSource::new()), calls.clone(), rx.clone()));
            write_frame(&mut io, Kind::Auth, &token().0[..], &limits(), &mut rx)
                .await
                .unwrap();
            read_frame(&mut io, &[Kind::AuthOk], &limits(), &mut rx).await.unwrap();
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
            serve_session(peer, &token(), limits(), peer_rx, move |_| async move {
                Ok(source as Arc<dyn PhysicalSource>)
            })
            .await
        });
        let mut client = PhysicalClient::connect_resume(io, &token(), limits(), rx, history, 10)
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
            read_frame(&mut peer, &[Kind::Auth], &limits(), &mut peer_rx).await?;
            write_frame(&mut peer, Kind::AuthOk, &[], &limits(), &mut peer_rx).await?;
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
        let mut client = PhysicalClient::connect_resume(io, &token(), limits(), rx, history, 10)
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
