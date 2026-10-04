//! EXTERNAL DRAFT: snapshot download and durable physical continuation.
//!
//! Every receiver mutation belongs to one dedicated, joined worker thread. A
//! dropped async future cannot detach an append/apply/publication operation.
//! Cancellation is observed between commands; an in-flight RocksDB/fsync call
//! cannot be forcibly interrupted, so Drop may wait for that OS call to finish.
//! No phase in this module declares SQL readiness or opens a serving database.

use super::checkpoint_transfer::{
    ClosedPhysicalStage, PhysicalStage, ReceiverStaging, ReopenedPhysicalStage, TransferLimits,
};
use super::physical::{PhysicalBatch, PhysicalCursor, MAX_BATCH_BYTES};
use super::physical_wire::{BatchSink, PhysicalClient, SourceSessionInfo, TransferToken, WireError, WireLimits};
use async_trait::async_trait;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
};
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
    /// Destination must be absent. A failed download retains its private sibling.
    Snapshot {
        target: PathBuf,
        expected_history: Option<Uuid>,
    },
    /// Only a fully initialized physical stage with a valid durable cursor.
    /// Existing partial downloads and logical replicas are never adopted.
    Resume { path: PathBuf, history: Uuid },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverMode {
    InstallOnce,
    Follow,
}

