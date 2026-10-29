//! `SELECT pg_promote(wait, wait_seconds)` (spec P5.2 §2-§3, §8): the
//! ONLINE promotion of a RUNNING physical standby to the primary of its
//! dataset at the next epoch — PostgreSQL's `pg_promote()`, Oracle's
//! `FAILOVER` / `SWITCHOVER` asked of the standby itself.
//!
//! # Why
//!
//! HeliosDB Proxy fails over (and switches over for upgrades) by sending
//! `SELECT pg_promote(true, N)` to a running standby and, right after the
//! `t`, opening a fresh connection that must already answer
//! `pg_is_in_recovery() = f`. The offline promotion (`super::promote`, P5.1)
//! needs the standby stopped; this one runs inside the standby process. The
//! standby's serving engine is write-guarded from its open and shares the
//! applier's RocksDB handle, so it cannot be flipped: the store is CLOSED,
//! converted with exactly the offline conversion ([`super::convert`],
//! [`super::identity_files`], under the same [`PROMOTION_FILE`] record) and
//! REOPENED as a primary's — in the same process, the listeners never closed.
//!
//! # Steps (each logged on one `promote: …` line)
//!
//! 1. **Preflight, then posture `promoting`** ([`StandbyHost::preflight`]:
//!    the standby is `streaming`, or `reconnecting` within its lag, carries
//!    no interrupted fast-forward, its primary configuration derives — TLS
//!    included — and its replication port is bound; nothing else changes on
//!    a refusal). From here `/ready` answers 503 and data reads 55000.
//! 2. **Sessions off the store**: the listeners serve the empty placeholder,
//!    then every PostgreSQL session of the store but the caller's is ended
//!    with FATAL 57P01 (`protocol::postgres::sessions`); the placeholder's
//!    sessions are answered as a not-ready standby's.
//! 3. **Wait for every session holder of the store to go**, bounded by
//!    `wait_seconds`. The receiver keeps applying meanwhile. A statement
//!    still inside the engine is waited for, never assumed gone: a session
//!    leaves the registry only when its task — and the database it held —
//!    is gone. A holder that does not go in time ABORTS the promotion while
//!    the primary is still unfenced (spec §8.A).
//! 4. **Fence through the live session, then catch up.** The receiver renews
//!    its session under the promotion's identity, proves the tail with one
//!    poll and sends `Fence { E + 1 }` ([`ReceiverCommand::Fence`]); on
//!    `Fenced { final }` it follows until its DURABLE cursor reaches `final`.
//!    `promote_without_fence = "allow"` applies ONLY while the receiver has
//!    no live session to its primary (it is unreachable) for
//!    `promote_fence_timeout`; once the primary answers, or once `Fence` was
//!    sent, the promotion waits for the answer at least the source's own
//!    bound ([`FENCE_ANSWER_BOUND`]), whatever `wait_seconds` (spec §8.B). A
//!    fence that failed without being refused is asked again on a new session
//!    (§8.D). After `Fenced` the promotion never goes ahead unfenced: a
//!    catch-up that cannot reach `final` aborts, naming both sequences
//!    (§8.C).
//! 5. **Stop replication**: the receiver (its in-flight run completes, its
//!    cursor is durable), the standby's archiver (its last epoch-E segment
//!    sealed through the cursor, `archiver.lock` released), the cascading
//!    source (its sessions refused).
//! 6. **Release the store**: only the runtime's own handles are left; it is
//!    dropped and its RocksDB handle proven closed (the LOCK released). A
//!    holder that persists here is a bug, logged at ERROR with the holders;
//!    after `Fenced` the promotion keeps waiting rather than abort.
//! 7. **Convert** under the record, with the offline promotion's functions.
//! 8. **Reopen as the primary** ([`StandbyHost::reopen_as_primary`]): the
//!    primary's configuration derived from the standby's, the replication
//!    port bound at the preflight, its archiver (`<H>/<E+1>/`) and gap fill,
//!    the primary posture, the standby guard lifted
//!    (`ha_state::enable_forwarding_after_promotion`), the store published to
//!    the listeners; then the placeholder's sessions are ended too.
//! 9. **Answer** the caller (`t`; `f` when `wait_seconds` ran out while the
//!    promotion still runs), which is then closed with FATAL 57P01.
//!
//! **Abort before the record**: the standby is restored — its store
//! re-published (steps 2-4), or its receiver restarted once the store is
//! closed (steps 5-6) — and the caller gets 55000 naming the reason, whether
//! the old primary is fenced, and the next command (spec §8.G). **Failure
//! after the record**: the process logs the exact command that finishes it
//! and EXITS with code 2; `start` refuses the directory until `heliosdb-nano
//! promote` finished it. No half-converted store is ever served. A panic of
//! the promotion takes the same two routes (spec §8.F).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use super::{
    blocking, convert, failed, identity_files, kill_hook, unix_secs, PromoteError, PromotionRecord, FORMAT,
    PROMOTION_FILE, PROMOTION_NODE_KEY,
};
use crate::protocol::postgres::sessions::{
    registry, SessionSummary, TerminateCause, TerminateScope, TERMINATE_DRAIN_TIMEOUT,
};
use crate::replication::fence::{ARCHIVE_SEAL_TIMEOUT, DRAIN_TIMEOUT};
use crate::replication::history::{remove_record_in, write_new_record_in};
use crate::replication::physical::{LocalBatch, FAST_FORWARD_KEY, LOCAL_CF};
use crate::replication::physical_receiver::{FenceEvent, ReceiverCommand};
use crate::replication::physical_wire::Fenced;
use crate::standby_posture::{
    clear_promoting, format_unix_secs, note_promoting, promotion_termination_message, PromotionStatus,
};
use crate::storage::StorageEngine;
use crate::{Config, EmbeddedDatabase};

/// `pg_promote`'s default `wait_seconds` (PostgreSQL's).
pub const DEFAULT_WAIT_SECONDS: u64 = 60;
/// The largest `wait_seconds` (spec P5.2 §2).
pub const MAX_WAIT_SECONDS: u64 = 3600;
/// The frame deadline of a source's replication session
/// (`physical_wire::WireLimits::default().frame_timeout`), the last part of
/// a fence's answer.
const SOURCE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// Spec P5.2 §8.B: how long a source may take to answer a `Fence` — its
/// drain of the writes past its gate, its archive's seal, one frame — and so
/// the least an online promotion waits for the answer once its primary
/// answers or the fence was sent, whatever `wait_seconds` (the caller gets
/// `f` meanwhile). Also the longest a catch-up with a live session may make
/// no progress.
pub const FENCE_ANSWER_BOUND: Duration =
    Duration::from_secs(DRAIN_TIMEOUT.as_secs() + ARCHIVE_SEAL_TIMEOUT.as_secs() + SOURCE_FRAME_TIMEOUT.as_secs());
/// Spec P5.2 §3 step 6: how long the release waits for the store's last
/// runtime handle before it says so at ERROR (and, unfenced, aborts).
const RELEASE_BUDGET: Duration = Duration::from_secs(10);
/// How long replication may take to stop before an UNFENCED promotion
/// aborts (a fenced one keeps waiting).
const STOP_BUDGET: Duration = Duration::from_secs(60);
/// How often the fence's watch looks at the receiver's posture.
const FENCE_TICK: Duration = Duration::from_millis(100);
/// A sign that the source answers (a refused connection attempt, an
/// established session) keeps it alive this long: longer than the
/// receiver's longest reconnect backoff (5 s), so a source that refuses
/// every attempt for a while is never taken for an unreachable one.
const ANSWERED_WINDOW: Duration = Duration::from_secs(7);
/// How often the session-holder wait looks again.
const HOLDER_POLL: Duration = Duration::from_millis(20);
/// `internal-tests` builds only: the online promotion FAILS — it does not
/// die — at the named point: `after-record` (step 7, before the conversion)
/// or `reopen` (step 8), the exit-2 tests' failure points; or `release`
/// (step 6 of an UNFENCED promotion), which aborts it with its replication
/// stopped — the restore tests' point.
pub const FAIL_ONLINE_PROMOTE_AT_ENV: &str = "HELIOSDB_NANO_TEST_FAIL_ONLINE_PROMOTE_AT";
/// `internal-tests` builds only: the online promotion waits, once it is
/// under way (posture `promoting`) and before it ends any session, for as
/// long as the file this variable names exists: a short `wait_seconds` runs
/// out while it still runs, and a second `pg_promote` meets the first —
/// gated on the file, not on a timer (spec P5.2 §8.K).
pub const PAUSE_ONLINE_PROMOTE_FILE_ENV: &str = "HELIOSDB_NANO_TEST_PAUSE_ONLINE_PROMOTE_FILE";
/// The FATAL 57P01 the `pg_promote` caller is closed with after an aborted
/// promotion: it let go of the store, and is served by the placeholder.
pub const ABORTED_CALLER_TERMINATION: &str =
    "terminating connection: the promotion of this standby was aborted (it stays a standby); reconnect";

