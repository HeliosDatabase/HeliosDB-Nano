//! `heliosdb-nano promote` (spec P5.1 §4, §11): the offline promotion of a
//! STOPPED physical standby to the primary of its dataset at the next
//! epoch — Oracle's `SWITCHOVER` (graceful) and `FAILOVER TO` (`--force`),
//! PostgreSQL's `pg_ctl promote`.
//!
//! # Steps (each logged on one `promote: …` line)
//!
//! 1. **Classify.** The directory must be an installed physical standby
//!    (its marker and its node-local family), stopped (RocksDB's `LOCK` is
//!    not held), with no interrupted fast-forward, and its stage must reopen
//!    as `start` would reopen it (marker, manifest, census, cursor). Read
//!    its history `H`, epoch `E` and cursor `C`. The promotion's identity —
//!    a RANDOM UUID, never the configured `node_id` (spec §11.C: two standbys
//!    sharing one configuration must not both be answered `Fenced`) — is
//!    created here once and kept in the node-local family
//!    ([`PROMOTION_NODE_KEY`]): the fence below names it, and a re-run after
//!    a crash fences under the same name, which the old primary answers with
//!    the same `Fenced` instead of refusing a "second promotion".
//! 2. **Fence the old primary** (spec §11.B: the tail is proven BEFORE the
//!    fence). As a receiver of `H` at `E` and `C`: open a resume session
//!    after `C` (naming the cursor's last write group, which the primary
//!    verifies, §11.F) and receive its first poll — the primary serves this
//!    standby's cursor — then send `Fence { E + 1 }` inside that session; on
//!    `Fenced { final }` tail the fenced primary through `final` with the
//!    ordinary offline receiver (the follow mode `resync` uses, over the
//!    same stage), so nothing it committed is lost. A primary that is
//!    unreachable, or a tail that does not reach `final`, within
//!    `--fence-timeout` stops the promotion — unless `--force`, an
//!    operator's attestation that the old primary is down or isolated
//!    (writes it committed past what this standby holds are lost; said as a
//!    WARNING). `--force` never skips the fence of a primary that answers:
//!    it only decides what happens when the fence or the tail cannot be had
//!    (and a reachable primary whose tail cannot be proven is fenced
//!    anyway). `--no-fence` (implies `--force`) never contacts the primary:
//!    a rehearsal on a cloned directory. Once a `Fence` was sent, every
//!    timeout or I/O error says the primary may already be fenced by this
//!    promotion — re-running answers the same `Fenced`. A primary already at
//!    a newer epoch, or already fenced by another promotion, stops it for
//!    good: a primary of the new epoch exists.
//! 3. **Dirty record** [`PROMOTION_FILE`], written whole before any store
//!    change. While it exists every engine open of the directory is refused
//!    (`start` in any role, `recover`, `resync`, `repl`, the embedded API),
//!    naming this command; a re-run finds it, skips steps 1-2 and finishes
//!    from step 4 with its values. A record that cannot be read on a store
//!    the conversion has not touched yet (spec §11.H: the node-local family
//!    still there, no epoch written by this promotion) is discarded with a
//!    WARNING and the promotion starts again from step 1.
//! 4. **Conversion** (`physical::PromotionWriter`, which writes raw, in the
//!    module the write funnel allows): (a) `repl:epoch` with `sync`, the
//!    first write of the new epoch, (b) in the same write the logical WAL's
//!    redo floor raised to its head, so the first open as a primary replays
//!    none of the old primary's logical entries and reuses no LSN, (c) the
//!    node-local family dropped, (d) the WAL fsynced, the default family
//!    flushed, the store closed.
//! 5. **Identity.** `NANO-PRIMARY-HISTORY.json` naming the INHERITED history
//!    `H` (a promotion never mints a history: the epoch says which incarnation
//!    of it this is), the physical marker and manifest removed (with their
//!    temporaries), the directory fsynced.
//! 6. The record removed; one summary line. `--then start` starts the node
//!    as a primary (`main.rs`).
//!
//! Every step checks before it acts, so a re-run of any interrupted
//! promotion converges on the same directory: epoch `E + 1`, history `H`,
//! no node-local family, no physical marker. Every failure names the
//! command that finishes it (spec §11.K). The test hook
//! [`KILL_PROMOTE_AT_ENV`] (`internal-tests` builds) SIGKILLs the process
//! after a step, to prove it.
//!
//! Spec P5.2: the same conversion ([`convert`], [`identity_files`], under the
//! same record) promotes a RUNNING standby online ([`online`], `SELECT
//! pg_promote(wait, wait_seconds)`): it fences through the standby's live
//! replication session, closes the store, converts it and reopens it as the
//! primary's in the same process. An online promotion interrupted after its
//! record is finished by this command, exactly like an offline one.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;

use super::checkpoint_transfer::{
    interrupted_fast_forward, physical_stage_history, ReopenedPhysicalStage, TransferLimits, PHYSICAL_MANIFEST_FILE,
    PHYSICAL_MARKER_FILE,
};
use super::epoch::{read_epoch, read_epoch_record, EpochRecord};
use super::fence::{read_fenced_record, FENCED_FILE};
use super::history::{
    read_record_in, remove_record_in, write_new_record_in, write_primary_history, PRIMARY_HISTORY_FILE,
};
use super::physical::{LocalBatch, LogicalWalFloor, PhysicalBatch, PromotionWriter, LOCAL_CF};
use super::physical_receiver::{run_receiver, ReceiverMode, ReceiverOptions, ReceiverStart, ReceiverStatus};
use super::physical_tls::ReplicationTransport;
use super::physical_wire::{
    BatchSink, Fenced, LastGroup, PeerRole, PhysicalClient, ReceiverIdentity, Refusal, RefuseCode, TransferToken,
    WireError, WireLimits, WireSyncMode,
};
pub use crate::storage::replication_records::{interrupted_promotion_refusal, PROMOTION_FILE};
use crate::Config;

/// Spec P5.2: `SELECT pg_promote(wait, wait_seconds)`, the online promotion
/// of a RUNNING standby, with this module's conversion.
pub mod online;

