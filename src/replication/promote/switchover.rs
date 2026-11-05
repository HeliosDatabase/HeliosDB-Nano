//! Spec R3b (Sprinter bbba90ddfc61): `SWITCHOVER TO '<target>'`, run on the
//! PRIMARY — Oracle Data Guard's `SWITCHOVER`, both roles converted in one
//! command: the target standby is promoted online and this primary is demoted
//! in-process into its standby. Built only from the P5 pieces: the target's
//! online promotion (`super::online`, spec P5.2), this primary's fence
//! (`replication::fence`, spec P5.1) and the rewind of a former primary
//! (`replication::recover_primary`, spec R3a).
//!
//! # Steps
//!
//! 1. **Preflight** ([`check`]; `SWITCHOVER CHECK TO '<target>'` answers it
//!    as one row per check — every check, in order, whatever failed — and
//!    changes nothing, here or on the target): this node is a primary, not
//!    fenced, with nothing else under way; it can be demoted (its host says:
//!    a persistent store, a replication port, no MySQL listener, no dump on
//!    shutdown); its directory can be rewound in place once its store is
//!    released (the rewind's own census, read-only — past the fence the
//!    demotion never full-copies in-process, so a directory it could not
//!    rewind is refused HERE); the target is ONE registered standby (by its
//!    node id, its replication peer address as `/health` lists it, or its
//!    alias), `streaming`, within `max_standby_lag`, on a session that
//!    offered [`CAP_PROMOTE`](crate::replication::physical_wire::CAP_PROMOTE);
//!    the target's own promotion preflight passes (asked over its session,
//!    the check form of the request: nothing changes there); this node,
//!    demoted, can follow the address the target serves its replication port
//!    on (it resolves, the transport this node would use is allowed, a pair
//!    that replicated over TLS keeps doing so); and the OTHER standbys of this
//!    primary are named (they keep following a node that becomes a standby:
//!    each is rebuilt against the new primary afterwards).
//! 2. **Ask the target**: a [`PromoteRequest`] for epoch `E + 1` over its live
//!    session, carrying the bound its answer must come within. A refusal, or
//!    a request that never reached the target, ends the switchover with
//!    nothing changed (SQLSTATE 55000 naming why). Accepted — or delivered
//!    and never answered (an answer lost after the target took the request:
//!    the target may be promoting, so that is NEVER "nothing changed") — the
//!    switchover goes on to step 3, which settles it.
//! 3. **Wait** for this node's fence by `E + 1` (the target fences THIS
//!    primary through a renewed session: its write gate fenced, its sessions
//!    ended with FATAL 57P01, its archive sealed through `final`), then for
//!    the target to answer as the primary of `E + 1` on its replication port.
//! 4. **Demote** this node in-process ([`DemotionHost::demote`]): its
//!    sessions off the store, its replication source and archiver stopped,
//!    the store released, rewound against the new primary (no divergence is
//!    possible after a clean fence: only what the new primary wrote since
//!    travels), reopened as a physical standby following it, its role
//!    recorded beside the data directory for the next start.
//! 5. **Answer** the caller, whose session then ends with FATAL 57P01 like a
//!    promotion's.
//!
//! The whole switchover runs on its own task from the moment its slot is
//! claimed: a caller that disconnects, or whose statement is cancelled, never
//! abandons a request the target may already be acting on.
//!
//! # Failure handling (never two writable primaries)
//!
//! Anything before the target took the request changes nothing. Once it took
//! it: a fence that never comes (its promotion aborted, or never started)
//! leaves this node an unfenced primary, said so; a target that does not come
//! up as the primary leaves this node FENCED, up, serving reads, naming the
//! commands that finish the switchover; a demotion that fails before the
//! store was released leaves it fenced and up the same way, and so does a
//! rewind that changed nothing of the directory (its store is reopened
//! fenced); one that fails after the rewind changed the directory ends the
//! process with exit code 2 — the directory then starts in no role until
//! `recover --from-primary` finishes it. Every wait is bounded.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::time::Instant;
use uuid::Uuid;

use super::online::{Refusal, FENCE_ANSWER_BOUND};
use super::unix_secs;
use crate::replication::physical_wire::{PromoteAnswer, PromoteRequest, MAX_PROMOTE_ANSWER_SECS};
use crate::standby_posture::format_unix_secs;
use crate::EmbeddedDatabase;

/// The deadline a switchover gives the target's promotion (its
/// `wait_seconds`): the drain of its sessions, then its fence, catch-up,
/// conversion and reopen.
pub const SWITCHOVER_DEADLINE_SECS: u32 = 60;
/// How often the waits look again.
const POLL: Duration = Duration::from_millis(200);
/// How long the target may take, once fenced, to answer as the primary: its
/// catch-up to the final sequence, its conversion and reopen (on top of the
/// promotion's deadline).
const SERVE_BOUND: Duration = Duration::from_secs(120);
/// What the asker allows on top of the answer bound a request carries: the
/// answer's frame on the wire, and its way back to the asker.
const ANSWER_SLACK: Duration = Duration::from_secs(5);
/// `internal-tests` builds only: the seconds a switchover waits, once this
/// primary is fenced, for the target to answer as the primary — its
/// failure test's bound instead of the production one.
pub const SERVE_SECS_ENV: &str = "HELIOSDB_NANO_TEST_SWITCHOVER_SERVE_SECS";
/// `internal-tests` builds only: the answer bound (seconds) a switchover's
/// request carries instead of the host's — the late-answer test's.
pub const ANSWER_SECS_ENV: &str = "HELIOSDB_NANO_TEST_SWITCHOVER_ANSWER_SECS";

/// The wait for the target to serve, once fenced ([`SERVE_BOUND`] plus the
/// promotion's deadline; [`SERVE_SECS_ENV`] in `internal-tests` builds).
fn serve_bound() -> Duration {
    #[cfg(feature = "internal-tests")]
    if let Some(secs) = std::env::var(SERVE_SECS_ENV)
        .ok()
        .and_then(|secs| secs.trim().parse::<u64>().ok())
    {
        return Duration::from_secs(secs);
    }
    SERVE_BOUND.saturating_add(Duration::from_secs(u64::from(SWITCHOVER_DEADLINE_SECS)))
}

/// The answer bound a request of this node carries, whole seconds within
/// what the wire allows ([`ANSWER_SECS_ENV`] in `internal-tests` builds).
fn answer_secs(host: &dyn DemotionHost) -> u32 {
    #[cfg(feature = "internal-tests")]
    if let Some(secs) = std::env::var(ANSWER_SECS_ENV)
        .ok()
        .and_then(|secs| secs.trim().parse::<u32>().ok())
    {
        return secs.clamp(1, MAX_PROMOTE_ANSWER_SECS);
    }
    u32::try_from(host.answer_bound().as_secs())
        .unwrap_or(MAX_PROMOTE_ANSWER_SECS)
        .clamp(1, MAX_PROMOTE_ANSWER_SECS)
}