/// `[replication] promote_without_fence` and `promote_fence_timeout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnlineSettings {
    /// `"allow"`: a primary that cannot be REACHED (no live replication
    /// session for `fence_timeout`) does not stop the promotion, which then
    /// holds what the standby had (Oracle's `FAILOVER`). `"refuse"`: it
    /// aborts (a cluster promoted by `SWITCHOVER` only). Never a slow
    /// primary that answers (spec P5.2 §8.B).
    pub allow_without_fence: bool,
    /// How long the primary must stay unreachable before `allow` / `refuse`
    /// applies, and how long a fenced primary may stay unreachable before
    /// the catch-up aborts.
    pub fence_timeout: Duration,
}

impl OnlineSettings {
    pub fn from_config(config: &crate::config::ReplicationConfig) -> crate::Result<Self> {
        Ok(Self {
            allow_without_fence: config.promote_allows_unfenced(),
            fence_timeout: config.promote_fence_timeout_duration()?,
        })
    }
}

/// What `SELECT pg_promote(wait, wait_seconds)` asked, and who asked.
#[derive(Clone, Debug)]
pub struct PromoteCall {
    pub wait: bool,
    /// 1..=[`MAX_WAIT_SECONDS`].
    pub wait_seconds: u64,
    pub backend_pid: i32,
    pub client: Option<String>,
}

impl PromoteCall {
    fn caller(&self) -> String {
        format!(
            "backend {} ({}), SELECT pg_promote({}, {})",
            self.backend_pid,
            self.client.as_deref().unwrap_or("local client"),
            self.wait,
            self.wait_seconds
        )
    }

    fn budget(&self) -> Duration {
        Duration::from_secs(self.wait_seconds)
    }
}

/// A `pg_promote` refused before anything changed (SQLSTATE 55000).
#[derive(Clone, Debug)]
pub struct Refusal {
    pub message: String,
    pub hint: Option<String>,
}

impl Refusal {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Where a promotion is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    Running,
    /// The node serves as the primary at `epoch`.
    Promoted {
        epoch: u64,
    },
    /// Aborted before its record: the node is a standby again; why, whether
    /// the old primary is fenced, and what to run next.
    Aborted(String),
}

/// A started promotion, as its caller holds it.
pub struct Started {
    placeholder: Arc<EmbeddedDatabase>,
    progress: watch::Receiver<Progress>,
    wait: bool,
    budget: Duration,
    to_epoch: u64,
}

impl Started {
    /// The empty database the caller's session is served by from now on
    /// (it lets go of the standby's store before the promotion reopens it).
    pub fn placeholder(&self) -> Arc<EmbeddedDatabase> {
        Arc::clone(&self.placeholder)
    }

    /// The FATAL 57P01 the caller's session is closed with after its answer.
    pub fn termination_message(&self) -> String {
        promotion_termination_message(self.to_epoch)
    }

    /// Spec P5.2 §2: `wait = false` — `t` at once (the promotion was
    /// started); `wait = true` — `t` once the node serves as the primary
    /// (a fresh connection sees `pg_is_in_recovery() = f`), `f` when
    /// `wait_seconds` ran out first (the promotion goes on), the reason when
    /// it was aborted before its record.
    pub async fn answer(mut self) -> Result<bool, String> {
        if !self.wait {
            return Ok(true);
        }
        let deadline = Instant::now() + self.budget;
        loop {
            let state = self.progress.borrow_and_update().clone();
            match state {
                Progress::Promoted { .. } => return Ok(true),
                Progress::Aborted(reason) => return Err(reason),
                Progress::Running => {}
            }
            match tokio::time::timeout_at(deadline, self.progress.changed()).await {
                Ok(Ok(())) => {}
                // Out of time, or the promotion's task is gone (the process
                // is exiting after its record): it has not answered.
                Ok(Err(_)) | Err(_) => return Ok(false),
            }
        }
    }
}

/// What the standby is when its promotion starts ([`StandbyHost::preflight`]).
#[derive(Clone, Debug)]
pub struct StandbyFacts {
    pub data_dir: PathBuf,
    /// Its configuration: the storage options the conversion opens the store
    /// with.
    pub config: Config,
    pub history: Uuid,
    /// The store's epoch `E`; the promotion makes it `E + 1`.
    pub epoch: u64,
    /// The durable source sequence when the promotion started (`C`).
    pub cursor: u64,
    /// The promotion's identity ([`promotion_identity`]).
    pub node_id: Uuid,
    /// The primary it follows, as configured.
    pub primary: Option<String>,
    /// The serving store's engine (`StorageEngine::instance_id`): the
    /// sessions step 3 waits for are the ones it serves.
    pub store_engine: u64,
    /// The placeholder's engine: its sessions are spared at step 2 and
    /// ended after the reopen.
    pub placeholder_engine: u64,
}

/// How an aborted promotion gives the node back its standby.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// Replication never stopped: the store goes back on the listeners.
    Store,
    /// Replication was stopped (and the store maybe closed): the receiver
    /// starts again once the store is closed — or after the budget, with
    /// the holders logged — and attaches it again.
    Replication,
}

/// What the server process does for an online promotion (`main.rs`'s
/// standby runtime): the parts that own its listeners, its receiver, its
/// services and its store.
#[async_trait]
pub trait StandbyHost: Send + Sync + 'static {
    /// Spec P5.2 §2, §8.F, §8.I: the facts of a standby ready to be
    /// promoted, or why it is not — its posture (not `streaming`, nor
    /// `reconnecting` within its lag), an interrupted fast-forward, no store
    /// attached, a primary configuration that cannot be derived, a primary
    /// that would serve its replication port in plaintext where the pair
    /// replicated over TLS, a replication port that cannot be bound —
    /// naming the next command. Binds the primary's replication listener
    /// (kept for the reopen) and has no other side effect: nothing changes
    /// on a refusal.
    async fn preflight(&self) -> Result<StandbyFacts, Refusal>;
    /// The running receiver's command channel; `None` without a receiver
    /// (no session can carry a fence).
    fn receiver(&self) -> Option<mpsc::Sender<ReceiverCommand>>;
    /// Whether the receiver has a live session to its primary now (posture
    /// `streaming` or `catching_up`, not `connecting` / `reconnecting`).
    fn replication_live(&self) -> bool;
    /// The receiver's own bound on a fence's answer (its operation deadline
    /// and one frame).
    fn fence_answer_bound(&self) -> Duration;
    /// Step 2: the listeners (and the cascade holder) serve the placeholder
    /// from now on; the store is kept by the host.
    fn publish_placeholder(&self);
    /// Step 5: the receiver, the standby archiver and the cascading source
    /// stopped, within `budget` (`None`: however long it takes — the old
    /// primary is fenced). Returns the durable source sequence the store
    /// holds.
    async fn stop_replication(&self, budget: Option<Duration>) -> Result<u64, String>;
    /// Step 6: the store's last holder let go of it within `budget` and its
    /// RocksDB handle is closed; `Err` names the holders left. Called again
    /// after an `Err` (a fenced promotion keeps waiting).
    async fn release_store(&self, budget: Duration) -> Result<(), String>;
    /// Step 8: the converted store reopened and served as the primary;
    /// returns its epoch. `Err` is after the conversion: the process exits.
    async fn reopen_as_primary(&self) -> Result<u64, String>;
    /// Abort: back to a following standby ([`Restore`]); `budget` bounds
    /// the wait for a closing store before the receiver starts again.
    async fn restore_standby(&self, restore: Restore, budget: Duration);
    /// The empty database the listeners serve while there is no store.
    fn placeholder(&self) -> Arc<EmbeddedDatabase>;
}