/// The node-local key holding the promotion's identity between step 1 and
/// the conversion, in [`LOCAL_CF`].
pub const PROMOTION_NODE_KEY: &[u8] = b"promotion-node";
/// `internal-tests` builds only: `promote` SIGKILLs its own process after the
/// named step — `after-record` (3), `after-epoch` (4a/4b), `after-drop`
/// (4c), `after-identity` (5) — the interrupted-promotion tests' crash points.
pub const KILL_PROMOTE_AT_ENV: &str = "HELIOSDB_NANO_TEST_KILL_PROMOTE_AT";
const FORMAT: u32 = crate::storage::replication_records::RECORD_FORMAT;
/// The tail's receiver cadence (as `resync`'s and the recover ladder's).
const TAIL_POLL_BYTES: usize = 8 * 1024 * 1024;
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TAIL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const TAIL_MAX_RECONNECTS: u32 = 5;
const TAIL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// The byte limit of the poll that proves the tail before the fence: the
/// first write group travels whatever its size (spec P4.1c), so one byte
/// reads one group and nothing more.
const PROOF_POLL_BYTES: usize = 1;

/// The content of [`PROMOTION_FILE`]. Spec P5.1 §4 step 3 names the first
/// seven fields; `node_id` (the promotion's identity the fence and
/// `repl:epoch` name), `cursor` (`C`, for the `--force` sentence), `reached`
/// and `primary` (the old primary's address, for the summary) let a re-run
/// finish with the same values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionRecord {
    pub format: u32,
    pub history: Uuid,
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// Unix seconds.
    pub started_at: u64,
    /// The old primary acknowledged the fence and its final sequence was
    /// tailed: nothing it committed is missing.
    pub fenced: bool,
    pub old_primary_final_sequence: Option<u64>,
    pub node_id: Uuid,
    /// The standby's source sequence when the promotion started (`C`).
    pub cursor: u64,
    /// Its source sequence after the tail (what the promoted store holds of
    /// the old primary).
    pub reached: u64,
    /// The old primary as `--primary-host` named it.
    pub primary: Option<String>,
}

impl PromotionRecord {
    fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT
            || self.history.is_nil()
            || self.node_id.is_nil()
            || self.from_epoch == 0
            || self.to_epoch != self.from_epoch.saturating_add(1)
            || self.fenced != self.old_primary_final_sequence.is_some()
        {
            return Err(format!("{PROMOTION_FILE} is malformed: {self:?}"));
        }
        Ok(())
    }
}

/// The promotion record of `dir`, if it carries one. A file that is there
/// but does not decode is an error: it still means a promotion started.
pub fn read_promotion_record(dir: &Path) -> Result<Option<PromotionRecord>, String> {
    match read_record_in::<PromotionRecord>(dir, PROMOTION_FILE) {
        Ok(Some(record)) => record.validate().map(|()| Some(record)),
        Ok(None) => Ok(None),
        Err(error) => Err(format!(
            "{} carries {PROMOTION_FILE}, which cannot be read ({error}): a promotion was interrupted",
            dir.display()
        )),
    }
}

/// The refusal every entry point but `promote` gives `dir` when it carries a
/// promotion record (or one that cannot be read), `None` otherwise.
pub fn refuse_promoting_directory(dir: &Path) -> Option<String> {
    fs_present(&dir.join(PROMOTION_FILE)).then(|| interrupted_promotion_refusal(dir))
}

fn fs_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Why a promotion did not complete.
#[derive(Debug, thiserror::Error)]
pub enum PromoteError {
    /// A precondition or a refusal: nothing of the store changed (or the
    /// interrupted promotion's record names what to run). `promote` exits 2.
    #[error("promote refused: {0}")]
    Refused(String),
    /// A failure part-way; the record (when written) keeps the promotion
    /// resumable. Spec §11.K: the message names the command that finishes
    /// it ([`promote`] appends it). `promote` exits 1.
    #[error("promote failed: {0}")]
    Failed(String),
}

fn refused(reason: impl Into<String>) -> PromoteError {
    PromoteError::Refused(reason.into())
}

fn failed(reason: impl Into<String>) -> PromoteError {
    PromoteError::Failed(reason.into())
}

/// Spec P5.1 §11.K: what a failed promotion of `dir` is finished by.
fn rerun_hint(dir: &Path) -> String {
    format!(
        "re-run `heliosdb-nano promote --data-dir {}` to finish it (every step checks before it acts; a fenced \
         primary answers the same fence)",
        dir.display()
    )
}

/// What [`promote`] does.
pub struct PromoteOptions {
    /// The STOPPED standby's data directory (`D`).
    pub data_dir: PathBuf,
    /// Its configuration: its storage options.
    pub config: Config,
    /// The old primary's replication port as `--primary-host` (or the
    /// standby's `[replication] primary_host`) names it, and its addresses;
    /// `None` when the standby has none (an archive-only standby): only
    /// `--force` promotes it then.
    pub primary_host: Option<String>,
    pub sources: Vec<SocketAddr>,
    pub transport: Option<ReplicationTransport>,
    pub token: Option<TransferToken>,
    pub wire: WireLimits,
    /// `--fence-timeout`: the bound of the fence (connection, handshake, the
    /// tail's proof, `Fenced`) and, separately, of the tail through the
    /// final sequence.
    pub fence_timeout: Duration,
    /// `--force`: promote even when the old primary cannot be fenced, or not
    /// tailed through its final sequence. A primary that answers is fenced
    /// all the same.
    pub force: bool,
    /// `--no-fence` (spec §11.B, implies `force`): never contact the old
    /// primary — a rehearsal on a cloned directory.
    pub no_fence: bool,
}

/// What a promotion did.
#[derive(Clone, Debug)]
pub struct PromoteReport {
    pub data_dir: PathBuf,
    pub history: Uuid,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub fenced: bool,
    pub old_primary_final_sequence: Option<u64>,
    /// `C`: the standby's source sequence when the promotion started.
    pub cursor: u64,
    /// The old primary's sequence the promoted store holds.
    pub reached: u64,
    pub primary: Option<String>,
    /// What the conversion did to the logical WAL.
    pub logical_wal: LogicalWalFloor,
    /// A re-run that finished an interrupted promotion.
    pub resumed: bool,
}

