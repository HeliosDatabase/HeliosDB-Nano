//! Owned by the physical-standby plan (docs/plans/PHYSICAL_STANDBY_REPLICATION.md).
//! Snapshot download and durable physical continuation.
//!
//! Every receiver mutation belongs to one dedicated, joined worker thread. A
//! dropped async future cannot detach an append/apply/publication operation.
//! Cancellation is observed between commands; an in-flight RocksDB/fsync call
//! cannot be forcibly interrupted, so Drop may wait for that OS call to finish.
//! No phase in this module declares SQL readiness or opens a serving database
//! itself: a serving standby hands [`run_serving_receiver`] the closure that
//! builds one (spec P1.3), and the readiness that follows is the posture's
//! (`standby_posture`), derived from this module's [`ReceiverStatus`].
//! The worker's stage owns the receiver's `LocalWriter`; every RocksDB open of
//! the stage uses the storage options of `ReceiverOptions::config`.
//!
//! Spec P2.1: the receiver's control plane rides its one `NPHYS002` session —
//! its `Hello` names it (node id, history, durable cursor), it acknowledges
//! every durable advance with a heartbeat ([`Heartbeat`] = the ACK the
//! primary's synchronous commits wait on), and a source's refusal arrives as
//! a typed code: [`RefuseCode::retryable`] ones are retried with backoff,
//! `AuthFailed` a few times, and every other one stops the receiver, which
//! the serving standby reports as `needs_recovery`.
//!
//! Spec P2.2: the session runs over the receiver's [`ReplicationTransport`]
//! — TLS to a remote source, plaintext on loopback — and a TLS failure is a
//! transport failure (retried with backoff, its reason reported while
//! connecting). A snapshot download that is interrupted — a transport error,
//! a killed process — continues where its durable progress record says, from
//! the same leased export of the source (`checkpoint_transfer`, "Resumable
//! download"); transport errors during a download reconnect instead of
//! failing it.

