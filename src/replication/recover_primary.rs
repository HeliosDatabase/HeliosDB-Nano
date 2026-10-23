//! Rebuilding a physical standby from its primary (spec P4.1, plan
//! `PHYSICAL_STANDBY_REPLICATION.md` §D6 "from the primary"):
//! `heliosdb-nano recover --from-primary`, Oracle's `RECOVER STANDBY
//! DATABASE FROM SERVICE`.
//!
//! # The ladder
//!
//! [`recover_from_primary`] takes the cheapest route that is safe, and says
//! which and what it moved:
//!
//! 1. **Tail.** The primary still serves the standby's next write group —
//!    from its WAL or its WAL archive (spec P3.2 §1): nothing to copy; the
//!    standby follows as `start` would.
//! 2. **Incremental fast-forward.** The primary no longer serves it, but the
//!    directory is a physical replica of the same history: both sides digest
//!    the replicated keyspace in leaves of `--leaf-bytes` ([`super::fast_forward`]),
//!    and only the leaves that differ travel, as SST files built from a
//!    checkpoint of the primary at the cut `T`. Each batch of leaves is
//!    emptied with range deletions and refilled by ingesting the files; then
//!    the stage is re-cut at `T` (a new manifest and marker, the cursor
//!    `{snapshot: T, source: T}`), so the standby resumes from `T` through
//!    the primary's WAL or archive.
//! 3. **Full copy.** The directory is no replica of this history (another
//!    history, an older epoch, damaged), `--full` asks for it, or more than
//!    `--full-copy-threshold` of the primary's bytes differ: the resumable
//!    snapshot download of `resync`, into the directory when it is empty,
//!    else beside it (`<D>.recover-full`) and swapped in with `--replace`.
//!
//! # Crash safety of the fast-forward
//!
//! A leaf replacement is two writes (the range deletion, the ingestion), so
//! the directory is between two cuts while the fast-forward runs. A durable
//! record in the node-local family (`physical::FastForwardRecord`) is
//! written before the first default-family write and deleted by the very
//! write that re-cuts the cursor; while it exists every other open of the
//! directory refuses it (`physical::PhysicalError::FastForwardInterrupted`):
//! a standby `start` stays up in `needs_recovery` naming this command. A
//! re-run finishes the job: the record names the cut, each replaced leaf
//! has a mark of its own beside it, the primary still leases that cut's
//! export for a while (else a new cut is taken and every leaf is decided
//! again), and replacing a leaf twice is harmless — its range is emptied
//! first. The stage is re-cut in the order the stage open checks it: tables
//! flushed and the store closed (so its census is of files that hold still),
//! manifest, marker, then — reopened — the cursor write that removes the
//! record. A store with no record whose sequence is not its cursor's is
//! never offered the tail: only the digest pass can tell what it holds.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use uuid::Uuid;

use super::archive_recovery::RecoverError;
use super::checkpoint_transfer::{
    check_stage_files, check_stage_limits, partial_snapshot, physical_stage_history, recut_stage_with,
    stage_census_manifest, ReceiverStaging, ReopenedPhysicalStage, StagingStart,
};
use super::fast_forward::{hash_file, local_leaf_digests, BATCH_LEAVES};
use super::physical::{FastForwardRecord, FastForwardWriter, PhysicalCursor};
use super::physical_receiver::{run_receiver, ReceiverMode, ReceiverOptions, ReceiverStart, ReceiverStatus};
use super::physical_tls::{ReplicationStream, ReplicationTransport};
use super::physical_wire::{
    now_millis, valid_leaf_bytes, Heartbeat, LeafEntry, LeafPlan, PeerRole, PhysicalClient, ReceiverIdentity,
    RefuseCode, SourceSessionInfo, TransferToken, WireError, WireLimits, WireSyncMode, LEAF_END,
};
use crate::standby_posture::EPOCH;
use crate::Config;

/// `internal-tests` builds only: `<phase>:<index>` makes `recover
/// --from-primary` SIGKILL its own process at that point of a fast-forward —
/// `after-delete:<batch>`, `after-ingest:<batch>` (batches numbered from 0),
/// `before-recut:0`, `after-manifest:0` — the crash points the spec's
/// crash-safety tests stop at, exactly instead of by timing.
pub const KILL_FAST_FORWARD_AT_ENV: &str = "HELIOSDB_NANO_TEST_KILL_FAST_FORWARD_AT";
/// `TooManySessions` / `SourceNotServing` are retried this many times, with
/// the bootstrap backoff, before the recovery gives up.
const REFUSAL_RETRIES: u32 = 5;
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);
/// A leaf file failing its SHA-256 is downloaded again this many times.
const DOWNLOAD_ATTEMPTS: usize = 2;
/// The full copy's receiver cadence (as `resync`'s defaults).
const COPY_POLL_BYTES: usize = 8 * 1024 * 1024;
const COPY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const COPY_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const COPY_MAX_RECONNECTS: u32 = 5;
const COPY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// A full copy's progress is reported at most this often.
const COPY_PROGRESS_EVERY: Duration = Duration::from_secs(5);
/// While local work runs, the fast-forward session is kept alive by a
/// heartbeat this often (bounded by a third of the frame deadline).
const KEEP_ALIVE_EVERY: Duration = Duration::from_millis(300);

/// What the data directory holds, as `recover` classified it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryState {
    /// Absent, or an empty directory: the full copy goes there.
    Fresh { empty_existing: bool },
    /// An installed physical standby, its marker naming `history`.
    Physical { history: Uuid },
}

/// What [`recover_from_primary`] does.
pub struct PrimaryRecoverOptions {
    /// The STOPPED standby's data directory (`D`).
    pub data_dir: PathBuf,
    pub layout: DirectoryState,
    /// The node's configuration: the storage options every open of the
    /// standby's store uses, and its `[replication.archive]` directory.
    pub config: Config,
    /// `--primary-host` as given, and its addresses.
    pub primary_host: String,
    pub sources: Vec<SocketAddr>,
    pub transport: ReplicationTransport,
    pub token: TransferToken,
    pub wire: WireLimits,
    /// `--full`: straight to the full copy.
    pub full: bool,
    /// `--no-full-copy`: never fall through to the full copy.
    pub no_full_copy: bool,
    /// `--full-copy-threshold`: the percentage (1..=100) of the primary's
    /// leaf bytes that may differ before a full copy is cheaper.
    pub full_copy_threshold: u8,
    /// `--leaf-bytes`.
    pub leaf_bytes: u32,
    /// `--replace`: a full copy may replace an occupied data directory (the
    /// old one is kept beside it).
    pub replace: bool,
    /// `--stage-dir`: where leaf files wait to be ingested (outside `D`).
    pub stage_dir: PathBuf,
}

impl PrimaryRecoverOptions {
    fn validate(&self) -> Result<(), RecoverError> {
        if !valid_leaf_bytes(self.leaf_bytes) {
            return Err(refused(format!(
                "--leaf-bytes {} is outside 64KiB..=256MiB",
                self.leaf_bytes
            )));
        }
        if !(1..=100).contains(&self.full_copy_threshold) {
            return Err(refused("--full-copy-threshold is a percentage, 1..=100"));
        }
        if self.sources.is_empty() {
            return Err(refused("--primary-host resolved to no address"));
        }
        // Both canonical, so neither a symlink nor a `..` hides a stage
        // inside the data directory.
        let data = canonical_path(&self.data_dir)?;
        let stage = canonical_path(&self.stage_dir)?;
        if stage == data || stage.starts_with(&data) {
            return Err(refused(format!(
                "--stage-dir {} is inside the data directory {}: leaf files are staged outside it",
                self.stage_dir.display(),
                self.data_dir.display()
            )));
        }
        Ok(())
    }
}