/// Spec P5.1 §11.C: the promotion's identity — a random UUID, created once
/// and kept in the standby's node-local family, never the configured
/// `node_id` — read (or created) through the serving engine's sanctioned
/// node-local write. The offline promotion reads the same key, so a re-run
/// after a crash fences under the same identity and the old primary answers
/// the same `Fenced`.
pub fn promotion_identity(engine: &StorageEngine) -> Result<Uuid, String> {
    let store = engine.db();
    let local = store
        .cf_handle(LOCAL_CF)
        .ok_or_else(|| "the standby's store has no node-local family".to_string())?;
    let stored = store
        .get_cf(local, PROMOTION_NODE_KEY)
        .map_err(|error| format!("the node-local family cannot be read: {error}"))?
        .and_then(|bytes| Uuid::from_slice(&bytes).ok())
        .filter(|node| !node.is_nil());
    if let Some(node) = stored {
        return Ok(node);
    }
    let node = Uuid::new_v4();
    let mut batch = LocalBatch::new();
    batch.put(PROMOTION_NODE_KEY, node.as_bytes());
    engine
        .write_local(batch)
        .map_err(|error| format!("the promotion's identity cannot be kept: {error}"))?;
    Ok(node)
}

/// Spec P5.2 §2: whether the standby's store carries an interrupted
/// fast-forward (read through the serving engine).
pub fn interrupted_fast_forward(engine: &StorageEngine) -> Result<bool, String> {
    let store = engine.db();
    let local = store
        .cf_handle(LOCAL_CF)
        .ok_or_else(|| "the standby's store has no node-local family".to_string())?;
    store
        .get_cf(local, FAST_FORWARD_KEY)
        .map(|record| record.is_some())
        .map_err(|error| format!("the node-local family cannot be read: {error}"))
}

/// Registered PostgreSQL sessions, as an abort message or a diagnostic
/// names them.
pub fn describe_sessions(sessions: &[SessionSummary]) -> String {
    if sessions.is_empty() {
        return "none".to_string();
    }
    sessions
        .iter()
        .map(|session| {
            format!(
                "#{} (pid {} from {}, connected since {}{}{})",
                session.id,
                session
                    .backend_pid
                    .map_or_else(|| "?".to_string(), |pid| pid.to_string()),
                session.peer.as_deref().unwrap_or("?"),
                format_unix_secs(session.admitted_at),
                if session.exempt { ", the caller" } else { "" },
                if session.established { "" } else { ", authenticating" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The online promoter of this process (one promotion at a time).
pub struct Promoter {
    host: Arc<dyn StandbyHost>,
    settings: OnlineSettings,
    /// Unix seconds the promotion under way (or in its preflight) started at.
    running: Mutex<Option<u64>>,
}

static PROMOTER: RwLock<Option<Arc<Promoter>>> = RwLock::new(None);

/// This process is a physical standby that `pg_promote` promotes through
/// `host` (installed by the standby runtime, like `install_standby`).
pub fn install(host: Arc<dyn StandbyHost>, settings: OnlineSettings) {
    *PROMOTER.write() = Some(Arc::new(Promoter {
        host,
        settings,
        running: Mutex::new(None),
    }));
}

/// No online promotion from now on (the node was promoted, or stops).
pub fn uninstall() {
    *PROMOTER.write() = None;
}

/// The installed promoter, if this process is a promotable standby.
pub fn installed() -> Option<Arc<Promoter>> {
    PROMOTER.read().clone()
}

/// Spec P5.2 §2: what `SELECT pg_promote(…)` on the PostgreSQL wire starts
/// (`protocol::postgres::handler`). A refusal changes nothing.
pub async fn request(call: PromoteCall) -> Result<Started, Refusal> {
    match installed() {
        Some(promoter) => promoter.begin(call).await,
        None => Err(not_promotable()),
    }
}

/// The refusal of a node that has no online promoter.
fn not_promotable() -> Refusal {
    if crate::standby_posture::standby().is_some() {
        return Refusal::new(
            "this standby follows archived WAL segments, not a primary: there is no replication session to fence a \
             primary through, so it is not promoted online; promote it offline: stop it, then run `heliosdb-nano \
             promote --force --data-dir <its data directory>`",
        );
    }
    let (epoch, _) = crate::standby_posture::epoch_and_previous(None);
    if let Some(fence) = crate::standby_posture::fence_status() {
        return Refusal::new(format!(
            "this node is a primary (epoch {}) that was fenced by epoch {}: it is not promoted; a primary of epoch {} \
             exists — rebuild this node as its standby (`heliosdb-nano recover --from-primary --primary-host <it> \
             --replace --data-dir <this node's data directory>`)",
            fence.epoch, fence.by_epoch, fence.by_epoch
        ));
    }
    if crate::replication::ha_state::ha_state().get_role() == crate::replication::ha_state::HARole::Primary {
        return Refusal::new(format!("this node is already the primary (epoch {epoch})"));
    }
    Refusal::new(
        "this node is not a physical standby (it was started without --replication-role standby): there is \
         nothing to promote",
    )
}

/// The `running` slot claimed for one promotion's preflight; released on
/// drop unless the promotion was started (the caller's task may be dropped
/// while the preflight awaits: its claim must not outlive it).
struct Claim<'a> {
    running: &'a Mutex<Option<u64>>,
    started: bool,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.started {
            *self.running.lock() = None;
        }
    }
}

/// What the old primary is when a promotion aborts (spec P5.2 §8.G).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OldPrimary {
    /// No fence was sent, or it was refused.
    Unfenced,
    /// A fence was sent and its answer never arrived: it may have been
    /// applied (a fenced primary refuses writes).
    MaybeFenced,
    /// It answered `Fenced`: it refuses every write.
    Fenced { final_sequence: u64 },
}

impl OldPrimary {
    /// [`Self::MaybeFenced`] once a fence was sent, else [`Self::Unfenced`].
    fn after(sent: bool) -> Self {
        if sent {
            Self::MaybeFenced
        } else {
            Self::Unfenced
        }
    }
}

/// Why a promotion is aborted before its record, what the old primary is,
/// what to run next, and how the standby is restored.
#[derive(Clone, Debug)]
struct Abort {
    reason: String,
    old_primary: OldPrimary,
    next: String,
    restore: Restore,
}

impl Abort {
    fn new(reason: impl Into<String>, old_primary: OldPrimary, next: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            old_primary,
            next: next.into(),
            restore: Restore::Store,
        }
    }

    #[must_use]
    fn restoring(mut self, restore: Restore) -> Self {
        self.restore = restore;
        self
    }

    /// The caller's 55000 and the ERROR line: the reason, whether the old
    /// primary is fenced, and the next command (spec P5.2 §8.G).
    fn message(&self, facts: &StandbyFacts) -> String {
        let to_epoch = facts.epoch.saturating_add(1);
        let primary = primary_name(facts);
        let state = match self.old_primary {
            OldPrimary::Unfenced => format!("The old primary at {primary} is NOT fenced"),
            OldPrimary::MaybeFenced => format!(
                "The old primary at {primary} MAY be fenced: a fence by epoch {to_epoch} was sent and its answer did \
                 not arrive (a fenced primary refuses every write; it answers this promotion's fence again the same \
                 way)"
            ),
            OldPrimary::Fenced { final_sequence } => format!(
                "The old primary at {primary} IS fenced by epoch {to_epoch} at its sequence {final_sequence}: it \
                 refuses every write"
            ),
        };
        format!(
            "the online promotion was aborted before anything was converted: {}. {state}; this node stays a standby. \
             Next: {}",
            self.reason, self.next
        )
    }
}

/// The next command of an abort that a later attempt can fix.
fn rerun_next(facts: &StandbyFacts) -> String {
    format!(
        "re-run `SELECT pg_promote(true, N)` on this standby (`/health` reports its posture), or promote it offline: \
         stop it, then run `heliosdb-nano promote --data-dir {}`",
        facts.data_dir.display()
    )
}

/// Where the promotion is, for a panic of its task (spec P5.2 §8.F): before
/// the record it aborts, after it the process exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Steps 1-3: the store is off the listeners, replication runs.
    Draining,
    /// Step 4 once a `Fence` was sent: the old primary may be fenced.
    Fencing,
    /// Step 4 after `Fenced`.
    Fenced { final_sequence: u64 },
    /// Steps 5-6: replication stopped (`fenced`: the old primary's final).
    Stopped { fenced: Option<u64> },
    /// Step 7: the record is written.
    Recorded,
    /// Step 8: the store is converted.
    Converted,
}

