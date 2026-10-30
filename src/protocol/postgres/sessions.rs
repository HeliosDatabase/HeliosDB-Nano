//! The PostgreSQL wire's session registry and push termination (spec P5.2
//! §1, `docs/plans/specs/P5_2_pg_promote_online.md`).
//!
//! # Why
//!
//! A proxy (HeliosDB Proxy, libpq's `target_session_attrs`) learns a node's
//! role once per connection: from the `in_hot_standby` and
//! `default_transaction_read_only` ParameterStatus frames of its startup. A
//! node that changes role while connections are open — a primary fenced by a
//! promoted standby, a standby promoted to primary — therefore has to END
//! those connections, or every client keeps the role it read at startup:
//! writes keep going to a fenced primary, reads keep treating a new primary as
//! a standby. PostgreSQL ends them with `FATAL 57P01 admin_shutdown`, the code
//! a proxy redials on. Until P5.2 this server could only do that lazily, at a
//! session's NEXT statement — an idle pooled connection learned nothing until
//! it was used, and a promotion could not wait for the store's last holder to
//! go. This module lets the node PUSH the termination to every session at
//! once and wait (bounded) until they are gone.
//!
//! # How
//!
//! Every accepted connection is [`SessionRegistry::admit`]ted before its
//! database is chosen and holds the returned [`SessionTicket`] until it ends
//! (the ticket unregisters on drop). A termination is a new GENERATION of the
//! one [`Terminate`] watch: every session admitted at an older generation —
//! and so served by whatever the node was before — is its target, except one
//! flagged exempt (the `pg_promote` caller, which is answered first). The
//! handler's read loop wakes on the watch: an idle session gets the FATAL at
//! once, a session part-way through a message cycle finishes it first
//! (bounded by `[server] close_timeout`). [`SessionRegistry::terminate_all`]
//! waits until the targets unregistered; one that does not in time, and one
//! still authenticating (it holds a database, has received nothing yet), has
//! its connection task aborted — its socket closed, its database released.
//!
//! The lazy 57P01 of the standby guard stays as the backstop: a session
//! mid-statement when the push arrives ends at its next statement either way.
//!
//! Spec P5.2 §8.A/§8.H: every ticket also says WHICH database serves it (the
//! engine the listener handed it, [`SessionTicket::serve`]), so a promotion
//! can end the sessions of the standby's store and spare the ones the empty
//! placeholder serves ([`TerminateScope`]), and wait until no session holds
//! the store any more ([`SessionRegistry::holders_of`]) — a session whose
//! task was told to end is not gone while its statement still runs. A
//! promotion closes by force only the sessions still authenticating
//! ([`SessionRegistry::abort_authenticating`]); it waits for the others
//! (bounded by `pg_promote`'s `wait_seconds`) instead of the drain's abort.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::{watch, Notify};
use tokio::task::AbortHandle;

/// Spec P5.2 §1: how long [`SessionRegistry::terminate_all`] waits for the
/// sessions it ended before it aborts the ones still there (a fence; and a
/// promotion's end of the placeholder's sessions, which hold no store).
pub const TERMINATE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// After aborting the sessions that did not drain: how long their tasks get
/// to unwind and unregister.
const ABORT_GRACE: Duration = Duration::from_secs(1);
/// How often a drain looks again when no session left in between.
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// Why the node ends its sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminateCause {
    /// Nothing was ended yet (generation 0).
    None,
    /// Spec P5.1 §3.2 step 5: this primary was fenced by a promoted standby.
    /// A session opened after the fence (served reads only) is not a target.
    Fence,
    /// Spec P5.2 §3: this standby is being promoted to primary.
    Promotion,
}

