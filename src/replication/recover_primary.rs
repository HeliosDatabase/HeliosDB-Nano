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
//!    history, damaged), `--full` asks for it, or more than
//!    `--full-copy-threshold` of the primary's bytes differ: the resumable
//!    snapshot download of `resync`, into the directory when it is empty,
//!    else beside it (`<D>.recover-full`) and swapped in with `--replace`.
//!
//! # Epochs (spec P5.1 §6)
//!
//! A standby of the same history whose primary was promoted away from it —
//! the primary answers `EpochStale` at a newer epoch — takes route 2: the
//! fast-forward re-cuts it at the new primary's checkpoint, in the new
//! primary's numbering and epoch (its `repl:epoch` arrives with the leaves),
//! and the summary says what that did to what it held of the old primary. A
//! primary that answers `SourceStale` is a superseded one: the command
//! refuses, naming the fix (point `--primary-host` at the current primary).
//!
//! # Rewinding a former primary (spec R3a, Sprinter 1c8963417a35)
//!
//! A primary's own directory — fenced by a promotion, or not (a primary a
//! forced promotion superseded) — of the SAME history as the primary, at a
//! LOWER epoch, holds the dataset's history byte for byte up to the
//! promotion point: only what diverged after it differs. It is rewound in
//! place (Oracle's `REINSTATE DATABASE`, PostgreSQL's `pg_rewind`): the
//! fast-forward's plan and the digest pass are made on the directory AS IT
//! IS, so a rewind that would ship more than `--full-copy-threshold` falls
//! through to the full copy without having changed anything; otherwise the
//! directory is converted in place into a physical stage — one synced write
//! of the node-local family (a cursor at an unknown position, the
//! fast-forward's record, the rewind's own record with a copy of the fence
//! record), then the primary-only files removed (its history and fence
//! records, RocksDB's WAL archive, the HNSW dump cache, its own leftover
//! export checkpoints), then an interim marker — and the differing leaves
//! are applied and the stage re-cut exactly as for a standby
//! ([`physical::RewindWriter`](super::physical::RewindWriter)). The
//! primary's own writes past the promotion point are replaced (said as a
//! WARNING when it was not fenced). A different history is refused, and so
//! is a source at the directory's own epoch or a lower one; `--full` (with
//! `--replace`) rebuilds it by a full copy beside it instead, and so does a
//! directory that cannot be rewound (damaged, holding foreign files). An
//! interrupted rewind is classified by its record ([`DirectoryState::Rewinding`])
//! and finished by a re-run, whichever of its steps it stopped after.
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
    check_stage_files, check_stage_limits, check_stage_limits_excluding, is_foreign_file_name, is_stage_file_name,
    partial_snapshot, physical_stage_history, recut_stage, recut_stage_with, stage_census_manifest, ReceiverStaging,
    ReopenedPhysicalStage, StagingStart, TransferLimits, PHYSICAL_MANIFEST_FILE, PHYSICAL_MANIFEST_TEMP_FILE,
    PHYSICAL_MARKER_FILE, PHYSICAL_MARKER_TEMP_FILE,
};
use super::epoch::{read_directory_epoch, read_epoch, INITIAL_EPOCH};
use super::fast_forward::{hash_file, local_leaf_digests, BATCH_LEAVES};
use super::fence::{read_fenced_record, FENCED_FILE};
use super::history::{is_record_temporary, remove_record_in, PRIMARY_HISTORY_FILE};
use super::physical::{
    directory_rewind_record, FastForwardRecord, FastForwardWriter, PhysicalCursor, RewindRecord, RewindStoreState,
    RewindWriter,
};
use super::physical_receiver::{run_receiver, ReceiverMode, ReceiverOptions, ReceiverStart, ReceiverStatus};
use super::physical_tls::{ReplicationStream, ReplicationTransport};
use super::physical_wire::{
    now_millis, valid_leaf_bytes, Heartbeat, LastGroup, LeafEntry, LeafPlan, PeerRole, PhysicalClient,
    ReceiverIdentity, RefuseCode, SourceSessionInfo, TransferToken, WireError, WireLimits, WireSyncMode, LEAF_END,
};
use crate::storage::replication_records::FencedRecord;
use crate::Config;
use std::sync::atomic::{AtomicU64, Ordering};

/// `internal-tests` builds only: `<phase>:<index>` makes `recover
/// --from-primary` SIGKILL its own process at that point of a fast-forward —
/// `after-delete:<batch>`, `after-ingest:<batch>` (batches numbered from 0),
/// `before-recut:0`, `after-manifest:0` — the crash points the spec's
/// crash-safety tests stop at, exactly instead of by timing. Spec R3a adds
/// the rewind's conversion: `rewind-after-convert:0` (the node-local
/// family's write is in, nothing of the directory's files changed yet) and
/// `rewind-after-files:0` (the primary-only files removed, the interim
/// marker not yet written).
pub const KILL_FAST_FORWARD_AT_ENV: &str = "HELIOSDB_NANO_TEST_KILL_FAST_FORWARD_AT";
/// Spec R3a: the directories a primary keeps inside its data directory that
/// a standby has none of — RocksDB's archived WAL (kept for the primary's
/// standbys while physical replication retains it) and the HNSW graph dump
/// cache (standbys rebuild their indexes from rows) — which a rewind removes.
const PRIMARY_LOCAL_DIRECTORIES: [&str; 2] = ["archive", crate::storage::index_snapshot::HNSW_SNAPSHOT_DIR];
/// Spec R3a: the lock-free ingestion engine's log directory. Removed only
/// when it is empty: entries in it are writes the store may not hold yet.
const LOCKFREE_WAL_DIRECTORY: &str = "lockfree_wal";
/// The prefix of an export checkpoint's lease directory
/// (`checkpoint_transfer::OnlineCheckpointExport`).
const EXPORT_LEASE_PREFIX: &str = "nano-checkpoint-";
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
    /// Spec P5.1 §6: a primary's directory of `history` — a fenced one (it
    /// carries `NANO-FENCED.json`: a standby was promoted over it), or one
    /// that is not fenced (a primary a forced promotion superseded, or an
    /// operator rebuilding a dead primary). Spec §11.G: `epoch` is the epoch
    /// its store holds; a source at a LOWER epoch is the superseded one, and
    /// the recovery is refused. Spec R3a (Sprinter 1c8963417a35): against a
    /// primary of the same history at a HIGHER epoch it is rewound in place
    /// (module documentation); a source at its own epoch is refused; `--full`
    /// (or a directory that cannot be rewound) rebuilds it by a full copy
    /// beside it, swapped in with `--replace` (the old directory is kept);
    /// without `--replace` the command exits 3.
    Primary { history: Uuid, fenced: bool, epoch: u64 },
    /// Spec R3a: a former primary's directory under an interrupted rewind —
    /// its store holds the rewind's record
    /// ([`physical::directory_rewind_record`](super::physical::directory_rewind_record)),
    /// whichever of the conversion's file steps were done. A re-run finishes
    /// the conversion and the fast-forward.
    Rewinding { history: Uuid },
}

/// Spec R3a: how `recover --from-primary` classifies a directory under an
/// interrupted rewind, before anything else: `Some` when its store holds the
/// rewind's record (read-only, without a lock). The caller classifies every
/// other directory as before.
pub fn rewinding_layout(data_dir: &Path) -> Result<Option<DirectoryState>, String> {
    directory_rewind_record(data_dir)
        .map(|record| {
            record.map(|record| DirectoryState::Rewinding {
                history: record.history,
            })
        })
        .map_err(|error| {
            format!(
                "the node-local state of {} cannot be read ({error}): it may hold an interrupted rewind, so it is \
                 not classified",
                data_dir.display()
            )
        })
}

/// `internal-tests` builds only: `before-convert` makes a rewind stop, with an
/// error, after it opened the former primary's store and before its
/// conversion — nothing of the directory changed.
pub const FAIL_REWIND_ENV: &str = "HELIOSDB_NANO_TEST_FAIL_REWIND_AT";

/// Spec R3b (Sprinter bbba90ddfc61), review item 5: whether a rewind of
/// `data_dir` that stopped left the directory exactly the fenced primary's it
/// was — so its store can be reopened, fenced, and served again — or changed
/// it (then only `recover --from-primary` finishes it). Changed when ANY step
/// of the conversion shows: the rewind's record, the node-local family (the
/// conversion creates it first, before its synced write), the stage's marker,
/// or a primary record already removed. A directory that cannot be read is
/// changed (fail closed: it is never reopened as a primary on a guess).
pub fn rewind_left_unchanged(data_dir: &Path) -> bool {
    if !matches!(rewinding_layout(data_dir), Ok(None)) {
        return false;
    }
    let carries = |name: &str| fs::symlink_metadata(data_dir.join(name)).is_ok();
    if !carries(FENCED_FILE) || !carries(PRIMARY_HISTORY_FILE) || carries(PHYSICAL_MARKER_FILE) {
        return false;
    }
    match rocksdb::DB::list_cf(&rocksdb::Options::default(), data_dir) {
        Ok(families) => !families.iter().any(|family| family == super::physical::LOCAL_CF),
        Err(_) => false,
    }
}