/// One row of `SWITCHOVER CHECK`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRow {
    pub check: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl CheckRow {
    fn passed(check: &'static str, detail: impl Into<String>) -> Self {
        Self {
            check,
            ok: true,
            detail: detail.into(),
        }
    }

    fn failed(check: &'static str, detail: impl Into<String>) -> Self {
        Self {
            check,
            ok: false,
            detail: detail.into(),
        }
    }

    /// A check that could not be made because one before it failed: listed,
    /// failed, saying so (every check has its row).
    fn skipped(check: &'static str, because: &str) -> Self {
        Self::failed(check, format!("not checked: {because}"))
    }
}

/// What a primary that can be demoted is ([`DemotionHost::preflight`]).
#[derive(Clone, Debug)]
pub struct PrimaryFacts {
    pub data_dir: std::path::PathBuf,
    pub history: Uuid,
    /// The store's epoch `E`; the target is promoted to `E + 1`.
    pub epoch: u64,
    /// This primary's node id (what the target is told asked it).
    pub node_id: Uuid,
    /// `[replication] max_standby_lag`: how far behind the target may be.
    pub max_standby_lag: Duration,
}

/// What the demotion is asked to do ([`DemotionHost::demote`]).
#[derive(Clone, Debug)]
pub struct DemotionPlan {
    pub switchover: Uuid,
    pub history: Uuid,
    /// This node's epoch, and the new primary's.
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// This node's last sequence (its fence's).
    pub final_sequence: u64,
    /// The new primary's replication port, as this node follows it.
    pub follow: String,
    /// The pair replicated over TLS: the demoted node follows over TLS (and
    /// records so beside its directory).
    pub pair_tls: bool,
}

/// Why a demotion did not complete. This node is still the fenced primary,
/// up, serving reads (nothing of its directory changed). The demotion ends
/// the process itself when it fails after its directory was changed.
#[derive(Clone, Debug)]
pub struct DemotionError(pub String);

/// What the server process does for a switchover on its primary (`main.rs`):
/// the parts that own its listeners, its replication source, its store.
/// Every method but [`Self::demote`] changes nothing (`SWITCHOVER CHECK`
/// calls them).
#[async_trait]
pub trait DemotionHost: Send + Sync + 'static {
    /// The facts of a primary this switchover can demote, or why it cannot
    /// be (in memory, no replication port, MySQL listeners or a dump on
    /// shutdown, which a standby does not serve).
    async fn preflight(&self) -> Result<PrimaryFacts, String>;
    /// Spec R3a: whether this node's directory could be rewound in place
    /// once its store is released — the rewind's census, read-only (one
    /// line saying what it would remove and keep), or why not.
    async fn rewind_check(&self) -> Result<String, String>;
    /// How this node, demoted, follows `endpoint` (the target's replication
    /// port), as one line — or why it cannot: an address it cannot resolve,
    /// a transport it is not allowed (plaintext to another host without the
    /// opt-in), plaintext where the pair replicated over TLS (`pair_tls`).
    fn follow_plan(&self, endpoint: &str, pair_tls: bool) -> Result<String, String>;
    /// Whether `endpoint` answers as the primary of `history` at `epoch` (a
    /// short replication handshake, no session opened).
    async fn serves_as_primary(
        &self,
        endpoint: &str,
        history: Uuid,
        epoch: u64,
        pair_tls: bool,
    ) -> Result<bool, String>;
    /// The empty database the caller's session moves to before the store
    /// is released (the listeners serve it during the demotion).
    fn placeholder(&self) -> Arc<EmbeddedDatabase>;
    /// How long the target may take to answer a delivered request (its
    /// promotion's preflight, one frame): carried in the request, which the
    /// target answers within.
    fn answer_bound(&self) -> Duration;
    /// How long a posted request may wait for the target's next poll to
    /// carry it.
    fn delivery_bound(&self) -> Duration;
    /// The flags a `recover` / `start` this node prints must carry to reach
    /// the new primary — its replication token and, when the pair replicated
    /// over TLS (`pair_tls`), `--primary-tls` and the CA (and server name) it
    /// verifies with — each with a leading space; empty when none applies.
    fn follow_flags(&self, pair_tls: bool) -> String;
    /// Step 4 (module documentation): demote this node into a standby of
    /// `plan.follow`. Returns once it serves as that standby. A failure after
    /// the directory was changed ends the process (exit code 2) instead of
    /// returning.
    async fn demote(&self, plan: DemotionPlan) -> Result<String, DemotionError>;
}

static HOST: parking_lot::RwLock<Option<Arc<dyn DemotionHost>>> = parking_lot::RwLock::new(None);
/// Unix seconds the switchover under way started at (0: none).
static RUNNING: AtomicU64 = AtomicU64::new(0);
/// Review (third pass, item 2): unix seconds until which the slot stays
/// closed after a switchover whose target took the request and did not fence
/// this primary in time (0: none) — its promotion may still fence it, and a
/// new switchover must never take that late fence for its own.
static LINGER_UNTIL: AtomicU64 = AtomicU64::new(0);
/// How long, after such a switchover, a late fence by its target is still
/// possible: the target's promotion deadline and the fence's own bound
/// (whatever its caller's wait, an answered fence is waited for that long).
fn late_fence_window() -> Duration {
    Duration::from_secs(u64::from(SWITCHOVER_DEADLINE_SECS)).saturating_add(FENCE_ANSWER_BOUND)
}

/// Why the slot is closed now: a switchover in progress, or the window after
/// one whose target may still fence this primary.
fn slot_closed_reason(running: u64) -> String {
    let linger = LINGER_UNTIL.load(Ordering::Acquire);
    if linger > unix_secs() {
        format!(
            "the switchover started at {} saw no fence by its target, whose promotion may still fence this primary \
             until {}: a new switchover waits until then (its fence could be taken for the new one's)",
            format_unix_secs(running),
            format_unix_secs(linger)
        )
    } else {
        format!("a switchover is in progress since {}", format_unix_secs(running))
    }
}

/// This process is a primary that `SWITCHOVER TO` can demote through `host`.
pub fn install(host: Arc<dyn DemotionHost>) {
    *HOST.write() = Some(host);
}

/// No switchover from now on (the node stops, or was demoted).
pub fn uninstall() {
    *HOST.write() = None;
}

fn installed() -> Option<Arc<dyn DemotionHost>> {
    HOST.read().clone()
}

// ---------------------------------------------------------------------------
// The target's sessions, and the request waiting for one of them.
// ---------------------------------------------------------------------------

/// A replication session of this primary whose receiver offered
/// `CAP_PROMOTE`.
#[derive(Clone, Debug)]
struct Promotable {
    id: u64,
    node_id: Uuid,
    peer: Option<SocketAddr>,
    tls: bool,
}

static SESSIONS: Mutex<Vec<Promotable>> = Mutex::new(Vec::new());
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// The registration of a promotable session, removed when it drops (the
/// session ended).
pub struct PromotableSession {
    id: u64,
    node_id: Uuid,
    peer: Option<SocketAddr>,
}