impl PromoteReport {
    /// Spec P5.1 §4 step 6: the summary line.
    pub fn summary(&self) -> String {
        let primary = self.primary.as_deref().map_or_else(
            || "the old primary".to_string(),
            |host| format!("the old primary at {host}"),
        );
        let fence = match self.old_primary_final_sequence {
            Some(final_sequence) => format!(
                "{primary} was fenced at its sequence {final_sequence} and everything through {final_sequence} is here"
            ),
            None => format!(
                "{primary} was NOT fenced (--force): writes committed on it after sequence {} are lost",
                self.reached
            ),
        };
        format!(
            "promote: {} is the primary of history {} at epoch {} (from {}; {fence}); start it with heliosdb-nano \
             start --replication-role primary --data-dir {}",
            self.data_dir.display(),
            self.history,
            self.to_epoch,
            self.from_epoch,
            self.data_dir.display()
        )
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Stop at a crash point of the promotion (`internal-tests` only).
fn kill_hook(step: &str) {
    #[cfg(all(feature = "internal-tests", unix))]
    kill_promote_hook(step);
    #[cfg(not(all(feature = "internal-tests", unix)))]
    let _ = step;
}

/// `internal-tests` builds only ([`KILL_PROMOTE_AT_ENV`]).
#[cfg(all(feature = "internal-tests", unix))]
fn kill_promote_hook(step: &str) {
    use std::io::Write;
    if std::env::var(KILL_PROMOTE_AT_ENV).is_ok_and(|at| at.trim() == step) {
        tracing::warn!("{KILL_PROMOTE_AT_ENV}: killing this process {step}");
        let _ = std::io::stdout().flush();
        // SAFETY: kill(2) of this very process with a standard signal; no
        // pointers, no preconditions.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    }
}

/// The process holding RocksDB's `LOCK` of `dir` (a running node), asked with
/// `F_GETLK`, which takes nothing.
fn lock_holder(dir: &Path) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("LOCK"))
            .ok()?;
        // SAFETY: an all-zero `flock` is a valid value of the plain C struct.
        let mut probe: libc::flock = unsafe { std::mem::zeroed() };
        probe.l_type = libc::F_WRLCK as libc::c_short;
        probe.l_whence = libc::SEEK_SET as libc::c_short;
        // SAFETY: fcntl(2) F_GETLK on a descriptor this function owns, with a
        // pointer to a live, initialized `flock` it only writes into.
        let answered = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) };
        (answered == 0 && probe.l_type != libc::F_UNLCK as libc::c_short).then_some(probe.l_pid)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

fn running_refusal(dir: &Path, pid: Option<i32>) -> PromoteError {
    refused(format!(
        "the standby is running{} on {}: stop it first (`heliosdb-nano stop`), or promote it online with `SELECT \
         pg_promote(true, N)` on its PostgreSQL port",
        pid.map(|pid| format!(" (process {pid} holds its LOCK)"))
            .unwrap_or_default(),
        dir.display()
    ))
}

/// Run blocking storage work off the async executor.
async fn blocking<T, F>(job: F) -> Result<T, PromoteError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PromoteError> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| failed(format!("the promotion's storage worker failed: {error}")))?
}

/// What step 1 found.
struct Classified {
    history: Uuid,
    epoch: u64,
    cursor: u64,
    /// The cursor's last applied write group (spec §11.F), which the old
    /// primary verifies when the promotion resumes from it.
    last: Option<LastGroup>,
    /// The promotion's random identity (spec §11.C).
    node_id: Uuid,
}

/// Step 1 on a blocking thread: classify `dir`, reopen its stage, read
/// `H`, `E`, `C`, and the promotion's identity (a random UUID, created once,
/// kept in the node-local family).
fn classify(dir: &Path, config: &Config, limits: TransferLimits) -> Result<Classified, PromoteError> {
    if fs_present(&dir.join(FENCED_FILE)) {
        let detail = match read_fenced_record(dir) {
            Ok(Some(record)) => super::fence::fenced_start_refusal(dir, &record),
            Ok(None) => String::new(),
            Err(error) => error,
        };
        return Err(refused(format!(
            "{} is a fenced primary's directory, not a standby's: nothing to promote ({detail})",
            dir.display()
        )));
    }
    if !fs_present(&dir.join(PHYSICAL_MARKER_FILE)) {
        return Err(refused(if fs_present(&dir.join(PRIMARY_HISTORY_FILE)) {
            format!(
                "{} is a primary's directory (it carries {PRIMARY_HISTORY_FILE}), not a standby's: nothing to \
                 promote",
                dir.display()
            )
        } else {
            format!(
                "{} is not an installed physical standby (no {PHYSICAL_MARKER_FILE}): only a physical standby is \
                 promoted",
                dir.display()
            )
        }));
    }
    let families = rocksdb::DB::list_cf(&rocksdb::Options::default(), dir)
        .map_err(|error| refused(format!("{} holds no readable store: {error}", dir.display())))?;
    if !families.iter().any(|family| family == LOCAL_CF) {
        return Err(refused(format!(
            "{} holds no node-local column family: it is not an installed physical standby of this release",
            dir.display()
        )));
    }
    if let Some(pid) = lock_holder(dir) {
        return Err(running_refusal(dir, Some(pid)));
    }
    if let Some(record) = interrupted_fast_forward(dir, config) {
        return Err(refused(format!(
            "{} carries an interrupted fast-forward (sequence {}→{}): finish it with `heliosdb-nano recover \
             --from-primary --data-dir {}` first",
            dir.display(),
            record.from,
            record.to,
            dir.display()
        )));
    }
    let history = physical_stage_history(dir).map_err(|error| {
        refused(format!(
            "the physical standby marker of {} cannot be read: {error}",
            dir.display()
        ))
    })?;
    let stage = ReopenedPhysicalStage::open(dir, history, limits, config).map_err(|error| {
        let text = error.to_string();
        if text.contains("lock") || text.contains("Resource temporarily unavailable") {
            running_refusal(dir, None)
        } else {
            refused(format!(
                "{} cannot be reopened as the physical standby it is: {text}",
                dir.display()
            ))
        }
    })?;
    let outcome = (|| -> Result<Classified, PromoteError> {
        let writer = stage.writer();
        let db = writer.db();
        // Spec §11.G: an epoch that cannot be read refuses, never epoch 1.
        let epoch = read_epoch(db).map_err(|error| {
            refused(format!(
                "the replication epoch record of {} cannot be read ({error}): its epoch is unknown, so it is not \
                 promoted",
                dir.display()
            ))
        })?;
        let local = db
            .cf_handle(LOCAL_CF)
            .ok_or_else(|| refused("the standby was opened without its node-local family"))?;
        let stored = db
            .get_cf(local, PROMOTION_NODE_KEY)
            .map_err(|error| failed(format!("the node-local family cannot be read: {error}")))?
            .and_then(|bytes| Uuid::from_slice(&bytes).ok())
            .filter(|node| !node.is_nil());
        let node_id = match stored {
            Some(node) => node,
            None => {
                // Spec §11.C: the promotion's own identity, never a
                // configured `node_id` two standbys could share.
                let node = Uuid::new_v4();
                let mut batch = LocalBatch::new();
                batch.put(PROMOTION_NODE_KEY, node.as_bytes());
                writer
                    .write_local(batch)
                    .map_err(|error| failed(format!("the promotion's identity cannot be kept: {error}")))?;
                node
            }
        };
        let cursor = writer.cursor();
        Ok(Classified {
            history,
            epoch,
            cursor: cursor.source_sequence(),
            last: cursor
                .last_group()
                .map(|(first_sequence, digest)| LastGroup { first_sequence, digest }),
            node_id,
        })
    })();
    let closed = stage
        .close()
        .map_err(|error| failed(format!("the standby's store could not be closed: {error}")));
    let classified = outcome?;
    closed?;
    Ok(classified)
}