/// Spec R3b (Sprinter bbba90ddfc61), review item 1: whether the directory
/// of the RUNNING primary `data_dir` could be rewound in place once a
/// switchover releases its store — the census its rewind takes first
/// ([`rewind_census`]), read-only: nothing changes. `Ok` says what the
/// conversion would remove and keep; `Err` why it could not take the
/// directory (an entry nobody accounts for, a symbolic link, a non-empty
/// lock-free WAL, a store file hard-linked from outside this node's own
/// export checkpoints — a hard-link backup). `export_dir`: the node's
/// `[replication.physical] export_dir`, `None` for the default beside the
/// data directory. A `SWITCHOVER` is refused before its target is asked when
/// this fails: past the fence the demotion rewinds the directory and never
/// full-copies it in-process.
pub fn rewind_census_check(data_dir: &Path, export_dir: Option<&Path>) -> Result<String, String> {
    let export_dir = match export_dir {
        Some(dir) => dir.to_path_buf(),
        None => sibling(data_dir, "physical-export").map_err(|error| error.to_string())?,
    };
    let census = rewind_census(data_dir, &export_dir)?;
    let mut detail = format!(
        "its directory can be rewound in place once demoted (removed: {})",
        if census.drop.is_empty() {
            "nothing but its history and fence records".to_string()
        } else {
            census.drop.join(", ")
        }
    );
    if !census.stale_exports.is_empty() {
        detail.push_str(&format!(
            "; {} of its own export checkpoint(s) go too",
            census.stale_exports.len()
        ));
    }
    if !census.kept.is_empty() {
        detail.push_str(&format!("; kept beside the store: {}", census.kept.join(", ")));
    }
    Ok(detail)
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
    /// Spec R3a: a former primary's directory was converted in place and
    /// fast-forwarded (only the differing leaves were shipped).
    Rewind,
    /// A full copy of the primary was installed.
    FullCopy,
}

impl RecoverRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tail => "tail",
            Self::FastForward => "fast-forward",
            Self::Rewind => "rewind",
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
    /// Sprinter 6146b39a5beb: the completed copy a previous run left beside
    /// the directory, when this run adopted it (the primary still served its
    /// next write group) instead of copying: nothing was copied in this run.
    pub adopted: Option<PathBuf>,
    /// Spec R3a: what a rewind converted — the former primary's epoch, its
    /// own last sequence, the fence it carried ([`RecoverRoute::Rewind`]).
    pub rewind: Option<RewindRecord>,
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
    /// The ladder's receiver identity; its epoch is [`Self::epoch`]'s.
    identity: ReceiverIdentity,
    /// Spec P5.1 §6: the epoch the ladder's `Hello`s announce — the
    /// standby's store's, and once the primary said it is at a newer epoch
    /// of the same history (`EpochStale`), that one: the fast-forward re-cuts
    /// the standby at the primary's checkpoint, `repl:epoch` included, so the
    /// standby IS at the primary's epoch once it is done.
    epoch: AtomicU64,
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
    progress: &mut (dyn FnMut(&str) + Send),
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
            epoch: INITIAL_EPOCH,
            pg_port: 0,
            sync_mode: WireSyncMode::Async,
        },
        // A fresh directory holds no epoch: its snapshot `Hello`s carry no
        // cursor, so the primary compares none. A standby's is read when its
        // store is opened.
        epoch: AtomicU64::new(INITIAL_EPOCH),
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
        // The ladder's futures are large: boxed, not inlined into this one.
        DirectoryState::Physical { history } => Box::pin(ladder.physical(history, None, progress)).await,
        DirectoryState::Rewinding { history } => Box::pin(ladder.continue_rewind(history, progress)).await,
        DirectoryState::Primary { history, fenced, epoch } => {
            // The primary must serve the same dataset: a primary's directory
            // of another history is never replaced, nor rewound, by this
            // command (spec P5.1 §6; spec R3a keeps it so — Sprinter
            // 1c8963417a35: a different history is refused, not copied over).
            let (primary, source_epoch) = ladder.identify_source().await?;
            if primary != history {
                return Err(refused(format!(
                    "{} is a primary's directory of history {history}; the primary at {} serves {primary}: a \
                     primary's directory is rewound or replaced only by its own dataset",
                    ladder.data_dir.display(),
                    options.primary_host
                )));
            }
            // Spec P5.1 §11.G: never replace a newer primary by an older one.
            if epoch > source_epoch {
                return Err(refused(format!(
                    "{} is a primary's directory at epoch {epoch}; the primary at {} is at epoch {source_epoch}: the \
                     source is the superseded one — point --primary-host at the current primary",
                    ladder.data_dir.display(),
                    options.primary_host
                )));
            }
            let what = if fenced {
                format!(
                    "{} is a fenced primary's directory ({FENCED_FILE}): a standby was promoted over it",
                    ladder.data_dir.display()
                )
            } else {
                format!(
                    "{} is a primary's directory of this dataset ({PRIMARY_HISTORY_FILE}, not fenced)",
                    ladder.data_dir.display()
                )
            };
            if options.full {
                return ladder
                    .full_copy(
                        CopyInto::Beside,
                        0,
                        Some(format!(
                            "{what}; --full asks for a full copy: it is rebuilt as a standby of the primary at {} \
                             beside it",
                            options.primary_host
                        )),
                        progress,
                    )
                    .await;
            }
            // Spec R3a (1c8963417a35): a primary is rewound only to the
            // primary promoted over it — a HIGHER epoch. One at its own
            // epoch was not (a copy of it, its own standby, a second forced
            // promotion of the same epoch): nothing tells which of the two
            // holds the dataset, so nothing is rewound.
            if epoch == source_epoch {
                return Err(refused(format!(
                    "{what}, at epoch {epoch}; the primary at {} is at the same epoch {epoch}: it was not promoted \
                     over this primary (a primary is rewound only to the one promoted over it, at a higher epoch), so \
                     nothing is rewound. If it IS the primary this node must follow, rebuild this one by a full copy \
                     beside it: re-run with --full --replace (the old directory is kept as {}.replaced-<UTC>)",
                    options.primary_host,
                    ladder.data_dir.display()
                )));
            }
            Box::pin(ladder.rewind(history, fenced, epoch, source_epoch, &what, progress)).await
        }
    }
}