use super::archiver::ArchiveFeed;
use super::checkpoint_transfer::{
    partial_snapshot, CheckpointManifest, ClosedPhysicalStage, PhysicalStage, ReceiverStaging, ReopenedPhysicalStage,
    StagingStart, TransferLimits,
};
use super::physical::{Durability, LocalWriter, PhysicalBatch, PhysicalCursor, MAX_BATCH_BYTES};
use super::physical_applier::PhysicalApplier;
use super::physical_tls::{ReplicationStream, ReplicationTransport};
use super::physical_wire::{
    now_millis, BatchSink, Heartbeat, PhysicalClient, PollSummary, ReceiverIdentity, RefuseCode, SourceSessionInfo,
    TransferToken, WireError, WireLimits,
};
use crate::Config;
use async_trait::async_trait;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// `AuthFailed` is retried this many times in all — a token file being
/// replaced on both nodes — before the receiver gives up (spec P2.1 §1).
const AUTH_ATTEMPTS: u32 = 3;
/// A failed connection round is retried after `reconnect_delay`, doubling up
/// to this bound (or `reconnect_delay`, if that is longer).
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);
/// Acknowledgements of durable progress are coalesced to one per this
/// interval, so a stream of small polls does not double the round trips.
pub const ACK_COALESCE: Duration = Duration::from_millis(50);
/// `internal-tests` builds only: a receiver whose download has received at
/// least this many bytes SIGKILLs its own process — the "standby killed
/// mid-download" of the resumable-download tests (spec P2.2), at an exact
/// point instead of a timing race.
pub const KILL_DOWNLOAD_AT_ENV: &str = "HELIOSDB_NANO_TEST_KILL_DOWNLOAD_AT";
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ReceiverError {
    #[error("physical receiver transport: {0}")]
    Wire(#[from] WireError),
    #[error("physical receiver storage/owner: {0}")]
    Storage(String),
    #[error("physical receiver configuration: {0}")]
    Invalid(String),
    #[error("physical receiver stopped before requested installation completed")]
    Interrupted,
}
pub type Result<T> = std::result::Result<T, ReceiverError>;
fn storage(error: impl std::fmt::Display) -> ReceiverError {
    ReceiverError::Storage(error.to_string())
}

#[derive(Clone, Debug)]
pub enum ReceiverStart {
    /// Destination must be absent. A failed download retains its private
    /// sibling with its progress record, and the next `Snapshot` into the
    /// same target resumes it — adopted only through a verified record of the
    /// same snapshot, else deleted and started over (spec P2.2).
    Snapshot {
        target: PathBuf,
        expected_history: Option<Uuid>,
    },
    /// Only a fully initialized physical stage with a valid durable cursor.
    /// Logical replicas are never adopted; a partial download is resumed by
    /// `Snapshot`, never by this.
    Resume { path: PathBuf, history: Uuid },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverMode {
    InstallOnce,
    Follow,
}

/// Token, transport and mode have no implicit default. A source on a
/// non-loopback address needs TLS, or the explicit plaintext opt-in (spec
/// P2.2, [`ReplicationTransport::allows`]).
pub struct ReceiverOptions {
    /// The node's configuration. Its storage section decides the RocksDB
    /// options of every open of the receiver's store, so the stage runs with
    /// the same options as the engine that serves it (and as the primary).
    pub config: Config,
    pub sources: Vec<SocketAddr>,
    /// How every connection reaches a source: TLS or plaintext.
    pub transport: ReplicationTransport,
    pub token: TransferToken,
    /// What every `Hello` of this receiver announces.
    pub identity: ReceiverIdentity,
    pub start: ReceiverStart,
    pub mode: ReceiverMode,
    pub wire: WireLimits,
    pub poll_byte_limit: usize,
    pub poll_interval: Duration,
    pub reconnect_delay: Duration,
    pub max_reconnects: u32,
    /// How often an idle receiver sends a heartbeat (spec P2.1: 5 s). A
    /// durable advance is acknowledged sooner ([`ACK_COALESCE`]).
    pub heartbeat_interval: Duration,
    /// Spec P3.1: a serving standby's WAL archive. Every batch its applier
    /// applied is offered to it right after the apply, without waiting
    /// ([`ArchiveFeed::offer`]); `None` archives nothing.
    pub archive: Option<ArchiveFeed>,
}
impl ReceiverOptions {
    fn validate(&self) -> Result<()> {
        if self.identity.node_id.is_nil()
            || self.identity.epoch == 0
            || self.heartbeat_interval.is_zero()
            || self.heartbeat_interval > Duration::from_secs(3600)
        {
            return Err(ReceiverError::Invalid(
                "invalid physical receiver identity or heartbeat interval".into(),
            ));
        }
        if self.sources.is_empty() || self.sources.len() > 16 || self.sources.iter().any(|source| source.port() == 0) {
            return Err(ReceiverError::Invalid(
                "physical receiver requires 1 to 16 source endpoints with a port".into(),
            ));
        }
        if let Some(source) = self.sources.iter().find(|source| !self.transport.allows(source)) {
            return Err(ReceiverError::Invalid(format!(
                "physical receiver: the source {source} is not loopback; reaching it needs TLS (--primary-tls) or \
                 the plaintext opt-in (--allow-insecure-replication-remote)"
            )));
        }
        let transfer = self.wire.transfer;
        if self.poll_byte_limit == 0
            || self.poll_byte_limit > MAX_BATCH_BYTES
            || self.poll_interval.is_zero()
            || self.poll_interval > Duration::from_secs(3600)
            || self.reconnect_delay.is_zero()
            || self.reconnect_delay > Duration::from_secs(3600)
            || self.max_reconnects > 1000
            || self.wire.frame_timeout.is_zero()
            || self.wire.frame_timeout > Duration::from_secs(3600)
            || self.wire.operation_timeout.is_zero()
            || self.wire.operation_timeout > Duration::from_secs(3600)
            || transfer.max_files == 0
            || transfer.max_files > 2048
            || transfer.max_total_bytes == 0
            || transfer.max_total_bytes > (1_u64 << 40)
            || transfer.max_file_bytes == 0
            || transfer.max_file_bytes > transfer.max_total_bytes
            || transfer.max_chunk_bytes == 0
            || transfer.max_chunk_bytes > 1024 * 1024
        {
            return Err(ReceiverError::Invalid(
                "invalid physical receiver bounds/deadlines".into(),
            ));
        }
        let history = match &self.start {
            ReceiverStart::Snapshot { expected_history, .. } => *expected_history,
            ReceiverStart::Resume { history, .. } => Some(*history),
        };
        if history.is_some_and(|history| history.is_nil()) {
            return Err(ReceiverError::Invalid("nil expected physical history".into()));
        }
        Ok(())
    }
}

/// Spec P1.3: builds the serving side of a following receiver over its
/// reopened store — `StorageEngine::open_physical_standby` over the stage's
/// writer, `EmbeddedDatabase::with_physical_standby`, and the
/// `PhysicalApplier` every later batch is applied through. Runs once, on the
/// receiver's owner thread, right after the stage is reopened and before the
/// first connection, so no batch can be applied around it; an `Err` stops the
/// receiver with nothing applied.
pub type ServingAttach =
    Box<dyn FnOnce(&Path, &Arc<LocalWriter>) -> std::result::Result<Arc<PhysicalApplier>, String> + Send>;

#[derive(Clone, Debug)]
pub enum ReceiverStatus {
    /// `last_error`: why the previous connection attempt failed, while the
    /// receiver retries (spec P2.2: a TLS failure reads `tls: …`).
    Connecting {
        history: Option<Uuid>,
        after: Option<u64>,
        last_error: Option<String>,
    },
    /// `received` counts every byte the download holds, resumed ones included.
    Downloading { path: PathBuf, received: u64, total: u64 },
    /// `session` numbers the receiver's connections (1 for the first, one
    /// more per reconnect), so an observer can tell a poll of the streaming
    /// session from the first poll of a new one even when the `Connecting` in
    /// between was overwritten before it saw it.
    ///
    /// `cursor` (here and in `CaughtUp`) is always durable: a poll's groups
    /// are applied deferred and synced once before anything reports past them
    /// (the applied-but-unsynced position is the writer's own cursor, which
    /// readers of the store see). `source_barrier` is the last physical
    /// barrier observed: the session's connect barrier or a poll's, reported
    /// when the poll announces it, before its groups are applied.
    ///
    /// `tls` (here and in `CaughtUp`): the session is on TLS, both ends said
    /// so (spec P2.2, `CAP_TLS`).
    Applying {
        path: PathBuf,
        cursor: PhysicalCursor,
        source_barrier: Option<u64>,
        session: u64,
        tls: bool,
    },
    /// Raw physical files installed only. This is not a serving/readiness signal.
    Installed { path: PathBuf, cursor: PhysicalCursor },
    /// Exact physical progress at the last sampled durable source barrier only.
    CaughtUp {
        path: PathBuf,
        cursor: PhysicalCursor,
        source_barrier: u64,
        session: u64,
        tls: bool,
    },
    Stopped {
        path: Option<PathBuf>,
        cursor: Option<PhysicalCursor>,
    },
    Failed {
        path: Option<PathBuf>,
        cursor: Option<PhysicalCursor>,
        error: String,
    },
}
#[derive(Clone, Debug, Default)]
struct Position {
    path: Option<PathBuf>,
    cursor: Option<PhysicalCursor>,
}
impl Position {
    fn initialized(&self) -> Result<(PathBuf, PhysicalCursor)> {
        Ok((
            self.path.clone().ok_or_else(|| storage("receiver has no owned path"))?,
            self.cursor
                .clone()
                .ok_or_else(|| storage("receiver has no durable cursor"))?,
        ))
    }
}

/// What a `Begin` did (spec P2.2): where each manifest file's download
/// continues, and whether the staging directory was new, resumed, or
/// started over.
#[derive(Clone, Debug)]
struct DownloadStart {
    offsets: Vec<u64>,
    start: StagingStart,
}

type Reply<T> = oneshot::Sender<Result<T>>;
enum Command {
    /// The snapshot an interrupted download into `target` belongs to: the
    /// live download's, else its staging directory's progress record's.
    Inspect {
        target: PathBuf,
        reply: Reply<Option<Uuid>>,
    },
    /// Start — or continue — the download of `manifest` into `target`.
    Begin {
        target: PathBuf,
        manifest: Vec<u8>,
        history: Uuid,
        limits: TransferLimits,
        reply: Reply<DownloadStart>,
    },
    Append {
        index: u32,
        offset: u64,
        bytes: Vec<u8>,
        reply: Reply<()>,
    },
    Open {
        reply: Reply<()>,
    },
    Reopen {
        path: PathBuf,
        history: Uuid,
        limits: TransferLimits,
        reply: Reply<()>,
    },
    /// Apply one source group. A poll's groups are applied
    /// [`Durability::Deferred`] and made durable by one [`Command::Sync`].
    Apply {
        batch: PhysicalBatch,
        durability: Durability,
        reply: Reply<()>,
    },
    /// fsync every deferred group of the live stage (`LocalWriter::sync`);
    /// replies the durable barrier's source sequence.
    Sync {
        reply: Reply<u64>,
    },
    /// A serving receiver's idle moment: run what its applier deferred
    /// (`PhysicalApplier::run_pending_rebuilds`). Replies whether anything
    /// was pending; a no-op without an applier.
    Maintain {
        reply: Reply<bool>,
    },
    Position {
        reply: Reply<Position>,
    },
    Publish {
        reply: Reply<ClosedPhysicalStage>,
    },
    Close {
        reply: Reply<ClosedPhysicalStage>,
    },
}
enum State {
    Empty,
    Download(ReceiverStaging),
    Initial(PhysicalStage),
    Reopened(ReopenedPhysicalStage),
    Closed(ClosedPhysicalStage),
    Failed,
}
impl State {
    fn close(self) -> Result<ClosedPhysicalStage> {
        match self {
            Self::Initial(stage) => stage.close().map_err(storage),
            Self::Reopened(stage) => stage.close().map_err(storage),
            Self::Closed(closed) => Ok(closed),
            Self::Empty | Self::Download(_) => Err(ReceiverError::Interrupted),
            Self::Failed => Err(storage("receiver worker already failed")),
        }
    }
    /// The live stage's writer, if there is one.
    fn writer(&self) -> Option<&Arc<LocalWriter>> {
        match self {
            Self::Initial(stage) => Some(stage.writer()),
            Self::Reopened(stage) => Some(stage.writer()),
            _ => None,
        }
    }
    fn position(&self) -> Position {
        match self {
            Self::Download(stage) => Position {
                path: Some(stage.staging_path().to_path_buf()),
                cursor: None,
            },
            Self::Initial(stage) => Position {
                path: Some(stage.staging_path().to_path_buf()),
                cursor: Some(stage.cursor()),
            },
            Self::Reopened(stage) => Position {
                path: Some(stage.path().to_path_buf()),
                cursor: Some(stage.cursor()),
            },
            Self::Closed(stage) => Position {
                path: Some(stage.path.clone()),
                cursor: Some(stage.cursor.clone()),
            },
            _ => Position::default(),
        }
    }
}

/// Never cloned: its sender and thread have one owner. Drop joins even if the
/// caller cancels run_receiver while a wire sink or command acknowledgement waits.
struct Worker {
    tx: Option<mpsc::Sender<Command>>,
    cancel: Arc<AtomicBool>,
    last: Arc<Mutex<Position>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Worker {
    fn spawn(config: Config, attach: Option<ServingAttach>, archive: Option<ArchiveFeed>) -> Result<Self> {
        let (tx, rx) = mpsc::channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let last = Arc::new(Mutex::new(Position::default()));
        let worker_cancel = cancel.clone();
        let worker_last = last.clone();
        let thread = thread::Builder::new()
            .name("nano-physical-receiver".into())
            .spawn(move || worker_loop(rx, worker_cancel, worker_last, &config, attach, archive))
            .map_err(storage)?;
        Ok(Self {
            tx: Some(tx),
            cancel,
            last,
            thread: Some(thread),
        })
    }
    fn last_position(&self) -> Position {
        self.last.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
    }
    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply, response) = oneshot::channel();
        self.tx
            .as_ref()
            .ok_or_else(|| storage("receiver worker closed"))?
            .send(make(reply))
            .await
            .map_err(|_| storage("receiver worker stopped before command"))?;
        response
            .await
            .map_err(|_| storage("receiver worker stopped before acknowledgement"))?
    }
    async fn position(&self) -> Result<Position> {
        // FIFO acknowledgement drains any apply whose async caller was cancelled.
        self.request(|reply| Command::Position { reply }).await
    }
    async fn close(&self) -> Result<ClosedPhysicalStage> {
        self.request(|reply| Command::Close { reply }).await
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.tx.take(); // unblock blocking_recv after any current command
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("physical receiver owner thread panicked during cleanup");
            }
        }
    }
}
fn record_position(state: &State, last: &Mutex<Position>) {
    let position = state.position();
    // Preserve the owned path/last known cursor if a command failed.
    if position.path.is_some() {
        *last.lock().unwrap_or_else(|poison| poison.into_inner()) = position;
    }
}
fn worker_loop(
    mut rx: mpsc::Receiver<Command>,
    cancel: Arc<AtomicBool>,
    last: Arc<Mutex<Position>>,
    config: &Config,
    mut attach: Option<ServingAttach>,
    archive: Option<ArchiveFeed>,
) {
    let mut state = State::Empty;
    // Spec P1.3: a serving receiver applies every batch through the applier
    // `attach` builds over the reopened stage, never through the raw stage, so
    // the serving engine's derived state follows each batch. Dropped before
    // the stage closes: the applier shares the stage's writer.
    let mut applier: Option<Arc<PhysicalApplier>> = None;
    while !cancel.load(Ordering::Acquire) {
        let Some(command) = rx.blocking_recv() else {
            break;
        };
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let mut stop = false;
        match command {
            Command::Inspect { target, reply } => {
                let snapshot = match &state {
                    State::Download(stage) => Some(stage.snapshot_id()),
                    _ => partial_snapshot(&target),
                };
                let _ = reply.send(Ok(snapshot));
            }
            Command::Begin {
                target,
                manifest,
                history,
                limits,
                reply,
            } => {
                let result = begin_download(&mut state, &target, &manifest, history, limits);
                stop = result.is_err();
                record_position(&state, &last);
                let _ = reply.send(result);
            }
            Command::Append {
                index,
                offset,
                bytes,
                reply,
            } => {
                let result = match &mut state {
                    State::Download(stage) => stage.append_chunk(index, offset, &bytes).map_err(storage),
                    _ => Err(storage("chunk received outside download state")),
                };
                stop = result.is_err();
                let _ = reply.send(result);
            }
            Command::Open { reply } => {
                let result = match std::mem::replace(&mut state, State::Failed) {
                    State::Download(stage) => stage
                        .finish_download(config)
                        .and_then(|checkpoint| checkpoint.open(config))
                        .map(|stage| {
                            state = State::Initial(stage);
                        })
                        .map_err(storage),
                    _ => Err(storage("checkpoint open outside download state")),
                };
                stop = result.is_err();
                record_position(&state, &last);
                let _ = reply.send(result);
            }
            Command::Reopen {
                path,
                history,
                limits,
                reply,
            } => {
                let result = if matches!(state, State::Empty | State::Closed(_)) {
                    ReopenedPhysicalStage::open(&path, history, limits, config)
                        .map(|stage| {
                            state = State::Reopened(stage);
                        })
                        .map_err(storage)
                } else {
                    Err(storage("resume cannot replace a live worker state"))
                };
                // Spec P1.3: build the serving side over the reopened writer,
                // here, before the first connection can deliver a batch.
                let result = result.and_then(|()| match (attach.take(), &state) {
                    (None, _) => Ok(()),
                    (Some(attach), State::Reopened(stage)) => attach(stage.path(), stage.writer())
                        .map(|built| {
                            applier = Some(built);
                        })
                        .map_err(|error| storage(format!("serving standby could not be built: {error}"))),
                    (Some(_), _) => Err(storage("serving attach without a reopened stage")),
                });
                stop = result.is_err();
                record_position(&state, &last);
                let _ = reply.send(result);
            }
            Command::Apply {
                batch,
                durability,
                reply,
            } => {
                let result = match (&mut state, &applier) {
                    (State::Reopened(stage), Some(applier)) => {
                        let before = stage.cursor().source_sequence();
                        let applied = stage
                            .verify()
                            .map_err(storage)
                            .and_then(|()| applier.apply_with(&batch, durability).map(|_| ()).map_err(storage));
                        // Spec P3.1: the standby archives what it applied —
                        // handed over without waiting, after the apply, never
                        // a retransmission it already held.
                        if let (Ok(()), Some(feed)) = (&applied, &archive) {
                            if batch.last_sequence() > before {
                                feed.offer(&batch, stage.writer().db());
                            }
                        }
                        applied
                    }
                    (State::Initial(stage), _) => stage.apply_with(&batch, durability).map(|_| ()).map_err(storage),
                    (State::Reopened(stage), None) => stage.apply_with(&batch, durability).map(|_| ()).map_err(storage),
                    _ => Err(storage("physical batch outside live stage state")),
                };
                stop = result.is_err();
                record_position(&state, &last);
                let _ = reply.send(result);
            }
            Command::Sync { reply } => {
                let result = match state.writer() {
                    Some(writer) => writer.sync().map_err(storage),
                    None => Err(storage("physical sync outside live stage state")),
                };
                stop = result.is_err();
                let _ = reply.send(result);
            }
            Command::Maintain { reply } => {
                // A failed re-derivation leaves the table pending (its readers
                // keep the scan paths); it never stops the receiver.
                let result = match &applier {
                    Some(applier) => applier.run_pending_rebuilds().map_err(storage),
                    None => Ok(false),
                };
                let _ = reply.send(result);
            }
            Command::Position { reply } => {
                let _ = reply.send(Ok(state.position()));
            }
            Command::Publish { reply } => {
                let result = match std::mem::replace(&mut state, State::Failed) {
                    State::Initial(stage) => stage
                        .publish()
                        .map(|installed| ClosedPhysicalStage {
                            path: installed.path,
                            snapshot_id: installed.snapshot_id,
                            cursor: installed.cursor,
                        })
                        .map_err(storage),
                    _ => Err(storage("only initial private stage may be installed")),
                };
                if let Ok(closed) = &result {
                    state = State::Closed(closed.clone());
                }
                stop = result.is_err();
                record_position(&state, &last);
                let _ = reply.send(result);
            }
            Command::Close { reply } => {
                drop(applier.take());
                sync_before_close(&state);
                let result = std::mem::replace(&mut state, State::Empty).close();
                if let Ok(closed) = &result {
                    state = State::Closed(closed.clone());
                }
                record_position(&state, &last);
                let _ = reply.send(result);
                stop = true;
            }
        }
        if stop {
            break;
        }
    }
    // Joining this thread proves the live DB/append handles are closed. A cleanup
    // error cannot turn a previous error/cancel into an installation success.
    // A serving standby's engine still shares the writer while its database
    // is alive, so the close below can only succeed once that is dropped too;
    // the deferred groups were fsynced first, so a handle closed by process
    // exit instead loses nothing.
    drop(applier.take());
    sync_before_close(&state);
    if matches!(state, State::Initial(_) | State::Reopened(_)) {
        if let Err(error) = state.close() {
            tracing::error!(%error,"physical receiver cleanup close failed");
        }
    }
}
/// The worker's `Begin` (spec P2.2). A live download of the same snapshot —
/// a reconnect inside one run — continues as it is; any other start goes
/// through the staging directory on disk, which
/// `ReceiverStaging::begin_or_resume` adopts only through a verified progress
/// record (another snapshot's is deleted and started over).
fn begin_download(
    state: &mut State,
    target: &Path,
    manifest: &[u8],
    history: Uuid,
    limits: TransferLimits,
) -> Result<DownloadStart> {
    match std::mem::replace(state, State::Empty) {
        State::Empty => {}
        State::Download(stage) => {
            let same = CheckpointManifest::decode(manifest, &limits).is_ok_and(|decoded| stage.holds(&decoded));
            if same {
                let offsets = stage.progress().to_vec();
                let held: u64 = offsets.iter().sum();
                *state = State::Download(stage);
                return Ok(DownloadStart {
                    offsets,
                    start: StagingStart::Resumed {
                        held,
                        redownload: Vec::new(),
                    },
                });
            }
            // Another snapshot: its record on disk says so, and the staging
            // directory is started over below.
        }
        other => {
            *state = other;
            return Err(storage("snapshot cannot replace existing worker state"));
        }
    }
    let (stage, start) = ReceiverStaging::begin_or_resume(target, manifest, history, limits).map_err(storage)?;
    let offsets = stage.progress().to_vec();
    *state = State::Download(stage);
    Ok(DownloadStart { offsets, start })
}