/// Which sessions a termination ends, besides being admitted before it
/// (spec P5.2 §8.A, §8.H). Engines are named by
/// `StorageEngine::instance_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminateScope {
    /// Every session (a fence).
    All,
    /// The sessions NOT served by this engine: a promotion ends the sessions
    /// of the standby's store and spares the placeholder's, which answer
    /// the always-answered probes while it runs.
    NotServedBy(u64),
    /// Only the sessions this engine serves: after the reopen, the
    /// placeholder's (their startup said `in_hot_standby = on`).
    ServedBy(u64),
}

impl TerminateScope {
    /// Whether a session served by `served_by` (0: not known yet) is in
    /// scope. A session whose database is not known yet is in every scope
    /// but [`Self::ServedBy`]: it may hold anything.
    fn covers(self, served_by: u64) -> bool {
        match self {
            Self::All => true,
            Self::NotServedBy(engine) => served_by != engine,
            Self::ServedBy(engine) => served_by == engine,
        }
    }
}

/// One termination: every session admitted before `generation` (and not
/// exempt, and in `scope`) is ended with `FATAL sqlstate message`.
#[derive(Clone, Debug)]
pub struct Terminate {
    pub generation: u64,
    pub cause: TerminateCause,
    pub scope: TerminateScope,
    pub sqlstate: &'static str,
    pub message: String,
}

/// A registered session, as [`SessionRegistry::sessions`] lists it (the
/// diagnostics a promotion logs when its store has holders left).
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: u64,
    pub backend_pid: Option<i32>,
    pub peer: Option<String>,
    /// Unix seconds.
    pub admitted_at: u64,
    pub admitted_generation: u64,
    pub exempt: bool,
    /// The session reached its message loop (it authenticated).
    pub established: bool,
    /// The engine serving it (`StorageEngine::instance_id`; 0: not known
    /// yet).
    pub served_by: u64,
}

/// How a [`SessionRegistry::terminate_all`] ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// The generation the termination was published as.
    pub generation: u64,
    /// Sessions it ended (admitted before it, not exempt).
    pub targeted: usize,
    /// Of those, the ones whose connection task was aborted (still
    /// authenticating, or not gone in time).
    pub aborted: usize,
    /// Of those, the ones still registered when it returned.
    pub remaining: usize,
}

struct Entry {
    admitted_generation: u64,
    admitted_at: u64,
    exempt: Arc<AtomicBool>,
    established: Arc<AtomicBool>,
    served_by: Arc<AtomicU64>,
    abort: Option<AbortHandle>,
    peer: Option<String>,
    backend_pid: Option<i32>,
}

/// The process's PostgreSQL sessions (module documentation). One per
/// process ([`registry`]), like the node it serves.
pub struct SessionRegistry {
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, Entry>>,
    terminate: watch::Sender<Terminate>,
    left: Notify,
}