impl Promoter {
    async fn begin(self: &Arc<Self>, call: PromoteCall) -> Result<Started, Refusal> {
        // Claimed, then released: never held across the preflight (which
        // reads the store and binds a port), so a second call is refused at
        // once rather than queued behind it (spec P5.2 §8.F).
        let since = {
            let mut running = self.running.lock();
            if let Some(since) = *running {
                return Err(
                    Refusal::new(format!("a promotion is in progress since {}", format_unix_secs(since)))
                        .with_hint("SELECT pg_is_in_recovery() on a new connection answers f once it is done"),
                );
            }
            let since = unix_secs();
            *running = Some(since);
            since
        };
        let mut claim = Claim {
            running: &self.running,
            started: false,
        };
        let asked_at = Instant::now();
        let facts = self.host.preflight().await?;
        claim.started = true;
        drop(claim);
        let to_epoch = facts.epoch.saturating_add(1);
        note_promoting(PromotionStatus {
            to_epoch,
            since,
            caller: call.caller(),
        });
        let (progress, watching) = watch::channel(Progress::Running);
        let started = Started {
            placeholder: self.host.placeholder(),
            progress: watching,
            wait: call.wait,
            budget: call.budget(),
            to_epoch,
        };
        tokio::spawn(supervise(Arc::clone(self), facts, call, Arc::new(progress), asked_at));
        Ok(started)
    }

    /// Abort before the record: the node is a standby again.
    async fn abort(&self, progress: &watch::Sender<Progress>, abort: Abort, facts: &StandbyFacts, budget: Duration) {
        let message = abort.message(facts);
        tracing::error!("promote: {message}");
        self.host.restore_standby(abort.restore, budget).await;
        clear_promoting();
        *self.running.lock() = None;
        progress.send_replace(Progress::Aborted(message));
    }
}

/// How step 4 ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fencing {
    /// The old primary is fenced and this store holds everything through
    /// its final sequence (`held` ≥ it).
    Fenced { final_sequence: u64, held: u64 },
    /// Promoted without the old primary's final sequence (`allow`: it could
    /// not be reached), why; `maybe_fenced` when a fence was sent before.
    Unfenced { reason: String, maybe_fenced: bool },
}

/// The primary, as messages name it.
fn primary_name(facts: &StandbyFacts) -> String {
    facts
        .primary
        .clone()
        .unwrap_or_else(|| "<no primary configured>".to_string())
}

/// Spec P5.2 §8.B: the primary could not be REACHED for
/// `promote_fence_timeout` before it answered a fence — `allow` continues
/// without the fence, `refuse` aborts.
fn without_fence(
    settings: &OnlineSettings,
    facts: &StandbyFacts,
    why: &str,
    maybe_fenced: bool,
) -> Result<Fencing, Abort> {
    if settings.allow_without_fence {
        let reason = format!(
            "{why}; promoting WITHOUT the fence ([replication] promote_without_fence = \"allow\"): any writes the old \
             primary committed past what this node holds are not here, and if it still runs it is {} (stop it, or \
             let the first receiver of epoch {} that reaches it fence it)",
            if maybe_fenced {
                "possibly fenced (a fence was sent, its answer never arrived)"
            } else {
                "NOT fenced"
            },
            facts.epoch.saturating_add(1)
        );
        tracing::warn!("promote: {reason}");
        return Ok(Fencing::Unfenced { reason, maybe_fenced });
    }
    Err(Abort::new(
        format!(
            "the primary at {} did not answer the fence ({why}; [replication] promote_without_fence = \"refuse\")",
            primary_name(facts)
        ),
        OldPrimary::after(maybe_fenced),
        format!(
            "re-run `SELECT pg_promote(true, N)` once the primary is reachable; or promote it offline with \
             `heliosdb-nano promote --force --data-dir {}` (stop this node first), or set [replication] \
             promote_without_fence = \"allow\"",
            facts.data_dir.display()
        ),
    ))
}

/// Spec P5.2 §8.C: after `Fenced`, the catch-up cannot reach the final
/// sequence — the promotion aborts naming both sequences; it never goes on
/// unfenced.
fn catch_up_abort(facts: &StandbyFacts, final_sequence: u64, held: u64, why: &str) -> Abort {
    Abort::new(
        format!(
            "the old primary answered the fence at its sequence {final_sequence}, but this standby could not follow \
             it there: {why}; this standby holds its writes through sequence {held}"
        ),
        OldPrimary::Fenced { final_sequence },
        format!(
            "re-run `SELECT pg_promote(true, N)` once the old primary is reachable again (it answers this promotion \
             the same Fenced, and the catch-up resumes), or accept losing its sequences {}..={final_sequence}: stop \
             this node and run `heliosdb-nano promote --force --data-dir {}`",
            held.saturating_add(1),
            facts.data_dir.display()
        ),
    )
}

/// Ask the receiver for the fence; its events.
async fn send_fence(
    receiver: &mpsc::Sender<ReceiverCommand>,
    facts: &StandbyFacts,
) -> Result<mpsc::UnboundedReceiver<FenceEvent>, String> {
    let (events, watching) = mpsc::unbounded_channel();
    let command = ReceiverCommand::Fence {
        epoch: facts.epoch.saturating_add(1),
        node: facts.node_id,
        events,
    };
    match tokio::time::timeout(Duration::from_secs(5), receiver.send(command)).await {
        Ok(Ok(())) => Ok(watching),
        Ok(Err(_)) => Err("the replication receiver is not running".to_string()),
        Err(_) => Err("the replication receiver did not take the fence (its command queue is full)".to_string()),
    }
}

/// Step 3 (module documentation): until no PostgreSQL session holds the
/// standby's store — `Err` with the ones left at `deadline`.
async fn wait_for_session_holders(facts: &StandbyFacts, deadline: Instant) -> Result<(), Vec<SessionSummary>> {
    loop {
        let holders = registry().holders_of(facts.store_engine);
        if holders.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(holders);
        }
        tokio::time::sleep(HOLDER_POLL).await;
    }
}