/// Loopback is deliberate: use an authenticated tunnel for remote sources until
/// a reviewed TLS transport is provided. Token and mode have no implicit default.
pub struct ReceiverOptions {
    pub sources: Vec<SocketAddr>,
    pub token: TransferToken,
    pub start: ReceiverStart,
    pub mode: ReceiverMode,
    pub wire: WireLimits,
    pub poll_byte_limit: usize,
    pub poll_interval: Duration,
    pub reconnect_delay: Duration,
    pub max_reconnects: u32,
}
impl ReceiverOptions {
    fn validate(&self) -> Result<()> {
        if self.sources.is_empty()
            || self.sources.len() > 16
            || self
                .sources
                .iter()
                .any(|source| !source.ip().is_loopback() || source.port() == 0)
        {
            return Err(ReceiverError::Invalid(
                "physical receiver requires a loopback tunnel endpoint".into(),
            ));
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

#[derive(Clone, Debug)]
pub enum ReceiverStatus {
    Connecting {
        history: Option<Uuid>,
        after: Option<u64>,
    },
    Downloading {
        path: PathBuf,
        received: u64,
        total: u64,
    },
    Applying {
        path: PathBuf,
        cursor: PhysicalCursor,
        source_barrier: Option<u64>,
    },
    /// Raw physical files installed only. This is not a serving/readiness signal.
    Installed {
        path: PathBuf,
        cursor: PhysicalCursor,
    },
    /// Exact physical progress at the last sampled durable source barrier only.
    CaughtUp {
        path: PathBuf,
        cursor: PhysicalCursor,
        source_barrier: u64,
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

type Reply<T> = oneshot::Sender<Result<T>>;
enum Command {
    Begin {
        target: PathBuf,
        manifest: Vec<u8>,
        history: Uuid,
        limits: TransferLimits,
        reply: Reply<()>,
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
    Apply {
        batch: PhysicalBatch,
        reply: Reply<()>,
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
    fn position(&self) -> Position {
        match self {
            Self::Download(stage) => Position {
                path: Some(stage.staging_path().to_path_buf()),
                cursor: None,
            },
            Self::Initial(stage) => Position {
                path: Some(stage.staging_path().to_path_buf()),
                cursor: Some(stage.cursor().clone()),
            },
            Self::Reopened(stage) => Position {
                path: Some(stage.path().to_path_buf()),
                cursor: Some(stage.cursor().clone()),
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
    fn spawn() -> Result<Self> {
        let (tx, rx) = mpsc::channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let last = Arc::new(Mutex::new(Position::default()));
        let worker_cancel = cancel.clone();
        let worker_last = last.clone();
        let thread = thread::Builder::new()
            .name("nano-physical-receiver".into())
            .spawn(move || worker_loop(rx, worker_cancel, worker_last))
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
fn worker_loop(mut rx: mpsc::Receiver<Command>, cancel: Arc<AtomicBool>, last: Arc<Mutex<Position>>) {
    let mut state = State::Empty;
    while !cancel.load(Ordering::Acquire) {
        let Some(command) = rx.blocking_recv() else {
            break;
        };
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let mut stop = false;
        match command {
            Command::Begin {
                target,
                manifest,
                history,
                limits,
                reply,
            } => {
                let result = if matches!(state, State::Empty) {
                    ReceiverStaging::begin(&target, &manifest, history, limits)
                        .map(|stage| {
                            state = State::Download(stage);
                        })
                        .map_err(storage)
                } else {
                    Err(storage("snapshot cannot replace existing worker state"))
                };
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
                        .finish_download()
                        .and_then(|checkpoint| checkpoint.open())
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
                    ReopenedPhysicalStage::open(&path, history, limits)
                        .map(|stage| {
                            state = State::Reopened(stage);
                        })
                        .map_err(storage)
                } else {
                    Err(storage("resume cannot replace a live worker state"))
                };
                stop = result.is_err();
                record_position(&state, &last);
                let _ = reply.send(result);
            }
            Command::Apply { batch, reply } => {
                let result = match &mut state {
                    State::Initial(stage) => stage.apply(&batch).map(|_| ()).map_err(storage),
                    State::Reopened(stage) => stage.apply(&batch).map(|_| ()).map_err(storage),
                    _ => Err(storage("physical batch outside live stage state")),
                };
                stop = result.is_err();
                record_position(&state, &last);
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
    if matches!(state, State::Initial(_) | State::Reopened(_)) {
        if let Err(error) = state.close() {
            tracing::error!(%error,"physical receiver cleanup close failed");
        }
    }
}
struct Sink<'a> {
    worker: &'a Worker,
}
#[async_trait]
impl BatchSink for Sink<'_> {
    async fn apply(&mut self, batch: PhysicalBatch) -> super::physical_wire::Result<()> {
        self.worker
            .request(|reply| Command::Apply { batch, reply })
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
fn retryable(error: &WireError) -> bool {
    matches!(error, WireError::Io(_) | WireError::Timeout)
}
fn check_session_barrier(client: &PhysicalClient<TcpStream>, known_barrier: u64) -> Result<u64> {
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
async fn connect(
    options: &ReceiverOptions,
    shutdown: &mut watch::Receiver<bool>,
    progress: &watch::Sender<ReceiverStatus>,
    cursor: Option<&PhysicalCursor>,
    attempts: &mut u32,
) -> Result<PhysicalClient<TcpStream>> {
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
            progress.send_replace(ReceiverStatus::Connecting { history, after });
            let result = if let Some(cursor) = cursor {
                PhysicalClient::connect_resume_loopback(
                    *source,
                    &options.token,
                    options.wire,
                    shutdown.clone(),
                    cursor.history(),
                    cursor.source_sequence(),
                )
                .await
            } else {
                PhysicalClient::connect_loopback(*source, &options.token, options.wire, shutdown.clone()).await
            };
            match result {
                Ok(client) => return Ok(client),
                Err(error) if retryable(&error) => last_error = Some(error),
                Err(error) => return Err(error.into()),
            }
        }
        let error = last_error.ok_or_else(|| ReceiverError::Invalid("no physical source addresses".into()))?;
        if *attempts >= options.max_reconnects {
            return Err(error.into());
        }
        *attempts += 1;
        if wait_or_stop(options.reconnect_delay, shutdown).await {
            return Err(WireError::Shutdown.into());
        }
    }
}

/// Runs actual snapshot download and physical replay. Any failure retains owned
/// state. Follow returns only after requested shutdown; InstallOnce returns only
/// after complete physical catch-up and closed no-replace installation (or a
/// closed resumed path). Neither outcome authorizes SQL serving.
pub async fn run_receiver(
    options: ReceiverOptions,
    mut shutdown: watch::Receiver<bool>,
    progress: watch::Sender<ReceiverStatus>,
) -> Result<ClosedPhysicalStage> {
    options.validate()?;
    let worker = Worker::spawn()?;
    let outcome = run_inner(&options, &mut shutdown, &progress, &worker).await;
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
) -> Result<ClosedPhysicalStage> {
    let mut reconnects = 0;
    let mut initial = matches!(&options.start, ReceiverStart::Snapshot { .. });
    let mut client = match &options.start {
        ReceiverStart::Snapshot {
            target,
            expected_history,
        } => {
            let mut client = connect(options, shutdown, progress, None, &mut reconnects).await?;
            let manifest = client.fetch_manifest(*expected_history).await?;
            let manifest_bytes = serde_json::to_vec(&manifest).map_err(storage)?;
            worker
                .request(|reply| Command::Begin {
                    target: target.clone(),
                    manifest: manifest_bytes,
                    history: manifest.history,
                    limits: options.wire.transfer,
                    reply,
                })
                .await?;
            let path = worker
                .position()
                .await?
                .path
                .ok_or_else(|| storage("download path missing"))?;
            let mut received = 0_u64;
            progress.send_replace(ReceiverStatus::Downloading {
                path: path.clone(),
                received,
                total: manifest.total_bytes,
            });
            for (index, file) in manifest.files.iter().enumerate() {
                let mut offset = 0_u64;
                while offset < file.size {
                    let length = (file.size - offset).min(options.wire.transfer.max_chunk_bytes as u64) as u32;
                    let bytes = client.read_chunk(index as u32, offset, length).await?;
                    let count = bytes.len() as u64;
                    worker
                        .request(|reply| Command::Append {
                            index: index as u32,
                            offset,
                            bytes,
                            reply,
                        })
                        .await?;
                    offset += count;
                    received += count;
                    progress.send_replace(ReceiverStatus::Downloading {
                        path: path.clone(),
                        received,
                        total: manifest.total_bytes,
                    });
                }
            }
            if stopped(shutdown) {
                return Err(WireError::Shutdown.into());
            }
            worker.request(|reply| Command::Open { reply }).await?;
            client
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
            connect(options, shutdown, progress, Some(&cursor), &mut reconnects).await?
        }
    };
    let mut observed_barrier = check_session_barrier(&client, 0)?;
    loop {
        if stopped(shutdown) {
            return Err(WireError::Shutdown.into());
        }
        let (path, cursor) = worker.position().await?.initialized()?;
        progress.send_replace(ReceiverStatus::Applying {
            path: path.clone(),
            cursor: cursor.clone(),
            source_barrier: None,
        });
        let outcome = client
            .poll(cursor.source_sequence(), options.poll_byte_limit, &mut Sink { worker })
            .await;
        let summary = match outcome {
            Ok(summary) => summary,
            Err(error) => {
                // Discard poisoned framing before reconnect. FIFO drain is
                // essential: a sink deadline may have expired while its durable
                // apply was still completing on the owned worker.
                drop(client);
                if !retryable(&error) || reconnects >= options.max_reconnects {
                    // run_receiver closes/joins the owner before reporting this
                    // original protocol/apply error; do not mask it with a later
                    // queue-closed diagnostic from an already failed worker.
                    return Err(error.into());
                }
                let (_, durable) = worker
                    .position()
                    .await
                    .map_err(|drain| storage(format!("{error}; cannot drain receiver apply: {drain}")))?
                    .initialized()?;
                if stopped(shutdown) {
                    return Err(WireError::Shutdown.into());
                }
                reconnects += 1;
                if wait_or_stop(options.reconnect_delay, shutdown).await {
                    return Err(WireError::Shutdown.into());
                }
                client = connect(options, shutdown, progress, Some(&durable), &mut reconnects).await?;
                observed_barrier = check_session_barrier(&client, observed_barrier)?;
                continue;
            }
        };
        let (path, durable) = worker.position().await?.initialized()?;
        if durable.history() != cursor.history()
            || durable.source_sequence() != summary.through
            || summary.durable_barrier < observed_barrier
        {
            return Err(storage("wire completion differs from receiver durable history/cursor"));
        }
        observed_barrier = summary.durable_barrier;
        reconnects = 0;
        let caught_up = summary.through == summary.durable_barrier;
        if !caught_up {
            progress.send_replace(ReceiverStatus::Applying {
                path,
                cursor: durable,
                source_barrier: Some(summary.durable_barrier),
            });
            continue;
        }
        progress.send_replace(ReceiverStatus::CaughtUp {
            path,
            cursor: durable.clone(),
            source_barrier: summary.durable_barrier,
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
            client = connect(options, shutdown, progress, Some(&durable), &mut reconnects).await?;
            observed_barrier = check_session_barrier(&client, observed_barrier)?;
            initial = false;
            continue;
        }
        if options.mode == ReceiverMode::InstallOnce {
            drop(client);
            return worker.close().await;
        }
        if wait_or_stop(options.poll_interval, shutdown).await {
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
    clippy::ignored_unit_patterns
)]
mod tests {
    use super::super::{
        history::load_or_create_primary_history,
        physical_source::{open_primary_session, SourceWorkLimiter},
        physical_wire::{serve_session, OpenRequest, PhysicalSource, SourceSessionInfo},
    };
    use super::*;
    use rocksdb::{Options, DB};
    use std::{future::Future, pin::Pin, sync::atomic::AtomicUsize};

    struct FaultSource {
        inner: Arc<dyn PhysicalSource>,
        corrupt: bool,
        disconnect_once: Arc<AtomicBool>,
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
            let mut bytes = self.inner.read_chunk(index, offset, length).await?;
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
        resumes: Arc<AtomicUsize>,
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
            let resumes = Arc::new(AtomicUsize::new(0));
            let observed_snapshots = snapshots.clone();
            let observed_resumes = resumes.clone();
            let work = SourceWorkLimiter::new(2).unwrap();
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
                                let disconnect_once=disconnect_once.clone();
                                let work=work.clone();
                                sessions.spawn(async move {
                                    serve_session(io,&fixture_token(),wire_limits(),stop,move |request|async move {
                                        match request {OpenRequest::Snapshot=>{snapshots.fetch_add(1,Ordering::SeqCst);},OpenRequest::Resume{..}=>{resumes.fetch_add(1,Ordering::SeqCst);}}
                                        let source=open_primary_session(db.clone(),history,parent,TransferLimits::default(),request,work).await?;
                                        if matches!(request,OpenRequest::Snapshot) {
                                            // Write AFTER the checkpoint: receiving this value proves
                                            // the continuation stream was consumed, not only file copy.
                                            tokio::task::spawn_blocking(move ||db.put(b"stream-only",b"after-checkpoint"))
                                                .await.map_err(|e|WireError::Source(e.to_string()))?
                                                .map_err(|e|WireError::Source(e.to_string()))?;
                                        }
                                        Ok(Arc::new(FaultSource{inner:source,corrupt,disconnect_once}) as Arc<dyn PhysicalSource>)
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
                resumes,
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
            sources: vec![source.address],
            token: fixture_token(),
            start,
            mode,
            wire: wire_limits(),
            poll_byte_limit: MAX_BATCH_BYTES,
            poll_interval: Duration::from_millis(10),
            reconnect_delay: Duration::from_millis(10),
            max_reconnects: 3,
        }
    }
    fn status() -> (watch::Sender<ReceiverStatus>, watch::Receiver<ReceiverStatus>) {
        watch::channel(ReceiverStatus::Connecting {
            history: None,
            after: None,
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
        let stage = ReopenedPhysicalStage::open(&target, history, TransferLimits::default()).unwrap();
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
        assert!(matches!(result, Err(ReceiverError::Wire(WireError::Authentication))));
        assert!(!target.exists());
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 0);
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nano-resync-")));
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
}