/// The process-global registry every PostgreSQL listener admits into.
pub fn registry() -> &'static SessionRegistry {
    static REGISTRY: OnceLock<SessionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(SessionRegistry::new)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl SessionRegistry {
    fn new() -> Self {
        let (terminate, _) = watch::channel(Terminate {
            generation: 0,
            cause: TerminateCause::None,
            scope: TerminateScope::All,
            sqlstate: crate::network::protocol::sqlstate::ADMIN_SHUTDOWN,
            message: String::new(),
        });
        Self {
            next_id: AtomicU64::new(1),
            entries: Mutex::new(HashMap::new()),
            terminate,
            left: Notify::new(),
        }
    }

    /// Register a connection just accepted from `peer`, BEFORE its database
    /// is chosen: a termination published after this call cannot miss a
    /// session served by what the node was before it.
    pub fn admit(&'static self, peer: Option<String>) -> SessionTicket {
        let exempt = Arc::new(AtomicBool::new(false));
        let established = Arc::new(AtomicBool::new(false));
        let served_by = Arc::new(AtomicU64::new(0));
        let mut entries = self.entries.lock();
        // Read — and subscribed — under the entries lock, which every
        // publication also holds: a session is either listed when a
        // termination computes its targets (and sees it as a change of its
        // watch), or admitted at the new generation.
        let generation = self.terminate.borrow().generation;
        let terminate = self.terminate.subscribe();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        entries.insert(
            id,
            Entry {
                admitted_generation: generation,
                admitted_at: unix_secs(),
                exempt: Arc::clone(&exempt),
                established: Arc::clone(&established),
                served_by: Arc::clone(&served_by),
                abort: None,
                peer: peer.clone(),
                backend_pid: None,
            },
        );
        drop(entries);
        SessionTicket {
            id,
            registry: self,
            admitted_generation: generation,
            exempt,
            established,
            served_by,
            peer,
            terminate,
        }
    }

    /// The task serving session `id`, aborted by a termination that it does
    /// not answer in time (its socket closes, its database is released).
    pub fn attach_abort(&self, id: u64, abort: AbortHandle) {
        if let Some(entry) = self.entries.lock().get_mut(&id) {
            entry.abort = Some(abort);
        }
    }

    fn describe(&self, id: u64, backend_pid: i32) {
        if let Some(entry) = self.entries.lock().get_mut(&id) {
            entry.backend_pid = Some(backend_pid);
        }
    }

    fn leave(&self, id: u64) {
        self.entries.lock().remove(&id);
        self.left.notify_waiters();
    }

    /// The registered sessions (diagnostics).
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let mut listed: Vec<SessionSummary> = self
            .entries
            .lock()
            .iter()
            .map(|(id, entry)| SessionSummary {
                id: *id,
                backend_pid: entry.backend_pid,
                peer: entry.peer.clone(),
                admitted_at: entry.admitted_at,
                admitted_generation: entry.admitted_generation,
                exempt: entry.exempt.load(Ordering::Acquire),
                established: entry.established.load(Ordering::Acquire),
                served_by: entry.served_by.load(Ordering::Acquire),
            })
            .collect();
        listed.sort_by_key(|session| session.id);
        listed
    }

    /// Spec P5.2 §8.A: the registered sessions that may hold the database
    /// of `engine` — served by it, or not known yet (between their admission
    /// and the listener's choice) — the caller of a promotion included until
    /// it let go of the store. A session leaves this list only when its task
    /// is gone, its handler (and the database it held) dropped with it: a
    /// statement still running is never assumed gone.
    pub fn holders_of(&self, engine: u64) -> Vec<SessionSummary> {
        self.sessions()
            .into_iter()
            .filter(|session| session.served_by == engine || session.served_by == 0)
            .collect()
    }

    /// The current generation (0 before any termination).
    pub fn generation(&self) -> u64 {
        self.terminate.borrow().generation
    }

    /// Publish a termination WITHOUT waiting: every session admitted before
    /// it (and not exempt) is pushed `FATAL 57P01 message`. Returns the
    /// generation and the targets' ids. What a fence calls from its blocking
    /// thread ([`Self::terminate_all`] waits on top of it).
    pub fn push(&self, cause: TerminateCause, message: impl Into<String>) -> (u64, Vec<u64>) {
        self.push_scoped(cause, TerminateScope::All, message)
    }

    /// [`Self::push`] to the sessions in `scope` only (spec P5.2 §8.A,
    /// §8.H).
    pub fn push_scoped(
        &self,
        cause: TerminateCause,
        scope: TerminateScope,
        message: impl Into<String>,
    ) -> (u64, Vec<u64>) {
        let message = message.into();
        let entries = self.entries.lock();
        let mut generation = 0;
        self.terminate.send_modify(|terminate| {
            terminate.generation += 1;
            terminate.cause = cause;
            terminate.scope = scope;
            terminate.sqlstate = crate::network::protocol::sqlstate::ADMIN_SHUTDOWN;
            terminate.message.clone_from(&message);
            generation = terminate.generation;
        });
        let targets = entries
            .iter()
            .filter(|(_, entry)| {
                entry.admitted_generation < generation
                    && !entry.exempt.load(Ordering::Acquire)
                    && scope.covers(entry.served_by.load(Ordering::Acquire))
            })
            .map(|(id, _)| *id)
            .collect();
        drop(entries);
        tracing::info!("terminating the PostgreSQL sessions opened before now (FATAL 57P01): {message}");
        (generation, targets)
    }

    /// Spec P5.2 §1: end every session admitted before now that is not
    /// exempt — [`Self::push`] — and wait until they are gone, at most
    /// `timeout`. A target still authenticating is aborted at once (it has
    /// received nothing, and holds a database); one not gone in time is
    /// aborted then, and given a moment to unregister.
    pub async fn terminate_all(&self, cause: TerminateCause, message: impl Into<String>, timeout: Duration) -> Drained {
        let (generation, targets) = self.push(cause, message);
        self.drain(generation, targets, timeout).await
    }

    /// [`Self::terminate_all`] of the sessions in `scope` only.
    pub async fn terminate_scoped(
        &self,
        cause: TerminateCause,
        scope: TerminateScope,
        message: impl Into<String>,
        timeout: Duration,
    ) -> Drained {
        let (generation, targets) = self.push_scoped(cause, scope, message);
        self.drain(generation, targets, timeout).await
    }

    /// End the connections of the `targets` still authenticating: they are
    /// not listening to the watch yet, have received nothing, and hold a
    /// database. Returns how many were aborted. Spec P5.2 §8.A: the one
    /// forced close an online promotion makes — a session past its
    /// authentication is told and waited for, never cut short.
    pub fn abort_authenticating(&self, targets: &[u64]) -> usize {
        let mut aborted = 0;
        for id in targets {
            let abort = {
                let entries = self.entries.lock();
                entries
                    .get(id)
                    .filter(|entry| !entry.established.load(Ordering::Acquire))
                    .and_then(|entry| entry.abort.clone())
            };
            if let Some(abort) = abort {
                abort.abort();
                aborted += 1;
            }
        }
        aborted
    }

    /// Wait for `targets` of termination `generation` to unregister
    /// ([`Self::terminate_all`]).
    pub async fn drain(&self, generation: u64, targets: Vec<u64>, timeout: Duration) -> Drained {
        let targeted = targets.len();
        // Not established: still authenticating, so not listening to the
        // watch yet. Its connection ends now.
        let mut aborted = self.abort_authenticating(&targets);
        let deadline = Instant::now() + timeout;
        let mut remaining = self.still_registered(&targets);
        while !remaining.is_empty() && Instant::now() < deadline {
            tokio::select! {
                () = self.left.notified() => {}
                () = tokio::time::sleep(DRAIN_POLL) => {}
            }
            remaining = self.still_registered(&targets);
        }
        if !remaining.is_empty() {
            for id in &remaining {
                let abort = self.entries.lock().get(id).and_then(|entry| entry.abort.clone());
                if let Some(abort) = abort {
                    abort.abort();
                    aborted += 1;
                }
            }
            tracing::warn!(
                "{} PostgreSQL session(s) did not end within {} s of the termination (FATAL 57P01); their \
                 connections were closed",
                remaining.len(),
                timeout.as_secs()
            );
            let grace = Instant::now() + ABORT_GRACE;
            while !remaining.is_empty() && Instant::now() < grace {
                tokio::select! {
                    () = self.left.notified() => {}
                    () = tokio::time::sleep(DRAIN_POLL) => {}
                }
                remaining = self.still_registered(&targets);
            }
        }
        Drained {
            generation,
            targeted,
            aborted,
            remaining: remaining.len(),
        }
    }

    fn still_registered(&self, targets: &[u64]) -> Vec<u64> {
        let entries = self.entries.lock();
        targets.iter().copied().filter(|id| entries.contains_key(id)).collect()
    }
}