/// Step 4 (module documentation): the fence through the live session and
/// the catch-up to the fenced primary's final sequence.
#[allow(clippy::too_many_lines)]
async fn fence_live(
    host: &dyn StandbyHost,
    facts: &StandbyFacts,
    settings: &OnlineSettings,
    phase: &Mutex<Phase>,
) -> Result<Fencing, Abort> {
    let to_epoch = facts.epoch.saturating_add(1);
    let mut answer_bound = FENCE_ANSWER_BOUND.max(host.fence_answer_bound());
    let Some(receiver) = host.receiver() else {
        return Err(Abort::new(
            "this standby's replication receiver is not running, so no session can carry the fence",
            OldPrimary::Unfenced,
            rerun_next(facts),
        ));
    };
    tracing::warn!(
        "promote: fencing the primary at {} through this standby's replication session (epoch {} → {to_epoch}, \
         promotion {}, this standby at sequence {})",
        primary_name(facts),
        facts.epoch,
        facts.node_id,
        facts.cursor
    );
    let begun = Instant::now();
    let mut events = send_fence(&receiver, facts)
        .await
        .map_err(|why| Abort::new(why, OldPrimary::Unfenced, rerun_next(facts)))?;
    let mut attempt = 1_u32;
    // When this attempt's `Fence` went out; whether any attempt's did.
    let mut sent_at: Option<Instant> = None;
    let mut ever_sent = false;
    // The source's answer, and the catch-up after it.
    let mut fenced: Option<Fenced> = None;
    let mut held = facts.cursor;
    let mut advanced_at = Instant::now();
    // Liveness (spec §8.B): the last sign that the source answers, since
    // when it could not be reached, since when this attempt saw it alive
    // with its fence not sent yet.
    let mut answered_at: Option<Instant> = None;
    let mut unreachable_since: Option<Instant> = None;
    let mut alive_unsent_since: Option<Instant> = None;
    let mut unreachable_reason: Option<String> = None;
    // Once the receiver took the command, only a FAILED connection attempt
    // says the source cannot be reached: one still under way may be a slow
    // primary's, which `allow` must never promote past (spec §8.B).
    let mut taken = false;
    let mut failed_at: Option<Instant> = None;
    loop {
        let event = match tokio::time::timeout(FENCE_TICK, events.recv()).await {
            Ok(Some(event)) => Some(event),
            Ok(None) => {
                // The receiver ended (every sender of the events dropped).
                return Err(match fenced {
                    Some(answer) => catch_up_abort(
                        facts,
                        answer.final_sequence,
                        held,
                        "the replication receiver stopped (`/health` says why)",
                    ),
                    None => Abort::new(
                        "the replication receiver stopped before the fence was answered (`/health` says why)",
                        OldPrimary::after(ever_sent),
                        rerun_next(facts),
                    ),
                });
            }
            Err(_) => None,
        };
        let now = Instant::now();
        taken |= event.is_some();
        match event {
            None | Some(FenceEvent::Taken) => {}
            Some(FenceEvent::Attempt { answered: true, reason }) => {
                answered_at = Some(now);
                tracing::info!("promote: the primary answered a connection of the fence's session: {reason}");
            }
            Some(FenceEvent::Attempt {
                answered: false,
                reason,
            }) => {
                failed_at = Some(now);
                unreachable_reason = Some(reason);
            }
            Some(FenceEvent::Connected | FenceEvent::Proven { .. }) => answered_at = Some(now),
            Some(FenceEvent::Sent { answer_within }) => {
                answered_at = Some(now);
                sent_at = Some(now);
                ever_sent = true;
                answer_bound = answer_bound.max(answer_within);
                *phase.lock() = Phase::Fencing;
            }
            Some(FenceEvent::Fenced(answer)) => {
                answered_at = Some(now);
                advanced_at = now;
                if fenced.is_none() {
                    tracing::warn!(
                        "promote: the primary at {} is fenced by epoch {} at its sequence {}; following it through \
                         that sequence (this standby holds {held})",
                        primary_name(facts),
                        answer.epoch,
                        answer.final_sequence
                    );
                }
                fenced = Some(answer);
                *phase.lock() = Phase::Fenced {
                    final_sequence: answer.final_sequence,
                };
            }
            Some(FenceEvent::Advanced { cursor }) => {
                answered_at = Some(now);
                advanced_at = now;
                if cursor > held {
                    tracing::info!(
                        "promote: catching up with the fenced primary: sequence {cursor} of its final {}",
                        fenced.map_or(0, |answer| answer.final_sequence)
                    );
                }
                held = held.max(cursor);
            }
            Some(FenceEvent::ReachedFinal { final_sequence, cursor }) => {
                tracing::info!(
                    "promote: this standby holds everything the fenced primary committed (sequence {cursor}, its \
                     final {final_sequence})"
                );
                return Ok(Fencing::Fenced {
                    final_sequence,
                    held: cursor.max(held),
                });
            }
            Some(FenceEvent::Failed { sent, refused, reason }) => {
                ever_sent |= sent;
                if refused {
                    // The source answers, but will never be fenced by this
                    // promotion: another one fenced it, it is at a newer
                    // epoch, it is another dataset (spec §8.D).
                    return Err(Abort::new(
                        format!(
                            "the primary at {} refused the fence: {reason}; a primary of epoch {to_epoch} (or newer) \
                             may exist",
                            primary_name(facts)
                        ),
                        OldPrimary::after(ever_sent),
                        format!(
                            "find the current primary and recover this standby from it (stop it, then \
                             `heliosdb-nano recover --from-primary --primary-host <the current primary> --data-dir \
                             {}`)",
                            facts.data_dir.display()
                        ),
                    ));
                }
                if now.saturating_duration_since(begun) >= answer_bound.saturating_mul(2) {
                    return Err(Abort::new(
                        format!(
                            "the fence did not complete after {attempt} attempt(s) over {} s: {reason}",
                            now.saturating_duration_since(begun).as_secs()
                        ),
                        OldPrimary::after(ever_sent),
                        rerun_next(facts),
                    ));
                }
                // Not refused (a fence left incomplete, a source draining, a
                // broken session): asked again on the receiver's next session
                // (spec §8.D, §8.E).
                attempt = attempt.saturating_add(1);
                tracing::warn!(
                    "promote: the fence did not complete ({reason}); asking again on a new session (attempt {attempt})"
                );
                events = send_fence(&receiver, facts)
                    .await
                    .map_err(|why| Abort::new(why, OldPrimary::after(ever_sent), rerun_next(facts)))?;
                sent_at = None;
                alive_unsent_since = None;
                taken = false;
                failed_at = None;
                continue;
            }
        }
        // Liveness: the receiver's posture, or a recent sign of the source.
        let alive = host.replication_live()
            || answered_at.is_some_and(|at| now.saturating_duration_since(at) < ANSWERED_WINDOW);
        if alive {
            unreachable_since = None;
        } else if unreachable_since.is_none() {
            unreachable_since = Some(now);
        }
        // Not taken: the receiver is stuck reconnecting (its posture says so).
        // Taken: a connection attempt failed since the last sign of life.
        let failure_seen = !taken || failed_at.is_some_and(|failed| answered_at.map_or(true, |alive| failed >= alive));
        let unreachable = failure_seen
            && unreachable_since.is_some_and(|since| now.saturating_duration_since(since) >= settings.fence_timeout);
        // The backstop of a fence that neither answers nor fails.
        if fenced.is_none()
            && now.saturating_duration_since(begun)
                >= answer_bound.saturating_mul(2).saturating_add(settings.fence_timeout)
        {
            return Err(Abort::new(
                format!(
                    "the fence of the primary at {} neither completed nor failed within {} s",
                    primary_name(facts),
                    now.saturating_duration_since(begun).as_secs()
                ),
                OldPrimary::after(ever_sent),
                rerun_next(facts),
            ));
        }
        let why_unreachable = || {
            format!(
                "it could not be reached for {} s{}",
                settings.fence_timeout.as_secs(),
                unreachable_reason
                    .as_deref()
                    .map(|reason| format!(" (last attempt: {reason})"))
                    .unwrap_or_default()
            )
        };
        if let Some(answer) = fenced {
            // Spec §8.C: never unfenced from here on.
            if unreachable {
                return Err(catch_up_abort(facts, answer.final_sequence, held, &why_unreachable()));
            }
            if now.saturating_duration_since(advanced_at) >= answer_bound {
                return Err(catch_up_abort(
                    facts,
                    answer.final_sequence,
                    held,
                    &format!(
                        "the catch-up made no progress for {} s although the primary answers",
                        answer_bound.as_secs()
                    ),
                ));
            }
            continue;
        }
        if let Some(at) = sent_at {
            // Sent, not answered: the receiver reports the answer (or its
            // failure) within its own deadline; this is the backstop.
            let waited = now.saturating_duration_since(at);
            if waited >= answer_bound.saturating_add(Duration::from_secs(5)) {
                return Err(Abort::new(
                    format!(
                        "the fence was sent to the primary at {} and not answered within {} s",
                        primary_name(facts),
                        waited.as_secs()
                    ),
                    OldPrimary::MaybeFenced,
                    rerun_next(facts),
                ));
            }
            continue;
        }
        if unreachable {
            return without_fence(settings, facts, &why_unreachable(), ever_sent);
        }
        if alive {
            let since = *alive_unsent_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= answer_bound {
                return Err(Abort::new(
                    format!(
                        "the primary at {} answers, but this standby's receiver did not get its fence out within {} s",
                        primary_name(facts),
                        answer_bound.as_secs()
                    ),
                    OldPrimary::after(ever_sent),
                    rerun_next(facts),
                ));
            }
        }
    }
}

/// Stop at an injected failure point (`internal-tests` only).
fn fail_hook(step: &str) -> Result<(), String> {
    #[cfg(feature = "internal-tests")]
    if std::env::var(FAIL_ONLINE_PROMOTE_AT_ENV).is_ok_and(|at| at.trim() == step) {
        return Err(format!("{FAIL_ONLINE_PROMOTE_AT_ENV}={step}: an injected failure"));
    }
    let _ = step;
    Ok(())
}

/// Wait while the pause file exists (`internal-tests` only).
async fn pause_hook() {
    #[cfg(feature = "internal-tests")]
    if let Some(file) = std::env::var_os(PAUSE_ONLINE_PROMOTE_FILE_ENV).map(PathBuf::from) {
        if file.exists() {
            tracing::warn!(
                "{PAUSE_ONLINE_PROMOTE_FILE_ENV}: the promotion waits while {} exists",
                file.display()
            );
            while file.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tracing::warn!("{PAUSE_ONLINE_PROMOTE_FILE_ENV}: the promotion goes on");
        }
    }
}

/// End the process with exit code 2 at once.
#[cfg(unix)]
fn end_process() -> ! {
    // SAFETY: `_exit(2)` takes no pointer and has no precondition; it ends
    // the process without running any destructor or atexit handler, which is
    // the point (spec P5.2 §8.F).
    unsafe { libc::_exit(2) }
}

/// End the process with exit code 2.
#[cfg(not(unix))]
fn end_process() -> ! {
    std::process::exit(2)
}

/// Log `message` at ERROR and on stderr, flush both streams, and end the
/// process with exit code 2 AT ONCE (`_exit`): no destructor, no atexit
/// handler and no other task runs past this point — none of them may touch
/// a store that is half-converted or not served (spec P5.2 §8.F).
fn exit_now(message: &str) -> ! {
    use std::io::Write;
    tracing::error!("{message}");
    eprintln!("{message}");
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    end_process()
}