/// `internal-tests` builds only ([`KILL_DOWNLOAD_AT_ENV`]): SIGKILL this
/// process once the download holds `received` bytes past the configured mark.
#[cfg(all(feature = "internal-tests", unix))]
fn kill_download_hook(received: u64) {
    static KILL_AT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let kill_at = KILL_AT.get_or_init(|| {
        std::env::var(KILL_DOWNLOAD_AT_ENV)
            .ok()
            .and_then(|value| value.trim().parse().ok())
    });
    if kill_at.is_some_and(|kill_at| received >= kill_at) {
        tracing::warn!("{KILL_DOWNLOAD_AT_ENV}: killing this process with {received} bytes downloaded");
        // SAFETY: kill(2) of this very process with a standard signal; no
        // pointers, no preconditions.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    }
}

/// fsync the live stage's deferred groups before it is closed — or left to
/// close with the process, when a serving database still shares its writer.
/// `LocalWriter::close` syncs too, but only once it owns the handle.
fn sync_before_close(state: &State) {
    // A poisoned writer refuses to vouch for its store; its close says so.
    if let Some(writer) = state.writer().filter(|writer| !writer.is_poisoned()) {
        if let Err(error) = writer.sync() {
            tracing::error!(%error, "physical receiver could not fsync its applied groups before closing");
        }
    }
}

/// The sink of one poll: every group is applied [`Durability::Deferred`] and
/// the poll's caller syncs them once ([`Command::Sync`]) before it reports or
/// acknowledges the cursor. It reports the source's durable barrier as soon
/// as the poll announces it, so the posture's `primary_cursor` is current
/// while a long poll is still being applied.
struct Sink<'a> {
    worker: &'a Worker,
    progress: &'a watch::Sender<ReceiverStatus>,
    path: &'a Path,
    /// The durable cursor the poll started from.
    cursor: &'a PhysicalCursor,
    session: u64,
    tls: bool,
}
#[async_trait]
impl BatchSink for Sink<'_> {
    async fn begin(&mut self, summary: PollSummary) -> super::physical_wire::Result<()> {
        if summary.batches > 0 {
            self.progress.send_replace(ReceiverStatus::Applying {
                path: self.path.to_path_buf(),
                cursor: self.cursor.clone(),
                source_barrier: Some(summary.durable_barrier),
                session: self.session,
                tls: self.tls,
            });
        }
        Ok(())
    }
    async fn apply(&mut self, batch: PhysicalBatch) -> super::physical_wire::Result<()> {
        self.worker
            .request(|reply| Command::Apply {
                batch,
                durability: Durability::Deferred,
                reply,
            })
            .await
            .map_err(|error| WireError::Source(error.to_string()))
    }
}
fn stopped(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}
async fn wait_or_stop(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if stopped(shutdown) {
            return true;
        }
        tokio::select! {biased;
            result=shutdown.changed()=>if result.is_err() {return true;},
            ()=tokio::time::sleep_until(deadline)=>return false,
        }
    }
}
/// Worth reconnecting for: a transport failure — a TLS failure included
/// (spec P2.2: a wrong CA or certificate says nothing about the data) — or a
/// source that refused with a code that says "later" ([`RefuseCode::retryable`]).
fn retryable(error: &WireError) -> bool {
    match error {
        WireError::Io(_) | WireError::Timeout | WireError::Tls(_) => true,
        WireError::Refused(refusal) => refusal.code.retryable(),
        _ => false,
    }
}

/// When this session's next heartbeat (= acknowledgement) is due: every
/// heartbeat interval, and once a poll has advanced the durable cursor —
/// coalesced to one per [`ACK_COALESCE`].
struct Acknowledgements {
    last: Instant,
    pending: bool,
}

impl Acknowledgements {
    /// A new session: its `Hello` carried the durable cursor already.
    fn new() -> Self {
        Self {
            last: Instant::now(),
            pending: false,
        }
    }

    fn advanced(&mut self) {
        self.pending = true;
    }