/// The route a recovery took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverRoute {
    /// The primary still serves the standby's next group: nothing copied.
    Tail,
    /// Only the differing leaves were shipped.
    FastForward,
    /// A full copy of the primary was installed.
    FullCopy,
}

impl RecoverRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tail => "tail",
            Self::FastForward => "fast-forward",
            Self::FullCopy => "full copy",
        }
    }
}

/// What a recovery did.
#[derive(Clone, Debug)]
pub struct PrimaryRecoverReport {
    pub route: RecoverRoute,
    /// The dataset history the standby follows now.
    pub history: Uuid,
    /// The standby's source sequence before (0 for a fresh directory).
    pub from: u64,
    /// Its source sequence now.
    pub to: u64,
    /// The fast-forward's plan: its leaves, those that differed (excluding
    /// those an interrupted run had replaced already), the primary's bytes,
    /// and the bytes of the differing leaves (shipped). A full copy reports
    /// the copy's bytes as both totals.
    pub leaves_total: u64,
    pub leaves_differing: u64,
    pub bytes_total: u64,
    pub bytes_shipped: u64,
    /// Of the differing leaves, those that held no key on the primary (only
    /// deleted, nothing shipped). Kept for callers; not printed.
    pub leaves_deleted_only: u64,
    pub duration: Duration,
    /// Where an occupied directory went when a full copy replaced it.
    pub replaced: Option<PathBuf>,
}

fn refused(reason: impl Into<String>) -> RecoverError {
    RecoverError::Refused(reason.into())
}

fn failed(reached: u64, reason: impl Into<String>) -> RecoverError {
    RecoverError::Failed {
        reached,
        reason: reason.into(),
    }
}

/// `path` made absolute and canonical: its deepest existing ancestor
/// resolved by the filesystem (symlinks and `..` included), the rest — which
/// does not exist yet, so holds no symlink — appended. A `..` in that rest
/// cannot be resolved and is refused. The recovery works on these paths
/// only (the stage checks refuse `..` and symlinks in the paths they are
/// given).
pub fn canonical_path(path: &Path) -> Result<PathBuf, RecoverError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| refused(format!("cannot resolve {}: {error}", path.display())))?
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for part in rest.iter().rev() {
                    canonical.push(part);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(refused(format!(
                        "{}: a `..` after a directory that does not exist cannot be resolved",
                        path.display()
                    )));
                };
                rest.push(name.to_os_string());
                existing = parent;
            }
            Err(error) => return Err(refused(format!("cannot resolve {}: {error}", path.display()))),
        }
    }
}

/// `31897` as `31 897`.
pub fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(digit);
    }
    grouped
}

/// A byte count as the progress lines print it: `1 017 MiB`, `44 KiB`,
/// `12 B` (rounded to the nearest unit).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{} MiB", format_count((bytes + MIB / 2) / MIB))
    } else if bytes >= KIB {
        format!("{} KiB", format_count((bytes + KIB / 2) / KIB))
    } else {
        format!("{bytes} B")
    }
}

/// `part` of `whole` as a percentage with one decimal.
pub fn format_percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// `<D>.<suffix>` beside the data directory.
fn sibling(data_dir: &Path, suffix: &str) -> Result<PathBuf, RecoverError> {
    let absolute = canonical_path(data_dir)?;
    let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return Err(refused(format!(
            "{} has no parent directory to place {suffix} beside it",
            data_dir.display()
        )));
    };
    let mut sibling = name.to_os_string();
    sibling.push(".");
    sibling.push(suffix);
    Ok(parent.join(sibling))
}

/// Where a full copy beside an occupied data directory goes.
pub fn recover_full_path(data_dir: &Path) -> Result<PathBuf, RecoverError> {
    sibling(data_dir, "recover-full")
}

/// Stop at a crash point of the fast-forward (`internal-tests` only).
fn kill_hook(phase: &str, index: u64) {
    #[cfg(all(feature = "internal-tests", unix))]
    kill_fast_forward_hook(phase, index);
    #[cfg(not(all(feature = "internal-tests", unix)))]
    let _ = (phase, index);
}

/// `internal-tests` builds only ([`KILL_FAST_FORWARD_AT_ENV`]): SIGKILL this
/// process at the configured crash point.
#[cfg(all(feature = "internal-tests", unix))]
fn kill_fast_forward_hook(phase: &str, index: u64) {
    static KILL_AT: std::sync::OnceLock<Option<(String, u64)>> = std::sync::OnceLock::new();
    let kill_at = KILL_AT.get_or_init(|| {
        let value = std::env::var(KILL_FAST_FORWARD_AT_ENV).ok()?;
        let (phase, index) = value.trim().split_once(':')?;
        Some((phase.to_string(), index.trim().parse().ok()?))
    });
    if kill_at
        .as_ref()
        .is_some_and(|(at, at_index)| at == phase && *at_index == index)
    {
        tracing::warn!("{KILL_FAST_FORWARD_AT_ENV}: killing this process at {phase}:{index}");
        let _ = std::io::stdout().flush();
        // SAFETY: kill(2) of this very process with a standard signal; no
        // pointers, no preconditions.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    }
}

/// Run blocking storage work off the async executor; its failure, or the
/// worker's, as text.
async fn run_blocking<T, F>(job: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| format!("the recovery's storage worker failed: {error}"))?
}

/// [`run_blocking`], a failure refusing the recovery (nothing was applied).
async fn blocking<T, F>(job: F) -> Result<T, RecoverError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    run_blocking(job).await.map_err(RecoverError::Refused)
}

/// Close (drop) the standby's store on a blocking thread.
async fn release(writer: Arc<FastForwardWriter>) {
    let _ = tokio::task::spawn_blocking(move || drop(writer)).await;
}

/// The primary's answer to the probe (step 3).
enum Probe {
    /// It still serves the standby's next group.
    Tail,
    /// Its WAL and archive no longer hold it: fast-forward. The refusal.
    Behind(String),
    /// Another dataset or epoch: only a full copy. The refusal.
    Mismatch(String),
}

/// A fast-forward session opened, or the primary's reason it cannot be one.
enum Opened {
    Session {
        client: PhysicalClient<ReplicationStream>,
        snapshot_id: Uuid,
        cut: u64,
    },
    Mismatch(String),
}

/// How a fast-forward ended, short of an error.
enum Routed {
    Done(PrimaryRecoverReport),
    /// Fall through to a full copy, for this reason.
    FullCopy(String),
}

/// The fast-forward's tallies, for the report.
#[derive(Default)]
struct Tally {
    leaves_total: u64,
    leaves_differing: u64,
    bytes_total: u64,
    bytes_shipped: u64,
    leaves_deleted_only: u64,
}

/// Where a full copy goes.
#[derive(Clone, Copy)]
enum CopyInto {
    /// The data directory itself, absent or empty.
    Fresh { empty_existing: bool },
    /// Beside the occupied data directory, swapped in with `--replace`.
    Beside,
}

struct Ladder<'a> {
    options: &'a PrimaryRecoverOptions,
    /// `options.data_dir` and `options.stage_dir`, canonical
    /// ([`canonical_path`]): every step works on these.
    data_dir: PathBuf,
    stage_dir: PathBuf,
    identity: ReceiverIdentity,
    /// Never sent: held so the sessions' shutdown watch stays open.
    _stop: watch::Sender<bool>,
    shutdown: watch::Receiver<bool>,
    started: Instant,
}