/// A session's registration ([`SessionRegistry::admit`]); unregisters when
/// dropped, however the connection ends.
pub struct SessionTicket {
    id: u64,
    registry: &'static SessionRegistry,
    admitted_generation: u64,
    exempt: Arc<AtomicBool>,
    established: Arc<AtomicBool>,
    served_by: Arc<AtomicU64>,
    peer: Option<String>,
    terminate: watch::Receiver<Terminate>,
}

impl std::fmt::Debug for SessionTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTicket")
            .field("id", &self.id)
            .field("admitted_generation", &self.admitted_generation)
            .field("exempt", &self.is_exempt())
            .finish_non_exhaustive()
    }
}

impl SessionTicket {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The client's address, as the listener accepted it.
    pub fn peer(&self) -> Option<&str> {
        self.peer.as_deref()
    }

    /// The generation the session was admitted at.
    pub fn admitted_generation(&self) -> u64 {
        self.admitted_generation
    }

    /// Spec P5.2 §2: the `pg_promote` caller is exempt from the termination
    /// of its own promotion — it is answered first, then closes itself.
    pub fn set_exempt(&self, exempt: bool) {
        self.exempt.store(exempt, Ordering::Release);
    }

    pub fn is_exempt(&self) -> bool {
        self.exempt.load(Ordering::Acquire)
    }