impl Drop for PromotableSession {
    fn drop(&mut self) {
        SESSIONS.lock().retain(|session| session.id != self.id);
    }
}

impl PromotableSession {
    /// The request waiting for this session's receiver, taken (it is
    /// delivered once). Its delivery is recorded under the same lock its
    /// withdrawal takes, so a request is either withdrawn undelivered or
    /// known delivered — never both.
    pub fn take_request(&self) -> Option<PromoteRequest> {
        let mut pending = PENDING.lock();
        let waiting = pending
            .as_mut()
            .filter(|pending| pending.target == self.node_id && pending.delivered.is_none())?;
        waiting.delivered = Some((Instant::now(), self.peer));
        Some(waiting.request)
    }

    /// The receiver answered `request` (or the session ended first: `None`).
    pub fn answered(&self, request: &PromoteRequest, answer: Option<PromoteAnswer>) {
        let mut pending = PENDING.lock();
        if let Some(waiting) = pending
            .as_mut()
            .filter(|pending| pending.request.switchover == request.switchover)
        {
            if let Some(reply) = waiting.reply.take() {
                let _ = reply.send(Delivered {
                    answer,
                    peer: self.peer,
                });
            }
        }
    }
}

/// Spec R3b: register a replication session of this primary whose receiver
/// `node_id` offered `CAP_PROMOTE` (`physical_source`'s observer, at its
/// admission). `tls`: the session runs over TLS on both ends.
pub fn note_promotable(node_id: Uuid, peer: Option<SocketAddr>, tls: bool) -> PromotableSession {
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    SESSIONS.lock().push(Promotable { id, node_id, peer, tls });
    PromotableSession { id, node_id, peer }
}

/// The receiver's answer and where its session came from.
struct Delivered {
    answer: Option<PromoteAnswer>,
    peer: Option<SocketAddr>,
}