/// Spec P4.1 §2: recover the STOPPED standby `options.data_dir` from its
/// primary by the cheapest safe route (see the module documentation).
/// `progress` receives the `recover: …` lines. Storage work runs on blocking
/// threads; the primary is reached on the replication port, over TLS as
/// `options.transport` says, with the ladder's own identity (an offline
/// receiver, role `Resync`: listed as bootstrapping, never counted for
/// synchronous commits).
pub async fn recover_from_primary(
    options: &PrimaryRecoverOptions,
    progress: &mut dyn FnMut(&str),
) -> Result<PrimaryRecoverReport, RecoverError> {
    options.validate()?;
    let (stop, shutdown) = watch::channel(false);
    let ladder = Ladder {
        options,
        data_dir: canonical_path(&options.data_dir)?,
        stage_dir: canonical_path(&options.stage_dir)?,
        identity: ReceiverIdentity {
            node_id: Uuid::new_v4(),
            role: PeerRole::Resync,
            epoch: EPOCH,
            pg_port: 0,
            sync_mode: WireSyncMode::Async,
        },
        _stop: stop,
        shutdown,
        started: Instant::now(),
    };
    match options.layout {
        DirectoryState::Fresh { empty_existing } => {
            progress(&format!(
                "recover: {} holds no standby yet: the primary's snapshot is installed there",
                ladder.data_dir.display()
            ));
            ladder
                .full_copy(CopyInto::Fresh { empty_existing }, 0, None, progress)
                .await
        }
        DirectoryState::Physical { history } => ladder.physical(history, progress).await,
    }
}