    /// Spec P5.2 §8.A: the database engine serving this session from now on
    /// (`StorageEngine::instance_id`) — set by the listener right after it
    /// chose the database, and by the `pg_promote` caller when it lets go of
    /// the standby's store.
    pub fn serve(&self, engine: u64) {
        self.served_by.store(engine, Ordering::Release);
    }

    /// The engine serving this session (0: not known yet).
    pub fn served_by(&self) -> u64 {
        self.served_by.load(Ordering::Acquire)
    }

    /// The session reached its message loop, where it watches for a
    /// termination (until then a termination aborts it); its backend pid is
    /// listed from now on.
    pub fn established(&self, backend_pid: Option<i32>) {
        self.established.store(true, Ordering::Release);
        if let Some(pid) = backend_pid {
            self.registry.describe(self.id, pid);
        }
    }

    /// A receiver of the registry's terminations, as of the admission: a
    /// termination published since then shows as changed.
    pub fn watch(&self) -> watch::Receiver<Terminate> {
        self.terminate.clone()
    }

    /// Whether `terminate` ends this session: newer than its admission, not
    /// exempt, and — for a fence — not a session opened after the fence
    /// (`admitted_after_fence`), which is served reads only.
    pub fn targeted_by(&self, terminate: &Terminate, admitted_after_fence: bool) -> bool {
        terminate.generation > self.admitted_generation
            && terminate.cause != TerminateCause::None
            && !self.is_exempt()
            && terminate.scope.covers(self.served_by())
            && !(terminate.cause == TerminateCause::Fence && admitted_after_fence)
    }
}

impl Drop for SessionTicket {
    fn drop(&mut self) {
        self.registry.leave(self.id);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A registry of the test's own: the process-global one may serve other
    /// tests' sessions, which a termination here must not end.
    fn private() -> &'static SessionRegistry {
        Box::leak(Box::new(SessionRegistry::new()))
    }

    #[tokio::test]
    async fn a_termination_targets_the_sessions_admitted_before_it_and_spares_the_exempt() {
        let registry = private();
        let before = registry.admit(Some("127.0.0.1:1".into()));
        let exempt = registry.admit(Some("127.0.0.1:2".into()));
        exempt.set_exempt(true);
        before.established(Some(41));
        exempt.established(Some(42));
        let (generation, targets) = registry.push(TerminateCause::Promotion, "terminating connection: test");
        assert_eq!(targets, vec![before.id()], "the exempt session is no target");
        let after = registry.admit(None);
        assert_eq!(
            after.admitted_generation(),
            generation,
            "admitted at the new generation"
        );
        let terminate = before.watch().borrow().clone();
        assert!(before.targeted_by(&terminate, false));
        assert!(!exempt.targeted_by(&terminate, false));
        assert!(
            !after.targeted_by(&terminate, false),
            "a session admitted after it is no target"
        );
        // The target leaves: the drain sees it gone.
        let id = before.id();
        drop(before);
        let drained = registry.drain(generation, vec![id], Duration::from_secs(1)).await;
        assert_eq!((drained.targeted, drained.remaining, drained.aborted), (1, 0, 0));
    }