/// Spec P5.2 §3 step 8: a failure after the record. Nothing half-converted
/// is served: the process exits 2 naming the command that finishes it.
fn exit_after_record(dir: &Path, reason: &str) -> ! {
    exit_now(&format!(
        "promote: the online promotion of {} FAILED after its record ({PROMOTION_FILE}) was written: {reason}. This \
         process exits so that no half-converted store is served; finish the promotion with `heliosdb-nano promote \
         --data-dir {}`, then start the node with `heliosdb-nano start --replication-role primary --data-dir {}`",
        dir.display(),
        dir.display(),
        dir.display()
    ))
}

/// Spec P5.2 §3 step 8: the store is converted, but this process cannot
/// serve it as the primary.
fn exit_after_conversion(dir: &Path, history: Uuid, epoch: u64, reason: &str) -> ! {
    exit_now(&format!(
        "promote: {} is converted to the primary of history {history} at epoch {epoch}, but this process could not \
         serve it as the primary: {reason}. This process exits; start the node with `heliosdb-nano start \
         --replication-role primary --data-dir {}`",
        dir.display(),
        dir.display()
    ))
}

fn record_failure(error: PromoteError) -> String {
    match error {
        PromoteError::Refused(reason) | PromoteError::Failed(reason) => reason,
    }
}

/// The text of a panic's payload.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_string())
}

/// Spec P5.2 §8.F: the promotion's steps on their own task; a panic of it
/// aborts the promotion before the record and ends the process after it.
async fn supervise(
    promoter: Arc<Promoter>,
    facts: StandbyFacts,
    call: PromoteCall,
    progress: Arc<watch::Sender<Progress>>,
    asked_at: Instant,
) {
    let phase = Arc::new(Mutex::new(Phase::Draining));
    let budget = call.budget();
    let task = tokio::spawn(run(
        Arc::clone(&promoter),
        facts.clone(),
        call,
        Arc::clone(&progress),
        asked_at,
        Arc::clone(&phase),
    ));
    let Err(error) = task.await else {
        return;
    };
    if !error.is_panic() {
        // Cancelled: the runtime is shutting down with the process.
        return;
    }
    let what = format!("the promotion panicked: {}", panic_text(error.into_panic().as_ref()));
    let at = *phase.lock();
    let abort = match at {
        Phase::Recorded => exit_after_record(&facts.data_dir, &what),
        Phase::Converted => exit_after_conversion(&facts.data_dir, facts.history, facts.epoch.saturating_add(1), &what),
        Phase::Draining => Abort::new(what, OldPrimary::Unfenced, rerun_next(&facts)),
        Phase::Fencing => Abort::new(what, OldPrimary::MaybeFenced, rerun_next(&facts)),
        Phase::Fenced { final_sequence } => Abort::new(what, OldPrimary::Fenced { final_sequence }, rerun_next(&facts)),
        Phase::Stopped { fenced } => Abort::new(
            what,
            fenced.map_or(OldPrimary::Unfenced, |final_sequence| OldPrimary::Fenced {
                final_sequence,
            }),
            rerun_next(&facts),
        )
        .restoring(Restore::Replication),
    };
    promoter.abort(&progress, abort, &facts, budget).await;
}