    fn sent(&mut self) {
        self.last = Instant::now();
        self.pending = false;
    }

    /// The heartbeat to send now, if one is due. `cursor` is durable (the
    /// previous poll's groups were synced), so applied = durable = it.
    fn due(&self, heartbeats: bool, cursor: &PhysicalCursor, interval: Duration) -> Option<Heartbeat> {
        let elapsed = self.last.elapsed();
        (heartbeats && ((self.pending && elapsed >= ACK_COALESCE) || elapsed >= interval)).then(|| Heartbeat {
            applied_sequence: cursor.source_sequence(),
            durable_sequence: cursor.source_sequence(),
            received_at: now_millis(),
        })
    }

    /// How long an idle receiver waits before its next poll: no longer than
    /// it takes a pending acknowledgement to become due.
    fn idle_wait(&self, poll_interval: Duration) -> Duration {
        if self.pending {
            ACK_COALESCE.saturating_sub(self.last.elapsed()).min(poll_interval)
        } else {
            poll_interval
        }
    }
}
fn check_session_barrier(client: &PhysicalClient<ReplicationStream>, known_barrier: u64) -> Result<u64> {
    session_barrier(client.session_info(), known_barrier)
}
fn session_barrier(info: Option<&SourceSessionInfo>, known_barrier: u64) -> Result<u64> {
    let barrier = match info {
        Some(SourceSessionInfo::Snapshot(manifest)) => manifest.checkpoint_sequence,
        Some(SourceSessionInfo::Resume { durable_barrier, .. }) => *durable_barrier,
        None => return Err(storage("receiver connection has no selected physical session")),
    };
    if barrier < known_barrier {
        return Err(ReceiverError::Wire(WireError::Protocol(
            "source barrier regressed across receiver reconnect".into(),
        )));
    }
    Ok(barrier)
}
/// `serving`: a serving standby's receiver (spec P1.3) retries a transport
/// failure for as long as it runs — it must come back by itself when its
/// primary does, however long that takes — so `max_reconnects` does not bound
/// it; a refusal still stops it (spec P2.1: unless its code is retryable, or
/// it is one of [`AUTH_ATTEMPTS`] `AuthFailed`s). Rounds back off from
/// `reconnect_delay` up to [`MAX_RECONNECT_BACKOFF`].
async fn connect(
    options: &ReceiverOptions,
    shutdown: &mut watch::Receiver<bool>,
    progress: &watch::Sender<ReceiverStatus>,
    cursor: Option<&PhysicalCursor>,
    attempts: &mut u32,
    serving: bool,
    lost: Option<String>,
) -> Result<PhysicalClient<ReplicationStream>> {
    // A fresh snapshot names the history it was pinned to, if any.
    let snapshot_history = match &options.start {
        ReceiverStart::Snapshot { expected_history, .. } => *expected_history,
        ReceiverStart::Resume { history, .. } => Some(*history),
    };
    let mut auth_failures = 0_u32;
    let mut delay = options.reconnect_delay;
    // Why the previous attempt failed — at first, why the session this
    // connection replaces was lost — reported while retrying (spec P2.2: a
    // TLS failure is visible on `/health` as the posture's reason).
    let mut previous_error: Option<String> = lost;
    loop {
        let mut last_error = None;
        // These are the actual authenticated connections, never readiness probes.
        // A resolver's unavailable IPv6/IPv4 address must not shadow another
        // working address, but authentication/history/protocol failures stop.
        for source in &options.sources {
            if stopped(shutdown) {
                return Err(WireError::Shutdown.into());
            }
            let history = cursor.map(PhysicalCursor::history);
            let after = cursor.map(PhysicalCursor::source_sequence);
            progress.send_replace(ReceiverStatus::Connecting {
                history,
                after,
                last_error: previous_error.clone(),
            });
            let result = if let Some(cursor) = cursor {
                PhysicalClient::connect_resume_replication(
                    *source,
                    &options.transport,
                    &options.token,
                    &options.identity,
                    options.wire,
                    shutdown.clone(),
                    cursor.history(),
                    cursor.source_sequence(),
                )
                .await
            } else {
                let hello = options.identity.hello(snapshot_history, None);
                PhysicalClient::connect_replication(
                    *source,
                    &options.transport,
                    &options.token,
                    hello,
                    options.wire,
                    shutdown.clone(),
                )
                .await
            };
            if let Err(error) = &result {
                if !matches!(error, WireError::Shutdown) {
                    previous_error = Some(error.to_string());
                }
                if matches!(error, WireError::Tls(_)) {
                    tracing::warn!("the TLS connection to the physical source at {source} failed: {error}");
                }
            }
            match result {
                Ok(client) => {
                    let ok = client.hello_ok();
                    tracing::info!(
                        "Standby dataset history verified: {} (primary node {}, epoch {}, replication endpoint {}, {})",
                        ok.history,
                        ok.node_id,
                        ok.epoch,
                        if ok.physical_endpoint.is_empty() {
                            source.to_string()
                        } else {
                            ok.physical_endpoint.clone()
                        },
                        if client.tls() { "TLS" } else { "plaintext" }
                    );
                    return Ok(client);
                }
                Err(WireError::Refused(refusal))
                    if refusal.code == RefuseCode::AuthFailed && auth_failures + 1 < AUTH_ATTEMPTS =>
                {
                    auth_failures += 1;
                    tracing::warn!(
                        "the physical source at {source} refused this receiver's token ({auth_failures} of \
                         {AUTH_ATTEMPTS} attempts): {refusal}"
                    );
                    last_error = Some(WireError::Refused(refusal));
                }
                Err(error) if retryable(&error) => last_error = Some(error),
                Err(error) => return Err(error.into()),
            }
        }
        let error = last_error.ok_or_else(|| ReceiverError::Invalid("no physical source addresses".into()))?;
        if !serving && *attempts >= options.max_reconnects {
            return Err(error.into());
        }
        *attempts = attempts.saturating_add(1);
        // The round failed: say why for the whole backoff, not only from the
        // next attempt on.
        progress.send_replace(ReceiverStatus::Connecting {
            history: cursor.map(PhysicalCursor::history),
            after: cursor.map(PhysicalCursor::source_sequence),
            last_error: previous_error.clone(),
        });
        if wait_or_stop(delay, shutdown).await {
            return Err(WireError::Shutdown.into());
        }
        delay = delay
            .saturating_mul(2)
            .min(MAX_RECONNECT_BACKOFF.max(options.reconnect_delay));
    }
}

/// Recover from a failed operation of `client`'s session: drop it, make the
/// receiver's applied groups durable, and reconnect at that durable cursor —
/// unless the failure is not worth retrying (the original error is returned).
#[allow(clippy::too_many_arguments)]
async fn reconnect(
    error: WireError,
    client: PhysicalClient<ReplicationStream>,
    options: &ReceiverOptions,
    shutdown: &mut watch::Receiver<bool>,
    progress: &watch::Sender<ReceiverStatus>,
    worker: &Worker,
    reconnects: &mut u32,
    serving: bool,
) -> Result<PhysicalClient<ReplicationStream>> {
    // Discard poisoned framing before reconnect. FIFO drain is essential: a
    // sink deadline may have expired while its durable apply was still
    // completing on the owned worker.
    drop(client);
    if !retryable(&error) || (!serving && *reconnects >= options.max_reconnects) {
        // run_receiver closes/joins the owner before reporting this original
        // protocol/apply error; do not mask it with a later queue-closed
        // diagnostic from an already failed worker.
        return Err(error.into());
    }
    // The FIFO sync also makes the groups the failed poll applied durable,
    // so the reconnect resumes after a durable cursor.
    worker
        .request(|reply| Command::Sync { reply })
        .await
        .map_err(|drain| storage(format!("{error}; cannot drain receiver apply: {drain}")))?;
    let (_, durable) = worker
        .position()
        .await
        .map_err(|drain| storage(format!("{error}; cannot drain receiver apply: {drain}")))?
        .initialized()?;
    if stopped(shutdown) {
        return Err(WireError::Shutdown.into());
    }
    *reconnects = reconnects.saturating_add(1);
    let lost = error.to_string();
    progress.send_replace(ReceiverStatus::Connecting {
        history: Some(durable.history()),
        after: Some(durable.source_sequence()),
        last_error: Some(lost.clone()),
    });
    if wait_or_stop(options.reconnect_delay, shutdown).await {
        return Err(WireError::Shutdown.into());
    }
    connect(
        options,
        shutdown,
        progress,
        Some(&durable),
        reconnects,
        serving,
        Some(lost),
    )
    .await
}

/// A snapshot download's transport failure (spec P2.2): reconnect with the
/// usual backoff and continue from the bytes held, instead of failing the
/// download — unless it is not worth retrying, or a non-serving receiver ran
/// out of reconnects (its next run resumes from the progress record).
async fn retry_download(
    error: WireError,
    options: &ReceiverOptions,
    shutdown: &mut watch::Receiver<bool>,
    reconnects: &mut u32,
    serving: bool,
) -> Result<()> {
    if !retryable(&error) || (!serving && *reconnects >= options.max_reconnects) {
        return Err(error.into());
    }
    *reconnects = reconnects.saturating_add(1);
    tracing::warn!("the snapshot download was interrupted ({error}); reconnecting to continue from the bytes held");
    if wait_or_stop(options.reconnect_delay, shutdown).await {
        return Err(WireError::Shutdown.into());
    }
    Ok(())
}