impl Ladder<'_> {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Steps 2-5 for an installed physical standby of `history`.
    async fn physical(
        &self,
        history: Uuid,
        progress: &mut dyn FnMut(&str),
    ) -> Result<PrimaryRecoverReport, RecoverError> {
        let path = self.data_dir.clone();
        let config = self.options.config.clone();
        let opened = run_blocking(move || {
            check_stage_files(&path).map_err(|error| error.to_string())?;
            FastForwardWriter::open(&path, &config, history).map_err(|error| error.to_string())
        })
        .await;
        let writer = match opened {
            Ok(writer) => Arc::new(writer),
            Err(error) => {
                let reason = format!(
                    "{} cannot be opened as a physical standby of {history} ({error})",
                    self.data_dir().display()
                );
                return self.full_copy(CopyInto::Beside, 0, Some(reason), progress).await;
            }
        };
        let from = writer.cursor().source_sequence();
        let record_writer = Arc::clone(&writer);
        let record = run_blocking(move || record_writer.record().map_err(|error| error.to_string())).await;
        if self.options.full {
            release(writer).await;
            return self
                .full_copy(
                    CopyInto::Beside,
                    from,
                    Some("--full asks for one".to_string()),
                    progress,
                )
                .await;
        }
        let record = match record {
            Ok(Some(record)) if record.history != history => {
                release(writer).await;
                return Err(refused(format!(
                    "{}'s fast-forward record follows the history {}, its marker {history}",
                    self.data_dir().display(),
                    record.history
                )));
            }
            Ok(Some(record)) => {
                progress(&format!(
                    "recover: {} carries an interrupted fast-forward from sequence {} to {}: finishing it",
                    self.data_dir().display(),
                    format_count(record.from),
                    format_count(record.to)
                ));
                Some(record)
            }
            Ok(None) if !writer.accounted() => {
                // No record, yet the store's sequence is not the cursor's:
                // the default family was written past what the cursor
                // describes, so a tail from the cursor would apply onto
                // unknown data (and the standby refuses to start anyway).
                // Only the digest pass can tell what the store holds.
                progress(&format!(
                    "recover: {}'s store is at its sequence {}, not the {} its cursor accounts for (it was written \
                     outside physical replication): the primary is not asked for a tail; every leaf is decided \
                     against the primary's",
                    self.data_dir().display(),
                    format_count(writer.db().latest_sequence_number()),
                    format_count(writer.cursor().local_sequence())
                ));
                None
            }
            Ok(None) => {
                match self.probe(history, from).await {
                    Err(error) => {
                        release(writer).await;
                        return Err(error);
                    }
                    Ok(Probe::Tail) => {
                        release(writer).await;
                        progress(&format!(
                            "recover: the primary still serves sequence {}… (from its WAL or archive): nothing to \
                             copy",
                            format_count(from.saturating_add(1))
                        ));
                        return Ok(self.report(RecoverRoute::Tail, history, from, from, Tally::default(), None));
                    }
                    Ok(Probe::Mismatch(reason)) => {
                        release(writer).await;
                        return self.full_copy(CopyInto::Beside, from, Some(reason), progress).await;
                    }
                    Ok(Probe::Behind(reason)) => progress(&format!(
                        "recover: standby {history} at sequence {} is behind the primary: {reason}",
                        format_count(from)
                    )),
                }
                None
            }
            Err(reason) => {
                // A record that cannot be read still means a fast-forward
                // was under way: finish it against a new cut.
                progress(&format!(
                    "recover: {} carries an unreadable fast-forward record ({reason}): every leaf is decided again",
                    self.data_dir().display()
                ));
                None
            }
        };
        match self.fast_forward(writer, history, from, record, progress).await? {
            Routed::Done(report) => Ok(report),
            Routed::FullCopy(reason) => self.full_copy(CopyInto::Beside, from, Some(reason), progress).await,
        }
    }

    fn report(
        &self,
        route: RecoverRoute,
        history: Uuid,
        from: u64,
        to: u64,
        tally: Tally,
        replaced: Option<PathBuf>,
    ) -> PrimaryRecoverReport {
        PrimaryRecoverReport {
            route,
            history,
            from,
            to,
            leaves_total: tally.leaves_total,
            leaves_differing: tally.leaves_differing,
            bytes_total: tally.bytes_total,
            bytes_shipped: tally.bytes_shipped,
            leaves_deleted_only: tally.leaves_deleted_only,
            duration: self.started.elapsed(),
            replaced,
        }
    }

    /// Why the primary could not be used, naming it.
    fn unusable(&self, error: &WireError) -> RecoverError {
        refused(format!(
            "the primary at {} cannot serve this recovery: {error}",
            self.options.primary_host
        ))
    }

    async fn backoff(&self, attempt: u32) {
        let delay = RETRY_BACKOFF
            .saturating_mul(2_u32.saturating_pow(attempt.saturating_sub(1)))
            .min(MAX_RETRY_BACKOFF);
        tokio::time::sleep(delay).await;
    }

    /// An authenticated session with the primary announcing `history` and
    /// `cursor`: the first address that answers; a refusal is final.
    async fn connect(
        &self,
        history: Option<Uuid>,
        cursor: Option<u64>,
    ) -> Result<PhysicalClient<ReplicationStream>, WireError> {
        let mut last = None;
        for source in &self.options.sources {
            match PhysicalClient::connect_replication(
                *source,
                &self.options.transport,
                &self.options.token,
                self.identity.hello(history, cursor),
                self.options.wire,
                self.shutdown.clone(),
            )
            .await
            {
                Ok(client) => return Ok(client),
                Err(error @ WireError::Refused(_)) => return Err(error),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| WireError::Protocol("no address of the primary".into())))
    }

    /// Whether a refusal says "come back later"; one more attempt is then
    /// made after the backoff, up to [`REFUSAL_RETRIES`].
    async fn retry_later(&self, error: &WireError, attempt: &mut u32) -> bool {
        let later = matches!(error, WireError::Refused(refusal) if refusal.code.retryable());
        if later && *attempt < REFUSAL_RETRIES {
            *attempt += 1;
            self.backoff(*attempt).await;
            return true;
        }
        false
    }

    /// Step 3: does the primary still serve the standby's next group?
    async fn probe(&self, history: Uuid, after: u64) -> Result<Probe, RecoverError> {
        let mut attempt = 0;
        loop {
            let outcome = async {
                let mut client = self.connect(Some(history), Some(after)).await?;
                client.resume(history, after).await?;
                Ok::<_, WireError>(client)
            }
            .await;
            let error = match outcome {
                Ok(client) => {
                    let _ = client.close().await;
                    return Ok(Probe::Tail);
                }
                Err(error) => error,
            };
            if let WireError::Refused(refusal) = &error {
                match &refusal.code {
                    RefuseCode::WalPruned { .. } | RefuseCode::Gap { .. } => {
                        return Ok(Probe::Behind(refusal.to_string()))
                    }
                    RefuseCode::HistoryMismatch | RefuseCode::EpochStale { .. } => {
                        return Ok(Probe::Mismatch(refusal.to_string()))
                    }
                    _ => {}
                }
            }
            if !self.retry_later(&error, &mut attempt).await {
                return Err(self.unusable(&error));
            }
        }
    }

    /// The primary's dataset history, from a fresh session's `HelloOk`.
    async fn identify(&self) -> Result<Uuid, RecoverError> {
        let mut attempt = 0;
        loop {
            match self.connect(None, None).await {
                Ok(client) => {
                    let history = client.hello_ok().history;
                    let _ = client.close().await;
                    return Ok(history);
                }
                Err(error) => {
                    if !self.retry_later(&error, &mut attempt).await {
                        return Err(self.unusable(&error));
                    }
                }
            }
        }
    }

    /// Open the fast-forward session (step 4), preferring the export an
    /// interrupted run used.
    async fn open_fast_forward(&self, history: Uuid, after: u64, prefer: Option<Uuid>) -> Result<Opened, RecoverError> {
        let mut attempt = 0;
        loop {
            let outcome = async {
                let mut client = self.connect(Some(history), Some(after)).await?;
                let info = client
                    .open_fast_forward(history, after, self.options.leaf_bytes, prefer)
                    .await?;
                Ok::<_, WireError>((client, info))
            }
            .await;
            let error = match outcome {
                Ok((
                    client,
                    SourceSessionInfo::FastForward {
                        snapshot_id, sequence, ..
                    },
                )) => {
                    return Ok(Opened::Session {
                        client,
                        snapshot_id,
                        cut: sequence,
                    })
                }
                Ok(_) => {
                    return Err(refused(
                        "the primary opened another kind of session than a fast-forward",
                    ))
                }
                Err(error) => error,
            };
            if let WireError::Refused(refusal) = &error {
                if matches!(
                    refusal.code,
                    RefuseCode::HistoryMismatch | RefuseCode::EpochStale { .. }
                ) {
                    return Ok(Opened::Mismatch(refusal.to_string()));
                }
            }
            if !self.retry_later(&error, &mut attempt).await {
                return Err(self.unusable(&error));
            }
        }
    }

    /// Run `job` on a blocking thread while the fast-forward session stays
    /// alive: the primary ends a session that sends no frame for its frame
    /// deadline, and a digest pass or a batch can take longer, so a
    /// heartbeat goes out meanwhile. The job always completes before this
    /// returns — a failed heartbeat is reported after it.
    async fn keep_alive<T, F>(
        &self,
        client: &mut PhysicalClient<ReplicationStream>,
        at: u64,
        job: F,
    ) -> Result<T, RecoverError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let every = KEEP_ALIVE_EVERY
            .min(self.options.wire.frame_timeout / 3)
            .max(Duration::from_millis(1));
        let mut handle = tokio::task::spawn_blocking(job);
        let mut lost: Option<WireError> = None;
        loop {
            tokio::select! {
                joined = &mut handle => {
                    let value = joined
                        .map_err(|error| failed(at, format!("the recovery's storage worker failed: {error}")))?;
                    return match lost {
                        None => Ok(value),
                        Some(error) => Err(failed(
                            at,
                            format!(
                                "the fast-forward session with the primary was lost ({error}); the progress is \
                                 kept: re-run the command to continue"
                            ),
                        )),
                    };
                }
                () = tokio::time::sleep(every), if lost.is_none() => {
                    let heartbeat = Heartbeat {
                        applied_sequence: at,
                        durable_sequence: at,
                        received_at: now_millis(),
                    };
                    if let Err(error) = client.heartbeat(heartbeat).await {
                        lost = Some(error);
                    }
                }
            }
        }
    }

    /// The session failed mid-way: the record keeps what was done.
    fn session_lost(&self, at: u64, error: &WireError) -> RecoverError {
        failed(
            at,
            format!(
                "the fast-forward session with the primary at {} failed ({error}); the progress is kept: re-run the \
                 command to continue",
                self.options.primary_host
            ),
        )
    }

    /// Step 4 (spec P4.1 §3.3): the incremental fast-forward. Owns the
    /// store's writer and closes it whatever happens.
    async fn fast_forward(
        &self,
        writer: Arc<FastForwardWriter>,
        history: Uuid,
        from: u64,
        record: Option<FastForwardRecord>,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Routed, RecoverError> {
        let outcome = self.fast_forward_steps(&writer, history, from, record, progress).await;
        match outcome {
            Ok(Steps::Applied {
                snapshot_id,
                cut,
                tally,
            }) => {
                let closed = match self.recut(writer, history, snapshot_id, cut).await {
                    Ok(closed) => closed,
                    Err(reason) => {
                        return Err(failed(
                            from,
                            format!("re-cutting the stage at {cut} failed ({reason}); re-run the command to finish"),
                        ))
                    }
                };
                if closed.source_sequence() != cut {
                    return Err(failed(
                        from,
                        format!(
                            "the re-cut cursor is at {}, not the cut {cut}",
                            closed.source_sequence()
                        ),
                    ));
                }
                let stage = self.stage_dir.clone();
                let _ = run_blocking(move || {
                    finish_stage_dir(&stage);
                    Ok(())
                })
                .await;
                self.archive_gap_note(from, cut, progress);
                Ok(Routed::Done(self.report(
                    RecoverRoute::FastForward,
                    history,
                    from,
                    cut,
                    tally,
                    None,
                )))
            }
            Ok(Steps::FullCopy(reason)) => {
                release(writer).await;
                Ok(Routed::FullCopy(reason))
            }
            Err(error) => {
                release(writer).await;
                Err(error)
            }
        }
    }

    /// The fast-forward's last step (spec P4.1 §3.3.5), once its session is
    /// closed: re-cut the stage at the primary's `cut`, in the order the
    /// stage open checks it. (i) Everything flushed and the
    /// store CLOSED — closing waits for RocksDB's background work, so the
    /// directory holds still and one census of it is exact; the record stays
    /// in the store, which every other open keeps refusing. (ii) The census
    /// as the manifest, (iii) then the marker naming it. (iv) The store
    /// reopened exclusively, the record found again, and ONE synced write
    /// puts the cursor derived from the observed sequence and deletes the
    /// record (the store's sequence checked against it after the write).
    async fn recut(
        &self,
        writer: Arc<FastForwardWriter>,
        history: Uuid,
        snapshot_id: Uuid,
        cut: u64,
    ) -> Result<PhysicalCursor, String> {
        let path = self.data_dir.clone();
        let config = self.options.config.clone();
        let limits = self.options.wire.transfer;
        run_blocking(move || -> Result<PhysicalCursor, String> {
            let writer = Arc::try_unwrap(writer)
                .map_err(|_| "the standby's store is still shared; it cannot be closed".to_string())?;
            writer.flush().map_err(|error| error.to_string())?;
            writer.release();
            kill_hook("before-recut", 0);
            let manifest =
                stage_census_manifest(&path, history, snapshot_id, cut, &limits).map_err(|error| error.to_string())?;
            recut_stage_with(&path, &manifest, &mut || kill_hook("after-manifest", 0))
                .map_err(|error| error.to_string())?;
            let writer = FastForwardWriter::open(&path, &config, history).map_err(|error| error.to_string())?;
            match writer.record().map_err(|error| error.to_string())? {
                Some(record) if record.to == cut && record.snapshot_id == snapshot_id => {}
                Some(record) => {
                    return Err(format!(
                        "the reopened store's fast-forward record names the cut {} of {}, not {cut} of {snapshot_id}",
                        record.to, record.snapshot_id
                    ))
                }
                None => return Err("the reopened store lost its fast-forward record".to_string()),
            }
            writer.recut(cut, snapshot_id).map_err(|error| error.to_string())?;
            writer.close().map_err(|error| error.to_string())
        })
        .await
    }

    async fn fast_forward_steps(
        &self,
        writer: &Arc<FastForwardWriter>,
        history: Uuid,
        from: u64,
        record: Option<FastForwardRecord>,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Steps, RecoverError> {
        let leaf_bytes = self.options.leaf_bytes;
        let prefer = record.as_ref().map(|record| record.snapshot_id);
        let (mut client, snapshot_id, cut) = match self.open_fast_forward(history, from, prefer).await? {
            Opened::Session {
                client,
                snapshot_id,
                cut,
            } => (client, snapshot_id, cut),
            Opened::Mismatch(reason) => return Ok(Steps::FullCopy(reason)),
        };
        progress(&format!(
            "recover: fast-forward from the primary at {} (cut {}, {} leaves)",
            self.options.primary_host,
            format_count(cut),
            format_bytes(u64::from(leaf_bytes))
        ));

        // 1. The record: an interrupted run's, continued when it is this
        // cut's (with the leaves it marked replaced); else a new one,
        // written below before the first default-family write.
        let (fresh, done) = match record {
            Some(record)
                if record.snapshot_id == snapshot_id && record.leaf_bytes == leaf_bytes && record.to == cut =>
            {
                let done_writer = Arc::clone(writer);
                let done = self
                    .keep_alive(&mut client, from, move || {
                        done_writer.done_leaves(cut).map_err(|error| error.to_string())
                    })
                    .await?
                    .map_err(|reason| refused(format!("the fast-forward's leaf marks cannot be read: {reason}")))?;
                progress(&format!(
                    "recover: continuing the interrupted fast-forward: {} leaves were already replaced",
                    format_count(done.len() as u64)
                ));
                (None, done)
            }
            Some(record) => {
                progress(&format!(
                    "recover: the interrupted fast-forward's cut {} is no longer served: every leaf is decided again \
                     against the cut {}",
                    format_count(record.to),
                    format_count(cut)
                ));
                (
                    Some(FastForwardRecord::new(history, snapshot_id, from, cut, leaf_bytes)),
                    BTreeSet::new(),
                )
            }
            None => (
                Some(FastForwardRecord::new(history, snapshot_id, from, cut, leaf_bytes)),
                BTreeSet::new(),
            ),
        };

        // 2. The primary's leaves, and room for one batch of them. This run
        // applied nothing yet: a primary that cannot send its plan (one still
        // planning past the client's wait included) is retried later.
        let plan = client.fetch_leaf_digests().await.map_err(|error| {
            failed(
                from,
                format!(
                    "the primary at {} did not send its leaf plan ({error}); nothing more was applied: retry the \
                     command later",
                    self.options.primary_host
                ),
            )
        })?;
        if plan.snapshot_id != snapshot_id {
            return Err(refused(
                "the primary's leaf plan names another snapshot than its session",
            ));
        }
        self.prepare_stage_dir(leaf_bytes).await?;

        // 3. One ordered scan of the standby against the primary's boundaries.
        let starts: Vec<Vec<u8>> = plan.leaves.iter().map(|leaf| leaf.start.clone()).collect();
        let scan_writer = Arc::clone(writer);
        let local = self
            .keep_alive(&mut client, from, move || {
                local_leaf_digests(scan_writer.db(), &starts).map_err(|error| error.to_string())
            })
            .await?
            .map_err(|reason| refused(format!("the standby's digest pass failed: {reason}")))?;
        if local.len() != plan.leaves.len() {
            return Err(refused(
                "the standby's digest pass disagrees with the primary's leaf count",
            ));
        }
        let mut tally = Tally {
            leaves_total: plan.leaves.len() as u64,
            bytes_total: plan.total_bytes,
            ..Tally::default()
        };
        let mut differing = Vec::new();
        for (index, (leaf, mine)) in plan.leaves.iter().zip(&local).enumerate() {
            let index = u32::try_from(index).map_err(|_| refused("too many leaves"))?;
            if done.contains(&index) || leaf.digest == *mine {
                continue;
            }
            differing.push(index);
            tally.leaves_differing += 1;
            tally.bytes_shipped += leaf.bytes;
            if leaf.keys == 0 {
                tally.leaves_deleted_only += 1;
            }
        }
        progress(&format!(
            "recover: {} leaves, {}; {} differ ({}), {} to ship",
            format_count(tally.leaves_total),
            format_bytes(tally.bytes_total),
            format_count(tally.leaves_differing),
            format_percent(tally.bytes_shipped, tally.bytes_total),
            format_bytes(tally.bytes_shipped)
        ));
        let threshold = self.options.full_copy_threshold;
        if u128::from(tally.bytes_shipped) * 100 > u128::from(threshold) * u128::from(tally.bytes_total) {
            let estimate = format!(
                "{} of the primary's {} differ ({})",
                format_bytes(tally.bytes_shipped),
                format_bytes(tally.bytes_total),
                format_percent(tally.bytes_shipped, tally.bytes_total)
            );
            let _ = client.close().await;
            if self.options.no_full_copy {
                return Err(RecoverError::FullCopyPending(format!(
                    "a full copy is required: {estimate}, above --full-copy-threshold {threshold}%, and \
                     --no-full-copy refuses it; re-run without --no-full-copy (or with a higher \
                     --full-copy-threshold) to recover"
                )));
            }
            return Ok(Steps::FullCopy(format!(
                "{estimate}, above --full-copy-threshold {threshold}%: a full copy is cheaper"
            )));
        }

        // 4. The stage must fit the limits its re-cut manifest is checked
        // against: refused here, before anything is applied, rather than at
        // the census after. Then the record of a new cut before any
        // default-family write, then the batches.
        let limits_path = self.data_dir.clone();
        let limits = self.options.wire.transfer;
        self.keep_alive(&mut client, from, move || {
            check_stage_limits(&limits_path, &limits).map_err(|error| error.to_string())
        })
        .await?
        .map_err(RecoverError::Refused)?;
        if let Some(record) = fresh {
            self.start_record(&mut client, writer, record, from).await?;
        }
        let differing_total = differing.len() as u64;
        let mut applied = 0_u64;
        let mut shipped = 0_u64;
        for (ordinal, batch) in differing.chunks(BATCH_LEAVES).enumerate() {
            let mut files = Vec::new();
            let mut ranges = Vec::with_capacity(batch.len());
            for &index in batch {
                let leaf = leaf_at(&plan, index)?;
                if let Some(file) = self.download_leaf(&mut client, index, from).await? {
                    files.push(file);
                }
                ranges.push((leaf.start.clone(), leaf_end(&plan, index)));
                shipped += leaf.bytes;
            }
            let job_writer = Arc::clone(writer);
            let job_files = files.clone();
            let ordinal = ordinal as u64;
            self.keep_alive(&mut client, from, move || -> Result<(), String> {
                job_writer.delete_ranges(&ranges).map_err(|error| error.to_string())?;
                kill_hook("after-delete", ordinal);
                job_writer.ingest(&job_files).map_err(|error| error.to_string())?;
                kill_hook("after-ingest", ordinal);
                Ok(())
            })
            .await?
            .map_err(|reason| {
                failed(
                    from,
                    format!(
                        "applying leaves {}..{} failed ({reason}); the progress is kept: re-run the command to \
                         continue",
                        batch.first().copied().unwrap_or_default(),
                        batch.last().copied().unwrap_or_default()
                    ),
                )
            })?;
            // Each leaf's own mark: the record itself is never rewritten.
            let marks_writer = Arc::clone(writer);
            let marked = batch.to_vec();
            self.keep_alive(&mut client, from, move || {
                marks_writer.mark_done(&marked, cut).map_err(|error| error.to_string())
            })
            .await?
            .map_err(|reason| failed(from, format!("the replaced leaves could not be marked: {reason}")))?;
            blocking(move || {
                for file in files {
                    match fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(format!("{}: {error}", file.display())),
                    }
                }
                Ok(())
            })
            .await?;
            applied += batch.len() as u64;
            progress(&format!(
                "recover: leaves {}..{} applied ({} of {}, {} of {})",
                batch.first().copied().unwrap_or_default(),
                batch.last().copied().unwrap_or_default(),
                format_count(applied),
                format_count(differing_total),
                format_bytes(shipped),
                format_bytes(tally.bytes_shipped)
            ));
        }
        // 5. The re-cut follows once the session is closed ([`Self::recut`]).
        let _ = client.close().await;
        Ok(Steps::Applied {
            snapshot_id,
            cut,
            tally,
        })
    }

    /// Write the record of a new cut (synced, its leaf marks of an earlier
    /// cut cleared), keeping the session alive.
    async fn start_record(
        &self,
        client: &mut PhysicalClient<ReplicationStream>,
        writer: &Arc<FastForwardWriter>,
        record: FastForwardRecord,
        from: u64,
    ) -> Result<(), RecoverError> {
        let job_writer = Arc::clone(writer);
        self.keep_alive(client, from, move || {
            job_writer.start_record(&record).map_err(|error| error.to_string())
        })
        .await?
        .map_err(|reason| failed(from, format!("the fast-forward record could not be written: {reason}")))
    }

    /// The stage directory exists (created owner-only) and has room for one
    /// batch of leaf files, twice over.
    async fn prepare_stage_dir(&self, leaf_bytes: u32) -> Result<(), RecoverError> {
        let stage = self.stage_dir.clone();
        blocking(move || {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(&stage)
                .map_err(|error| format!("the stage directory {}: {error}", stage.display()))?;
            let need = 2 * u64::from(leaf_bytes) * BATCH_LEAVES as u64;
            let free = crate::storage::disk::free_bytes(&stage)
                .map_err(|error| format!("the free space of {} cannot be measured: {error}", stage.display()))?;
            if free < need {
                return Err(format!(
                    "the stage directory {} has {} free; one batch of {} leaves of {} needs {} (--stage-dir \
                     elsewhere, or a smaller --leaf-bytes)",
                    stage.display(),
                    format_bytes(free),
                    BATCH_LEAVES,
                    format_bytes(u64::from(leaf_bytes)),
                    format_bytes(need)
                ));
            }
            Ok(())
        })
        .await
    }

    /// Step 6: the standby's own archive now lacks what was skipped.
    fn archive_gap_note(&self, from: u64, to: u64, progress: &mut dyn FnMut(&str)) {
        let archive = match &self.options.config.replication.archive.dir {
            Some(dir) => Ok(dir.clone()),
            None => sibling(self.data_dir(), "archive"),
        };
        if let Ok(archive) = archive {
            if archive.is_dir() {
                progress(&format!(
                    "recover: the standby's own WAL archive {} lacks sequences {}..{}, which the fast-forward \
                     skipped; the primary's archive is authoritative for them, and a standby cascaded from this one \
                     is recovered with recover --from-primary against it",
                    archive.display(),
                    format_count(from.saturating_add(1)),
                    format_count(to)
                ));
            }
        }
    }

    /// Download leaf `index`'s file into the stage directory (`<i>.sst`):
    /// resumed by size from a file an interrupted run left, verified whole
    /// by SHA-256, downloaded again on a mismatch. `None` for a leaf the
    /// primary holds no key in.
    async fn download_leaf(
        &self,
        client: &mut PhysicalClient<ReplicationStream>,
        index: u32,
        from: u64,
    ) -> Result<Option<PathBuf>, RecoverError> {
        let ready = client
            .request_leaf(index)
            .await
            .map_err(|error| self.session_lost(from, &error))?;
        if ready.size == 0 {
            return Ok(None);
        }
        if ready.size > self.options.wire.transfer.max_file_bytes {
            return Err(refused(format!(
                "leaf {index}'s file is {} bytes, above the transfer bound --physical-max-file-bytes {}",
                ready.size, self.options.wire.transfer.max_file_bytes
            )));
        }
        let path = self.stage_dir.join(format!("{index}.sst"));
        let chunk = self.options.wire.transfer.max_chunk_bytes as u64;
        for _ in 0..DOWNLOAD_ATTEMPTS {
            let held_path = path.clone();
            let size = ready.size;
            let mut offset = blocking(move || held_bytes(&held_path, size)).await?;
            while offset < ready.size {
                let length = (ready.size - offset).min(chunk) as u32;
                let bytes = client
                    .read_leaf_chunk(index, offset, length)
                    .await
                    .map_err(|error| self.session_lost(from, &error))?;
                let count = bytes.len() as u64;
                let chunk_path = path.clone();
                let at = offset;
                blocking(move || append_at(&chunk_path, at, &bytes)).await?;
                offset += count;
            }
            let hashed_path = path.clone();
            let (size, sha256) = blocking(move || {
                OpenOptions::new()
                    .write(true)
                    .open(&hashed_path)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| format!("{}: {error}", hashed_path.display()))?;
                let (size, sha256, _) = hash_file(&hashed_path).map_err(|error| error.to_string())?;
                Ok((size, sha256))
            })
            .await?;
            if size == ready.size && sha256 == ready.sha256 {
                return Ok(Some(path));
            }
            let truncated = path.clone();
            blocking(move || {
                OpenOptions::new()
                    .write(true)
                    .open(&truncated)
                    .and_then(|file| file.set_len(0))
                    .map_err(|error| format!("{}: {error}", truncated.display()))
            })
            .await?;
        }
        Err(failed(
            from,
            format!("leaf {index}'s file failed its SHA-256 check twice; re-run the command to continue"),
        ))
    }

    /// Step 5 (spec P4.1 §2.5): the full copy — into the data directory when
    /// it holds nothing, else into `<D>.recover-full`, swapped in with
    /// `--replace` (the old directory is kept as `<D>.replaced-<UTC>`).
    async fn full_copy(
        &self,
        into: CopyInto,
        from: u64,
        reason: Option<String>,
        progress: &mut dyn FnMut(&str),
    ) -> Result<PrimaryRecoverReport, RecoverError> {
        if let Some(reason) = &reason {
            progress(&format!("recover: a full copy of the primary is needed: {reason}"));
        }
        if self.options.no_full_copy {
            return Err(RecoverError::FullCopyPending(format!(
                "a full copy of the primary is required{} and --no-full-copy refuses it; re-run without \
                 --no-full-copy to copy it",
                reason.map(|reason| format!(" ({reason})")).unwrap_or_default()
            )));
        }
        let history = self.identify().await?;
        let data_dir = canonical_path(self.data_dir())?;
        let target = match into {
            CopyInto::Fresh { .. } => data_dir.clone(),
            CopyInto::Beside => recover_full_path(&data_dir)?,
        };
        let adopted = match into {
            CopyInto::Fresh { .. } => None,
            CopyInto::Beside => self.completed_copy(&target, history).await?,
        };
        let (to, copied) = match adopted {
            Some(to) => {
                progress(&format!(
                    "recover: {} already holds a completed copy of the primary (history {history}, sequence {}): it \
                     is used as it is",
                    target.display(),
                    format_count(to)
                ));
                (to, 0)
            }
            None => {
                if let CopyInto::Fresh { empty_existing: true } = into {
                    let empty = data_dir.clone();
                    blocking(move || {
                        fs::remove_dir(&empty).map_err(|error| {
                            format!(
                                "the empty data directory {} cannot be replaced by the copy ({error}); give a path \
                                 that does not exist yet",
                                empty.display()
                            )
                        })
                    })
                    .await?;
                }
                let total = self.prepare_copy(&target, history, progress).await?;
                let to = self.copy(&target, history, total, progress).await?;
                (to, total)
            }
        };
        let tally = Tally {
            bytes_total: copied,
            bytes_shipped: copied,
            ..Tally::default()
        };
        if let CopyInto::Fresh { .. } = into {
            return Ok(self.report(RecoverRoute::FullCopy, history, from, to, tally, None));
        }
        let replaced = sibling(
            &data_dir,
            &format!("replaced-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ")),
        )?;
        if !self.options.replace {
            return Err(RecoverError::FullCopyPending(format!(
                "the full copy of the primary is complete at {target} (sequence {}), but {data} is occupied and \
                 --replace was not given; swap them by hand:\n  mv {data} {replaced}\n  mv {target} {data}\nor \
                 re-run with --replace, which uses the completed copy",
                format_count(to),
                target = target.display(),
                data = data_dir.display(),
                replaced = replaced.display()
            )));
        }
        let (old, new, kept) = (data_dir.clone(), target.clone(), replaced.clone());
        let old_size = blocking(move || {
            if kept.exists() {
                return Err(format!("{} exists already", kept.display()));
            }
            let size = directory_bytes(&old);
            fs::rename(&old, &kept).map_err(|error| format!("mv {} {}: {error}", old.display(), kept.display()))?;
            if let Err(error) = fs::rename(&new, &old) {
                // Never leave the data directory missing: the old one goes
                // back where it was.
                return Err(match fs::rename(&kept, &old) {
                    Ok(()) => format!(
                        "mv {} {}: {error}; the old directory was moved back to {}, and the full copy is still at {}",
                        new.display(),
                        old.display(),
                        old.display(),
                        new.display()
                    ),
                    Err(back) => format!(
                        "mv {} {}: {error}, and moving the old directory back failed too (mv {} {}: {back}): the old \
                         directory is at {}, the full copy at {}; move one of them to {} by hand",
                        new.display(),
                        old.display(),
                        kept.display(),
                        old.display(),
                        kept.display(),
                        new.display(),
                        old.display()
                    ),
                });
            }
            if let Some(parent) = old.parent() {
                fs::File::open(parent)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            Ok(size)
        })
        .await?;
        progress(&format!(
            "recover: moved the old directory {} to {} ({}); it is kept for the operator",
            data_dir.display(),
            replaced.display(),
            format_bytes(old_size)
        ));
        progress(&format!(
            "recover: moved the full copy {} to {}",
            target.display(),
            data_dir.display()
        ));
        Ok(self.report(RecoverRoute::FullCopy, history, from, to, tally, Some(replaced)))
    }

    /// The source sequence of a completed copy of `history` at `target`
    /// (a published physical stage a previous run left), `None` when there
    /// is no copy there. Anything else at that path is refused, untouched.
    async fn completed_copy(&self, target: &Path, history: Uuid) -> Result<Option<u64>, RecoverError> {
        let path = target.to_path_buf();
        let config = self.options.config.clone();
        let limits = self.options.wire.transfer;
        blocking(move || {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(format!("{}: {error}", path.display())),
                Ok(_) => {}
            }
            match physical_stage_history(&path) {
                Ok(found) if found == history => {}
                Ok(found) => {
                    return Err(format!(
                        "{} holds a copy of another dataset history ({found}, the primary's is {history}): move it \
                         aside and re-run",
                        path.display()
                    ))
                }
                Err(error) => {
                    return Err(format!(
                        "{} exists and is not a completed copy of the primary ({error}): move it aside and re-run",
                        path.display()
                    ))
                }
            }
            let stage = ReopenedPhysicalStage::open(&path, history, limits, &config)
                .map_err(|error| format!("{} cannot be opened: {error}; move it aside and re-run", path.display()))?;
            let closed = stage.close().map_err(|error| error.to_string())?;
            Ok(Some(closed.cursor.source_sequence()))
        })
        .await
    }

    /// Before the copy: the primary's manifest (preferring the snapshot an
    /// interrupted copy into `target` belongs to), its staging directory
    /// begun — so the copy resumes that same leased export — and the room it
    /// needs (`total + 10%`, less what is held already). Returns the copy's
    /// bytes.
    async fn prepare_copy(
        &self,
        target: &Path,
        history: Uuid,
        progress: &mut dyn FnMut(&str),
    ) -> Result<u64, RecoverError> {
        let inspected = target.to_path_buf();
        let prefer = blocking(move || Ok(partial_snapshot(&inspected))).await?;
        let mut attempt = 0;
        let manifest = loop {
            let outcome = async {
                let mut client = self.connect(Some(history), None).await?;
                let manifest = client.fetch_manifest_preferring(Some(history), prefer).await?;
                let _ = client.close().await;
                Ok::<_, WireError>(manifest)
            }
            .await;
            match outcome {
                Ok(manifest) => break manifest,
                Err(error) => {
                    if !self.retry_later(&error, &mut attempt).await {
                        return Err(self.unusable(&error));
                    }
                }
            }
        };
        let total = manifest.total_bytes;
        let bytes = serde_json::to_vec(&manifest).map_err(|error| refused(error.to_string()))?;
        let staged = target.to_path_buf();
        let limits = self.options.wire.transfer;
        let held = blocking(move || {
            let (stage, start) = ReceiverStaging::begin_or_resume(&staged, &bytes, history, limits)
                .map_err(|error| format!("the copy cannot be staged beside {}: {error}", staged.display()))?;
            drop(stage);
            Ok(match start {
                StagingStart::Resumed { held, .. } => held,
                StagingStart::Fresh | StagingStart::StartedOver { .. } => 0,
            })
        })
        .await?;
        let parent = target
            .parent()
            .ok_or_else(|| refused(format!("{} has no parent directory", target.display())))?
            .to_path_buf();
        let measured = parent.clone();
        let free = blocking(move || {
            crate::storage::disk::free_bytes(&measured)
                .map_err(|error| format!("the free space of {} cannot be measured: {error}", measured.display()))
        })
        .await?;
        let need = total.saturating_add(total / 10).saturating_sub(held);
        if free < need {
            return Err(refused(format!(
                "a full copy needs {} bytes free in {} (the copy's {} bytes and 10%, less {} already held); {} are \
                 available",
                format_count(need),
                parent.display(),
                format_count(total),
                format_count(held),
                format_count(free)
            )));
        }
        progress(&format!(
            "recover: copying the primary's snapshot {} ({}{}) into {}",
            manifest.snapshot_id,
            format_bytes(total),
            if held > 0 {
                format!(", {} already held", format_bytes(held))
            } else {
                String::new()
            },
            target.display()
        ));
        Ok(total)
    }

    /// Run the resumable download of `resync --once` into `target`; returns
    /// the installed copy's source sequence.
    async fn copy(
        &self,
        target: &Path,
        history: Uuid,
        total: u64,
        progress: &mut dyn FnMut(&str),
    ) -> Result<u64, RecoverError> {
        let options = ReceiverOptions {
            config: self.options.config.clone(),
            sources: self.options.sources.clone(),
            transport: self.options.transport.clone(),
            token: self.options.token.clone(),
            identity: self.identity.clone(),
            start: ReceiverStart::Snapshot {
                target: target.to_path_buf(),
                expected_history: Some(history),
            },
            mode: ReceiverMode::InstallOnce,
            wire: self.options.wire,
            poll_byte_limit: COPY_POLL_BYTES,
            poll_interval: COPY_POLL_INTERVAL,
            reconnect_delay: COPY_RECONNECT_DELAY,
            max_reconnects: COPY_MAX_RECONNECTS,
            heartbeat_interval: COPY_HEARTBEAT_INTERVAL,
            archive: None,
        };
        let (status, mut statuses) = watch::channel(ReceiverStatus::Connecting {
            history: Some(history),
            after: None,
            last_error: None,
        });
        let receiver = run_receiver(options, self.shutdown.clone(), status);
        tokio::pin!(receiver);
        let mut reported = Instant::now();
        let mut open = true;
        let closed = loop {
            tokio::select! {
                biased;
                result = &mut receiver => break result,
                changed = statuses.changed(), if open => {
                    if changed.is_err() {
                        open = false;
                        continue;
                    }
                    let current = statuses.borrow_and_update().clone();
                    if let ReceiverStatus::Downloading { received, .. } = current {
                        if reported.elapsed() >= COPY_PROGRESS_EVERY {
                            reported = Instant::now();
                            progress(&format!(
                                "recover: copied {} of {}",
                                format_bytes(received),
                                format_bytes(total)
                            ));
                        }
                    }
                }
            }
        }
        .map_err(|error| {
            refused(format!(
                "the full copy into {} stopped: {error}; re-run the command to resume it",
                target.display()
            ))
        })?;
        if closed.cursor.history() != history {
            return Err(refused("the installed copy follows another history than the primary's"));
        }
        Ok(closed.cursor.source_sequence())
    }
}