    #[test]
    fn a_fence_spares_the_sessions_opened_after_it() {
        let registry = private();
        let ticket = registry.admit(None);
        let fence = Terminate {
            generation: ticket.admitted_generation() + 1,
            cause: TerminateCause::Fence,
            scope: TerminateScope::All,
            sqlstate: crate::network::protocol::sqlstate::ADMIN_SHUTDOWN,
            message: "terminating connection: this primary was fenced by epoch 2".into(),
        };
        assert!(ticket.targeted_by(&fence, false));
        assert!(
            !ticket.targeted_by(&fence, true),
            "opened after the fence: served reads only"
        );
        assert!(!ticket.targeted_by(
            &Terminate {
                cause: TerminateCause::None,
                ..fence
            },
            false
        ));
    }

    /// Spec P5.2 §8.A/§8.H: a promotion ends the sessions of the store and
    /// spares the placeholder's; after the reopen it ends the placeholder's
    /// only. A session whose database is not known yet may hold the store.
    #[test]
    fn a_scoped_termination_targets_the_sessions_of_one_database() {
        const STORE: u64 = 7;
        const PLACEHOLDER: u64 = 9;
        let registry = private();
        let on_store = registry.admit(None);
        on_store.serve(STORE);
        let on_placeholder = registry.admit(None);
        on_placeholder.serve(PLACEHOLDER);
        let unknown = registry.admit(None);
        let holders: Vec<u64> = registry.holders_of(STORE).iter().map(|session| session.id).collect();
        assert_eq!(holders, vec![on_store.id(), unknown.id()]);
        let (_, mut targets) = registry.push_scoped(
            TerminateCause::Promotion,
            TerminateScope::NotServedBy(PLACEHOLDER),
            "terminating connection: test",
        );
        targets.sort_unstable();
        assert_eq!(targets, vec![on_store.id(), unknown.id()]);
        let terminate = on_store.watch().borrow().clone();
        assert!(on_store.targeted_by(&terminate, false));
        assert!(
            !on_placeholder.targeted_by(&terminate, false),
            "the placeholder answers the probes"
        );
        let (_, targets) = registry.push_scoped(
            TerminateCause::Promotion,
            TerminateScope::ServedBy(PLACEHOLDER),
            "terminating connection: test",
        );
        assert_eq!(targets, vec![on_placeholder.id()]);
        let terminate = on_placeholder.watch().borrow().clone();
        assert!(on_placeholder.targeted_by(&terminate, false));
        assert!(!on_store.targeted_by(&terminate, false));
    }

    #[tokio::test]
    async fn a_session_that_does_not_leave_is_aborted() {
        let registry = private();
        let ticket = registry.admit(None);
        ticket.established(None);
        let id = ticket.id();
        // The "connection task": holds the ticket until it is aborted.
        let task = tokio::spawn(async move {
            let _ticket = ticket;
            std::future::pending::<()>().await;
        });
        registry.attach_abort(id, task.abort_handle());
        let (generation, targets) = registry.push(TerminateCause::Promotion, "terminating connection: test");
        assert_eq!(targets, vec![id]);
        let drained = registry.drain(generation, targets, Duration::from_millis(100)).await;
        assert_eq!((drained.targeted, drained.aborted, drained.remaining), (1, 1, 0));
        assert!(task.await.unwrap_err().is_cancelled());
    }
}