/// Download the source's snapshot into `target` (spec P2.2: resumable) and
/// open it as the initial stage; returns the session the download finished
/// on, which continues with physical polls.
#[allow(clippy::too_many_arguments)]
async fn download_snapshot(
    options: &ReceiverOptions,
    shutdown: &mut watch::Receiver<bool>,
    progress: &watch::Sender<ReceiverStatus>,
    worker: &Worker,
    target: &Path,
    expected_history: Option<Uuid>,
    reconnects: &mut u32,
    serving: bool,
) -> Result<PhysicalClient<ReplicationStream>> {
    // Bytes this run fetched from the source (resumed bytes are not fetched).
    let mut fetched = 0_u64;
    loop {
        let mut client = connect(options, shutdown, progress, None, reconnects, serving, None).await?;
        let preferred = worker
            .request(|reply| Command::Inspect {
                target: target.to_path_buf(),
                reply,
            })
            .await?;
        let manifest: CheckpointManifest = match client.fetch_manifest_preferring(expected_history, preferred).await {
            Ok(manifest) => manifest,
            Err(error) => {
                drop(client);
                retry_download(error, options, shutdown, reconnects, serving).await?;
                continue;
            }
        };
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(storage)?;
        let begun = worker
            .request(|reply| Command::Begin {
                target: target.to_path_buf(),
                manifest: manifest_bytes,
                history: manifest.history,
                limits: options.wire.transfer,
                reply,
            })
            .await?;
        let mut received: u64 = begun.offsets.iter().sum();
        match &begun.start {
            StagingStart::Fresh => tracing::info!(
                "downloading snapshot {} of {}: {} bytes in {} files",
                manifest.snapshot_id,
                manifest.history,
                manifest.total_bytes,
                manifest.files.len()
            ),
            StagingStart::Resumed { held, redownload } => tracing::info!(
                "resuming the download of snapshot {}: {held} of {} bytes already held and verified{}",
                manifest.snapshot_id,
                manifest.total_bytes,
                if redownload.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; downloading again, after a digest mismatch: {}",
                        redownload.join(", ")
                    )
                }
            ),
            StagingStart::StartedOver { reason } => tracing::warn!(
                "the snapshot download is starting over with snapshot {}: {reason}",
                manifest.snapshot_id
            ),
        }
        let path = worker
            .position()
            .await?
            .path
            .ok_or_else(|| storage("download path missing"))?;
        progress.send_replace(ReceiverStatus::Downloading {
            path: path.clone(),
            received,
            total: manifest.total_bytes,
        });
        let mut interrupted = None;
        'files: for (index, file) in manifest.files.iter().enumerate() {
            let mut offset = begun.offsets.get(index).copied().unwrap_or(0);
            while offset < file.size {
                if stopped(shutdown) {
                    return Err(WireError::Shutdown.into());
                }
                let length = (file.size - offset).min(options.wire.transfer.max_chunk_bytes as u64) as u32;
                let bytes = match client.read_chunk(index as u32, offset, length).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        interrupted = Some(error);
                        break 'files;
                    }
                };
                let count = bytes.len() as u64;
                worker
                    .request(|reply| Command::Append {
                        index: index as u32,
                        offset,
                        bytes,
                        reply,
                    })
                    .await?;
                // Progress since the last reconnect: the budget starts again.
                *reconnects = 0;
                offset += count;
                received += count;
                fetched += count;
                progress.send_replace(ReceiverStatus::Downloading {
                    path: path.clone(),
                    received,
                    total: manifest.total_bytes,
                });
                #[cfg(all(feature = "internal-tests", unix))]
                kill_download_hook(received);
            }
        }
        if let Some(error) = interrupted {
            drop(client);
            retry_download(error, options, shutdown, reconnects, serving).await?;
            continue;
        }
        if stopped(shutdown) {
            return Err(WireError::Shutdown.into());
        }
        tracing::info!(
            "the download of snapshot {} is complete: {} bytes, {fetched} fetched by this run, {} resumed",
            manifest.snapshot_id,
            manifest.total_bytes,
            manifest.total_bytes.saturating_sub(fetched)
        );
        worker.request(|reply| Command::Open { reply }).await?;
        return Ok(client);
    }
}

/// Runs actual snapshot download and physical replay. Any failure retains owned
/// state. Follow returns only after requested shutdown; InstallOnce returns only
/// after complete physical catch-up and closed no-replace installation (or a
/// closed resumed path). Neither outcome authorizes SQL serving.
pub async fn run_receiver(
    options: ReceiverOptions,
    shutdown: watch::Receiver<bool>,
    progress: watch::Sender<ReceiverStatus>,
) -> Result<ClosedPhysicalStage> {
    run_receiver_with(options, None, shutdown, progress).await
}

/// Spec P1.3: follow an installed physical standby AND serve it. `attach`
/// builds the serving database and its applier over the reopened store
/// ([`ServingAttach`]); every batch is then applied through that applier, so
/// the database's derived state follows each one, and an idle poll runs what
/// the applier deferred. Only `ReceiverStart::Resume` with
/// `ReceiverMode::Follow`. Transport failures are retried for as long as the
/// receiver runs ([`connect`]); it returns after shutdown, or on a refusal.
pub async fn run_serving_receiver(
    options: ReceiverOptions,
    attach: ServingAttach,
    shutdown: watch::Receiver<bool>,
    progress: watch::Sender<ReceiverStatus>,
) -> Result<ClosedPhysicalStage> {
    if options.mode != ReceiverMode::Follow || !matches!(options.start, ReceiverStart::Resume { .. }) {
        return Err(ReceiverError::Invalid(
            "a serving receiver follows an installed physical standby (Resume, Follow)".into(),
        ));
    }
    run_receiver_with(options, Some(attach), shutdown, progress).await
}