/// What the steps of a fast-forward came to.
enum Steps {
    /// Every differing leaf of `snapshot_id`'s plan replaced, the session
    /// closed; the writer is still open, the stage not yet re-cut at `cut`.
    Applied {
        snapshot_id: Uuid,
        cut: u64,
        tally: Tally,
    },
    FullCopy(String),
}

fn leaf_at(plan: &LeafPlan, index: u32) -> Result<&LeafEntry, RecoverError> {
    plan.leaves
        .get(index as usize)
        .ok_or_else(|| refused(format!("leaf {index} is past the primary's plan")))
}

/// Where leaf `index` ends: the next leaf's start, or [`LEAF_END`].
fn leaf_end(plan: &LeafPlan, index: u32) -> Vec<u8> {
    plan.leaves
        .get(index as usize + 1)
        .map_or_else(|| LEAF_END.to_vec(), |next| next.start.clone())
}

/// The bytes an interrupted download left in `path`, kept when they can be
/// a prefix of a `size`-byte file (verified by the whole file's SHA-256
/// later), else dropped.
fn held_bytes(path: &Path, size: u64) -> Result<u64, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!("{}: {error}", path.display())),
        Ok(metadata) if !metadata.is_file() => Err(format!("{} is not a regular file", path.display())),
        Ok(metadata) if metadata.len() <= size => Ok(metadata.len()),
        Ok(_) => {
            OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.set_len(0))
                .map_err(|error| format!("{}: {error}", path.display()))?;
            Ok(0)
        }
    }
}