/// The promotion's steps (module documentation).
#[allow(clippy::too_many_lines)]
async fn run(
    promoter: Arc<Promoter>,
    facts: StandbyFacts,
    call: PromoteCall,
    progress: Arc<watch::Sender<Progress>>,
    asked_at: Instant,
    phase: Arc<Mutex<Phase>>,
) {
    let host = Arc::clone(&promoter.host);
    let to_epoch = facts.epoch.saturating_add(1);
    let dir = facts.data_dir.clone();
    let budget = call.budget();
    tracing::warn!(
        "promote: an ONLINE promotion of {} to primary was asked by {}: history {}, epoch {} → {to_epoch}, sequence {} \
         (promotion {})",
        dir.display(),
        call.caller(),
        facts.history,
        facts.epoch,
        facts.cursor,
        facts.node_id
    );
    pause_hook().await;
    // Step 2: the store off the listeners, its sessions ended (the caller,
    // exempt, lets go of it itself; the placeholder's are spared). Only a
    // session still authenticating is closed by force: one past it ends
    // after its current message cycle — a statement inside the engine runs
    // to its end and is answered first.
    host.publish_placeholder();
    let (_, targets) = registry().push_scoped(
        TerminateCause::Promotion,
        TerminateScope::NotServedBy(facts.placeholder_engine),
        promotion_termination_message(to_epoch),
    );
    let authenticating = registry().abort_authenticating(&targets);
    tracing::info!(
        "promote: told {} PostgreSQL session(s) of the standby's store to end (FATAL 57P01; {authenticating} still \
         authenticating closed)",
        targets.len()
    );
    // Step 3: every session holder gone, within the caller's budget; the
    // receiver keeps applying meanwhile.
    if let Err(holders) = wait_for_session_holders(&facts, asked_at + budget).await {
        let abort = Abort::new(
            format!(
                "{} PostgreSQL session(s) still use the standby's store {} s after the promotion was asked (they were \
                 told to end with FATAL 57P01; a statement still running is waited for, never cut short): {}",
                holders.len(),
                budget.as_secs(),
                describe_sessions(&holders)
            ),
            OldPrimary::Unfenced,
            "re-run `SELECT pg_promote(true, N)` with a larger N once those statements end (or end those sessions \
             first)",
        );
        return promoter.abort(&progress, abort, &facts, budget).await;
    }
    tracing::info!("promote: no PostgreSQL session holds the standby's store any more");
    // Step 4.
    let fencing = match fence_live(host.as_ref(), &facts, &promoter.settings, &phase).await {
        Ok(fencing) => fencing,
        Err(abort) => return promoter.abort(&progress, abort, &facts, budget).await,
    };
    let fenced_at = match &fencing {
        Fencing::Fenced { final_sequence, .. } => Some(*final_sequence),
        Fencing::Unfenced { .. } => None,
    };
    let maybe_fenced = matches!(&fencing, Fencing::Unfenced { maybe_fenced: true, .. });
    // Step 5: a fenced promotion never aborts on time from here on.
    let reached = match host.stop_replication(fenced_at.is_none().then_some(STOP_BUDGET)).await {
        Ok(reached) => reached,
        Err(reason) => {
            let abort = Abort::new(
                format!("replication could not be stopped: {reason}"),
                OldPrimary::after(maybe_fenced),
                rerun_next(&facts),
            )
            .restoring(Restore::Replication);
            return promoter.abort(&progress, abort, &facts, budget).await;
        }
    };
    *phase.lock() = Phase::Stopped { fenced: fenced_at };
    tracing::info!("promote: replication stopped at sequence {reached} (the standby archive sealed through it)");
    if fenced_at.is_none() {
        tracing::warn!(
            "promote: promoting WITHOUT the fence: this node holds the old primary's writes through sequence {reached}"
        );
    }
    // `internal-tests`: an unfenced promotion that fails here is restored
    // with its replication stopped — the receiver and the cascading port
    // started again (spec P5.2 §8.J, §8.K).
    if fenced_at.is_none() {
        if let Err(reason) = fail_hook("release") {
            let abort =
                Abort::new(reason, OldPrimary::after(maybe_fenced), rerun_next(&facts)).restoring(Restore::Replication);
            return promoter.abort(&progress, abort, &facts, budget).await;
        }
    }
    // Step 6: only the runtime's handles are left; one that persists is a
    // bug, said at ERROR — and waited out once the old primary is fenced.
    loop {
        match host.release_store(RELEASE_BUDGET).await {
            Ok(()) => break,
            Err(reason) if fenced_at.is_some() => tracing::error!(
                "promote: {reason}; the old primary is fenced, so the promotion keeps waiting for the store to close"
            ),
            Err(reason) => {
                let abort = Abort::new(
                    format!("the standby's store could not be released: {reason}"),
                    OldPrimary::after(maybe_fenced),
                    rerun_next(&facts),
                )
                .restoring(Restore::Replication);
                return promoter.abort(&progress, abort, &facts, budget).await;
            }
        }
    }
    tracing::info!("promote: the standby's store is closed");
    // Step 7: the record, then the offline promotion's conversion.
    let record = PromotionRecord {
        format: FORMAT,
        history: facts.history,
        from_epoch: facts.epoch,
        to_epoch,
        started_at: unix_secs(),
        fenced: fenced_at.is_some(),
        old_primary_final_sequence: fenced_at,
        node_id: facts.node_id,
        cursor: facts.cursor,
        reached,
        primary: facts.primary.clone(),
    };
    let not_recorded = |reason: String| {
        Abort::new(
            reason,
            fenced_at.map_or(OldPrimary::after(maybe_fenced), |final_sequence| OldPrimary::Fenced {
                final_sequence,
            }),
            rerun_next(&facts),
        )
        .restoring(Restore::Replication)
    };
    if let Err(reason) = record.validate() {
        return promoter.abort(&progress, not_recorded(reason), &facts, budget).await;
    }
    let written = {
        let (dir, record) = (dir.clone(), record.clone());
        blocking(move || {
            write_new_record_in(&dir, PROMOTION_FILE, &record)
                .map_err(|error| failed(format!("{PROMOTION_FILE} cannot be written: {error}")))
        })
        .await
    };
    if let Err(error) = written {
        // Written whole or not at all: no record, nothing converted.
        return promoter
            .abort(&progress, not_recorded(record_failure(error)), &facts, budget)
            .await;
    }
    *phase.lock() = Phase::Recorded;
    tracing::info!(
        "promote: recorded the promotion in {}",
        dir.join(PROMOTION_FILE).display()
    );
    kill_hook("after-record");
    if let Err(reason) = fail_hook("after-record") {
        exit_after_record(&dir, &reason);
    }
    let converted = {
        let (dir, config, record) = (dir.clone(), facts.config.clone(), record.clone());
        blocking(move || {
            let floor = convert(&dir, &config, &record)?;
            identity_files(&dir, &record)?;
            remove_record_in(&dir, PROMOTION_FILE)
                .map_err(|error| failed(format!("{PROMOTION_FILE} cannot be removed: {error}")))?;
            Ok(floor)
        })
        .await
    };
    match converted {
        Ok(floor) => tracing::info!(
            "promote: {} is the primary of history {} at epoch {to_epoch} (inherited history; logical WAL redo floor \
             {:?}, next LSN past {})",
            dir.display(),
            facts.history,
            floor.checkpoint_after,
            floor.last_lsn
        ),
        Err(error) => exit_after_record(&dir, &record_failure(error)),
    }
    *phase.lock() = Phase::Converted;
    // Step 8.
    let reopened = match fail_hook("reopen") {
        Ok(()) => host.reopen_as_primary().await,
        Err(reason) => Err(reason),
    };
    let epoch = match reopened {
        Ok(epoch) => epoch,
        Err(reason) => exit_after_conversion(&dir, facts.history, to_epoch, &reason),
    };
    // Spec §8.H: the sessions the placeholder served meanwhile (their
    // startup said `in_hot_standby = on`) redial too — only them.
    let (generation, targets) = registry().push_scoped(
        TerminateCause::Promotion,
        TerminateScope::ServedBy(facts.placeholder_engine),
        promotion_termination_message(to_epoch),
    );
    let ended = targets.len();
    tokio::spawn(async move {
        let _ = registry().drain(generation, targets, TERMINATE_DRAIN_TIMEOUT).await;
    });
    clear_promoting();
    uninstall();
    match &fencing {
        Fencing::Fenced { final_sequence, held } => tracing::warn!(
            "promote: this node is the PRIMARY of history {} at epoch {epoch} (from {}); the old primary at {} was \
             fenced at its sequence {final_sequence} and everything through it is here (sequence {held}; {ended} \
             placeholder session(s) ended)",
            facts.history,
            facts.epoch,
            primary_name(&facts)
        ),
        Fencing::Unfenced { reason, .. } => tracing::warn!(
            "promote: this node is the PRIMARY of history {} at epoch {epoch} (from {}), NOT fenced: {reason} (it \
             holds the old primary's writes through sequence {reached})",
            facts.history,
            facts.epoch
        ),
    }
    progress.send_replace(Progress::Promoted { epoch });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn facts() -> StandbyFacts {
        StandbyFacts {
            data_dir: PathBuf::from("/srv/standby"),
            config: Config::default(),
            history: Uuid::new_v4(),
            epoch: 1,
            cursor: 41,
            node_id: Uuid::new_v4(),
            primary: Some("10.0.0.1:5433".into()),
            store_engine: 7,
            placeholder_engine: 9,
        }
    }

    /// Spec P5.2 §8.B, §8.G: a primary that cannot be reached — `allow`
    /// promotes through, saying what may be lost and whether it may be
    /// fenced; `refuse` aborts naming the offline command and the setting,
    /// and says the old primary is not fenced (or may be, once a fence was
    /// sent).
    #[test]
    fn an_unreachable_primary_is_allowed_or_refused_by_the_setting() {
        let allow = OnlineSettings {
            allow_without_fence: true,
            fence_timeout: Duration::from_secs(10),
        };
        match without_fence(&allow, &facts(), "unreachable", false) {
            Ok(Fencing::Unfenced { reason, maybe_fenced }) => {
                assert!(!maybe_fenced);
                assert!(
                    reason.contains("WITHOUT the fence") && reason.contains("NOT fenced"),
                    "{reason}"
                );
            }
            other => panic!("allow continues: {other:?}"),
        }
        let refuse = OnlineSettings {
            allow_without_fence: false,
            ..allow
        };
        let Err(abort) = without_fence(&refuse, &facts(), "unreachable", false) else {
            panic!("refuse aborts");
        };
        let message = abort.message(&facts());
        assert!(
            message.contains("did not answer the fence")
                && message.contains("heliosdb-nano promote --force --data-dir /srv/standby")
                && message.contains("promote_without_fence = \"allow\"")
                && message.contains("is NOT fenced")
                && message.contains("Next: re-run `SELECT pg_promote"),
            "{message}"
        );
        assert_eq!(abort.restore, Restore::Store, "replication never stopped");
        let Err(abort) = without_fence(&refuse, &facts(), "unreachable", true) else {
            panic!("refuse aborts");
        };
        assert!(abort.message(&facts()).contains("MAY be fenced"));
    }

    /// Spec P5.2 §8.C, §8.G: after `Fenced` the abort names both sequences,
    /// says the old primary IS fenced, and names `promote --force` with the
    /// range it would lose.
    #[test]
    fn a_catch_up_abort_names_the_fence_and_the_loss() {
        let abort = catch_up_abort(&facts(), 60, 52, "it could not be reached for 3 s");
        assert_eq!(abort.old_primary, OldPrimary::Fenced { final_sequence: 60 });
        let message = abort.message(&facts());
        assert!(
            message.contains("IS fenced by epoch 2 at its sequence 60")
                && message.contains("holds its writes through sequence 52")
                && message.contains("re-run `SELECT pg_promote(true, N)`")
                && message.contains("accept losing its sequences 53..=60")
                && message.contains("heliosdb-nano promote --force --data-dir /srv/standby"),
            "{message}"
        );
    }

    /// Spec P5.2 §8.B: DRAIN_TIMEOUT + ARCHIVE_SEAL_TIMEOUT + one frame.
    #[test]
    fn the_fence_answer_bound_is_the_sources_own() {
        assert_eq!(FENCE_ANSWER_BOUND, Duration::from_secs(75));
    }

    /// A host whose receiver is the test: [`fence_live`] only asks for the
    /// receiver's channel, its posture and its answer bound.
    struct FakeHost {
        commands: mpsc::Sender<ReceiverCommand>,
        live: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl StandbyHost for FakeHost {
        async fn preflight(&self) -> Result<StandbyFacts, Refusal> {
            panic!("not used by fence_live")
        }
        fn receiver(&self) -> Option<mpsc::Sender<ReceiverCommand>> {
            Some(self.commands.clone())
        }
        fn replication_live(&self) -> bool {
            self.live.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn fence_answer_bound(&self) -> Duration {
            Duration::from_secs(150)
        }
        fn publish_placeholder(&self) {}
        async fn stop_replication(&self, _budget: Option<Duration>) -> Result<u64, String> {
            Ok(0)
        }
        async fn release_store(&self, _budget: Duration) -> Result<(), String> {
            Ok(())
        }
        async fn reopen_as_primary(&self) -> Result<u64, String> {
            Ok(2)
        }
        async fn restore_standby(&self, _restore: Restore, _budget: Duration) {}
        fn placeholder(&self) -> Arc<EmbeddedDatabase> {
            panic!("not used by fence_live")
        }
    }

    fn fake(live: bool) -> (FakeHost, mpsc::Receiver<ReceiverCommand>) {
        let (commands, taken) = mpsc::channel(4);
        (
            FakeHost {
                commands,
                live: std::sync::atomic::AtomicBool::new(live),
            },
            taken,
        )
    }

    fn settings(allow: bool) -> OnlineSettings {
        OnlineSettings {
            allow_without_fence: allow,
            fence_timeout: Duration::from_secs(3),
        }
    }

    async fn next_fence(taken: &mut mpsc::Receiver<ReceiverCommand>) -> mpsc::UnboundedSender<FenceEvent> {
        match taken.recv().await {
            Some(ReceiverCommand::Fence { epoch, events, .. }) => {
                assert_eq!(epoch, 2, "the store's epoch + 1");
                events
            }
            None => panic!("no fence asked"),
        }
    }

    /// Spec P5.2 §8.B: a receiver that never takes the command (stuck
    /// reconnecting to a dead primary) — `allow` goes ahead unfenced once
    /// `promote_fence_timeout` passed, never sooner.
    #[tokio::test(start_paused = true)]
    async fn an_unreachable_primary_is_promoted_past_after_the_fence_timeout() {
        let (host, _taken) = fake(false);
        let started = Instant::now();
        let phase = Mutex::new(Phase::Draining);
        let fencing = fence_live(&host, &facts(), &settings(true), &phase).await;
        assert!(
            matches!(
                fencing,
                Ok(Fencing::Unfenced {
                    maybe_fenced: false,
                    ..
                })
            ),
            "{fencing:?}"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(3) && waited < Duration::from_secs(4),
            "{waited:?}"
        );
    }

    /// Spec P5.2 §8.B: a primary that is slow to accept the renewed session
    /// (no attempt FAILED) is waited for — far past `promote_fence_timeout` —
    /// and fenced; `allow` never promotes past it.
    #[tokio::test(start_paused = true)]
    async fn a_slow_primary_is_waited_for_and_fenced() {
        let (host, mut taken) = fake(false);
        let receiver = tokio::spawn(async move {
            let events = next_fence(&mut taken).await;
            let _ = events.send(FenceEvent::Taken);
            tokio::time::sleep(Duration::from_secs(30)).await;
            for event in [
                FenceEvent::Connected,
                FenceEvent::Proven { cursor: 41 },
                FenceEvent::Sent {
                    answer_within: Duration::from_secs(150),
                },
            ] {
                let _ = events.send(event);
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
            let _ = events.send(FenceEvent::Fenced(Fenced {
                epoch: 2,
                final_sequence: 50,
            }));
            let _ = events.send(FenceEvent::Advanced { cursor: 45 });
            let _ = events.send(FenceEvent::ReachedFinal {
                final_sequence: 50,
                cursor: 50,
            });
            (taken, events)
        });
        let phase = Mutex::new(Phase::Draining);
        let fencing = fence_live(&host, &facts(), &settings(true), &phase).await;
        assert_eq!(
            fencing.map_err(|abort| abort.message(&facts())),
            Ok(Fencing::Fenced {
                final_sequence: 50,
                held: 50
            })
        );
        assert_eq!(*phase.lock(), Phase::Fenced { final_sequence: 50 });
        drop(receiver.await.unwrap());
    }

    /// Spec P5.2 §8.C: after `Fenced`, a primary that cannot be reached any
    /// more aborts the promotion — `allow` or not — naming the fence.
    #[tokio::test(start_paused = true)]
    async fn a_primary_lost_after_fenced_aborts_the_promotion() {
        let (host, mut taken) = fake(false);
        let receiver = tokio::spawn(async move {
            let events = next_fence(&mut taken).await;
            for event in [
                FenceEvent::Taken,
                FenceEvent::Connected,
                FenceEvent::Proven { cursor: 41 },
                FenceEvent::Sent {
                    answer_within: Duration::from_secs(150),
                },
                FenceEvent::Fenced(Fenced {
                    epoch: 2,
                    final_sequence: 60,
                }),
                FenceEvent::Advanced { cursor: 52 },
            ] {
                let _ = events.send(event);
            }
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if events
                    .send(FenceEvent::Attempt {
                        answered: false,
                        reason: "connection refused".into(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            taken
        });
        let phase = Mutex::new(Phase::Draining);
        let Err(abort) = fence_live(&host, &facts(), &settings(true), &phase).await else {
            panic!("never unfenced after Fenced");
        };
        assert_eq!(abort.old_primary, OldPrimary::Fenced { final_sequence: 60 });
        let message = abort.message(&facts());
        assert!(
            message.contains("holds its writes through sequence 52") && message.contains("53..=60"),
            "{message}"
        );
        drop(receiver.await.unwrap());
    }

    /// Spec P5.2 §8.D: a fence left incomplete (`SourceNotServing`) is asked
    /// again and completes; a REFUSED one aborts at once.
    #[tokio::test(start_paused = true)]
    async fn an_incomplete_fence_is_asked_again_and_a_refused_one_aborts() {
        let (host, mut taken) = fake(true);
        let receiver = tokio::spawn(async move {
            let events = next_fence(&mut taken).await;
            for event in [
                FenceEvent::Taken,
                FenceEvent::Connected,
                FenceEvent::Proven { cursor: 41 },
                FenceEvent::Sent {
                    answer_within: Duration::from_secs(150),
                },
                FenceEvent::Failed {
                    sent: true,
                    refused: false,
                    reason: "physical replication refused: SourceNotServing: fence incomplete".into(),
                },
            ] {
                let _ = events.send(event);
            }
            let again = next_fence(&mut taken).await;
            for event in [
                FenceEvent::Connected,
                FenceEvent::Sent {
                    answer_within: Duration::from_secs(150),
                },
                FenceEvent::Fenced(Fenced {
                    epoch: 2,
                    final_sequence: 44,
                }),
                FenceEvent::ReachedFinal {
                    final_sequence: 44,
                    cursor: 44,
                },
            ] {
                let _ = again.send(event);
            }
            (taken, again)
        });
        let phase = Mutex::new(Phase::Draining);
        let fencing = fence_live(&host, &facts(), &settings(false), &phase).await;
        assert_eq!(
            fencing.map_err(|abort| abort.message(&facts())),
            Ok(Fencing::Fenced {
                final_sequence: 44,
                held: 44
            })
        );
        drop(receiver.await.unwrap());

        let (host, mut taken) = fake(true);
        let receiver = tokio::spawn(async move {
            let events = next_fence(&mut taken).await;
            let _ = events.send(FenceEvent::Taken);
            let _ = events.send(FenceEvent::Failed {
                sent: false,
                refused: true,
                reason: "this primary was already fenced by epoch 2 (node x)".into(),
            });
            (taken, events)
        });
        let Err(abort) = fence_live(&host, &facts(), &settings(true), &phase).await else {
            panic!("a refused fence aborts, allow or not");
        };
        let message = abort.message(&facts());
        assert!(
            message.contains("refused the fence")
                && message.contains("is NOT fenced")
                && message.contains("heliosdb-nano recover --from-primary"),
            "{message}"
        );
        drop(receiver.await.unwrap());
    }

    #[test]
    fn sessions_are_described_for_an_abort() {
        let session = SessionSummary {
            id: 3,
            backend_pid: Some(4242),
            peer: Some("127.0.0.1:50000".into()),
            admitted_at: 0,
            admitted_generation: 0,
            exempt: false,
            established: true,
            served_by: 7,
        };
        let text = describe_sessions(&[session]);
        assert!(text.contains("#3 (pid 4242 from 127.0.0.1:50000"), "{text}");
        assert_eq!(describe_sessions(&[]), "none");
    }

    #[tokio::test]
    async fn the_answer_waits_for_the_promotion_or_its_budget() {
        let placeholder = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let started = |wait: bool, budget: Duration| {
            let (progress, watching) = watch::channel(Progress::Running);
            (
                progress,
                Started {
                    placeholder: Arc::clone(&placeholder),
                    progress: watching,
                    wait,
                    budget,
                    to_epoch: 2,
                },
            )
        };
        // wait = false: `t` at once.
        let (_progress, now) = started(false, Duration::from_secs(60));
        assert_eq!(now.answer().await, Ok(true));
        // wait = true, promoted.
        let (progress, waiting) = started(true, Duration::from_secs(60));
        let answer = tokio::spawn(waiting.answer());
        progress.send_replace(Progress::Promoted { epoch: 2 });
        assert_eq!(answer.await.unwrap(), Ok(true));
        // wait = true, out of time: `f`, the promotion goes on.
        let (progress, short) = started(true, Duration::from_millis(50));
        assert_eq!(short.answer().await, Ok(false));
        drop(progress);
        // wait = true, aborted: the reason.
        let (progress, aborted) = started(true, Duration::from_secs(60));
        progress.send_replace(Progress::Aborted("refused".into()));
        assert_eq!(aborted.answer().await, Err("refused".to_string()));
        let (_progress, message) = started(true, Duration::from_secs(1));
        assert_eq!(
            message.termination_message(),
            "terminating connection: this standby is being promoted to primary (epoch 2); reconnect"
        );
    }
}