/// Spec §11.H: whether the conversion of `dir` has NOT started — its store
/// still holds the node-local family, and no epoch written by the promotion
/// that family names. Read without a lock (the caller checked none is held)
/// and without the engine: every column family opened read-only, RocksDB's
/// `LOG` kept out of the directory.
fn conversion_untouched(dir: &Path) -> Result<bool, String> {
    let families = rocksdb::DB::list_cf(&rocksdb::Options::default(), dir)
        .map_err(|error| format!("{} holds no readable store: {error}", dir.display()))?;
    if !families.iter().any(|family| family == LOCAL_CF) {
        return Ok(false);
    }
    let log = tempfile::Builder::new()
        .prefix("nano-promote-read-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let mut options = rocksdb::Options::default();
    options.set_db_log_dir(log.path());
    let db = rocksdb::DB::open_cf_for_read_only(&options, dir, families, false).map_err(|error| error.to_string())?;
    let local = db
        .cf_handle(LOCAL_CF)
        .ok_or_else(|| "the node-local family did not open".to_string())?;
    let promotion = db
        .get_cf(local, PROMOTION_NODE_KEY)
        .map_err(|error| error.to_string())?
        .and_then(|bytes| Uuid::from_slice(&bytes).ok());
    let epoch = read_epoch_record(&db).map_err(|error| error.to_string())?;
    Ok(!matches!((epoch, promotion), (Some(record), Some(node)) if record.node_id == node))
}

/// How step 2 ended.
enum Fencing {
    /// The old primary is fenced and tailed through `final_sequence`.
    Fenced { final_sequence: u64, reached: u64 },
    /// `--force` / `--no-fence`: promoted without the old primary's final
    /// sequence (the reason is printed as a WARNING).
    Forced { reached: u64, reason: String },
}

/// The receiver identity of the promotion's sessions.
fn identity(node_id: Uuid, epoch: u64) -> ReceiverIdentity {
    ReceiverIdentity {
        node_id,
        role: PeerRole::Resync,
        epoch,
        pg_port: 0,
        sync_mode: WireSyncMode::Async,
    }
}

/// The poll that proves the tail before the fence: its groups are fetched
/// again (and applied) by the tail's own session.
struct ProofSink;

#[async_trait]
impl BatchSink for ProofSink {
    async fn apply(&mut self, _batch: PhysicalBatch) -> super::physical_wire::Result<()> {
        Ok(())
    }
}

/// Refusals that say this standby cannot tail the primary from its cursor
/// (it needs recovery first), though the primary answers.
fn untailable(code: &RefuseCode) -> bool {
    matches!(
        code,
        RefuseCode::WalPruned { .. }
            | RefuseCode::Gap { .. }
            | RefuseCode::GroupTooLarge { .. }
            | RefuseCode::Diverged { .. }
    )
}

/// What the fence session reached.
enum Reached {
    /// The tail was proven, then the primary fenced.
    Fenced(Fenced),
    /// `--force`: the primary answers but cannot be tailed from this
    /// standby's cursor (`why`); it was fenced all the same.
    FencedUntailable { fenced: Fenced, why: String },
}

/// A session with the old primary announcing the standby's history, epoch
/// and cursor, checked: the primary is of this epoch and can be fenced.
async fn fence_session(
    options: &PromoteOptions,
    classified: &Classified,
    source: SocketAddr,
    shutdown: &watch::Receiver<bool>,
) -> Result<PhysicalClient<super::physical_tls::ReplicationStream>, WireError> {
    let transport = options
        .transport
        .as_ref()
        .ok_or_else(|| WireError::Protocol("no transport to the primary".into()))?;
    let token = options
        .token
        .as_ref()
        .ok_or_else(|| WireError::Protocol("no replication token".into()))?;
    let hello = identity(classified.node_id, classified.epoch).hello(Some(classified.history), Some(classified.cursor));
    let client =
        PhysicalClient::connect_replication(source, transport, token, hello, options.wire, shutdown.clone()).await?;
    if client.hello_ok().epoch > classified.epoch {
        return Err(WireError::Refused(Refusal::new(
            RefuseCode::EpochStale {
                have: client.hello_ok().epoch,
                want: classified.epoch,
            },
            "the primary is at a newer epoch",
        )));
    }
    if !client.fenceable() {
        return Err(WireError::Refused(Refusal::new(
            RefuseCode::Version,
            "the source at this address offers no fencing: it is not a primary of this release (a cascading \
             standby, or an older build)",
        )));
    }
    Ok(client)
}

/// Step 2's first half (spec §11.B): prove the tail — a resume session after
/// `C`, verified by its last group, and its first poll — then fence the old
/// primary inside that session. `sent` is raised the moment a `Fence` goes
/// out: from then on the primary may be fenced, whatever happens next.
async fn fence_primary(
    options: &PromoteOptions,
    classified: &Classified,
    shutdown: &watch::Receiver<bool>,
    sent: &AtomicBool,
) -> Result<Reached, WireError> {
    let next_epoch = classified.epoch.saturating_add(1);
    let mut last_error = None;
    for source in &options.sources {
        let mut client = match fence_session(options, classified, *source, shutdown).await {
            Ok(client) => client,
            Err(error @ WireError::Refused(_)) => return Err(error),
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let proved = async {
            client
                .resume(classified.history, classified.cursor, classified.last)
                .await?;
            client.poll(classified.cursor, PROOF_POLL_BYTES, &mut ProofSink).await?;
            Ok::<_, WireError>(())
        }
        .await;
        match proved {
            Ok(()) => {
                sent.store(true, Ordering::SeqCst);
                let fenced = client.fence(next_epoch).await?;
                let _ = client.close().await;
                return Ok(Reached::Fenced(fenced));
            }
            // `--force`: the primary answers, so it is fenced all the same —
            // on a session of its own (this one ended with the refusal).
            Err(WireError::Refused(refusal)) if options.force && untailable(&refusal.code) => {
                let mut fencer = fence_session(options, classified, *source, shutdown).await?;
                sent.store(true, Ordering::SeqCst);
                let fenced = fencer.fence(next_epoch).await?;
                let _ = fencer.close().await;
                return Ok(Reached::FencedUntailable {
                    fenced,
                    why: refusal.to_string(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| WireError::Protocol("the primary has no address".into())))
}

/// Tail the (fenced) old primary through `through` with the offline
/// receiver over the standby's own stage (step 2's second half): the follow
/// mode `resync` runs, stopped once the durable cursor reaches `through`.
/// Returns the cursor reached; an error says how far it got.
async fn tail_through(
    options: &PromoteOptions,
    classified: &Classified,
    through: u64,
    progress: &mut dyn FnMut(&str),
) -> Result<u64, (u64, String)> {
    let start = classified.cursor;
    if start >= through {
        return Ok(start);
    }
    let (Some(transport), Some(token)) = (options.transport.clone(), options.token.clone()) else {
        return Err((start, "no connection settings for the primary".to_string()));
    };
    let receiver = ReceiverOptions {
        config: options.config.clone(),
        sources: options.sources.clone(),
        transport,
        token,
        identity: identity(classified.node_id, classified.epoch),
        start: ReceiverStart::Resume {
            path: options.data_dir.clone(),
            history: classified.history,
        },
        mode: ReceiverMode::Follow,
        wire: options.wire,
        poll_byte_limit: TAIL_POLL_BYTES,
        poll_interval: TAIL_POLL_INTERVAL,
        reconnect_delay: TAIL_RECONNECT_DELAY,
        max_reconnects: TAIL_MAX_RECONNECTS,
        heartbeat_interval: TAIL_HEARTBEAT_INTERVAL,
        archive: None,
    };
    let (stop, shutdown) = watch::channel(false);
    let (status, mut statuses) = watch::channel(ReceiverStatus::Connecting {
        history: Some(classified.history),
        after: Some(start),
        last_error: None,
    });
    let run = run_receiver(receiver, shutdown, status);
    tokio::pin!(run);
    let deadline = tokio::time::sleep(options.fence_timeout);
    tokio::pin!(deadline);
    let mut reached = start;
    let mut open = true;
    let mut timed_out = false;
    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            changed = statuses.changed(), if open => {
                if changed.is_err() {
                    open = false;
                    continue;
                }
                let current = statuses.borrow_and_update().clone();
                if let ReceiverStatus::Applying { cursor, .. } | ReceiverStatus::CaughtUp { cursor, .. } = current {
                    reached = reached.max(cursor.source_sequence());
                    if reached >= through {
                        let _ = stop.send(true);
                    }
                }
            }
            () = &mut deadline, if !timed_out => {
                timed_out = true;
                let _ = stop.send(true);
            }
        }
    };
    match outcome {
        Ok(closed) => {
            let reached = closed.cursor.source_sequence();
            if reached >= through {
                progress(&format!(
                    "promote: tailed the fenced primary from sequence {start} through its final sequence {through}"
                ));
                Ok(reached)
            } else {
                Err((
                    reached,
                    format!(
                        "the tail reached sequence {reached}, not the old primary's final {through}, within {} s",
                        options.fence_timeout.as_secs()
                    ),
                ))
            }
        }
        Err(error) => Err((reached, format!("the tail of the old primary stopped: {error}"))),
    }
}

/// Spec P5.1 §4 step 2: the WARNING / refusal sentence of a primary that
/// did not answer before a fence was sent.
fn unreachable_sentence(options: &PromoteOptions, cursor: u64) -> String {
    format!(
        "the primary at {} did not answer the fence within {} s; if it is down or isolated, re-run with --force \
         (writes committed on it after sequence {cursor} are lost)",
        options.primary_host.as_deref().unwrap_or("<no primary configured>"),
        options.fence_timeout.as_secs()
    )
}

/// Spec P5.1 §11.B: what a timeout or an I/O error AFTER a `Fence` was sent
/// says — the primary may be fenced already, by this very promotion.
fn after_fence_sentence(options: &PromoteOptions, what: &str) -> String {
    format!(
        "{what}; the primary at {} may already be fenced by this promotion: re-run `heliosdb-nano promote --data-dir \
         {}` (it answers the same Fenced); use --force only if it is gone",
        options.primary_host.as_deref().unwrap_or("<no primary configured>"),
        options.data_dir.display()
    )
}

/// The refusal of a primary that answered but refused, as the operator reads
/// it (`host` is the configured `--primary-host`).
fn fence_refusal(options: &PromoteOptions, classified: &Classified, host: &str, refusal: &Refusal) -> String {
    let dir = options.data_dir.display();
    match &refusal.code {
        RefuseCode::EpochStale { have, .. } => format!(
            "the primary at {host} is at epoch {have}, past this standby's {}: another node was already promoted \
             to epoch {have}; recover this standby from the current primary instead (`heliosdb-nano recover \
             --from-primary --primary-host <the current primary> --data-dir {dir}`) — {refusal}",
            classified.epoch
        ),
        RefuseCode::SourceStale { have, want } => format!(
            "the primary at {host} is a superseded primary (epoch {have} < this standby's {want}): another node was \
             already promoted; point --primary-host at the current primary — {refusal}"
        ),
        // Spec P5.1 §8 test 7: another promotion fenced it first.
        RefuseCode::Version if refusal.reason.contains("already fenced") => {
            let what = refusal.reason.trim_start_matches("this primary ");
            let what = what.split("; recover the receiver").next().unwrap_or(what);
            format!(
                "the primary at {host} {what}; recover {dir} from it instead (`heliosdb-nano recover --from-primary \
                 --primary-host <that primary> --data-dir {dir}`)"
            )
        }
        // Spec §11.B: the tail cannot be proven, so nothing was fenced.
        code if untailable(code) => format!(
            "the primary at {host} does not serve this standby's cursor (sequence {}): {refusal}; nothing was \
             fenced. Recover the standby from it first (`heliosdb-nano recover --from-primary --primary-host {host} \
             --data-dir {dir}`), then promote; or promote with --force (the primary is fenced all the same, and the \
             writes this standby lacks are lost)",
            classified.cursor
        ),
        // It answers but cannot serve the promotion now (a fence that could
        // not complete, a shutdown, its session limit): it may be fenced (or
        // fencing) already; the same promotion fences it again.
        RefuseCode::SourceNotServing | RefuseCode::TooManySessions => format!(
            "the primary at {host} cannot serve the promotion now: {refusal}; it may already refuse writes — re-run \
             `heliosdb-nano promote --data-dir {dir}` (the same promotion fences it again)"
        ),
        _ => format!("the primary at {host} refused the fence: {refusal}"),
    }
}

/// Step 2.
async fn fence_and_tail(
    options: &PromoteOptions,
    classified: &Classified,
    progress: &mut dyn FnMut(&str),
) -> Result<Fencing, PromoteError> {
    if options.no_fence {
        return Ok(Fencing::Forced {
            reached: classified.cursor,
            reason: format!(
                "--no-fence: the old primary was not contacted (a rehearsal on a cloned directory?); if it is \
                 running it is NOT fenced and still accepts writes, and writes committed on it after sequence {} are \
                 not here",
                classified.cursor
            ),
        });
    }
    let Some(host) = options.primary_host.clone() else {
        let reason =
            "no primary is configured (--primary-host, [replication] primary_host), so none can be fenced".to_string();
        if options.force {
            return Ok(Fencing::Forced {
                reached: classified.cursor,
                reason,
            });
        }
        return Err(refused(format!(
            "{reason}; if this standby's primary is down or isolated, re-run with --force (writes committed on it \
             after sequence {} are lost)",
            classified.cursor
        )));
    };
    progress(&format!(
        "promote: fencing the primary at {host} (epoch {} → {}, this standby at sequence {})",
        classified.epoch,
        classified.epoch.saturating_add(1),
        classified.cursor
    ));
    let (stop, shutdown) = watch::channel(false);
    let sent = AtomicBool::new(false);
    let attempt = tokio::time::timeout(
        options.fence_timeout,
        fence_primary(options, classified, &shutdown, &sent),
    )
    .await;
    let _ = stop.send(true);
    let was_sent = sent.load(Ordering::SeqCst);
    let fenced = match attempt {
        Ok(Ok(Reached::Fenced(fenced))) => fenced,
        Ok(Ok(Reached::FencedUntailable { fenced, why })) => {
            progress(&format!(
                "promote: the primary at {host} is fenced by epoch {} at its sequence {} (it could not be tailed \
                 from this standby's cursor)",
                fenced.epoch, fenced.final_sequence
            ));
            return Ok(Fencing::Forced {
                reached: classified.cursor,
                reason: format!(
                    "the primary at {host} was fenced at its sequence {}, but this standby cannot tail it from its \
                     sequence {} ({why}): the writes in between are lost (--force)",
                    fenced.final_sequence, classified.cursor
                ),
            });
        }
        Ok(Err(WireError::Refused(refusal))) => {
            return Err(refused(fence_refusal(options, classified, &host, &refusal)));
        }
        Ok(Err(error)) if was_sent => {
            return forced_or_refused(
                options,
                classified,
                after_fence_sentence(options, &format!("the fence's session failed ({error})")),
            )
        }
        Ok(Err(error)) => {
            return forced_or_refused(
                options,
                classified,
                format!("{} ({error})", unreachable_sentence(options, classified.cursor)),
            )
        }
        Err(_) if was_sent => {
            return forced_or_refused(
                options,
                classified,
                after_fence_sentence(
                    options,
                    &format!(
                        "the primary did not answer the fence within {} s",
                        options.fence_timeout.as_secs()
                    ),
                ),
            )
        }
        Err(_) => return forced_or_refused(options, classified, unreachable_sentence(options, classified.cursor)),
    };
    progress(&format!(
        "promote: the primary at {host} is fenced by epoch {} at its sequence {}",
        fenced.epoch, fenced.final_sequence
    ));
    match tail_through(options, classified, fenced.final_sequence, progress).await {
        Ok(reached) => Ok(Fencing::Fenced {
            final_sequence: fenced.final_sequence,
            reached,
        }),
        Err((reached, reason)) => {
            let sentence = after_fence_sentence(
                options,
                &format!(
                    "{reason}: the old primary is fenced, but writes it committed after sequence {reached} are not \
                     here yet"
                ),
            );
            if options.force {
                Ok(Fencing::Forced {
                    reached,
                    reason: sentence,
                })
            } else {
                Err(refused(sentence))
            }
        }
    }
}

fn forced_or_refused(
    options: &PromoteOptions,
    classified: &Classified,
    sentence: String,
) -> Result<Fencing, PromoteError> {
    if options.force {
        Ok(Fencing::Forced {
            reached: classified.cursor,
            reason: sentence,
        })
    } else {
        Err(refused(sentence))
    }
}

/// Spec P5.1 §4: promote the STOPPED physical standby `options.data_dir`
/// (module documentation). `progress` receives the `promote: …` lines.
/// Spec §11.K: a failure names the command that finishes it.
pub async fn promote(options: &PromoteOptions, progress: &mut dyn FnMut(&str)) -> Result<PromoteReport, PromoteError> {
    promote_steps(options, progress).await.map_err(|error| match error {
        PromoteError::Failed(reason) => PromoteError::Failed(format!("{reason}; {}", rerun_hint(&options.data_dir))),
        other => other,
    })
}

/// The pending promotion of `dir`: its record, `None` when there is none —
/// or (spec §11.H) when the record cannot be read but the conversion has not
/// touched the store yet, in which case the record is discarded (WARNING)
/// and the promotion starts again from step 1. A record that cannot be read
/// once the conversion started is refused, naming the file and what to do.
async fn pending_promotion(
    dir: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<Option<PromotionRecord>, PromoteError> {
    let read = {
        let dir = dir.to_path_buf();
        blocking(move || Ok(read_promotion_record(&dir))).await?
    };
    let reason = match read {
        Ok(record) => return Ok(record),
        Err(reason) => reason,
    };
    if let Some(pid) = lock_holder(dir) {
        return Err(running_refusal(dir, Some(pid)));
    }
    let untouched = {
        let dir = dir.to_path_buf();
        blocking(move || Ok(conversion_untouched(&dir))).await?
    };
    match untouched {
        Ok(true) => {
            let warning = format!(
                "WARNING: {reason}; the store is still the standby it was (its node-local family is there and no \
                 epoch of this promotion was written): the record is discarded and the promotion starts again"
            );
            progress(&warning);
            tracing::warn!("promote: {warning}");
            let owned = dir.to_path_buf();
            blocking(move || {
                remove_record_in(&owned, PROMOTION_FILE)
                    .map_err(|error| failed(format!("{PROMOTION_FILE} cannot be removed: {error}")))
            })
            .await?;
            Ok(None)
        }
        Ok(false) => Err(refused(format!(
            "{reason}, and the conversion already changed the store (this promotion's epoch is written, or the \
             node-local family is gone): the promotion cannot be finished without its record. Keep {} as it is \
             for inspection and build the node again from the current primary into a new directory \
             (`heliosdb-nano recover --from-primary --primary-host <the current primary> --data-dir <a new \
             directory>`)",
            dir.display()
        ))),
        Err(error) => Err(refused(format!(
            "{reason}, and the store cannot be read to tell how far the promotion got ({error}): restore the \
             directory from a copy, or rebuild the node from the current primary into a new directory"
        ))),
    }
}

async fn promote_steps(
    options: &PromoteOptions,
    progress: &mut dyn FnMut(&str),
) -> Result<PromoteReport, PromoteError> {
    let dir = options.data_dir.clone();
    // A record means steps 1-2 are done: finish from step 4 with its values.
    let (record, resumed) = match pending_promotion(&dir, progress).await? {
        Some(record) => {
            if let Some(pid) = lock_holder(&dir) {
                return Err(running_refusal(&dir, Some(pid)));
            }
            progress(&format!(
                "promote: {} carries an interrupted promotion to epoch {} (started at unix {}): finishing it",
                dir.display(),
                record.to_epoch,
                record.started_at
            ));
            (record, true)
        }
        None => {
            let classified = {
                let (dir, config, limits) = (dir.clone(), options.config.clone(), options.wire.transfer);
                blocking(move || classify(&dir, &config, limits)).await?
            };
            progress(&format!(
                "promote: {} is a stopped physical standby of history {} at epoch {}, sequence {} (promotion {})",
                dir.display(),
                classified.history,
                classified.epoch,
                classified.cursor,
                classified.node_id
            ));
            let fencing = fence_and_tail(options, &classified, progress).await?;
            let (fenced, final_sequence, reached) = match fencing {
                Fencing::Fenced {
                    final_sequence,
                    reached,
                } => (true, Some(final_sequence), reached),
                Fencing::Forced { reached, reason } => {
                    progress(&format!("WARNING: {reason}"));
                    tracing::warn!("promote --force: {reason}");
                    (false, None, reached)
                }
            };
            let record = PromotionRecord {
                format: FORMAT,
                history: classified.history,
                from_epoch: classified.epoch,
                to_epoch: classified.epoch.saturating_add(1),
                started_at: unix_secs(),
                fenced,
                old_primary_final_sequence: final_sequence,
                node_id: classified.node_id,
                cursor: classified.cursor,
                reached,
                primary: options.primary_host.clone(),
            };
            record.validate().map_err(failed)?;
            // Step 3: before any store change, written whole.
            {
                let (dir, record) = (dir.clone(), record.clone());
                blocking(move || {
                    write_new_record_in(&dir, PROMOTION_FILE, &record)
                        .map_err(|error| failed(format!("{PROMOTION_FILE} cannot be written: {error}")))
                })
                .await?;
            }
            progress(&format!(
                "promote: recorded the promotion in {}",
                dir.join(PROMOTION_FILE).display()
            ));
            kill_hook("after-record");
            (record, false)
        }
    };
    // Steps 4-6, each idempotent.
    let config = options.config.clone();
    let converted = {
        let (dir, record) = (dir.clone(), record.clone());
        blocking(move || convert(&dir, &config, &record)).await?
    };
    let logical = match (converted.highest_entry, converted.checkpoint_after) {
        (Some(highest), Some(floor)) => format!(
            "the logical WAL's redo floor is {floor} (it was {}; the old primary's retained entries up to {highest} \
             are reclaimed at the first open, never replayed; the next LSN is past {})",
            converted
                .checkpoint_before
                .map_or_else(|| "absent".to_string(), |before| before.to_string()),
            converted.last_lsn
        ),
        _ => "the logical WAL retains no entry (nothing to replay)".to_string(),
    };
    progress(&format!(
        "promote: wrote epoch {} into the store; {logical}; the node-local family is dropped",
        record.to_epoch
    ));
    {
        let (dir, record) = (dir.clone(), record.clone());
        blocking(move || identity_files(&dir, &record)).await?;
    }
    progress(&format!(
        "promote: {} carries the primary's identity of history {} (inherited, not a new one); the standby marker is \
         removed",
        dir.display(),
        record.history
    ));
    {
        let dir = dir.clone();
        blocking(move || {
            remove_record_in(&dir, PROMOTION_FILE)
                .map_err(|error| failed(format!("{PROMOTION_FILE} cannot be removed: {error}")))
        })
        .await?;
    }
    Ok(PromoteReport {
        data_dir: dir,
        history: record.history,
        from_epoch: record.from_epoch,
        to_epoch: record.to_epoch,
        fenced: record.fenced,
        old_primary_final_sequence: record.old_primary_final_sequence,
        cursor: record.cursor,
        reached: record.reached,
        primary: record.primary,
        logical_wal: converted,
        resumed,
    })
}

/// Step 4 (blocking): the conversion, through `physical::PromotionWriter`.
fn convert(dir: &Path, config: &Config, record: &PromotionRecord) -> Result<LogicalWalFloor, PromoteError> {
    let epoch = EpochRecord::promotion(
        record.from_epoch,
        record.node_id,
        record.started_at,
        record.old_primary_final_sequence,
    )
    .map_err(|error| failed(error.to_string()))?;
    let mut writer = PromotionWriter::open(dir, config).map_err(|error| {
        failed(format!(
            "the standby's store cannot be opened for the conversion: {error}"
        ))
    })?;
    // Spec §11.I: the message names the record, not its key (the keyspace
    // tripwire reads key-shaped literals as keys).
    let floor = writer
        .write_epoch(&epoch)
        .map_err(|error| failed(format!("the replication epoch record cannot be written: {error}")))?;
    kill_hook("after-epoch");
    writer
        .drop_local_family()
        .map_err(|error| failed(format!("the node-local family cannot be dropped: {error}")))?;
    kill_hook("after-drop");
    writer
        .close()
        .map_err(|error| failed(format!("the converted store cannot be closed: {error}")))?;
    Ok(floor)
}

/// Step 5 (blocking): the primary's identity with the inherited history; the
/// standby's marker and manifest (and their temporaries) removed.
fn identity_files(dir: &Path, record: &PromotionRecord) -> Result<(), PromoteError> {
    write_primary_history(dir, record.history)
        .map_err(|error| failed(format!("{PRIMARY_HISTORY_FILE} cannot be written: {error}")))?;
    for name in [PHYSICAL_MARKER_FILE, PHYSICAL_MANIFEST_FILE] {
        remove_record_in(dir, name).map_err(|error| failed(format!("{name} cannot be removed: {error}")))?;
    }
    kill_hook("after-identity");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn record() -> PromotionRecord {
        PromotionRecord {
            format: FORMAT,
            history: Uuid::new_v4(),
            from_epoch: 1,
            to_epoch: 2,
            started_at: 1_761_000_000,
            fenced: true,
            old_primary_final_sequence: Some(99),
            node_id: Uuid::new_v4(),
            cursor: 90,
            reached: 99,
            primary: Some("127.0.0.1:5440".into()),
        }
    }

    #[test]
    fn a_promotion_record_round_trips_and_refuses_every_other_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_promotion_record(dir.path()).unwrap(), None);
        assert_eq!(refuse_promoting_directory(dir.path()), None);
        let written = record();
        write_new_record_in(dir.path(), PROMOTION_FILE, &written).unwrap();
        assert_eq!(read_promotion_record(dir.path()).unwrap(), Some(written));
        let refusal = refuse_promoting_directory(dir.path()).unwrap();
        assert!(
            refusal.starts_with("promotion interrupted: finish it with `heliosdb-nano promote --data-dir"),
            "{refusal}"
        );
        remove_record_in(dir.path(), PROMOTION_FILE).unwrap();
        assert_eq!(refuse_promoting_directory(dir.path()), None);
    }

    #[test]
    fn a_malformed_promotion_record_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        for broken in [
            PromotionRecord {
                to_epoch: 3,
                ..record()
            },
            PromotionRecord {
                fenced: false,
                ..record()
            },
            PromotionRecord {
                node_id: Uuid::nil(),
                ..record()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
        std::fs::write(dir.path().join(PROMOTION_FILE), b"{").unwrap();
        assert!(read_promotion_record(dir.path()).is_err());
        assert!(
            refuse_promoting_directory(dir.path()).is_some(),
            "an unreadable record still refuses"
        );
    }

    #[test]
    fn the_summary_says_fenced_or_not() {
        let report = PromoteReport {
            data_dir: PathBuf::from("/srv/s"),
            history: Uuid::nil(),
            from_epoch: 1,
            to_epoch: 2,
            fenced: true,
            old_primary_final_sequence: Some(99),
            cursor: 90,
            reached: 99,
            primary: Some("10.0.0.1:5440".into()),
            logical_wal: LogicalWalFloor {
                checkpoint_before: None,
                checkpoint_after: None,
                last_lsn: 0,
                highest_entry: None,
            },
            resumed: false,
        };
        let fenced = report.summary();
        assert!(
            fenced.starts_with("promote: /srv/s is the primary of history")
                && fenced.contains("at epoch 2 (from 1; the old primary at 10.0.0.1:5440 was fenced at its sequence 99 and everything through 99 is here)")
                && fenced.ends_with("start it with heliosdb-nano start --replication-role primary --data-dir /srv/s"),
            "{fenced}"
        );
        let forced = PromoteReport {
            fenced: false,
            old_primary_final_sequence: None,
            reached: 90,
            ..report
        }
        .summary();
        assert!(
            forced.contains("was NOT fenced (--force): writes committed on it after sequence 90 are lost"),
            "{forced}"
        );
    }

    #[test]
    fn a_directory_that_is_no_standby_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let error = classify(dir.path(), &Config::default(), TransferLimits::default())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("not an installed physical standby"), "{error}");
        std::fs::write(dir.path().join(PRIMARY_HISTORY_FILE), b"{}").unwrap();
        let error = classify(dir.path(), &Config::default(), TransferLimits::default())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("is a primary's directory"), "{error}");
        std::fs::write(dir.path().join(FENCED_FILE), b"{}").unwrap();
        let error = classify(dir.path(), &Config::default(), TransferLimits::default())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("fenced primary's directory"), "{error}");
    }

    /// Spec P5.1 §11.K: every failure names the command that finishes it;
    /// a refusal says its own next step and is left as it is.
    #[test]
    fn a_failure_names_the_command_that_finishes_it() {
        let dir = PathBuf::from("/srv/standby");
        let rendered = match PromoteError::Failed("the converted store cannot be closed: disk full".into()) {
            PromoteError::Failed(reason) => PromoteError::Failed(format!("{reason}; {}", rerun_hint(&dir))),
            other => other,
        }
        .to_string();
        assert!(
            rendered.starts_with("promote failed: the converted store cannot be closed")
                && rendered.contains("re-run `heliosdb-nano promote --data-dir /srv/standby`"),
            "{rendered}"
        );
        assert!(after_fence_sentence(
            &PromoteOptions {
                data_dir: dir.clone(),
                config: Config::default(),
                primary_host: Some("10.0.0.1:5433".into()),
                sources: Vec::new(),
                transport: None,
                token: None,
                wire: WireLimits::default(),
                fence_timeout: Duration::from_secs(30),
                force: false,
                no_fence: false,
            },
            "the primary did not answer the fence within 30 s"
        )
        .contains("may already be fenced by this promotion: re-run `heliosdb-nano promote --data-dir /srv/standby`"));
    }

    /// Spec P5.1 §11.H: an unreadable promotion record is discarded only when
    /// the conversion has not touched the store — the node-local family is
    /// there and no epoch of the promotion it names is written.
    #[test]
    fn the_conversion_is_untouched_until_this_promotions_epoch_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let node = Uuid::new_v4();
        {
            let mut options = rocksdb::Options::default();
            options.create_if_missing(true);
            options.create_missing_column_families(true);
            let db = rocksdb::DB::open_cf(&options, &path, ["default", LOCAL_CF]).unwrap();
            db.put_cf(db.cf_handle(LOCAL_CF).unwrap(), PROMOTION_NODE_KEY, node.as_bytes())
                .unwrap();
        }
        assert!(conversion_untouched(&path).unwrap(), "no epoch written yet");
        {
            let db = rocksdb::DB::open_cf(&rocksdb::Options::default(), &path, ["default", LOCAL_CF]).unwrap();
            // An earlier promotion's epoch (another promotion's identity).
            let earlier = EpochRecord::promotion(1, Uuid::new_v4(), 1, None).unwrap();
            db.put(crate::replication::epoch::EPOCH_KEY, earlier.encode().unwrap())
                .unwrap();
        }
        assert!(conversion_untouched(&path).unwrap(), "an earlier promotion's epoch");
        {
            let db = rocksdb::DB::open_cf(&rocksdb::Options::default(), &path, ["default", LOCAL_CF]).unwrap();
            let ours = EpochRecord::promotion(2, node, 1, None).unwrap();
            db.put(crate::replication::epoch::EPOCH_KEY, ours.encode().unwrap())
                .unwrap();
        }
        assert!(
            !conversion_untouched(&path).unwrap(),
            "this promotion's epoch is written"
        );
    }
}