/// A request waiting for its target's next poll.
struct Pending {
    target: Uuid,
    request: PromoteRequest,
    /// When, and to which session's peer, it was delivered.
    delivered: Option<(Instant, Option<SocketAddr>)>,
    reply: Option<oneshot::Sender<Delivered>>,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

/// What became of a request ([`ask`]).
enum Asked {
    /// The target answered.
    Answered {
        answer: PromoteAnswer,
        peer: Option<SocketAddr>,
    },
    /// It never reached the target: nothing changed there.
    NotDelivered(String),
    /// The target took it, and no answer came back (its session ended, or
    /// the answer outlasted its bound): the target may be acting on it.
    Unanswered { reason: String, peer: Option<SocketAddr> },
}

/// What a withdrawal found ([`withdraw`]).
enum Withdrawn {
    /// It was still waiting, never delivered: nothing reached the target.
    Undelivered,
    /// It had been delivered, to a session from this peer.
    Delivered(Option<SocketAddr>),
    /// It was not there any more: it cannot be shown undelivered.
    Gone,
}

/// The pending request of `switchover` withdrawn, if it is still there:
/// whether (and to whom) it had been delivered. One critical section with
/// [`PromotableSession::take_request`]'s, so nothing is delivered after it.
fn withdraw(switchover: Uuid) -> Withdrawn {
    let mut pending = PENDING.lock();
    if pending
        .as_ref()
        .is_some_and(|pending| pending.request.switchover == switchover)
    {
        match pending.take().and_then(|pending| pending.delivered) {
            Some((_, peer)) => Withdrawn::Delivered(peer),
            None => Withdrawn::Undelivered,
        }
    } else {
        Withdrawn::Gone
    }
}

/// The request withdrawn when its asker stops waiting (a runtime shutting
/// down drops the task).
struct Posted {
    switchover: Uuid,
}

impl Drop for Posted {
    fn drop(&mut self) {
        let _ = withdraw(self.switchover);
    }
}

/// Post `request` for the receiver `target` and wait for what becomes of
/// it: at most `delivery` for the target's next poll to carry it, then the
/// request's own answer bound (the one the target answers within) and a
/// little more.
async fn ask(target: Uuid, request: PromoteRequest, delivery: Duration) -> Asked {
    let (reply, mut answer) = oneshot::channel();
    {
        let mut pending = PENDING.lock();
        if pending.is_some() {
            return Asked::NotDelivered("another switchover request is waiting for its standby".to_string());
        }
        *pending = Some(Pending {
            target,
            request,
            delivered: None,
            reply: Some(reply),
        });
    }
    let _posted = Posted {
        switchover: request.switchover,
    };
    let answer_wait = Duration::from_secs(u64::from(request.answer_secs)).saturating_add(ANSWER_SLACK);
    let posted_at = Instant::now();
    let from = |delivered: Delivered| match delivered.answer {
        Some(answer) => Asked::Answered {
            answer,
            peer: delivered.peer,
        },
        None => Asked::Unanswered {
            reason: "the standby's replication session ended after it took the request, before its answer \
                     arrived"
                .to_string(),
            peer: delivered.peer,
        },
    };
    loop {
        let delivered_at = PENDING
            .lock()
            .as_ref()
            .filter(|pending| pending.request.switchover == request.switchover)
            .and_then(|pending| pending.delivered.map(|(at, _)| at));
        let deadline = match delivered_at {
            Some(at) => at + answer_wait,
            None => posted_at + delivery,
        };
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout(deadline.saturating_duration_since(now).min(POLL), &mut answer).await {
            Ok(Ok(delivered)) => {
                let _ = withdraw(request.switchover);
                return from(delivered);
            }
            // The reply was dropped unsent: only a withdrawal does that.
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }
    // Out of time: withdrawn in one critical section with its delivery, then
    // an answer sent before the withdrawal is still taken.
    let delivered = withdraw(request.switchover);
    if let Ok(answered) = answer.try_recv() {
        return from(answered);
    }
    match delivered {
        Withdrawn::Undelivered => Asked::NotDelivered(format!(
            "the standby did not poll its replication session within {} s, so the request was never delivered",
            delivery.as_secs()
        )),
        Withdrawn::Delivered(peer) => Asked::Unanswered {
            reason: format!(
                "the standby took the request but did not answer within {} s",
                answer_wait.as_secs()
            ),
            peer,
        },
        Withdrawn::Gone => Asked::Unanswered {
            reason: "the request's delivery cannot be ruled out".to_string(),
            peer: None,
        },
    }
}

// ---------------------------------------------------------------------------
// The preflight.
// ---------------------------------------------------------------------------

/// The target as the preflight found it.
#[derive(Clone, Debug)]
struct Target {
    node_id: Uuid,
    /// Its replication session's peer address.
    peer: Option<SocketAddr>,
    /// Its session runs over TLS on both ends: the pair replicates over TLS.
    tls: bool,
    /// What this node follows once demoted.
    follow: String,
    /// Review (third pass, item 2): the identity its promotion fences with
    /// (the answer's `promotion`), `None` until its answer to the REAL
    /// request said it (a lost answer: then the fence is told by its epoch
    /// alone, which the slot kept closed after a failed switchover makes
    /// unambiguous).
    promotion: Option<Uuid>,
}

/// Which registered standby `name` names: its node id, its replication
/// peer address as `/health` lists it, or its topology alias.
fn resolve(name: &str) -> Result<crate::replication::ha_state::StandbyInfo, String> {
    let standbys = crate::replication::ha_state::ha_state().get_standbys();
    let alias = crate::replication::topology_manager().resolve_node_id(name);
    let id = Uuid::parse_str(name.trim()).ok().or(alias);
    let matching: Vec<_> = standbys
        .iter()
        .filter(|standby| Some(standby.node_id) == id || standby.address == name.trim())
        .cloned()
        .collect();
    let mut distinct: Vec<Uuid> = matching.iter().map(|standby| standby.node_id).collect();
    distinct.sort();
    distinct.dedup();
    match (matching.first(), distinct.len()) {
        (Some(standby), 1) => Ok(standby.clone()),
        (None, _) => {
            Err(format!(
            "no standby of this primary is named '{name}' (by node id, replication address or alias); its standbys: \
             {}",
            if standbys.is_empty() {
                "none".to_string()
            } else {
                standbys
                    .iter()
                    .map(|standby| format!("{} at {} ({})", standby.node_id, standby.address, standby.state.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ))
        }
        (Some(_), _) => Err(format!(
            "'{name}' names more than one standby ({}): name it by its node id",
            distinct.iter().map(Uuid::to_string).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The address this node follows the target at, from its answer and its
/// session's peer address: the advertised endpoint, the peer's address at
/// its replication port when it listens on every address, and a loopback
/// endpoint only when the two are on one host.
fn follow_endpoint(answer: &PromoteAnswer, peer: Option<SocketAddr>) -> Result<String, String> {
    if answer.replication_port == 0 {
        return Err(
            "the target would serve no replication port once promoted (it has no replication token, or \
             --physical-replication=false): this node could not follow it"
                .to_string(),
        );
    }
    let at_peer = || {
        peer.map(|peer| SocketAddr::new(peer.ip(), answer.replication_port).to_string())
            .ok_or_else(|| "the target's session has no peer address to follow it at".to_string())
    };
    match answer.endpoint.parse::<SocketAddr>() {
        Ok(address) if address.ip().is_unspecified() => at_peer(),
        Ok(address) if address.ip().is_loopback() => match peer {
            Some(peer) if !peer.ip().is_loopback() => Err(format!(
                "the target would listen for replication on {address} (loopback only: it serves no TLS and has no \
                 plaintext opt-in), and this node reaches it from another host ({}): give the target \
                 --replication-tls (and this node the CA), or switch over to a standby on this host",
                peer.ip()
            )),
            _ => Ok(address.to_string()),
        },
        Ok(address) => Ok(address.to_string()),
        Err(_) if !answer.endpoint.is_empty() => Ok(answer.endpoint.clone()),
        Err(_) => at_peer(),
    }
}

/// The newest promotable session of the receiver `node_id`.
fn session_of(node_id: Uuid) -> Option<Promotable> {
    SESSIONS
        .lock()
        .iter()
        .rev()
        .find(|session| session.node_id == node_id)
        .cloned()
}

/// Spec R3b (review item 6d): the other standbys of this primary — they
/// keep following a node that becomes a standby — as one line each, naming
/// what rebuilds each against the new primary (`follow`, when known).
fn other_standbys(target: Option<Uuid>, follow: Option<&str>, pair_tls: bool) -> Vec<String> {
    let new_primary = follow.unwrap_or("<the new primary's replication port>");
    // Review (third pass, item 7): a pair that replicated over TLS is
    // followed over TLS by every standby of it.
    let tls = if pair_tls {
        " --primary-tls --primary-ca <the CA of the new primary's certificate>"
    } else {
        ""
    };
    crate::replication::ha_state::ha_state()
        .get_standbys()
        .into_iter()
        .filter(|standby| Some(standby.node_id) != target)
        .map(|standby| {
            format!(
                "{} at {} ({}) follows this primary and is not re-pointed: once this node is demoted, rebuild it \
                 against the new primary with `heliosdb-nano recover --from-primary --data-dir <its data directory> \
                 --primary-host {new_primary} --replication-token-file <its token file>{tls} --replace --then stop` \
                 (a fast-forward; `--replace` only lets a full copy, when one is needed, move the old directory aside \
                 — never deleted), then start it again through its service manager with --replication-role standby \
                 --primary-host {new_primary}{tls}",
                standby.node_id,
                standby.address,
                standby.state.as_str()
            )
        })
        .collect()
}

/// The preflight's rows for the other standbys: one per standby, or one
/// saying there is none.
fn other_standby_rows(target: Option<Uuid>, follow: Option<&str>, pair_tls: bool) -> Vec<CheckRow> {
    let others = other_standbys(target, follow, pair_tls);
    if others.is_empty() {
        return vec![CheckRow::passed(
            "other standbys",
            "none: no other standby follows this primary",
        )];
    }
    others
        .into_iter()
        .map(|detail| CheckRow::passed("other standby", detail))
        .collect()
}

/// Step 1 of the module documentation, as rows — EVERY check, in order (a
/// check that could not be made because one before it failed has its row,
/// failed, saying so); `Some` when every check passed. `claimed`: the caller
/// holds the switchover slot (a `SWITCHOVER TO`), so a switchover in progress
/// is not one of the failures. Nothing changes here or on the target: the
/// target is asked the check form of the request.
async fn preflight(
    name: &str,
    host: Option<&Arc<dyn DemotionHost>>,
    claimed: bool,
) -> (Vec<CheckRow>, Option<(PrimaryFacts, Target)>) {
    let mut rows = Vec::new();
    // 1. This node.
    let role = crate::replication::ha_state::ha_state().get_role();
    let running = if claimed { 0 } else { RUNNING.load(Ordering::Acquire) };
    let node_ok = if role != crate::replication::ha_state::HARole::Primary {
        rows.push(CheckRow::failed(
            "primary",
            "this node is not a primary: a switchover is run on the primary",
        ));
        false
    } else if crate::standby_posture::is_fenced() {
        rows.push(CheckRow::failed(
            "primary",
            "this primary is fenced (a standby was promoted over it): stop it, rebuild it as a standby with \
             `heliosdb-nano recover --from-primary --data-dir <its data directory> --primary-host <the new primary> \
             --replication-token-file <its token file> --replace --then stop`, then start it with \
             --replication-role standby --primary-host <the new primary>",
        ));
        false
    } else if running != 0 {
        rows.push(CheckRow::failed("primary", slot_closed_reason(running)));
        false
    } else {
        rows.push(CheckRow::passed("primary", "this node is a primary, not fenced"));
        true
    };
    let facts = match host {
        None => {
            rows.push(CheckRow::failed(
                "demotable",
                "this node cannot be demoted in-process: it serves no replication port, or it runs in memory (a \
                 switchover is answered by the PostgreSQL wire of a server started with --replication-role primary)",
            ));
            None
        }
        Some(host) => match host.preflight().await {
            Ok(facts) => {
                rows.push(CheckRow::passed(
                    "demotable",
                    format!(
                        "this node can be demoted into a standby (history {}, epoch {})",
                        facts.history, facts.epoch
                    ),
                ));
                Some(facts)
            }
            Err(reason) => {
                rows.push(CheckRow::failed("demotable", reason));
                None
            }
        },
    };
    // Spec R3a, review item 1: past the fence the demotion rewinds this
    // node's directory and never full-copies it in-process, so a directory
    // the rewind could not take is refused before anything is asked.
    let rewindable = match host {
        None => {
            rows.push(CheckRow::skipped("rewind", "this node cannot be demoted in-process"));
            false
        }
        Some(host) => match host.rewind_check().await {
            Ok(detail) => {
                rows.push(CheckRow::passed("rewind", detail));
                true
            }
            Err(reason) => {
                rows.push(CheckRow::failed(
                    "rewind",
                    format!("once demoted, this node's directory could not be rewound in place: {reason}"),
                ));
                false
            }
        },
    };
    // 2. The target, as this primary sees it.
    let standby = match resolve(name) {
        Ok(standby) => {
            rows.push(CheckRow::passed(
                "target",
                format!("standby {} at {}", standby.node_id, standby.address),
            ));
            Some(standby)
        }
        Err(reason) => {
            rows.push(CheckRow::failed("target", reason));
            None
        }
    };
    let lag_bound = facts
        .as_ref()
        .map_or(Duration::from_secs(30), |facts| facts.max_standby_lag);
    let streaming = match &standby {
        None => {
            rows.push(CheckRow::skipped("streaming", "no target"));
            false
        }
        Some(standby) => {
            let ok = standby.state == crate::replication::ha_state::StandbyState::Streaming
                && standby.counts_for_sync
                && Duration::from_millis(standby.lag_ms) <= lag_bound;
            rows.push(if ok {
                CheckRow::passed(
                    "streaming",
                    format!(
                        "streaming, {} write(s) and {:.1} s behind (max_standby_lag {} s)",
                        standby.lag_bytes,
                        standby.lag_ms as f64 / 1000.0,
                        lag_bound.as_secs()
                    ),
                )
            } else {
                CheckRow::failed(
                    "streaming",
                    format!(
                        "the standby is {} and {:.1} s behind (it must be streaming within max_standby_lag {} s)",
                        standby.state.as_str(),
                        standby.lag_ms as f64 / 1000.0,
                        lag_bound.as_secs()
                    ),
                )
            });
            ok
        }
    };
    let session = standby.as_ref().and_then(|standby| session_of(standby.node_id));
    match (&standby, &session) {
        (None, _) => rows.push(CheckRow::skipped("online promotion", "no target")),
        (Some(_), Some(_)) => rows.push(CheckRow::passed(
            "online promotion",
            "its replication session offers online promotion (CAP_PROMOTE)",
        )),
        (Some(_), None) => rows.push(CheckRow::failed(
            "online promotion",
            "its replication session does not offer online promotion (CAP_PROMOTE): it is no serving standby of \
             this release (an offline copy, a resync, an older build)",
        )),
    }
    // 3. The target's own preflight, and how this node would follow it.
    let target_id = standby.as_ref().map(|standby| standby.node_id);
    let local_ok = node_ok && facts.is_some() && rewindable && streaming && session.is_some();
    let (Some(host), Some(facts), Some(standby), Some(session), true) = (host, facts, standby, session, local_ok)
    else {
        rows.push(CheckRow::skipped("target preflight", "a check above failed"));
        rows.push(CheckRow::skipped("follow", "a check above failed"));
        rows.extend(other_standby_rows(target_id, None, false));
        return (rows, None);
    };
    let check = PromoteRequest {
        switchover: Uuid::new_v4(),
        epoch: facts.epoch.saturating_add(1),
        requested_by: facts.node_id,
        deadline_secs: SWITCHOVER_DEADLINE_SECS,
        answer_secs: answer_secs(&**host),
        check_only: true,
    };
    let answer = match ask(standby.node_id, check, host.delivery_bound()).await {
        Asked::Answered { answer, peer } if answer.accepted => {
            rows.push(CheckRow::passed(
                "target preflight",
                format!(
                    "the standby can be promoted to epoch {}: {}",
                    check.epoch, answer.reason
                ),
            ));
            Some((answer, peer))
        }
        Asked::Answered { answer, .. } => {
            rows.push(CheckRow::failed("target preflight", answer.reason));
            None
        }
        // A check changes nothing on the target, so an unanswered one is
        // only a failed check.
        Asked::NotDelivered(reason) | Asked::Unanswered { reason, .. } => {
            rows.push(CheckRow::failed("target preflight", reason));
            None
        }
    };
    let Some((answer, peer)) = answer else {
        rows.push(CheckRow::skipped("follow", "the target's preflight did not pass"));
        rows.extend(other_standby_rows(Some(standby.node_id), None, session.tls));
        return (rows, None);
    };
    let follow = follow_endpoint(&answer, peer.or(session.peer))
        .and_then(|endpoint| host.follow_plan(&endpoint, session.tls).map(|how| (endpoint, how)));
    match follow {
        Ok((endpoint, how)) => {
            rows.push(CheckRow::passed(
                "follow",
                format!("demoted, this node follows the new primary at {endpoint} ({how})"),
            ));
            rows.extend(other_standby_rows(Some(standby.node_id), Some(&endpoint), session.tls));
            (
                rows,
                Some((
                    facts,
                    Target {
                        node_id: standby.node_id,
                        peer: peer.or(session.peer),
                        tls: session.tls,
                        follow: endpoint,
                        promotion: None,
                    },
                )),
            )
        }
        Err(reason) => {
            rows.push(CheckRow::failed("follow", reason));
            rows.extend(other_standby_rows(Some(standby.node_id), None, session.tls));
            (rows, None)
        }
    }
}

/// `SWITCHOVER CHECK TO '<target>'`: every check, one row each; nothing
/// changes, here or on the target.
pub async fn check(name: &str) -> Vec<CheckRow> {
    let host = installed();
    preflight(name, host.as_ref(), false).await.0
}

// ---------------------------------------------------------------------------
// The switchover.
// ---------------------------------------------------------------------------

/// What a completed switchover did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switched {
    /// The new primary's replication port, as this node follows it.
    pub new_primary: String,
    pub epoch: u64,
    /// This node's last sequence as the primary.
    pub final_sequence: u64,
    /// How the demotion went (one line).
    pub demotion: String,
    /// The other standbys of the old primary, one line each naming what
    /// rebuilds it against the new primary (empty: none).
    pub other_standbys: Vec<String>,
}

/// A switchover the target took, as its caller holds it.
pub struct Running {
    placeholder: Arc<EmbeddedDatabase>,
    outcome: oneshot::Receiver<Result<Switched, String>>,
    to_epoch: u64,
}

impl Running {
    /// The empty database the caller's session is served by from now on:
    /// it lets go of the store before the demotion releases it.
    pub fn placeholder(&self) -> Arc<EmbeddedDatabase> {
        Arc::clone(&self.placeholder)
    }

    /// The FATAL 57P01 the caller's session is closed with after its answer.
    pub fn termination_message(&self) -> String {
        format!(
            "terminating connection: this primary switched over to a standby promoted to epoch {} and is now its \
             standby; reconnect (writes go to the new primary)",
            self.to_epoch
        )
    }

    /// The switchover's end: done, or what failed and what to run.
    pub async fn answer(self) -> Result<Switched, String> {
        self.outcome
            .await
            .unwrap_or_else(|_| Err("the switchover's task ended without an answer (see the log)".to_string()))
    }
}

/// The switchover slot, released when the switchover's task ends (or when
/// it was never started).
struct Claim;

impl Drop for Claim {
    fn drop(&mut self) {
        RUNNING.store(0, Ordering::Release);
    }
}

/// What the switchover's task tells its caller once the target took the
/// request.
struct Taken {
    placeholder: Arc<EmbeddedDatabase>,
    to_epoch: u64,
}

/// Steps 1-2 of the module documentation (`SWITCHOVER TO '<target>'`): a
/// refusal changes nothing (SQLSTATE 55000). `Ok` once the target took the
/// request; the rest runs on the switchover's own task
/// ([`Running::answer`]), which also ran steps 1-2 — so a caller cancelled
/// while it waits abandons nothing. `caller`: who asked, for the log.
pub async fn begin(name: &str, caller: String) -> Result<Running, Refusal> {
    let Some(host) = installed() else {
        let (rows, _) = preflight(name, None, false).await;
        return Err(first_failure(&rows));
    };
    let since = unix_secs().max(1);
    if RUNNING
        .compare_exchange(0, since, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Refusal::new(slot_closed_reason(RUNNING.load(Ordering::Acquire))));
    }
    let claim = Claim;
    let (taken_tx, taken_rx) = oneshot::channel::<Result<Taken, Refusal>>();
    let (outcome_tx, outcome) = oneshot::channel();
    let name = name.to_string();
    tokio::spawn(async move {
        if let Some((result, linger)) = drive(host, &name, caller, taken_tx, claim).await {
            if let Err(reason) = &result {
                tracing::error!("switchover: {reason}");
            }
            let _ = outcome_tx.send(result);
            // The slot stays closed while the target may still fence.
            if let Some(claim) = linger {
                let window = late_fence_window();
                let until = unix_secs().saturating_add(window.as_secs());
                LINGER_UNTIL.store(until, Ordering::Release);
                let deadline = Instant::now() + window;
                while Instant::now() < deadline && crate::standby_posture::fence_status().is_none() {
                    tokio::time::sleep(POLL).await;
                }
                LINGER_UNTIL.store(0, Ordering::Release);
                drop(claim);
            }
        }
    });
    match taken_rx.await {
        Ok(Ok(Taken { placeholder, to_epoch })) => Ok(Running {
            placeholder,
            outcome,
            to_epoch,
        }),
        Ok(Err(refusal)) => Err(refusal),
        Err(_) => Err(Refusal::new(
            "SWITCHOVER failed: its task ended before it could say whether the standby took the request (see the \
             log, and this node's /health)",
        )),
    }
}

/// The switchover's task: steps 1-2, `taken` answered once the target took
/// the request (or with the refusal), then steps 3-4. `None`: refused before
/// the target took it (the refusal went to `taken`). `claim`: the slot,
/// released before a refusal is answered and when the switchover ends —
/// or handed back (`Some` in the result) when its target took the request
/// and no fence came: the caller keeps the slot closed while one still may.
async fn drive(
    host: Arc<dyn DemotionHost>,
    name: &str,
    caller: String,
    taken: oneshot::Sender<Result<Taken, Refusal>>,
    claim: Claim,
) -> Option<(Result<Switched, String>, Option<Claim>)> {
    let refuse = |claim: Claim, taken: oneshot::Sender<Result<Taken, Refusal>>, refusal: Refusal| {
        drop(claim);
        let _ = taken.send(Err(refusal));
    };
    let (rows, ready) = preflight(name, Some(&host), true).await;
    let Some((facts, target)) = ready else {
        refuse(claim, taken, first_failure(&rows));
        return None;
    };
    let switchover = Uuid::new_v4();
    let to_epoch = facts.epoch.saturating_add(1);
    let request = PromoteRequest {
        switchover,
        epoch: to_epoch,
        requested_by: facts.node_id,
        deadline_secs: SWITCHOVER_DEADLINE_SECS,
        answer_secs: answer_secs(&*host),
        check_only: false,
    };
    tracing::warn!(
        "switchover {switchover}: {caller} asks the standby {} (at {}) to be promoted to epoch {to_epoch}; this \
         primary (epoch {}) is then demoted into its standby, following it at {}",
        target.node_id,
        target
            .peer
            .map_or_else(|| "an unknown address".to_string(), |peer| peer.to_string()),
        facts.epoch,
        target.follow
    );
    let target = match ask(target.node_id, request, host.delivery_bound()).await {
        Asked::Answered { answer, peer } if answer.accepted => {
            tracing::warn!(
                "switchover {switchover}: the standby {} accepted ({}); waiting for it to fence this primary",
                target.node_id,
                answer.reason
            );
            refollow(&*host, switchover, target, &answer, peer)
        }
        Asked::Answered { answer, .. } => {
            refuse(
                claim,
                taken,
                Refusal::new(format!(
                    "SWITCHOVER refused by the standby {}: {}; nothing was changed",
                    target.node_id, answer.reason
                )),
            );
            return None;
        }
        Asked::NotDelivered(reason) => {
            refuse(
                claim,
                taken,
                Refusal::new(format!("SWITCHOVER refused: {reason}; nothing was changed")),
            );
            return None;
        }
        // Review item 3: the target took the request, and what it did with
        // it is unknown — it may be promoting. Never "nothing changed": the
        // wait for this primary's fence settles it (no fence: still the
        // primary, said so).
        Asked::Unanswered { reason, peer } => {
            tracing::warn!(
                "switchover {switchover}: the standby {} took the request, but its answer was lost ({reason}): it \
                 may be promoting itself; waiting for it to fence this primary",
                target.node_id
            );
            Target {
                peer: peer.or(target.peer),
                ..target
            }
        }
    };
    let _ = taken.send(Ok(Taken {
        placeholder: host.placeholder(),
        to_epoch,
    }));
    let result = run(host, switchover, facts, target, to_epoch).await;
    let linger = if result.is_err() && crate::standby_posture::fence_status().is_none() {
        Some(claim)
    } else {
        drop(claim);
        None
    };
    Some((result, linger))
}

/// Review item 6a: where this node follows the target, from the answer to
/// the REAL request (the preflight's came from its check). One that does not
/// work any more keeps the preflight's address, said at WARN: the target is
/// promoting by now, and the fence and the serving check below decide.
fn refollow(
    host: &dyn DemotionHost,
    switchover: Uuid,
    target: Target,
    answer: &PromoteAnswer,
    peer: Option<SocketAddr>,
) -> Target {
    let peer = peer.or(target.peer);
    let target = Target {
        promotion: Some(answer.promotion).filter(|promotion| !promotion.is_nil()),
        ..target
    };
    match follow_endpoint(answer, peer).and_then(|endpoint| host.follow_plan(&endpoint, target.tls).map(|_| endpoint)) {
        Ok(endpoint) => {
            if endpoint != target.follow {
                tracing::warn!(
                    "switchover {switchover}: the standby will serve its replication port at {endpoint} (its \
                     preflight said {}): this node follows {endpoint}",
                    target.follow
                );
            }
            Target {
                peer,
                follow: endpoint,
                ..target
            }
        }
        Err(reason) => {
            tracing::warn!(
                "switchover {switchover}: the standby's answer names no address this node can follow ({reason}); it \
                 keeps the preflight's, {}",
                target.follow
            );
            Target { peer, ..target }
        }
    }
}

/// The refusal a failed preflight answers: its first failed check.
fn first_failure(rows: &[CheckRow]) -> Refusal {
    match rows.iter().find(|row| !row.ok) {
        Some(row) => Refusal::new(format!(
            "SWITCHOVER refused ({}): {}; nothing was changed",
            row.check, row.detail
        ))
        .with_hint("SWITCHOVER CHECK TO '<target>' lists every check"),
        None => Refusal::new("SWITCHOVER refused: its preflight did not complete; nothing was changed"),
    }
}

/// Why the `recover` a switchover names carries `--replace` (review fourth
/// pass, item 1): the command must finish the switchover whatever the
/// recovery's plan picks — the rewind in place, or a full copy when most of a
/// small store differs — and a full copy swaps a directory in only with
/// `--replace`. And `--then stop` (fifth pass): by default `recover` goes on
/// to run the node in the process that ran it — a server in the operator's
/// shell, on default ports, outside its service manager. A command printed
/// to be copied exits when its work is done, and the `start` is named after
/// it.
pub const REPLACE_NOTE: &str = "--replace matters only when the recovery chooses a full copy (most of the \
                                store differs): the old directory is then moved aside to \
                                <data dir>.replaced-<UTC>, never deleted";

/// What finishes a switchover this node could not finish itself. `flags`:
/// how this node reaches the new primary (its token, and the pair's TLS:
/// review third pass, item 7) — [`DemotionHost::follow_flags`].
fn finish_command(facts: &PrimaryFacts, follow: &str, flags: &str) -> String {
    format!(
        "stop this node, run `heliosdb-nano recover --from-primary --data-dir {dir} --primary-host {follow}{flags} \
         --replace --then stop`, then start it as a standby of the new primary, through its service manager and with \
         its usual ports: `heliosdb-nano start --data-dir {dir} --replication-role standby --primary-host \
         {follow}{flags}` (a role recorded beside the directory by an earlier switchover is rewritten by the recover \
         and agrees with these flags). {REPLACE_NOTE}",
        dir = facts.data_dir.display()
    )
}

/// Steps 3-4 of the module documentation, once the target took the request.
async fn run(
    host: Arc<dyn DemotionHost>,
    switchover: Uuid,
    facts: PrimaryFacts,
    target: Target,
    to_epoch: u64,
) -> Result<Switched, String> {
    let flags = host.follow_flags(target.tls);
    // 3a. This primary's fence, through the target's renewed session.
    let fence_bound = Duration::from_secs(u64::from(SWITCHOVER_DEADLINE_SECS))
        .saturating_add(FENCE_ANSWER_BOUND)
        .saturating_add(host.answer_bound());
    let deadline = Instant::now() + fence_bound;
    let final_sequence = loop {
        if let Some(fence) = crate::standby_posture::fence_status() {
            // Review (third pass, item 2): only THIS promotion's fence — its
            // epoch and, once its answer said it, its identity.
            let ours =
                fence.by_epoch == to_epoch && target.promotion.is_none_or(|promotion| fence.by_node == promotion);
            if ours {
                if let Some(final_sequence) = fence.final_sequence {
                    break final_sequence;
                }
            } else {
                return Err(format!(
                    "this primary was fenced by another promotion (epoch {}, promotion {}) than the one the \
                     switchover asked for (epoch {to_epoch}{}): it serves reads only; recover it from the primary \
                     that fenced it (`heliosdb-nano recover --from-primary --data-dir {} --primary-host <that \
                     primary>{} --replace --then stop`), then start it with --replication-role standby --primary-host \
                     <that primary>. {REPLACE_NOTE}",
                    fence.by_epoch,
                    fence.by_node,
                    target
                        .promotion
                        .map(|promotion| format!(", promotion {promotion}"))
                        .unwrap_or_default(),
                    facts.data_dir.display(),
                    flags
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(match crate::standby_posture::fence_status() {
                None => format!(
                    "SWITCHOVER failed: the standby {} took the request but did not fence this primary within {} s \
                     (its promotion may have been refused or aborted: its log and /health say why). This primary \
                     was NOT fenced and keeps serving as the primary; if the standby still fences it later, it stays \
                     fenced: then {}",
                    target.node_id,
                    fence_bound.as_secs(),
                    finish_command(&facts, &target.follow, &flags)
                ),
                Some(_) => format!(
                    "SWITCHOVER failed: this primary's fence by epoch {to_epoch} did not complete within {} s; it \
                     refuses every write. {}",
                    fence_bound.as_secs(),
                    finish_command(&facts, &target.follow, &flags)
                ),
            });
        }
        tokio::time::sleep(POLL).await;
    };
    tracing::warn!(
        "switchover {switchover}: this primary is fenced by epoch {to_epoch} at its sequence {final_sequence}; \
         waiting for the new primary at {} to serve",
        target.follow
    );
    // 3b. The target serving as the primary of `to_epoch`.
    let serve_within = serve_bound();
    let deadline = Instant::now() + serve_within;
    loop {
        let last = match host
            .serves_as_primary(&target.follow, facts.history, to_epoch, target.tls)
            .await
        {
            Ok(true) => break,
            Ok(false) => format!(
                "{} does not answer as the primary of epoch {to_epoch} yet",
                target.follow
            ),
            Err(reason) => reason,
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "SWITCHOVER incomplete: this primary is FENCED by epoch {to_epoch} at its sequence {final_sequence} \
                 (it serves reads only, nothing it committed is lost), but the new primary did not answer at {} \
                 within {} s ({last}). Once it serves, finish the switchover: {}",
                target.follow,
                serve_within.as_secs(),
                finish_command(&facts, &target.follow, &flags)
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tracing::warn!(
        "switchover {switchover}: the new primary at {} serves epoch {to_epoch}; demoting this node into its standby",
        target.follow
    );
    // The other standbys, named while this node is still their source.
    let others = other_standbys(Some(target.node_id), Some(&target.follow), target.tls);
    // 4. The demotion.
    let plan = DemotionPlan {
        switchover,
        history: facts.history,
        from_epoch: facts.epoch,
        to_epoch,
        final_sequence,
        follow: target.follow.clone(),
        pair_tls: target.tls,
    };
    match host.demote(plan).await {
        Ok(demotion) => {
            uninstall();
            tracing::warn!(
                "switchover {switchover}: done — the primary at {} serves epoch {to_epoch}; this node is its standby \
                 ({demotion})",
                target.follow
            );
            for other in &others {
                tracing::warn!("switchover {switchover}: the standby {other}");
            }
            Ok(Switched {
                new_primary: target.follow,
                epoch: to_epoch,
                final_sequence,
                demotion,
                other_standbys: others,
            })
        }
        Err(DemotionError(reason)) => Err(format!(
            "SWITCHOVER incomplete: the new primary at {} serves epoch {to_epoch} and this node is FENCED (it serves \
             reads only), but its demotion failed: {reason}. Finish it: {}",
            target.follow,
            finish_command(&facts, &target.follow, &flags)
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn answer(port: u16, endpoint: &str) -> PromoteAnswer {
        PromoteAnswer {
            switchover: Uuid::new_v4(),
            accepted: true,
            epoch: 2,
            promotion: Uuid::new_v4(),
            replication_port: port,
            endpoint: endpoint.to_string(),
            reason: String::new(),
        }
    }

    fn request(answer_secs: u32) -> PromoteRequest {
        PromoteRequest {
            switchover: Uuid::new_v4(),
            epoch: 2,
            requested_by: Uuid::new_v4(),
            deadline_secs: 60,
            answer_secs,
            check_only: true,
        }
    }

    /// Spec R3b: where a demoted primary follows the target — its endpoint,
    /// the peer's address at its port when it listens everywhere, loopback
    /// only on one host.
    #[test]
    fn the_follow_address_comes_from_the_answer_and_the_peer() {
        let local: SocketAddr = "127.0.0.1:41000".parse().unwrap();
        let remote: SocketAddr = "10.0.0.6:41000".parse().unwrap();
        assert_eq!(
            follow_endpoint(&answer(5433, "127.0.0.1:5433"), Some(local)).unwrap(),
            "127.0.0.1:5433"
        );
        assert!(follow_endpoint(&answer(5433, "127.0.0.1:5433"), Some(remote))
            .unwrap_err()
            .contains("loopback only"));
        assert_eq!(
            follow_endpoint(&answer(5433, "0.0.0.0:5433"), Some(remote)).unwrap(),
            "10.0.0.6:5433"
        );
        assert_eq!(
            follow_endpoint(&answer(5433, "10.0.0.7:5433"), Some(remote)).unwrap(),
            "10.0.0.7:5433"
        );
        assert_eq!(
            follow_endpoint(&answer(5433, "db-2.example:5433"), Some(remote)).unwrap(),
            "db-2.example:5433"
        );
        assert_eq!(
            follow_endpoint(&answer(5433, ""), Some(remote)).unwrap(),
            "10.0.0.6:5433"
        );
        assert!(follow_endpoint(&answer(0, ""), Some(remote))
            .unwrap_err()
            .contains("no replication port"));
    }

    /// Spec R3b, review item 3: a request waits for its target's session,
    /// is delivered once, and its answer reaches the asker; one nobody takes
    /// is withdrawn as never delivered; one taken and never answered is
    /// UNANSWERED (the target may be acting on it), never "not delivered";
    /// a session that ends after taking it leaves it unanswered too. One
    /// test: the request slot is process-wide.
    #[tokio::test]
    async fn a_request_is_delivered_once_and_one_taken_without_an_answer_is_never_called_undelivered() {
        async fn take(session: &PromotableSession) -> Option<PromoteRequest> {
            for _ in 0..200 {
                if let Some(taken) = session.take_request() {
                    return Some(taken);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            None
        }
        let target = Uuid::new_v4();
        let other = note_promotable(Uuid::new_v4(), None, false);
        let session = note_promotable(target, "127.0.0.1:41000".parse().ok(), true);
        let asked = request(1);
        let asking = tokio::spawn(ask(target, asked, Duration::from_secs(5)));
        assert_eq!(take(&session).await, Some(asked));
        assert!(other.take_request().is_none(), "never delivered to another standby");
        assert!(session.take_request().is_none(), "delivered once");
        session.answered(&asked, Some(answer(5433, "")));
        match asking.await.unwrap() {
            Asked::Answered { answer, peer } => {
                assert!(answer.accepted);
                assert_eq!(peer, "127.0.0.1:41000".parse().ok());
            }
            _ => panic!("the answer did not reach the asker"),
        }
        assert!(PENDING.lock().is_none(), "withdrawn once answered");

        // Nobody polls: never delivered, withdrawn.
        match ask(Uuid::new_v4(), request(1), Duration::from_millis(50)).await {
            Asked::NotDelivered(reason) => assert!(reason.contains("never delivered"), "{reason}"),
            _ => panic!("a request nobody took is not delivered"),
        }
        assert!(PENDING.lock().is_none(), "withdrawn after its timeout");

        // Taken, never answered: unanswered once its answer bound ran out.
        let late = request(1);
        let asking = tokio::spawn(ask(target, late, Duration::from_secs(5)));
        assert_eq!(take(&session).await, Some(late));
        match asking.await.unwrap() {
            Asked::Unanswered { reason, peer } => {
                assert!(reason.contains("did not answer"), "{reason}");
                assert_eq!(peer, "127.0.0.1:41000".parse().ok());
            }
            _ => panic!("a request taken without an answer must be unanswered"),
        }
        assert!(PENDING.lock().is_none());
        // An answer arriving after the withdrawal reaches no one, harmlessly.
        session.answered(&late, Some(answer(5433, "")));

        // Taken, then the session ended (`answered(None)`): unanswered.
        let dropped = request(30);
        let asking = tokio::spawn(ask(target, dropped, Duration::from_secs(5)));
        assert_eq!(take(&session).await, Some(dropped));
        session.answered(&dropped, None);
        match asking.await.unwrap() {
            Asked::Unanswered { reason, .. } => assert!(reason.contains("session ended"), "{reason}"),
            _ => panic!("a session that ended after taking the request leaves it unanswered"),
        }
        assert!(PENDING.lock().is_none());

        drop(session);
        assert!(
            session_of(target).is_none(),
            "a session that ended is no target any more"
        );
    }
}