impl Ladder<'_> {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Steps 2-5 for an installed physical standby of `history` — or (spec
    /// R3a, `rewind`) a former primary's directory its rewind already
    /// converted into one: its cursor names no position, so the primary is
    /// never asked for a tail, and the route is reported as the rewind.
    async fn physical(
        &self,
        history: Uuid,
        rewind: Option<RewindRecord>,
        progress: &mut (dyn FnMut(&str) + Send),
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
        // Spec P5.1 §11.F: the probe's resume names the cursor's last group.
        let last = writer
            .cursor()
            .last_group()
            .map(|(first_sequence, digest)| LastGroup { first_sequence, digest });
        // Spec P5.1 §6: the ladder announces the standby's own epoch. Spec
        // §11.G: a record that cannot be read refuses the recovery — a store
        // that cannot say which epoch it follows must not announce one.
        let epoch_writer = Arc::clone(&writer);
        let epoch = run_blocking(move || read_epoch(epoch_writer.db()).map_err(|error| error.to_string())).await;
        let store_epoch = match epoch {
            Ok(epoch) => epoch,
            // A full copy announces no epoch (its snapshot `Hello` carries no
            // cursor): asked for, it needs none of this store's.
            Err(error) if self.options.full => {
                progress(&format!(
                    "recover: the replication epoch record of {} cannot be read ({error}); the full copy replaces it",
                    self.data_dir().display()
                ));
                INITIAL_EPOCH
            }
            Err(error) => {
                release(writer).await;
                return Err(refused(format!(
                    "the replication epoch record of {} cannot be read ({error}): its epoch is unknown, so it is not \
                     recovered in place; re-run with --full (and --replace) to rebuild it by a full copy beside it",
                    self.data_dir().display()
                )));
            }
        };
        // Never lowered: a rewind adopted the primary's (higher) epoch
        // before its converted store's was read (spec R3a).
        self.epoch.fetch_max(store_epoch, Ordering::AcqRel);
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
            // Spec R3a: a rewind's cursor is at an unknown position (the
            // conversion's write carries the record too; this is the store
            // whose record a new cut's write replaced and lost): never a
            // tail, every leaf decided against the primary's.
            Ok(None) if rewind.is_some() => {
                progress(&format!(
                    "recover: {} is a former primary under a rewind: its cursor names no position of the primary, so \
                     every leaf is decided against the primary's",
                    self.data_dir().display()
                ));
                None
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
                match self.probe(history, from, last).await {
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
        let route = if rewind.is_some() {
            RecoverRoute::Rewind
        } else {
            RecoverRoute::FastForward
        };
        match self
            .fast_forward(writer, history, from, record, route, progress)
            .await?
        {
            Routed::Done(mut report) => {
                match rewind {
                    Some(rewound) => {
                        self.rewind_note(&rewound, progress).await;
                        report.rewind = Some(rewound);
                    }
                    None => self.promotion_note(store_epoch, from, progress).await,
                }
                Ok(report)
            }
            Routed::FullCopy(reason) => {
                // Spec P5.1 §11.G: across a promotion the fast-forward's
                // full-copy reason says why the standby's position cannot be
                // continued — it counts in the old primary's numbering.
                let adopted = self.epoch.load(Ordering::Acquire);
                let reason = if adopted > store_epoch {
                    format!(
                        "{reason} (the primary was promoted to epoch {adopted}; this standby followed epoch \
                         {store_epoch} to sequence {from} in the old primary's numbering, which the new primary's own \
                         sequences do not continue)"
                    )
                } else {
                    reason
                };
                self.full_copy(CopyInto::Beside, from, Some(reason), progress).await
            }
        }
    }

    /// Spec P5.1 §6: after a fast-forward across a promotion, say what the
    /// re-cut did to what the standby held of the old primary — known from
    /// the new primary's `repl:epoch` (now the standby's): nothing lost when
    /// the old primary was fenced first; when it was not (`promote
    /// --force`), rows it committed after the promotion point are not in the
    /// new primary and are gone from this standby too.
    async fn promotion_note(&self, epoch_before: u64, from: u64, progress: &mut (dyn FnMut(&str) + Send)) {
        let path = self.data_dir.clone();
        let Ok(Some(record)) = run_blocking(move || read_directory_epoch(&path).map_err(|e| e.to_string())).await
        else {
            return;
        };
        if record.epoch <= epoch_before {
            return;
        }
        progress(&match record.old_primary_final_sequence {
            Some(final_sequence) => format!(
                "recover: the standby followed epoch {} to sequence {}; the primary was promoted to epoch {} after \
                 fencing the old primary at its sequence {final_sequence}: nothing it committed is lost",
                record.previous,
                format_count(from),
                record.epoch
            ),
            None => format!(
                "recover: the standby followed epoch {} to sequence {}; the primary was promoted to epoch {} WITHOUT \
                 fencing the old primary (--force, or `pg_promote` past an unreachable primary with \
                 promote_without_fence = \"allow\"): rows the old primary committed after the promotion are not in \
                 the new primary, and the re-cut replaced them on this standby too",
                record.previous,
                format_count(from),
                record.epoch
            ),
        });
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
            adopted: None,
            rewind: None,
        }
    }

    /// Spec R3a: after a rewind, say what it did to what the former primary
    /// held — known from the primary's `repl:epoch` (now this node's): when
    /// that primary fenced it first, everything it committed through its
    /// final sequence is in the primary; when it did not (`promote --force`,
    /// `--no-fence`, `pg_promote` past an unreachable primary), its writes
    /// after the promotion point were replaced.
    async fn rewind_note(&self, rewound: &RewindRecord, progress: &mut (dyn FnMut(&str) + Send)) {
        let path = self.data_dir.clone();
        let record = run_blocking(move || read_directory_epoch(&path).map_err(|e| e.to_string()))
            .await
            .ok()
            .flatten();
        let epoch = record.as_ref().map_or(rewound.to_epoch, |record| record.epoch);
        let fence = match &rewound.fence {
            Some(fence) => format!(
                "it was fenced by epoch {} at its sequence {}",
                fence.fenced_by_epoch, fence.final_sequence
            ),
            None => "it was NOT fenced".to_string(),
        };
        progress(&match record.and_then(|record| record.old_primary_final_sequence) {
            Some(final_sequence) => format!(
                "recover: rewound the former primary (epoch {} → {epoch}; {fence}; it was at its own sequence {}): \
                     the primary was promoted after fencing it at its sequence {final_sequence}, so nothing it \
                     committed is lost — it is all in the primary",
                rewound.from_epoch,
                format_count(rewound.primary_sequence)
            ),
            None => format!(
                "recover: WARNING: rewound the former primary (epoch {} → {epoch}; {fence}; it was at its own \
                     sequence {}): the primary was promoted WITHOUT fencing it, so the writes it committed after the \
                     promotion point are not in the primary, and the rewind replaced them here",
                rewound.from_epoch,
                format_count(rewound.primary_sequence)
            ),
        });
    }

    /// Where this node's physical source checkpointed its exports — its
    /// `[replication.physical] export_dir`, else `<D>.physical-export` (the
    /// default a primary uses).
    fn export_dir(&self) -> Result<PathBuf, RecoverError> {
        match &self.options.config.replication.physical.export_dir {
            Some(dir) => Ok(dir.clone()),
            None => sibling(self.data_dir(), "physical-export"),
        }
    }

    /// Spec R3a (Sprinter 1c8963417a35): rewind the STOPPED former primary's
    /// directory in place into a standby of the primary at `source_epoch`
    /// (module documentation). `what` names the directory for the messages.
    ///
    /// 1. The directory's census: what the conversion removes, what it keeps,
    ///    the fence record to keep a copy of; anything it cannot account for
    ///    (a foreign file, a hard link from outside its own exports) falls
    ///    through to the full copy, nothing changed.
    /// 2. The store opened exclusively as it is (damaged: the full copy).
    /// 3. The fast-forward session at an unknown position (`after = 0`), the
    ///    primary's plan, and the digest pass over the UNCONVERTED store: a
    ///    divergence above `--full-copy-threshold` falls through to the full
    ///    copy with nothing changed.
    /// 4. The conversion: the node-local family's one write
    ///    ([`RewindWriter::convert`]), the store closed, the primary-only
    ///    files removed, the interim marker written; the stage reopened by
    ///    the fast-forward's writer.
    /// 5. The differing leaves applied and the stage re-cut, as for a
    ///    standby ([`Self::fast_forward`]'s second half).
    async fn rewind(
        &self,
        history: Uuid,
        fenced: bool,
        dir_epoch: u64,
        source_epoch: u64,
        what: &str,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<PrimaryRecoverReport, RecoverError> {
        progress(&format!(
            "recover: {what}, at epoch {dir_epoch}; the primary at {} is at epoch {source_epoch}: it is REWOUND in \
             place into a standby of that primary — only what differs is shipped",
            self.options.primary_host
        ));
        if !fenced {
            progress(&format!(
                "recover: WARNING: {} was not fenced: any writes it committed after the promotion point are not in \
                 the primary and are replaced by the rewind (copy the directory away first if they matter; --full \
                 --replace rebuilds it by a full copy and keeps the old directory instead)",
                self.data_dir().display()
            ));
        }
        // 1. The census.
        let census = {
            let (dir, exports) = (self.data_dir.clone(), self.export_dir()?);
            run_blocking(move || rewind_census(&dir, &exports)).await
        };
        let census = match census {
            Ok(census) => census,
            Err(reason) => {
                return self
                    .full_copy(
                        CopyInto::Beside,
                        0,
                        Some(format!(
                            "{} cannot be rewound in place: {reason}",
                            self.data_dir().display()
                        )),
                        progress,
                    )
                    .await
            }
        };
        if !census.kept.is_empty() {
            progress(&format!(
                "recover: kept beside the store, untouched and never counted by the stage: {}",
                census.kept.join(", ")
            ));
        }
        // 2. The store, as it is.
        let opened = {
            let (dir, config) = (self.data_dir.clone(), self.options.config.clone());
            run_blocking(move || {
                let writer = RewindWriter::open(&dir, &config, history).map_err(|error| error.to_string())?;
                let state = writer.state().map_err(|error| error.to_string())?;
                Ok((writer, state))
            })
            .await
        };
        let writer = match opened {
            Ok((writer, RewindStoreState::Rewinding(_))) => {
                // Converted already (a caller that classified it as a
                // primary's): finished as an interrupted rewind.
                let _ = tokio::task::spawn_blocking(move || writer.close()).await;
                return Box::pin(self.continue_rewind(history, progress)).await;
            }
            Ok((writer, _)) => Arc::new(writer),
            Err(reason) => {
                return self
                    .full_copy(
                        CopyInto::Beside,
                        0,
                        Some(format!(
                            "the store of {} cannot be opened for a rewind ({reason})",
                            self.data_dir().display()
                        )),
                        progress,
                    )
                    .await
            }
        };
        // `internal-tests` builds only: a rewind made to stop with its store
        // opened and nothing converted (the switchover's "rewind changed
        // nothing: reopen fenced" test).
        #[cfg(feature = "internal-tests")]
        if std::env::var(FAIL_REWIND_ENV).is_ok_and(|at| at.trim() == "before-convert") {
            release_rewind(writer).await;
            return Err(refused(format!(
                "internal-tests: the rewind was made to stop before its conversion ({FAIL_REWIND_ENV}=before-convert)"
            )));
        }
        let primary_sequence = writer.latest_sequence();
        // 3. At the primary's epoch (verified higher than the store's): a
        // `Hello` at a LOWER epoch than a source's is refused, and one at a
        // HIGHER epoch fences it — never sent.
        self.adopt_epoch(source_epoch);
        match self
            .rewind_steps(writer, history, dir_epoch, primary_sequence, census, progress)
            .await?
        {
            RewindSteps::Applied {
                writer,
                snapshot_id,
                cut,
                tally,
                record,
            } => {
                let mut report = self
                    .finish_fast_forward(
                        writer,
                        history,
                        0,
                        snapshot_id,
                        cut,
                        tally,
                        RecoverRoute::Rewind,
                        progress,
                    )
                    .await?;
                self.rewind_note(&record, progress).await;
                report.rewind = Some(record);
                Ok(report)
            }
            RewindSteps::FullCopy(reason) => {
                self.full_copy(
                    CopyInto::Beside,
                    0,
                    Some(format!(
                        "{reason} (nothing of {} was changed: it is still the primary's directory)",
                        self.data_dir().display()
                    )),
                    progress,
                )
                .await
            }
        }
    }

    /// Steps 3-5 of [`Self::rewind`] on the opened store, which this owns:
    /// released on every outcome short of the conversion, which consumes it.
    async fn rewind_steps(
        &self,
        writer: Arc<RewindWriter>,
        history: Uuid,
        dir_epoch: u64,
        primary_sequence: u64,
        census: RewindCensus,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<RewindSteps, RecoverError> {
        let leaf_bytes = self.options.leaf_bytes;
        let (mut client, snapshot_id, cut, planned) = match self.rewind_plan(&writer, history, progress).await {
            Ok(RewindPlan::Go {
                client,
                snapshot_id,
                cut,
                planned,
            }) => (client, snapshot_id, cut, planned),
            Ok(RewindPlan::FullCopy(reason)) => {
                release_rewind(writer).await;
                return Ok(RewindSteps::FullCopy(reason));
            }
            Err(error) => {
                release_rewind(writer).await;
                return Err(error);
            }
        };
        // The stage's limits, as its re-cut will count it: without what the
        // conversion removes. Refused here, before anything changed.
        let limits_path = self.data_dir.clone();
        let limits = self.options.wire.transfer;
        let excluded = census.excluded.clone();
        let fits = self
            .keep_alive(&mut client, 0, move || {
                check_stage_limits_excluding(&limits_path, &limits, &excluded).map_err(|error| error.to_string())
            })
            .await
            .and_then(|fits| fits.map_err(RecoverError::Refused));
        if let Err(error) = fits {
            let _ = client.close().await;
            release_rewind(writer).await;
            return Err(error);
        }
        // 4. The conversion: from its first write on, the directory is no
        // primary's (the engine refuses a store holding the node-local
        // family in every mode but the standby's) and only this command
        // finishes it.
        let record = RewindRecord {
            format: 1,
            history,
            from_epoch: dir_epoch,
            to_epoch: self.epoch.load(Ordering::Acquire),
            started_at: unix_secs(),
            primary_sequence,
            snapshot_id,
            primary: self.options.primary_host.clone(),
            fence: census.fence.clone(),
        };
        let fast_forward = FastForwardRecord::new(history, snapshot_id, 0, cut, leaf_bytes);
        let job = {
            let (dir, config) = (self.data_dir.clone(), self.options.config.clone());
            let record = record.clone();
            move || convert_in_place(writer, &dir, &config, history, &fast_forward, &record, &census, limits)
        };
        let converted = self.keep_alive(&mut client, 0, job).await?.map_err(|reason| {
            failed(
                0,
                format!(
                    "the rewind's conversion of {} stopped ({reason}); re-run the command to finish it (every start \
                     refuses the directory meanwhile)",
                    self.data_dir().display()
                ),
            )
        })?;
        let writer = Arc::new(converted);
        progress(&format!(
            "recover: converted {} in place into a physical stage of history {history} (from epoch {dir_epoch}, at \
             its own sequence {}): the primary-only files are removed{}",
            self.data_dir().display(),
            format_count(primary_sequence),
            if record.fence.is_some() {
                "; the fence record is kept in the rewind's record"
            } else {
                ""
            }
        ));
        // 5. The leaves.
        if let Err(error) = self
            .apply_leaves(&mut client, &writer, &planned, cut, 0, progress)
            .await
        {
            release(writer).await;
            return Err(error);
        }
        let _ = client.close().await;
        Ok(RewindSteps::Applied {
            writer,
            snapshot_id,
            cut,
            tally: planned.tally,
            record,
        })
    }

    /// Step 3 of [`Self::rewind`]: the fast-forward session at an unknown
    /// position (`after = 0`), the primary's plan and the digest pass over
    /// the store AS IT IS — the full copy's reason when the primary cannot
    /// serve the fast-forward or the divergence is above the threshold.
    async fn rewind_plan(
        &self,
        writer: &Arc<RewindWriter>,
        history: Uuid,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<RewindPlan, RecoverError> {
        let (mut client, snapshot_id, cut) = match self.open_fast_forward(history, 0, None).await? {
            Opened::Session {
                client,
                snapshot_id,
                cut,
            } => (client, snapshot_id, cut),
            Opened::Mismatch(reason) => return Ok(RewindPlan::FullCopy(reason)),
        };
        progress(&format!(
            "recover: fast-forward from the primary at {} (cut {}, {} leaves)",
            self.options.primary_host,
            format_count(cut),
            format_bytes(u64::from(self.options.leaf_bytes))
        ));
        let scan = Arc::clone(writer);
        let planned = self
            .plan_leaves(
                &mut client,
                snapshot_id,
                0,
                &BTreeSet::new(),
                move |starts| local_leaf_digests(scan.db(), &starts).map_err(|error| error.to_string()),
                progress,
            )
            .await;
        let planned = match planned {
            Ok(planned) => planned,
            Err(error) => {
                let _ = client.close().await;
                return Err(error);
            }
        };
        if let Some(estimate) = self.over_threshold(&planned.tally) {
            let _ = client.close().await;
            return self.above_threshold(estimate).map(RewindPlan::FullCopy);
        }
        Ok(RewindPlan::Go {
            client,
            snapshot_id,
            cut,
            planned,
        })
    }

    /// Spec R3a: finish an interrupted rewind — the conversion's file steps
    /// the directory still lacks (the store's write is in: that is what the
    /// directory was classified by), then the fast-forward, as for a standby
    /// whose cursor names no position.
    async fn continue_rewind(
        &self,
        history: Uuid,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<PrimaryRecoverReport, RecoverError> {
        let read = {
            let dir = self.data_dir.clone();
            run_blocking(move || directory_rewind_record(&dir).map_err(|error| error.to_string())).await
        };
        let record = match read {
            Ok(Some(record)) if record.history == history => record,
            Ok(Some(record)) => {
                return Err(refused(format!(
                    "{}'s rewind record follows the history {}, not {history}",
                    self.data_dir().display(),
                    record.history
                )))
            }
            Ok(None) => {
                return Err(refused(format!(
                    "{} carries no rewind record any more: classify it again (re-run the command)",
                    self.data_dir().display()
                )))
            }
            Err(reason) => {
                return Err(refused(format!(
                    "the rewind record of {} cannot be read ({reason}): it is not finished in place; re-run with \
                     --full --replace to rebuild it by a full copy beside it",
                    self.data_dir().display()
                )))
            }
        };
        progress(&format!(
            "recover: {} carries an interrupted rewind of a former primary (history {history}, epoch {} → {}, started \
             at unix {}, at its own sequence {}): finishing it",
            self.data_dir().display(),
            record.from_epoch,
            record.to_epoch,
            record.started_at,
            format_count(record.primary_sequence)
        ));
        // The rewind goes on toward the primary it was started toward, or one
        // promoted after it — never one at a lower epoch (the store's
        // `repl:epoch` may still be the former primary's own, which such a
        // source would accept).
        let (primary, source_epoch) = self.identify_source().await?;
        if primary != history {
            return Err(refused(format!(
                "{} is under a rewind into a standby of history {history}; the primary at {} serves {primary}",
                self.data_dir().display(),
                self.options.primary_host
            )));
        }
        if source_epoch < record.to_epoch {
            return Err(refused(format!(
                "{} is under a rewind toward a primary at epoch {}; the primary at {} is at epoch {source_epoch}: \
                 point --primary-host at the current primary (the one {} named, or one promoted after it)",
                self.data_dir().display(),
                record.to_epoch,
                self.options.primary_host,
                record.primary
            )));
        }
        let finished = {
            let (dir, exports, limits) = (self.data_dir.clone(), self.export_dir()?, self.options.wire.transfer);
            let snapshot_id = record.snapshot_id;
            run_blocking(move || {
                let census = rewind_census(&dir, &exports)?;
                convert_files(&dir, history, snapshot_id, &census, &limits)
            })
            .await
        };
        if let Err(reason) = finished {
            return self
                .full_copy(
                    CopyInto::Beside,
                    0,
                    Some(format!(
                        "the interrupted rewind of {} cannot be finished in place: {reason}",
                        self.data_dir().display()
                    )),
                    progress,
                )
                .await;
        }
        self.adopt_epoch(source_epoch);
        Box::pin(self.physical(history, Some(record), progress)).await
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
        self.connect_as(self.epoch.load(Ordering::Acquire), history, cursor)
            .await
    }

    /// [`Self::connect`], announcing `epoch` (a completed copy's own, which
    /// may be another than the standby's).
    async fn connect_as(
        &self,
        epoch: u64,
        history: Option<Uuid>,
        cursor: Option<u64>,
    ) -> Result<PhysicalClient<ReplicationStream>, WireError> {
        let mut last = None;
        let identity = ReceiverIdentity {
            epoch,
            ..self.identity.clone()
        };
        for source in &self.options.sources {
            match PhysicalClient::connect_replication(
                *source,
                &self.options.transport,
                &self.options.token,
                identity.hello(history, cursor),
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

    /// Step 3: does the primary still serve the standby's next group — and
    /// (spec P5.1 §11.F) is the standby's last group `last` the primary's?
    async fn probe(&self, history: Uuid, after: u64, last: Option<LastGroup>) -> Result<Probe, RecoverError> {
        let mut attempt = 0;
        loop {
            let outcome = async {
                let mut client = self.connect(Some(history), Some(after)).await?;
                client.resume(history, after, last).await?;
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
                    // A write group too large for any frame cannot be
                    // streamed either: the standby is advanced past it like
                    // one the WAL no longer holds.
                    RefuseCode::WalPruned { .. } | RefuseCode::Gap { .. } | RefuseCode::GroupTooLarge { .. } => {
                        return Ok(Probe::Behind(refusal.to_string()))
                    }
                    // Spec P5.1 §11.F: the standby's last group is not the
                    // primary's (it followed another primary of this epoch):
                    // the fast-forward re-cuts it at the primary's checkpoint.
                    RefuseCode::Diverged { .. } => return Ok(Probe::Behind(refusal.to_string())),
                    // Spec P5.1 §6: the SAME history (the history is checked
                    // before the epoch) at a newer epoch — a standby was
                    // promoted. The fast-forward re-cuts this standby at the
                    // new primary's checkpoint (its numbering, its epoch).
                    RefuseCode::EpochStale { have, want } if have > want => {
                        self.adopt_epoch(*have);
                        return Ok(Probe::Behind(format!(
                            "the primary was promoted to epoch {have} (this standby followed epoch {want}): the \
                             standby is re-cut at the new primary's checkpoint; anything it held of the old \
                             primary past the promotion is replaced"
                        )));
                    }
                    RefuseCode::SourceStale { have, want } => return Err(self.superseded(*have, *want)),
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

    /// Spec P5.1 §6: from here on the ladder's `Hello`s announce `epoch`, the
    /// primary's (module documentation of [`Ladder::epoch`]).
    fn adopt_epoch(&self, epoch: u64) {
        self.epoch.fetch_max(epoch, Ordering::AcqRel);
    }

    /// Spec P5.1 §2: the configured primary is a superseded one.
    fn superseded(&self, have: u64, want: u64) -> RecoverError {
        refused(format!(
            "the primary at {} is a superseded primary (epoch {have} < this standby's {want}); point --primary-host \
             at the current primary",
            self.options.primary_host
        ))
    }

    /// The primary's dataset history, from a fresh session's `HelloOk`.
    async fn identify(&self) -> Result<Uuid, RecoverError> {
        Ok(self.identify_source().await?.0)
    }

    /// The primary's dataset history and epoch, from a fresh session's
    /// `HelloOk`.
    async fn identify_source(&self) -> Result<(Uuid, u64), RecoverError> {
        let mut attempt = 0;
        loop {
            match self.connect(None, None).await {
                Ok(client) => {
                    let identity = (client.hello_ok().history, client.hello_ok().epoch);
                    let _ = client.close().await;
                    return Ok(identity);
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
                    // Spec R1 §1: a source serves a fast-forward session to a
                    // receiver AT its cut too (`standby verify` compares a
                    // caught-up standby there). The ladder fast-forwards only
                    // a receiver BEHIND the cut — its record spans `from <
                    // to` — and keeps the answer it had before for one at it
                    // (a diverged or unaccounted store whose cursor is the
                    // primary's sequence): a full copy.
                    if sequence <= after {
                        let _ = client.close().await;
                        return Ok(Opened::Mismatch(format!(
                            "history mismatch: the receiver at {after} is not behind the primary's cut {sequence}, \
                             so it cannot be fast-forwarded; only a full copy rebuilds it"
                        )));
                    }
                    return Ok(Opened::Session {
                        client,
                        snapshot_id,
                        cut: sequence,
                    });
                }
                Ok(_) => {
                    return Err(refused(
                        "the primary opened another kind of session than a fast-forward",
                    ))
                }
                Err(error) => error,
            };
            if let WireError::Refused(refusal) = &error {
                match &refusal.code {
                    // Spec P5.1 §6: the same history at a newer epoch (an
                    // interrupted fast-forward toward a promoted primary skips
                    // the probe that would have said so): open the session at
                    // the primary's epoch — the re-cut makes it the standby's.
                    RefuseCode::EpochStale { have, want } if have > want => {
                        self.adopt_epoch(*have);
                        continue;
                    }
                    RefuseCode::SourceStale { have, want } => return Err(self.superseded(*have, *want)),
                    RefuseCode::HistoryMismatch | RefuseCode::EpochStale { .. } => {
                        return Ok(Opened::Mismatch(refusal.to_string()));
                    }
                    _ => {}
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
        route: RecoverRoute,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Routed, RecoverError> {
        let outcome = self.fast_forward_steps(&writer, history, from, record, progress).await;
        match outcome {
            Ok(Steps::Applied {
                snapshot_id,
                cut,
                tally,
            }) => Ok(Routed::Done(
                self.finish_fast_forward(writer, history, from, snapshot_id, cut, tally, route, progress)
                    .await?,
            )),
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

    /// Every differing leaf applied and the session closed: re-cut the stage
    /// at `cut` ([`Self::recut`]), remove the stage directory, and report.
    #[allow(clippy::too_many_arguments)]
    async fn finish_fast_forward(
        &self,
        writer: Arc<FastForwardWriter>,
        history: Uuid,
        from: u64,
        snapshot_id: Uuid,
        cut: u64,
        tally: Tally,
        route: RecoverRoute,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<PrimaryRecoverReport, RecoverError> {
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
        Ok(self.report(route, history, from, cut, tally, None))
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
        progress: &mut (dyn FnMut(&str) + Send),
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

        // 2-3. The primary's leaves, and the standby's digests against them.
        let scan_writer = Arc::clone(writer);
        let planned = self
            .plan_leaves(
                &mut client,
                snapshot_id,
                from,
                &done,
                move |starts| local_leaf_digests(scan_writer.db(), &starts).map_err(|error| error.to_string()),
                progress,
            )
            .await?;
        if let Some(estimate) = self.over_threshold(&planned.tally) {
            let _ = client.close().await;
            return self.above_threshold(estimate).map(Steps::FullCopy);
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
        self.apply_leaves(&mut client, writer, &planned, cut, from, progress)
            .await?;
        // 5. The re-cut follows once the session is closed ([`Self::recut`]).
        let _ = client.close().await;
        Ok(Steps::Applied {
            snapshot_id,
            cut,
            tally: planned.tally,
        })
    }

    /// Steps 2-3 of spec P4.1 §3.3 on an open fast-forward session: the
    /// primary's leaf plan, room for one batch of leaves in the stage
    /// directory, and ONE ordered scan of the local store (`scan`, given the
    /// plan's leaf starts, on a blocking thread while the session is kept
    /// alive) against the primary's boundaries. Leaves in `done` (replaced by
    /// an interrupted run of this cut) are not counted. This run applied
    /// nothing yet: a primary that cannot send its plan (one still planning
    /// past the client's wait included) is retried later.
    async fn plan_leaves<F>(
        &self,
        client: &mut PhysicalClient<ReplicationStream>,
        snapshot_id: Uuid,
        from: u64,
        done: &BTreeSet<u32>,
        scan: F,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Planned, RecoverError>
    where
        F: FnOnce(Vec<Vec<u8>>) -> Result<Vec<[u8; 32]>, String> + Send + 'static,
    {
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
        self.prepare_stage_dir(self.options.leaf_bytes).await?;
        let starts: Vec<Vec<u8>> = plan.leaves.iter().map(|leaf| leaf.start.clone()).collect();
        let local = self
            .keep_alive(client, from, move || scan(starts))
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
        // At debug level, where the differences are: the first differing
        // leaves by their start key (the key family tells whether a change was
        // clustered or spread over the keyspace; row ids are decimal text, so a
        // numeric range is only contiguous in key order inside one prefix band).
        if tracing::enabled!(tracing::Level::DEBUG) {
            for index in differing.iter().take(64) {
                if let Some(leaf) = plan.leaves.get(*index as usize) {
                    let start: String = String::from_utf8_lossy(&leaf.start).chars().take(48).collect();
                    tracing::debug!(
                        "recover: leaf {index} differs: starts at {start:?}, {} keys, {} on the primary",
                        leaf.keys,
                        format_bytes(leaf.bytes)
                    );
                }
            }
            if differing.len() > 64 {
                tracing::debug!("recover: … and {} more differing leaves", differing.len() - 64);
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
        Ok(Planned { plan, differing, tally })
    }

    /// The estimate of a divergence above `--full-copy-threshold`, `None`
    /// when a fast-forward is cheaper.
    fn over_threshold(&self, tally: &Tally) -> Option<String> {
        let threshold = self.options.full_copy_threshold;
        (u128::from(tally.bytes_shipped) * 100 > u128::from(threshold) * u128::from(tally.bytes_total)).then(|| {
            format!(
                "{} of the primary's {} differ ({})",
                format_bytes(tally.bytes_shipped),
                format_bytes(tally.bytes_total),
                format_percent(tally.bytes_shipped, tally.bytes_total)
            )
        })
    }

    /// What a divergence above the threshold leads to: the full copy's
    /// reason, or (`--no-full-copy`) the refusal that exits 3.
    fn above_threshold(&self, estimate: String) -> Result<String, RecoverError> {
        let threshold = self.options.full_copy_threshold;
        if self.options.no_full_copy {
            return Err(RecoverError::FullCopyPending(format!(
                "a full copy is required: {estimate}, above --full-copy-threshold {threshold}%, and \
                 --no-full-copy refuses it; re-run without --no-full-copy (or with a higher \
                 --full-copy-threshold) to recover"
            )));
        }
        Ok(format!(
            "{estimate}, above --full-copy-threshold {threshold}%: a full copy is cheaper"
        ))
    }

    /// Step 4 of spec P4.1 §3.3: the differing leaves, in batches of
    /// [`BATCH_LEAVES`] — downloaded into the stage directory, their ranges
    /// emptied in one synced write, the files ingested, the leaves marked —
    /// on the fast-forward's writer.
    async fn apply_leaves(
        &self,
        client: &mut PhysicalClient<ReplicationStream>,
        writer: &Arc<FastForwardWriter>,
        planned: &Planned,
        cut: u64,
        from: u64,
        progress: &mut (dyn FnMut(&str) + Send),
    ) -> Result<(), RecoverError> {
        let plan = &planned.plan;
        let differing_total = planned.differing.len() as u64;
        let mut applied = 0_u64;
        let mut shipped = 0_u64;
        for (ordinal, batch) in planned.differing.chunks(BATCH_LEAVES).enumerate() {
            let mut files = Vec::new();
            let mut ranges = Vec::with_capacity(batch.len());
            for &index in batch {
                let leaf = leaf_at(plan, index)?;
                if let Some(file) = self.download_leaf(client, index, from).await? {
                    files.push(file);
                }
                ranges.push((leaf.start.clone(), leaf_end(plan, index)));
                shipped += leaf.bytes;
            }
            let job_writer = Arc::clone(writer);
            let job_files = files.clone();
            let ordinal = ordinal as u64;
            self.keep_alive(client, from, move || -> Result<(), String> {
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
            self.keep_alive(client, from, move || {
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
                format_bytes(planned.tally.bytes_shipped)
            ));
        }
        Ok(())
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
    fn archive_gap_note(&self, from: u64, to: u64, progress: &mut (dyn FnMut(&str) + Send)) {
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
        progress: &mut (dyn FnMut(&str) + Send),
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
        let completed = match into {
            CopyInto::Fresh { .. } => None,
            CopyInto::Beside => self.completed_copy(&target, history).await?,
        };
        // Sprinter 6146b39a5beb: a completed copy is adopted only while the
        // primary still serves its next write group. One the primary's WAL
        // and archive have moved past is a standby nothing can continue: it
        // is removed and the copy is made again.
        let adopted = match completed {
            None => None,
            Some(copy) => match self.copy_served(history, &copy, &target).await? {
                CopyServed::Served => {
                    progress(&if self.options.full {
                        format!(
                            "recover: --full: {} already holds a completed copy of the primary (history {history}, \
                             sequence {}), which the primary still serves now (nothing pins it): it is adopted instead \
                             of copying again",
                            target.display(),
                            format_count(copy.sequence)
                        )
                    } else {
                        format!(
                            "recover: {} already holds a completed copy of the primary (history {history}, sequence \
                             {}), which the primary still serves now (nothing pins it: a primary that prunes past it \
                             before the standby starts sends the standby to recovery again): it is used as it is",
                            target.display(),
                            format_count(copy.sequence)
                        )
                    });
                    Some(copy.sequence)
                }
                CopyServed::Stale(reason) => {
                    progress(&format!(
                        "recover: the completed copy at {} (sequence {}) is older than what the primary retains \
                         ({reason}); copying again",
                        target.display(),
                        format_count(copy.sequence)
                    ));
                    let stale = target.clone();
                    blocking(move || {
                        fs::remove_dir_all(&stale).map_err(|error| {
                            format!(
                                "the stale completed copy {} cannot be removed ({error}); move it aside and re-run",
                                stale.display()
                            )
                        })
                    })
                    .await?;
                    None
                }
            },
        };
        let (to, copied) = match adopted {
            Some(to) => (to, 0),
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
        let adopted_path = adopted.map(|_| target.clone());
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
        let mut report = self.report(RecoverRoute::FullCopy, history, from, to, tally, Some(replaced));
        report.adopted = adopted_path;
        Ok(report)
    }

    /// Sprinter 6146b39a5beb: whether the primary still serves `copy`'s next
    /// write group — a resume after its cursor, announcing its own epoch and
    /// last group, as the standby it becomes would at its first `start`.
    /// Accepted: the copy can be adopted — as of now; nothing pins its tail.
    /// Refused for a reason about the copy ([`copy_refusal_is_stale`]): it is
    /// stale. Any other refusal or failure (after the usual retries) says
    /// nothing about the copy: the recovery stops and the copy is kept.
    async fn copy_served(
        &self,
        history: Uuid,
        copy: &CompletedCopy,
        target: &Path,
    ) -> Result<CopyServed, RecoverError> {
        let Some(epoch) = copy.epoch else {
            return Err(refused(format!(
                "the completed copy at {} has a replication epoch record that cannot be read: it is kept; move it \
                 aside and re-run",
                target.display()
            )));
        };
        let mut attempt = 0;
        loop {
            let outcome = async {
                #[cfg(all(feature = "internal-tests", unix))]
                if let Some(code) = hooked_copy_probe_refusal() {
                    return Err(WireError::Refused(super::physical_wire::Refusal::new(
                        code,
                        COPY_PROBE_REFUSE_ENV,
                    )));
                }
                let mut client = self.connect_as(epoch, Some(history), Some(copy.sequence)).await?;
                client.resume(history, copy.sequence, copy.last).await?;
                Ok::<_, WireError>(client)
            }
            .await;
            let error = match outcome {
                Ok(client) => {
                    let _ = client.close().await;
                    return Ok(CopyServed::Served);
                }
                Err(error) => error,
            };
            if self.retry_later(&error, &mut attempt).await {
                continue;
            }
            // Only a refusal that says the primary no longer holds what the
            // copy needs makes it stale; anything else (a busy or shutting
            // down primary, a token or version problem, a superseded
            // primary, a transport error) says nothing about the copy, which
            // may hold hours of transfer: it is kept, and the run stops.
            return match error {
                WireError::Refused(refusal) if copy_refusal_is_stale(&refusal.code) => {
                    Ok(CopyServed::Stale(refusal.to_string()))
                }
                other => Err(refused(format!(
                    "the primary at {} could not say whether it still serves the completed copy at {} ({other}); \
                     the copy is kept: re-run the command when the primary answers",
                    self.options.primary_host,
                    target.display()
                ))),
            };
        }
    }

    /// The completed copy of `history` at `target` (a published physical
    /// stage a previous run left) — its cursor's source sequence and last
    /// group, and its epoch — `None` when there is no copy there. Anything
    /// else at that path is refused, untouched.
    async fn completed_copy(&self, target: &Path, history: Uuid) -> Result<Option<CompletedCopy>, RecoverError> {
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
            let epoch = read_epoch(stage.writer().db()).ok();
            let closed = stage.close().map_err(|error| error.to_string())?;
            Ok(Some(CompletedCopy {
                sequence: closed.cursor.source_sequence(),
                last: closed
                    .cursor
                    .last_group()
                    .map(|(first_sequence, digest)| LastGroup { first_sequence, digest }),
                epoch,
            }))
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
        progress: &mut (dyn FnMut(&str) + Send),
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
        progress: &mut (dyn FnMut(&str) + Send),
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

/// A completed full copy beside the data directory, as a previous run left
/// it (sprinter 6146b39a5beb).
struct CompletedCopy {
    /// Its cursor's source sequence: where the standby it becomes resumes.
    sequence: u64,
    /// Its cursor's last applied group (`None` at its checkpoint cut).
    last: Option<LastGroup>,
    /// Its store's epoch; `None` when the record cannot be read.
    epoch: Option<u64>,
}

/// Sprinter 6146b39a5beb: whether the primary's refusal of a completed
/// copy's resume proves the copy stale — the primary no longer holds the
/// copy's next write group (`WalPruned`, `Gap`, `GroupTooLarge`), the copy's
/// last group is not the primary's (`Diverged`), the primary serves another
/// dataset (`HistoryMismatch`), or it was promoted past the copy's epoch
/// (`EpochStale` with a newer epoch). Nothing else does.
fn copy_refusal_is_stale(code: &RefuseCode) -> bool {
    match code {
        RefuseCode::WalPruned { .. }
        | RefuseCode::Gap { .. }
        | RefuseCode::GroupTooLarge { .. }
        | RefuseCode::Diverged { .. }
        | RefuseCode::HistoryMismatch => true,
        RefuseCode::EpochStale { have, want } => have > want,
        RefuseCode::AuthFailed
        | RefuseCode::TooManySessions
        | RefuseCode::SourceNotServing
        | RefuseCode::Version
        | RefuseCode::SourceStale { .. } => false,
    }
}

/// `internal-tests` builds only: `<RefuseCode name>` (`TooManySessions`,
/// `SourceNotServing`, `AuthFailed`, `Version`, `WalPruned`) makes every
/// probe of a completed copy refused with that code, without asking the
/// primary — the transient refusal a copy must survive.
#[cfg(all(feature = "internal-tests", unix))]
pub const COPY_PROBE_REFUSE_ENV: &str = "HELIOSDB_NANO_TEST_COPY_PROBE_REFUSE";

#[cfg(all(feature = "internal-tests", unix))]
fn hooked_copy_probe_refusal() -> Option<RefuseCode> {
    Some(match std::env::var(COPY_PROBE_REFUSE_ENV).ok()?.trim() {
        "TooManySessions" => RefuseCode::TooManySessions,
        "SourceNotServing" => RefuseCode::SourceNotServing,
        "AuthFailed" => RefuseCode::AuthFailed,
        "Version" => RefuseCode::Version,
        "WalPruned" => RefuseCode::WalPruned { oldest: u64::MAX },
        _ => return None,
    })
}

/// Whether the primary still serves a completed copy's next write group.
enum CopyServed {
    Served,
    /// It does not: the primary's refusal.
    Stale(String),
}

/// Steps 2-3 of a fast-forward: the primary's plan and the leaves that
/// differ from the local store, with their tallies.
struct Planned {
    plan: LeafPlan,
    differing: Vec<u32>,
    tally: Tally,
}

/// Spec R3a: what step 3 of a rewind came to.
enum RewindPlan {
    /// The session is open, the plan made, the divergence below the
    /// threshold: the conversion may go ahead.
    Go {
        client: PhysicalClient<ReplicationStream>,
        snapshot_id: Uuid,
        cut: u64,
        planned: Planned,
    },
    /// A full copy instead, for this reason (nothing was changed).
    FullCopy(String),
}

/// Spec R3a: what the steps of a rewind came to.
enum RewindSteps {
    /// Converted, every differing leaf applied, the session closed; the
    /// fast-forward's writer is still open, the stage not yet re-cut.
    Applied {
        writer: Arc<FastForwardWriter>,
        snapshot_id: Uuid,
        cut: u64,
        tally: Tally,
        record: RewindRecord,
    },
    /// A full copy instead, for this reason (nothing was changed).
    FullCopy(String),
}

/// Spec R3a: what a former primary's directory holds, as its rewind sees it
/// ([`rewind_census`]).
#[derive(Default)]
struct RewindCensus {
    /// Entries of the directory the conversion removes, primary-only state:
    /// RocksDB's WAL archive, the HNSW dump cache, an empty lock-free WAL
    /// directory, RocksDB's temporaries. (The history and fence records go
    /// through their own remover.)
    drop: Vec<String>,
    /// This stopped primary's own leftover export checkpoints — outside the
    /// directory — that hard-link files of its store: a stage holds every
    /// file of its store alone.
    stale_exports: Vec<PathBuf>,
    /// The fence record the directory carries (a copy goes into the
    /// rewind's record).
    fence: Option<FencedRecord>,
    /// Every name the re-cut's census will not count.
    excluded: Vec<String>,
    /// A user's regular files beside the store (what the engine tolerates
    /// in a data directory: `checkpoint_transfer::is_foreign_file_name`),
    /// kept where they are; a stage never counts them.
    kept: Vec<String>,
}

/// Close (drop) a rewind's store on a blocking thread.
async fn release_rewind(writer: Arc<RewindWriter>) {
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(writer) = Arc::try_unwrap(writer) {
            let _ = writer.close();
        }
    })
    .await;
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Spec R3a (Sprinter 1c8963417a35): the census of a STOPPED former
/// primary's directory `dir` before (or, finishing one, during) its rewind:
/// every entry is the store's own RocksDB file (a regular file, kept), the
/// physical stage's marker or manifest (rewritten), the history or fence
/// record with their temporaries (removed: the fence record is copied into
/// the rewind's record), or primary-only state the conversion removes
/// ([`RewindCensus::drop`]). Anything else — a foreign file, a symbolic
/// link, a non-empty lock-free WAL, a file linked from outside this node's
/// own leftover export checkpoints (found under `export_dir`) — is the
/// reason this directory is not rewound in place.
fn rewind_census(dir: &Path, export_dir: &Path) -> Result<RewindCensus, String> {
    let mut census = RewindCensus::default();
    // The store's files that have more than one link: (device, inode) →
    // (links, name).
    let mut linked: std::collections::HashMap<(u64, u64), (u64, String)> = std::collections::HashMap::new();
    let entries = fs::read_dir(dir).map_err(|error| format!("{} cannot be listed: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{} cannot be listed: {error}", dir.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| format!("{} holds an entry whose name is not UTF-8 ({name:?})", dir.display()))?;
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            // Gone since the listing (a running primary's compaction, when
            // the switchover's preflight takes this census): not counted.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{name} cannot be inspected: {error}")),
        };
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{name} is a symbolic link: a standby's stage holds no link out of its directory"
            ));
        }
        if is_stage_file_name(&name) {
            if !metadata.is_file() {
                return Err(format!("{name} is not a regular file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() > 1 {
                    linked.insert((metadata.dev(), metadata.ino()), (metadata.nlink(), name));
                }
            }
            continue;
        }
        if [
            PHYSICAL_MARKER_FILE,
            PHYSICAL_MANIFEST_FILE,
            PHYSICAL_MARKER_TEMP_FILE,
            PHYSICAL_MANIFEST_TEMP_FILE,
        ]
        .contains(&name.as_str())
        {
            if !metadata.is_file() {
                return Err(format!("{name} is not a regular file"));
            }
            census.excluded.push(name);
            continue;
        }
        if [FENCED_FILE, PRIMARY_HISTORY_FILE].contains(&name.as_str())
            || is_record_temporary(&name, FENCED_FILE)
            || is_record_temporary(&name, PRIMARY_HISTORY_FILE)
        {
            census.excluded.push(name);
            continue;
        }
        if metadata.is_dir() && PRIMARY_LOCAL_DIRECTORIES.contains(&name.as_str()) {
            census.drop.push(name.clone());
            census.excluded.push(name);
            continue;
        }
        if metadata.is_dir() && name == LOCKFREE_WAL_DIRECTORY {
            let empty = fs::read_dir(entry.path())
                .map_err(|error| format!("{name} cannot be listed: {error}"))?
                .next()
                .is_none();
            if !empty {
                return Err(format!(
                    "{name} holds the lock-free ingestion engine's log, which may hold writes the store does not: \
                     start the node once as a primary so it drains, or rebuild it by a full copy (--full --replace)"
                ));
            }
            census.drop.push(name.clone());
            census.excluded.push(name);
            continue;
        }
        // RocksDB's own temporaries (`<n>.dbtmp`, `OPTIONS-<n>.dbtmp`, by the
        // engine's grammar): a user's file that merely ends so is kept below.
        if metadata.is_file()
            && crate::storage::is_rocksdb_name(&name)
            && Path::new(&name)
                .extension()
                .is_some_and(|extension| extension == "dbtmp")
        {
            census.drop.push(name.clone());
            census.excluded.push(name);
            continue;
        }
        // A user's regular file the engine tolerates beside its store (a
        // `heliosdb-nano.toml`, a `server.key`): kept where it is, out of the
        // stage's census — a rewind never blocks on it, and never moves it
        // (a configuration may name its path).
        if metadata.is_file() && is_foreign_file_name(&name) {
            census.kept.push(name.clone());
            census.excluded.push(name);
            continue;
        }
        return Err(format!(
            "{name} is not the database's: a rewind keeps the store's own RocksDB files and a user's regular files \
             beside them, and removes only what is primary-local (its history and fence records, RocksDB's WAL \
             archive, the HNSW dump cache); move {name} out of {} and re-run, or rebuild the node by a full copy \
             (--full --replace)",
            dir.display()
        ));
    }
    if fs::symlink_metadata(dir.join(FENCED_FILE)).is_ok() {
        census.fence =
            Some(read_fenced_record(dir)?.ok_or_else(|| format!("{FENCED_FILE} vanished while it was read"))?);
    }
    if !linked.is_empty() {
        let mut found: std::collections::HashMap<(u64, u64), u64> = std::collections::HashMap::new();
        if let Ok(leases) = fs::read_dir(export_dir) {
            for lease in leases.flatten() {
                let is_lease = lease
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(EXPORT_LEASE_PREFIX));
                let is_dir = fs::symlink_metadata(lease.path()).is_ok_and(|metadata| metadata.is_dir());
                if is_lease && is_dir && count_links(&lease.path(), &linked, &mut found, 0) {
                    census.stale_exports.push(lease.path());
                }
            }
        }
        for (key, (links, name)) in &linked {
            let seen = found.get(key).copied().unwrap_or(0);
            if seen.saturating_add(1) != *links {
                return Err(format!(
                    "{name} has {links} links, and only {seen} of the others are in this node's own leftover export \
                     checkpoints under {}: something else shares the file, and a standby's stage holds every file \
                     of its store alone",
                    export_dir.display()
                ));
            }
        }
    }
    Ok(census)
}

/// Count, under the export checkpoint `dir` (a few levels deep), the links
/// of the store's linked files; whether it holds any.
fn count_links(
    dir: &Path,
    linked: &std::collections::HashMap<(u64, u64), (u64, String)>,
    found: &mut std::collections::HashMap<(u64, u64), u64>,
    depth: usize,
) -> bool {
    if depth > 4 {
        return false;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut holds = false;
    for entry in entries.flatten() {
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.is_dir() {
            holds |= count_links(&entry.path(), linked, found, depth + 1);
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let key = (metadata.dev(), metadata.ino());
            if metadata.is_file() && linked.contains_key(&key) {
                *found.entry(key).or_default() += 1;
                holds = true;
            }
        }
    }
    holds
}

/// Spec R3a: the rewind's conversion, on a blocking thread (step 4 of
/// [`Ladder::rewind`]): the node-local family's one write
/// ([`RewindWriter::convert`]), the store closed, the directory's files made
/// a stage's ([`convert_files`]), and the stage reopened by the
/// fast-forward's writer. `writer` must be the store's only handle.
#[allow(clippy::too_many_arguments)]
fn convert_in_place(
    writer: Arc<RewindWriter>,
    dir: &Path,
    config: &Config,
    history: Uuid,
    fast_forward: &FastForwardRecord,
    record: &RewindRecord,
    census: &RewindCensus,
    limits: TransferLimits,
) -> Result<FastForwardWriter, String> {
    let mut writer = Arc::try_unwrap(writer)
        .map_err(|_| "the former primary's store is still shared; it cannot be converted".to_string())?;
    writer
        .convert(fast_forward, record)
        .map_err(|error| format!("the node-local family cannot be written: {error}"))?;
    writer
        .close()
        .map_err(|error| format!("the converted store cannot be closed: {error}"))?;
    kill_hook("rewind-after-convert", 0);
    convert_files(dir, history, record.snapshot_id, census, &limits)?;
    check_stage_files(dir).map_err(|error| format!("the converted directory is no stage yet: {error}"))?;
    FastForwardWriter::open(dir, config, history)
        .map_err(|error| format!("the converted store cannot be reopened as a stage: {error}"))
}

/// Spec R3a: the conversion's file steps, each idempotent (an interrupted
/// rewind repeats them): the primary-only entries and this node's own
/// leftover export checkpoints removed, the fence record then the history
/// record removed (with their temporaries), the directory fsynced, and —
/// when the directory has no marker yet — the interim stage: the census of
/// the CLOSED store as a manifest at the unknown position (sequence 0) of
/// the export `snapshot_id`, and the marker naming it. The re-cut rewrites
/// both at the cut.
fn convert_files(
    dir: &Path,
    history: Uuid,
    snapshot_id: Uuid,
    census: &RewindCensus,
    limits: &TransferLimits,
) -> Result<(), String> {
    let gone = |result: std::io::Result<()>, what: &Path| match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{} cannot be removed: {error}", what.display())),
    };
    for name in &census.drop {
        let path = dir.join(name);
        let is_dir = fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        gone(
            if is_dir {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            },
            &path,
        )?;
    }
    for lease in &census.stale_exports {
        gone(fs::remove_dir_all(lease), lease)?;
    }
    for record in [FENCED_FILE, PRIMARY_HISTORY_FILE] {
        remove_record_in(dir, record).map_err(|error| format!("{record} cannot be removed: {error}"))?;
    }
    fs::File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("{} cannot be fsynced: {error}", dir.display()))?;
    kill_hook("rewind-after-files", 0);
    if fs::symlink_metadata(dir.join(PHYSICAL_MARKER_FILE)).is_err() {
        let manifest = stage_census_manifest(dir, history, snapshot_id, 0, limits)
            .map_err(|error| format!("the interim stage's manifest: {error}"))?;
        recut_stage(dir, &manifest).map_err(|error| format!("the interim stage's marker: {error}"))?;
    }
    Ok(())
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

    /// Sprinter 6146b39a5beb: only a refusal that says the primary no longer
    /// holds what a completed copy needs (or is another dataset, or was
    /// promoted past it) makes the copy stale; a busy, shutting-down,
    /// superseded or mis-authenticated primary keeps it.
    #[test]
    fn only_a_refusal_about_the_copy_makes_it_stale() {
        for code in [
            RefuseCode::WalPruned { oldest: 9 },
            RefuseCode::Gap {
                expected: 5,
                have_from: 7,
                have_to: 9,
            },
            RefuseCode::GroupTooLarge { first: 5, bytes: 1 },
            RefuseCode::Diverged { at: 4 },
            RefuseCode::HistoryMismatch,
            RefuseCode::EpochStale { have: 2, want: 1 },
        ] {
            assert!(copy_refusal_is_stale(&code), "{code}");
        }
        for code in [
            RefuseCode::TooManySessions,
            RefuseCode::SourceNotServing,
            RefuseCode::AuthFailed,
            RefuseCode::Version,
            RefuseCode::SourceStale { have: 1, want: 2 },
            RefuseCode::EpochStale { have: 1, want: 2 },
        ] {
            assert!(!copy_refusal_is_stale(&code), "{code}");
        }
    }

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

    /// Spec R3a (Sprinter 1c8963417a35): the census of a former primary's
    /// directory keeps the store's own RocksDB files, lists the primary-only
    /// state its conversion removes (RocksDB's WAL archive, the HNSW dump
    /// cache, an empty lock-free WAL directory, RocksDB's temporaries) and
    /// this node's own leftover export checkpoints that hard-link its files,
    /// copies the fence record, and refuses anything else by name — a link
    /// from elsewhere, a non-empty lock-free WAL, a foreign file, a symbolic
    /// link.
    #[test]
    fn a_rewind_census_keeps_the_store_lists_primary_state_and_refuses_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("primary");
        let exports = root.path().join("primary.physical-export");
        fs::create_dir_all(dir.join("archive")).unwrap();
        fs::write(dir.join("archive").join("000012.log"), b"wal").unwrap();
        fs::create_dir_all(dir.join("hnsw_snapshots")).unwrap();
        for name in [
            "CURRENT",
            "IDENTITY",
            "MANIFEST-000005",
            "OPTIONS-000007",
            "000010.sst",
            "000011.log",
            "LOCK",
            "LOG",
            "LOG.old.1761000000",
            PRIMARY_HISTORY_FILE,
            "OPTIONS-000008.dbtmp",
        ] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let census = rewind_census(&dir, &exports).unwrap();
        let mut dropped = census.drop.clone();
        dropped.sort();
        assert_eq!(dropped, vec!["OPTIONS-000008.dbtmp", "archive", "hnsw_snapshots"]);
        assert!(census.fence.is_none());
        assert!(census.stale_exports.is_empty());
        assert!(census.excluded.iter().any(|name| name == PRIMARY_HISTORY_FILE));

        // A hard link from this node's own leftover export checkpoint is
        // listed (the conversion removes the checkpoint); one from anywhere
        // else refuses the rewind.
        let lease = exports.join("nano-checkpoint-abc");
        fs::create_dir_all(lease.join("checkpoint")).unwrap();
        fs::hard_link(dir.join("000010.sst"), lease.join("checkpoint").join("000010.sst")).unwrap();
        let census = rewind_census(&dir, &exports).unwrap();
        assert_eq!(census.stale_exports, vec![lease.clone()]);
        let elsewhere = root.path().join("elsewhere.sst");
        fs::hard_link(dir.join("000010.sst"), &elsewhere).unwrap();
        let refused = rewind_census(&dir, &exports).err().unwrap();
        assert!(refused.contains("000010.sst has 3 links"), "{refused}");
        fs::remove_file(&elsewhere).unwrap();

        // The fence record is copied.
        let fence = FencedRecord {
            format: 1,
            history: Uuid::new_v4(),
            epoch: 1,
            fenced_by_epoch: 2,
            fenced_at: 1_761_000_000,
            by_node: Uuid::new_v4(),
            final_sequence: 77,
        };
        fs::write(dir.join(FENCED_FILE), serde_json::to_vec(&fence).unwrap()).unwrap();
        assert_eq!(rewind_census(&dir, &exports).unwrap().fence, Some(fence));

        // A non-empty lock-free WAL refuses; an empty one is removed.
        fs::create_dir_all(dir.join("lockfree_wal")).unwrap();
        fs::write(dir.join("lockfree_wal").join("1.wal"), b"w").unwrap();
        let refused = rewind_census(&dir, &exports).err().unwrap();
        assert!(
            refused.contains("lockfree_wal holds the lock-free ingestion engine's log"),
            "{refused}"
        );
        fs::remove_file(dir.join("lockfree_wal").join("1.wal")).unwrap();
        assert!(rewind_census(&dir, &exports)
            .unwrap()
            .drop
            .iter()
            .any(|name| name == "lockfree_wal"));

        // A user's regular files beside the store are kept (out of the
        // stage's census); a directory nobody accounts for and a symbolic
        // link refuse, by name.
        fs::write(dir.join("heliosdb-nano.toml"), b"[storage]\n").unwrap();
        fs::write(dir.join("server.key"), b"key").unwrap();
        let census = rewind_census(&dir, &exports).unwrap();
        let mut kept = census.kept.clone();
        kept.sort();
        assert_eq!(kept, vec!["heliosdb-nano.toml", "server.key"]);
        assert!(census.excluded.iter().any(|name| name == "server.key"));
        assert!(!census.drop.iter().any(|name| name == "server.key"), "never removed");
        fs::create_dir_all(dir.join("backup")).unwrap();
        let refused = rewind_census(&dir, &exports).err().unwrap();
        assert!(refused.contains("backup is not the database's"), "{refused}");
        fs::remove_dir_all(dir.join("backup")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("CURRENT"), dir.join("000099.sst")).unwrap();
            let refused = rewind_census(&dir, &exports).err().unwrap();
            assert!(refused.contains("000099.sst is a symbolic link"), "{refused}");
        }
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
        let error = Box::pin(recover_from_primary(&options, &mut |line| lines.push(line.to_string())))
            .await
            .unwrap_err();
        assert!(matches!(error, RecoverError::Refused(_)), "{error}");
        assert!(error.to_string().contains(&address.to_string()), "{error}");
        assert!(!data.exists(), "a fresh directory stays absent");
    }
}