/// Write `bytes` at `offset` of the stage file `path` (created owner-only).
fn append_at(path: &Path, offset: u64, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.write_all(bytes))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Remove the stage's leftover leaf files (`<n>.sst`) and the directory
/// itself once empty — it may be the operator's: nothing else in it is
/// touched.
fn finish_stage_dir(stage: &Path) {
    if let Ok(entries) = fs::read_dir(stage) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let leaf_file = name.to_str().is_some_and(|name| {
                name.strip_suffix(".sst")
                    .is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
            });
            if leaf_file {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let _ = fs::remove_dir(stage);
}

/// The bytes of the regular files under `path` (for the report of what a
/// replaced directory still holds).
fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |metadata| metadata.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn counts_bytes_and_percentages_read_as_the_progress_lines_print_them() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_827), "1 827");
        assert_eq!(format_count(31_897), "31 897");
        assert_eq!(format_count(1_000_000), "1 000 000");
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(44 * 1024), "44 KiB");
        assert_eq!(format_bytes(1_017 * 1024 * 1024), "1 017 MiB");
        assert_eq!(format_percent(43, 1000), "4.3%");
        assert_eq!(format_percent(1, 0), "0.0%");
    }

    /// The paths are canonical: a `..` through an existing directory resolves,
    /// one after a directory that does not exist is refused.
    #[test]
    fn the_full_copy_goes_beside_the_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        assert_eq!(
            recover_full_path(&root.path().join("standby")).unwrap(),
            canonical.join("standby.recover-full")
        );
        assert_eq!(
            recover_full_path(&root.path().join("a").join("..").join("standby")).unwrap(),
            canonical.join("standby.recover-full")
        );
        assert!(recover_full_path(&root.path().join("missing").join("..").join("standby")).is_err());
    }

    /// A download resumes from the bytes held, unless they are more than the
    /// file can have; `append_at` writes at the offset, owner-only.
    #[test]
    fn a_leaf_download_resumes_from_what_its_file_holds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("7.sst");
        assert_eq!(held_bytes(&path, 10).unwrap(), 0);
        append_at(&path, 0, b"abcd").unwrap();
        assert_eq!(held_bytes(&path, 10).unwrap(), 4);
        append_at(&path, 4, b"ef").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
        assert_eq!(held_bytes(&path, 3).unwrap(), 0, "longer than the file can be");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    fn options(data_dir: &Path, stage_dir: &Path) -> PrimaryRecoverOptions {
        PrimaryRecoverOptions {
            data_dir: data_dir.to_path_buf(),
            layout: DirectoryState::Fresh { empty_existing: false },
            config: Config::default(),
            primary_host: "127.0.0.1:1".into(),
            sources: vec!["127.0.0.1:1".parse().unwrap()],
            transport: ReplicationTransport::default(),
            token: TransferToken::new([5; 32]).unwrap(),
            wire: WireLimits::default(),
            full: false,
            no_full_copy: false,
            full_copy_threshold: 50,
            leaf_bytes: 4 * 1024 * 1024,
            replace: false,
            stage_dir: stage_dir.to_path_buf(),
        }
    }

    #[test]
    fn the_options_refuse_a_stage_inside_the_data_directory_and_bad_bounds() {
        let ok = options(Path::new("/srv/standby"), Path::new("/srv/standby.recover-stage"));
        assert!(ok.validate().is_ok());
        let inside = options(Path::new("/srv/standby"), Path::new("/srv/standby/stage"));
        assert!(inside.validate().is_err());
        let same = options(Path::new("/srv/standby"), Path::new("/srv/standby"));
        assert!(same.validate().is_err());
        // A symlink beside the data directory that points into it.
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("standby");
        fs::create_dir(&data).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&data, root.path().join("link")).unwrap();
            let linked = options(&data, &root.path().join("link").join("stage"));
            assert!(linked.validate().is_err(), "a symlink into the data directory");
        }
        let dotted = options(
            &root.path().join("a-missing-dir").join("..").join("standby"),
            Path::new("/b"),
        );
        assert!(dotted.validate().is_err(), "a `..` that cannot be resolved");
        fs::create_dir(root.path().join("a")).unwrap();
        let dotted = options(&root.path().join("a").join("..").join("standby"), Path::new("/b"));
        assert!(dotted.validate().is_ok(), "a `..` through an existing directory");
        let small = PrimaryRecoverOptions {
            leaf_bytes: 1024,
            ..options(Path::new("/a"), Path::new("/b"))
        };
        assert!(small.validate().is_err());
        let threshold = PrimaryRecoverOptions {
            full_copy_threshold: 0,
            ..options(Path::new("/a"), Path::new("/b"))
        };
        assert!(threshold.validate().is_err());
    }

    /// With nothing listening, the ladder names the primary it could not
    /// reach and changes nothing.
    #[tokio::test]
    async fn an_unreachable_primary_is_named_and_nothing_is_written() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("standby");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let options = PrimaryRecoverOptions {
            sources: vec![address],
            primary_host: address.to_string(),
            ..options(&data, &root.path().join("stage"))
        };
        let mut lines = Vec::new();
        let error = recover_from_primary(&options, &mut |line| lines.push(line.to_string()))
            .await
            .unwrap_err();
        assert!(matches!(error, RecoverError::Refused(_)), "{error}");
        assert!(error.to_string().contains(&address.to_string()), "{error}");
        assert!(!data.exists(), "a fresh directory stays absent");
    }
}