async fn run_receiver_with(
    options: ReceiverOptions,
    attach: Option<ServingAttach>,
    mut shutdown: watch::Receiver<bool>,
    progress: watch::Sender<ReceiverStatus>,
) -> Result<ClosedPhysicalStage> {
    options.validate()?;
    let serving = attach.is_some();
    // Only a serving standby archives (spec P3.1): a bootstrap copy or an
    // offline `resync` follows no binding the archive could be filed under.
    let archive = if serving { options.archive.clone() } else { None };
    let worker = Worker::spawn(options.config.clone(), attach, archive)?;
    let outcome = run_inner(&options, &mut shutdown, &progress, &worker, serving).await;
    match outcome {
        Ok(closed) => {
            progress.send_replace(ReceiverStatus::Stopped {
                path: Some(closed.path.clone()),
                cursor: Some(closed.cursor.clone()),
            });
            // Even cancellation between command acknowledgement and this return
            // runs Worker's join. No mutation continues after this future exits.
            drop(worker);
            Ok(closed)
        }
        Err(error) => {
            let before = worker.last_position();
            // Drain any cancelled apply, close the DB, then join before exposing
            // the failure. The close error cannot conceal the triggering error.
            let cleanup = worker.close().await;
            let after = worker.last_position();
            drop(worker);
            let position = if after.path.is_some() { after } else { before };
            if matches!(&error, ReceiverError::Wire(WireError::Shutdown)) {
                progress.send_replace(ReceiverStatus::Stopped {
                    path: position.path,
                    cursor: position.cursor,
                });
                if options.mode == ReceiverMode::Follow {
                    if let Ok(closed) = cleanup {
                        return Ok(closed);
                    }
                }
                return Err(ReceiverError::Interrupted);
            }
            let message = match cleanup {
                Ok(_) => error.to_string(),
                Err(cleanup) => format!("{error}; owner close: {cleanup}"),
            };
            progress.send_replace(ReceiverStatus::Failed {
                path: position.path,
                cursor: position.cursor,
                error: message,
            });
            Err(error)
        }
    }
}
async fn run_inner(
    options: &ReceiverOptions,
    shutdown: &mut watch::Receiver<bool>,
    progress: &watch::Sender<ReceiverStatus>,
    worker: &Worker,
    serving: bool,
) -> Result<ClosedPhysicalStage> {
    let mut reconnects: u32 = 0;
    // Numbers this receiver's connections for `ReceiverStatus` (one per
    // successful `connect`).
    let mut session: u64 = 1;
    let mut initial = matches!(&options.start, ReceiverStart::Snapshot { .. });
    let mut client = match &options.start {
        ReceiverStart::Snapshot {
            target,
            expected_history,
        } => {
            download_snapshot(
                options,
                shutdown,
                progress,
                worker,
                target,
                *expected_history,
                &mut reconnects,
                serving,
            )
            .await?
        }
        ReceiverStart::Resume { path, history } => {
            if stopped(shutdown) {
                return Err(WireError::Shutdown.into());
            }
            worker
                .request(|reply| Command::Reopen {
                    path: path.clone(),
                    history: *history,
                    limits: options.wire.transfer,
                    reply,
                })
                .await?;
            let (_, cursor) = worker.position().await?.initialized()?;
            connect(
                options,
                shutdown,
                progress,
                Some(&cursor),
                &mut reconnects,
                serving,
                None,
            )
            .await?
        }
    };
    let mut observed_barrier = check_session_barrier(&client, 0)?;
    // Spec P2.1: the control plane rides this session — a heartbeat every
    // `heartbeat_interval`, and the acknowledgement of every durable advance.
    let mut acks = Acknowledgements::new();
    loop {
        if stopped(shutdown) {
            return Err(WireError::Shutdown.into());
        }
        let (path, cursor) = worker.position().await?.initialized()?;
        if let Some(heartbeat) = acks.due(client.heartbeats(), &cursor, options.heartbeat_interval) {
            match client.heartbeat(heartbeat).await {
                Ok(_) => acks.sent(),
                Err(error) => {
                    client = reconnect(
                        error,
                        client,
                        options,
                        shutdown,
                        progress,
                        worker,
                        &mut reconnects,
                        serving,
                    )
                    .await?;
                    session += 1;
                    observed_barrier = check_session_barrier(&client, observed_barrier)?;
                    acks = Acknowledgements::new();
                    continue;
                }
            }
        }
        // Every reported cursor is durable: the previous poll's groups were
        // synced before this one starts. The barrier is the last physical one
        // observed (the session's connect barrier, or the previous poll's).
        let tls = client.tls();
        progress.send_replace(ReceiverStatus::Applying {
            path: path.clone(),
            cursor: cursor.clone(),
            source_barrier: Some(observed_barrier),
            session,
            tls,
        });
        let mut sink = Sink {
            worker,
            progress,
            path: &path,
            cursor: &cursor,
            session,
            tls,
        };
        let outcome = client
            .poll(cursor.source_sequence(), options.poll_byte_limit, &mut sink)
            .await;
        let summary = match outcome {
            Ok(summary) => summary,
            Err(error) => {
                client = reconnect(
                    error,
                    client,
                    options,
                    shutdown,
                    progress,
                    worker,
                    &mut reconnects,
                    serving,
                )
                .await?;
                session += 1;
                observed_barrier = check_session_barrier(&client, observed_barrier)?;
                acks = Acknowledgements::new();
                continue;
            }
        };
        // One fsync for the whole poll: its groups were applied deferred.
        let synced = if summary.batches > 0 {
            worker.request(|reply| Command::Sync { reply }).await?
        } else {
            cursor.source_sequence()
        };
        let (path, durable) = worker.position().await?.initialized()?;
        if durable.history() != cursor.history()
            || durable.source_sequence() != summary.through
            || synced != summary.through
            || summary.durable_barrier < observed_barrier
        {
            return Err(storage("wire completion differs from receiver durable history/cursor"));
        }
        if summary.batches > 0 {
            // Durable progress: acknowledged at the next loop turn (coalesced).
            acks.advanced();
        }
        observed_barrier = summary.durable_barrier;
        reconnects = 0;
        let caught_up = summary.through == summary.durable_barrier;
        if !caught_up {
            progress.send_replace(ReceiverStatus::Applying {
                path,
                cursor: durable,
                source_barrier: Some(summary.durable_barrier),
                session,
                tls,
            });
            continue;
        }
        progress.send_replace(ReceiverStatus::CaughtUp {
            path,
            cursor: durable.clone(),
            source_barrier: summary.durable_barrier,
            session,
            tls,
        });
        if stopped(shutdown) {
            return Err(WireError::Shutdown.into());
        }
        if initial {
            let closed = worker.request(|reply| Command::Publish { reply }).await?;
            progress.send_replace(ReceiverStatus::Installed {
                path: closed.path.clone(),
                cursor: closed.cursor.clone(),
            });
            if options.mode == ReceiverMode::InstallOnce {
                drop(client);
                return Ok(closed);
            }
            drop(client);
            worker
                .request(|reply| Command::Reopen {
                    path: closed.path.clone(),
                    history: closed.cursor.history(),
                    limits: options.wire.transfer,
                    reply,
                })
                .await?;
            let (_, durable) = worker.position().await?.initialized()?;
            client = connect(
                options,
                shutdown,
                progress,
                Some(&durable),
                &mut reconnects,
                serving,
                None,
            )
            .await?;
            session += 1;
            observed_barrier = check_session_barrier(&client, observed_barrier)?;
            acks = Acknowledgements::new();
            initial = false;
            continue;
        }
        if options.mode == ReceiverMode::InstallOnce {
            drop(client);
            return worker.close().await;
        }
        if serving {
            // An idle moment: what the applier deferred (a re-derivation that
            // waits for a later key, a full re-derivation after an unknown
            // key) runs now rather than at the next batch.
            match worker.request(|reply| Command::Maintain { reply }).await {
                Ok(true) => tracing::debug!("physical standby: deferred derived-state work completed"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "physical standby: deferred derived-state work failed; it stays pending")
                }
            }
        }
        // A poll that carried groups is followed by the next one at once: the
        // primary kept writing while they were applied. Only an empty poll
        // (nothing new at the primary's barrier) waits `poll_interval` — or
        // less, while an acknowledgement is pending: a synchronous commit on
        // the primary waits for it.
        if summary.batches > 0 {
            continue;
        }
        if wait_or_stop(acks.idle_wait(options.poll_interval), shutdown).await {
            return Err(WireError::Shutdown.into());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::ignored_unit_patterns,
    clippy::large_futures
)]
mod tests {
    use super::super::{
        checkpoint_transfer::{staging_path_for, PHYSICAL_PARTIAL_FILE},
        history::load_or_create_primary_history,
        physical_source::{open_primary_session, ExportLeases, SourceWorkLimiter},
        physical_wire::{
            serve_session, OpenRequest, PeerRole, PhysicalSource, SourceContext, SourceIdentity, SourceSessionInfo,
            WireSyncMode,
        },
    };
    use super::*;
    use rocksdb::{Options, DB};
    use std::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicU64, AtomicUsize},
    };

    /// What the harness's sources served, across sessions.
    #[derive(Default)]
    struct Served {
        /// Chunks served so far (1-based count) and the bytes in them.
        chunks: AtomicUsize,
        bytes: AtomicU64,
        /// Fail this chunk (1-based, across sessions) once, ending its
        /// session as a dropped connection would; 0: never.
        fail_chunk: AtomicUsize,
    }

    struct FaultSource {
        inner: Arc<dyn PhysicalSource>,
        corrupt: bool,
        disconnect_once: Arc<AtomicBool>,
        served: Arc<Served>,
    }
    #[async_trait]
    impl PhysicalSource for FaultSource {
        fn session_info(&self) -> &SourceSessionInfo {
            self.inner.session_info()
        }
        async fn read_chunk(
            &self,
            index: u32,
            offset: u64,
            length: u32,
        ) -> super::super::physical_wire::Result<Vec<u8>> {
            let number = self.served.chunks.fetch_add(1, Ordering::SeqCst) + 1;
            if self
                .served
                .fail_chunk
                .compare_exchange(number, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Err(WireError::Source("injected chunk transport failure".into()));
            }
            let mut bytes = self.inner.read_chunk(index, offset, length).await?;
            self.served.bytes.fetch_add(bytes.len() as u64, Ordering::SeqCst);
            if self.corrupt {
                if let Some(first) = bytes.first_mut() {
                    *first ^= 1;
                }
            }
            Ok(bytes)
        }
        async fn poll(
            &self,
            after: u64,
            limit: usize,
        ) -> super::super::physical_wire::Result<super::super::physical::PhysicalPoll> {
            if self.disconnect_once.swap(false, Ordering::SeqCst) {
                return Err(WireError::Source("injected first-poll transport disconnect".into()));
            }
            self.inner.poll(after, limit).await
        }
    }
    /// Test listener/task lifetime is owned by this joined runtime thread. Even
    /// assertion unwinding joins all session/blocking work before TempDir drops.
    struct SourceHarness {
        address: SocketAddr,
        shutdown: watch::Sender<bool>,
        thread: Option<thread::JoinHandle<()>>,
        snapshots: Arc<AtomicUsize>,
        /// `OpenRequest::SnapshotOf`: a receiver resuming a download.
        snapshot_ofs: Arc<AtomicUsize>,
        resumes: Arc<AtomicUsize>,
        served: Arc<Served>,
    }
    impl SourceHarness {
        fn start(db: Arc<DB>, history: Uuid, parent: PathBuf, corrupt: bool) -> Self {
            Self::start_faults(db, history, parent, corrupt, false)
        }
        fn start_faults(db: Arc<DB>, history: Uuid, parent: PathBuf, corrupt: bool, disconnect: bool) -> Self {
            let disconnect_once = Arc::new(AtomicBool::new(disconnect));
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let (shutdown, mut stop) = watch::channel(false);
            let snapshots = Arc::new(AtomicUsize::new(0));
            let snapshot_ofs = Arc::new(AtomicUsize::new(0));
            let resumes = Arc::new(AtomicUsize::new(0));
            let served = Arc::new(Served::default());
            let observed_snapshots = snapshots.clone();
            let observed_snapshot_ofs = snapshot_ofs.clone();
            let observed_resumes = resumes.clone();
            let observed_served = served.clone();
            let work = SourceWorkLimiter::new(2).unwrap();
            let leases = ExportLeases::new(Duration::from_secs(3600), 4);
            let context = Arc::new(SourceContext::unobserved(SourceIdentity {
                node_id: Uuid::new_v4(),
                history,
                epoch: 1,
                physical_endpoint: address.to_string(),
                pg_port: 0,
            }));
            let thread = thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let listener=tokio::net::TcpListener::from_std(listener).unwrap();
                    let mut sessions=tokio::task::JoinSet::new();
                    loop {
                        tokio::select! {biased;
                            result=stop.changed()=>{if result.is_err() || *stop.borrow() {break;}},
                            Some(_result)=sessions.join_next(),if !sessions.is_empty()=>{},
                            accepted=listener.accept()=>{
                                let (io,_)=accepted.unwrap();
                                let db=db.clone();let parent=parent.clone();let stop=stop.clone();
                                let snapshots=observed_snapshots.clone();let resumes=observed_resumes.clone();
                                let snapshot_ofs=observed_snapshot_ofs.clone();let served=observed_served.clone();
                                let disconnect_once=disconnect_once.clone();
                                let work=work.clone();
                                let leases=leases.clone();
                                let context=context.clone();
                                sessions.spawn(async move {
                                    serve_session(io,&fixture_token(),wire_limits(),stop,&context,None,move |request|async move {
                                        match request {
                                            OpenRequest::Snapshot=>{snapshots.fetch_add(1,Ordering::SeqCst);},
                                            OpenRequest::SnapshotOf{..}=>{snapshot_ofs.fetch_add(1,Ordering::SeqCst);},
                                            OpenRequest::Resume{..}=>{resumes.fetch_add(1,Ordering::SeqCst);},
                                        }
                                        let source=open_primary_session(db.clone(),history,parent,TransferLimits::default(),request,work,leases).await?;
                                        if matches!(request,OpenRequest::Snapshot) {
                                            // Write AFTER the checkpoint: receiving this value proves
                                            // the continuation stream was consumed, not only file copy.
                                            tokio::task::spawn_blocking(move ||db.put(b"stream-only",b"after-checkpoint"))
                                                .await.map_err(|e|WireError::Source(e.to_string()))?
                                                .map_err(|e|WireError::Source(e.to_string()))?;
                                        }
                                        Ok(Arc::new(FaultSource{inner:source,corrupt,disconnect_once,served}) as Arc<dyn PhysicalSource>)
                                    }).await
                                });
                            }
                        }
                    }
                    // Every session sees the same shutdown watch. Runtime drop
                    // additionally waits for any source-owned blocking task.
                    while sessions.join_next().await.is_some() {}
                    work.drain().await.expect("source blocking jobs drain");
                });
            });
            Self {
                address,
                shutdown,
                thread: Some(thread),
                snapshots,
                snapshot_ofs,
                resumes,
                served,
            }
        }
    }
    impl Drop for SourceHarness {
        fn drop(&mut self) {
            self.shutdown.send_replace(true);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    fn fixture_token() -> TransferToken {
        TransferToken::new([29; 32]).unwrap()
    }
    fn wire_limits() -> WireLimits {
        WireLimits {
            frame_timeout: Duration::from_secs(5),
            operation_timeout: Duration::from_secs(15),
            ..Default::default()
        }
    }
    fn primary(path: &std::path::Path) -> (Arc<DB>, Uuid) {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_wal_ttl_seconds(3600);
        let db = Arc::new(DB::open(&options, path.join("source")).unwrap());
        db.put(b"seed", b"original").unwrap();
        let history = load_or_create_primary_history(db.path()).unwrap();
        (db, history)
    }
    fn options(source: &SourceHarness, start: ReceiverStart, mode: ReceiverMode) -> ReceiverOptions {
        ReceiverOptions {
            config: Config::default(),
            sources: vec![source.address],
            transport: ReplicationTransport::default(),
            token: fixture_token(),
            identity: ReceiverIdentity {
                node_id: Uuid::new_v4(),
                role: PeerRole::Resync,
                epoch: 1,
                pg_port: 0,
                sync_mode: WireSyncMode::Async,
            },
            start,
            mode,
            wire: wire_limits(),
            poll_byte_limit: MAX_BATCH_BYTES,
            poll_interval: Duration::from_millis(10),
            reconnect_delay: Duration::from_millis(10),
            max_reconnects: 3,
            heartbeat_interval: Duration::from_millis(20),
            archive: None,
        }
    }
    fn status() -> (watch::Sender<ReceiverStatus>, watch::Receiver<ReceiverStatus>) {
        watch::channel(ReceiverStatus::Connecting {
            history: None,
            after: None,
            last_error: None,
        })
    }
    async fn complete(future: impl Future<Output = Result<ClosedPhysicalStage>>) -> ClosedPhysicalStage {
        // This owns the run future (not a detached JoinHandle); timeout drops it
        // and joins its worker before this test can release private directories.
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("receiver test deadline")
            .unwrap()
    }

    #[tokio::test]
    async fn actual_snapshot_install_and_later_resume_preserve_exact_durable_rows() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db.clone(), history, root.path().to_path_buf(), false);
        let target = root.path().join("receiver");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let installed = complete(run_receiver(
            options(
                &source,
                ReceiverStart::Snapshot {
                    target: target.clone(),
                    expected_history: Some(history),
                },
                ReceiverMode::InstallOnce,
            ),
            shutdown.clone(),
            progress,
        ))
        .await;
        assert_eq!(installed.path, target);
        assert_eq!(installed.cursor.source_sequence(), 2);
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 1);
        {
            let raw = DB::open_for_read_only(&Options::default(), &target, false).unwrap();
            assert_eq!(raw.get(b"seed").unwrap(), Some(b"original".to_vec()));
            assert_eq!(raw.get(b"stream-only").unwrap(), Some(b"after-checkpoint".to_vec()));
        }
        db.put(b"after-disconnect", b"resumed").unwrap();
        let (progress, _) = status();
        let resumed = complete(run_receiver(
            options(
                &source,
                ReceiverStart::Resume {
                    path: target.clone(),
                    history,
                },
                ReceiverMode::InstallOnce,
            ),
            shutdown,
            progress,
        ))
        .await;
        assert_eq!(resumed.path, target);
        assert_eq!(resumed.cursor.source_sequence(), 3);
        assert_eq!(resumed.snapshot_id, installed.snapshot_id);
        assert_eq!(
            source.snapshots.load(Ordering::SeqCst),
            1,
            "Resume must not export another checkpoint"
        );
        assert!(source.resumes.load(Ordering::SeqCst) >= 1);
        let raw = DB::open_for_read_only(&Options::default(), &target, false).unwrap();
        assert_eq!(raw.get(b"after-disconnect").unwrap(), Some(b"resumed".to_vec()));
        assert_eq!(raw.get(b"seed").unwrap(), Some(b"original".to_vec()));
    }

    type Running<'a> = Pin<Box<dyn Future<Output = Result<ClosedPhysicalStage>> + Send + 'a>>;
    async fn wait_caught_up(
        run: &mut Running<'_>,
        progress: &mut watch::Receiver<ReceiverStatus>,
        path: &std::path::Path,
        sequence: u64,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let matched = matches!(&*progress.borrow_and_update(),ReceiverStatus::CaughtUp {path:actual,cursor,..} if actual==path && cursor.source_sequence()>=sequence);
            if matched {
                return;
            }
            tokio::select! {
                result=run.as_mut()=>panic!("receiver stopped early: {result:?}"),
                changed=progress.changed()=>changed.expect("progress channel closed"),
                _=tokio::time::sleep_until(deadline)=>panic!("receiver catch-up deadline"),
            }
        }
    }
    #[tokio::test]
    async fn follow_installs_reopens_resumes_and_joins_before_shutdown_returns() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db.clone(), history, root.path().to_path_buf(), false);
        let target = root.path().join("following");
        let (stop, shutdown) = watch::channel(false);
        let (progress, mut observed) = status();
        let mut run: Running<'_> = Box::pin(run_receiver(
            options(
                &source,
                ReceiverStart::Snapshot {
                    target: target.clone(),
                    expected_history: Some(history),
                },
                ReceiverMode::Follow,
            ),
            shutdown,
            progress,
        ));
        wait_caught_up(&mut run, &mut observed, &target, 2).await;
        assert!(source.resumes.load(Ordering::SeqCst) >= 1);
        db.put(b"followed", b"while-running").unwrap();
        wait_caught_up(&mut run, &mut observed, &target, 3).await;
        stop.send_replace(true);
        let closed = complete(run).await;
        assert_eq!(closed.path, target);
        assert_eq!(closed.cursor.source_sequence(), 3);
        // A second exclusive owner can now open the same path: no detached
        // worker retains its lock or can mutate it after this function returns.
        let stage =
            ReopenedPhysicalStage::open(&target, history, TransferLimits::default(), &Config::default()).unwrap();
        assert_eq!(stage.cursor().source_sequence(), 3);
        stage.close().unwrap();
        let raw = DB::open_for_read_only(&Options::default(), &target, false).unwrap();
        assert_eq!(raw.get(b"followed").unwrap(), Some(b"while-running".to_vec()));
    }

    #[tokio::test]
    async fn corrupt_complete_file_is_retained_without_installing_destination() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db, history, root.path().to_path_buf(), true);
        let target = root.path().join("never-install");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, observed) = status();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            run_receiver(
                options(
                    &source,
                    ReceiverStart::Snapshot {
                        target: target.clone(),
                        expected_history: Some(history),
                    },
                    ReceiverMode::InstallOnce,
                ),
                shutdown,
                progress,
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(!target.exists());
        let observed_status = observed.borrow();
        match &*observed_status {
            ReceiverStatus::Failed {
                path: Some(path),
                cursor,
                ..
            } => {
                assert!(path.is_dir());
                assert!(cursor.is_none());
                assert!(path
                    .join(super::super::checkpoint_transfer::PHYSICAL_MANIFEST_FILE)
                    .is_file());
            }
            other => panic!("missing retained failure state: {other:?}"),
        }
    }
    #[tokio::test]
    async fn authentication_failure_creates_no_receiver_and_invokes_no_export() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db, history, root.path().to_path_buf(), false);
        let target = root.path().join("unauthorized");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let mut options = options(
            &source,
            ReceiverStart::Snapshot {
                target: target.clone(),
                expected_history: Some(history),
            },
            ReceiverMode::InstallOnce,
        );
        options.token = TransferToken::new([30; 32]).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), run_receiver(options, shutdown, progress))
            .await
            .unwrap();
        // Spec P2.1: a typed refusal, after `AUTH_ATTEMPTS` tries.
        assert!(
            matches!(&result, Err(ReceiverError::Wire(WireError::Refused(refusal))) if refusal.code == RefuseCode::AuthFailed),
            "{:?}",
            result.err()
        );
        assert!(!target.exists());
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 0);
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nano-resync-")));
    }
    /// Spec P2.1: a source of another dataset refuses the receiver with a
    /// typed code it does not retry (the P1.3 residual: a refusal looked like a
    /// dropped socket and was retried forever).
    #[tokio::test]
    async fn history_mismatch_is_a_typed_refusal_that_is_never_retried() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db, history, root.path().to_path_buf(), false);
        let target = root.path().join("other-history");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let mut options = options(
            &source,
            ReceiverStart::Snapshot {
                target: target.clone(),
                expected_history: Some(Uuid::new_v4()),
            },
            ReceiverMode::InstallOnce,
        );
        options.reconnect_delay = Duration::from_secs(5);
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(10), run_receiver(options, shutdown, progress))
            .await
            .unwrap();
        assert!(
            matches!(&result, Err(ReceiverError::Wire(WireError::Refused(refusal)))
                if refusal.code == RefuseCode::HistoryMismatch && refusal.code.needs_recovery()),
            "{:?}",
            result.err()
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no reconnect delay was paid"
        );
        assert!(!target.exists());
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn dropping_worker_waits_for_owned_inflight_command_instead_of_detaching_it() {
        let (tx, mut rx) = mpsc::channel(1);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let finished = completed.clone();
        let thread = thread::spawn(move || {
            let _command = rx.blocking_recv().expect("fixture command");
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            finished.store(true, Ordering::Release);
        });
        let worker = Worker {
            tx: Some(tx),
            cancel: Arc::new(AtomicBool::new(false)),
            last: Arc::new(Mutex::new(Position::default())),
            thread: Some(thread),
        };
        let (reply, _) = oneshot::channel();
        worker
            .tx
            .as_ref()
            .unwrap()
            .blocking_send(Command::Position { reply })
            .unwrap();
        let started = started_rx.recv_timeout(Duration::from_secs(2));
        let (dropping_tx, dropping_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let dropper = thread::spawn(move || {
            let _ = dropping_tx.send(());
            drop(worker);
            let _ = done_tx.send(());
        });
        let dropping = dropping_rx.recv_timeout(Duration::from_secs(2));
        let early = done_rx.recv_timeout(Duration::from_millis(50));
        // Always release/reap before assertions, including failed scheduling.
        let _ = release_tx.send(());
        let done = if early.is_ok() {
            Ok(())
        } else {
            done_rx.recv_timeout(Duration::from_secs(2))
        };
        let joined = dropper.join();
        assert!(started.is_ok() && dropping.is_ok());
        assert!(early.is_err(), "Drop detached the blocked command");
        assert!(done.is_ok() && joined.is_ok());
        assert!(completed.load(Ordering::Acquire));
    }
    #[test]
    fn reconnect_cannot_forget_a_larger_previously_observed_source_barrier() {
        let info = SourceSessionInfo::Resume {
            session_id: Uuid::new_v4(),
            history: Uuid::new_v4(),
            initial_after: 10,
            durable_barrier: 15,
        };
        assert!(session_barrier(Some(&info), 20).is_err());
        assert_eq!(session_barrier(Some(&info), 15).unwrap(), 15);
        assert!(session_barrier(None, 0).is_err());
    }
    #[tokio::test]
    async fn interrupted_physical_poll_reconnects_with_durable_cursor_without_new_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start_faults(db, history, root.path().to_path_buf(), false, true);
        let target = root.path().join("reconnected");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let installed = complete(run_receiver(
            options(
                &source,
                ReceiverStart::Snapshot {
                    target: target.clone(),
                    expected_history: Some(history),
                },
                ReceiverMode::InstallOnce,
            ),
            shutdown,
            progress,
        ))
        .await;
        assert_eq!(installed.cursor.source_sequence(), 2);
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 1);
        assert_eq!(source.resumes.load(Ordering::SeqCst), 1);
        let raw = DB::open_for_read_only(&Options::default(), &target, false).unwrap();
        assert_eq!(raw.get(b"stream-only").unwrap(), Some(b"after-checkpoint".to_vec()));
        assert_eq!(raw.get(b"seed").unwrap(), Some(b"original".to_vec()));
    }
    #[tokio::test]
    async fn unusable_first_resolved_endpoint_does_not_shadow_second_real_connection() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db, history, root.path().to_path_buf(), false);
        // This owned first endpoint closes the actual accepted connection. It
        // never asks for credentials and cannot be confused with a reused port.
        struct ClosingEndpoint {
            address: SocketAddr,
            cancel: Arc<AtomicBool>,
            thread: Option<thread::JoinHandle<()>>,
        }
        impl Drop for ClosingEndpoint {
            fn drop(&mut self) {
                self.cancel.store(true, Ordering::Release);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !stop.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        });
        let first = ClosingEndpoint {
            address,
            cancel,
            thread: Some(thread),
        };
        let target = root.path().join("second-address");
        let mut options = options(
            &source,
            ReceiverStart::Snapshot {
                target: target.clone(),
                expected_history: Some(history),
            },
            ReceiverMode::InstallOnce,
        );
        options.sources = vec![first.address, source.address];
        options.max_reconnects = 0; // every address is attempted in the first round
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let installed = complete(run_receiver(options, shutdown, progress)).await;
        assert_eq!(installed.path, target);
        assert_eq!(installed.cursor.source_sequence(), 2);
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 1);
    }

    /// The installed copy's manifest total and file count.
    fn installed_total(target: &Path) -> (u64, usize) {
        let bytes = std::fs::read(target.join(super::super::checkpoint_transfer::PHYSICAL_MANIFEST_FILE)).unwrap();
        let manifest = CheckpointManifest::decode(&bytes, &TransferLimits::default()).unwrap();
        (manifest.total_bytes, manifest.files.len())
    }

    /// Spec P2.2: a transport failure in the middle of a download reconnects
    /// and continues — from the same leased export, without fetching a byte
    /// twice — instead of failing the download.
    #[tokio::test]
    async fn a_download_interrupted_by_the_transport_reconnects_and_continues() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db, history, root.path().to_path_buf(), false);
        source.served.fail_chunk.store(3, Ordering::SeqCst);
        let target = root.path().join("interrupted");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let installed = complete(run_receiver(
            options(
                &source,
                ReceiverStart::Snapshot {
                    target: target.clone(),
                    expected_history: Some(history),
                },
                ReceiverMode::InstallOnce,
            ),
            shutdown,
            progress,
        ))
        .await;
        assert_eq!(installed.path, target);
        assert_eq!(
            source.served.fail_chunk.load(Ordering::SeqCst),
            0,
            "the failure was injected"
        );
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 1, "one export");
        assert_eq!(
            source.snapshot_ofs.load(Ordering::SeqCst),
            1,
            "the reconnect asked for the same snapshot"
        );
        let (total, _) = installed_total(&target);
        assert_eq!(
            source.served.bytes.load(Ordering::SeqCst),
            total,
            "every byte fetched exactly once"
        );
        assert!(
            !target.join(PHYSICAL_PARTIAL_FILE).exists(),
            "an installed copy carries no record"
        );
        let raw = DB::open_for_read_only(&Options::default(), &target, false).unwrap();
        assert_eq!(raw.get(b"seed").unwrap(), Some(b"original".to_vec()));
        assert_eq!(raw.get(b"stream-only").unwrap(), Some(b"after-checkpoint".to_vec()));
    }

    /// Spec P2.2: a receiver that gave up mid-download (here: no reconnects
    /// allowed) leaves its staging directory and progress record behind; the
    /// next download into the same target resumes it from the same export.
    #[tokio::test]
    async fn the_next_download_into_the_same_target_resumes_an_abandoned_one() {
        let root = tempfile::tempdir().unwrap();
        let (db, history) = primary(root.path());
        let source = SourceHarness::start(db, history, root.path().to_path_buf(), false);
        source.served.fail_chunk.store(2, Ordering::SeqCst);
        let target = root.path().join("abandoned");
        let (_stop, shutdown) = watch::channel(false);
        let (progress, _) = status();
        let mut first = options(
            &source,
            ReceiverStart::Snapshot {
                target: target.clone(),
                expected_history: Some(history),
            },
            ReceiverMode::InstallOnce,
        );
        first.max_reconnects = 0;
        let failed = tokio::time::timeout(Duration::from_secs(30), run_receiver(first, shutdown.clone(), progress))
            .await
            .unwrap();
        assert!(failed.is_err(), "the first run gives up");
        assert!(!target.exists());
        let staging = staging_path_for(&target).unwrap();
        let record: super::super::checkpoint_transfer::PartialRecord =
            serde_json::from_slice(&std::fs::read(staging.join(PHYSICAL_PARTIAL_FILE)).unwrap()).unwrap();
        let held: u64 = record.files.iter().map(|file| file.bytes_done).sum();
        assert!(held > 0, "the first file was recorded complete: {record:?}");
        let fetched_before = source.served.bytes.load(Ordering::SeqCst);
        assert_eq!(held, fetched_before, "the record covers everything received");

        let (progress, _) = status();
        let installed = complete(run_receiver(
            options(
                &source,
                ReceiverStart::Snapshot {
                    target: target.clone(),
                    expected_history: Some(history),
                },
                ReceiverMode::InstallOnce,
            ),
            shutdown,
            progress,
        ))
        .await;
        assert_eq!(
            installed.snapshot_id, record.snapshot_id,
            "the same snapshot was installed"
        );
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 1);
        assert_eq!(source.snapshot_ofs.load(Ordering::SeqCst), 1);
        let (total, _) = installed_total(&target);
        assert_eq!(
            source.served.bytes.load(Ordering::SeqCst),
            total,
            "the resumed run fetched only what the first did not"
        );
        assert!(!staging.exists(), "the staging directory became the target");
        assert!(!target.join(PHYSICAL_PARTIAL_FILE).exists());
    }
}
