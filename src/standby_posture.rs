//! What a physical standby tells the world about itself (spec P1.3,
//! `docs/plans/specs/P1_3_standby_start_posture_pgwire.md`; plan §D3.5, §D6,
//! §D8).
//!
//! # Why
//!
//! A physical standby stays up whatever happens to its receiver (field
//! finding J2): a primary that restarts, a history it may not follow, a WAL
//! gap. Staying up is only useful if every reader can tell what the standby is
//! doing, and the readers are three different surfaces that must never
//! disagree: `/health` and `/ready` on the HTTP port, the PostgreSQL wire that
//! HeliosDB Proxy probes (`heliosdb_require_ready()`, `pg_is_in_recovery()`,
//! the LSN functions, `in_hot_standby`), and the read guard that refuses data
//! reads while the standby cannot vouch for them. All three read the one
//! [`StandbySnapshot`] this module publishes.
//!
//! # Posture
//!
//! [`PostureTracker`] derives the posture from the receiver's progress
//! (`physical_receiver::ReceiverStatus`) and from the start path's own verdicts
//! (a refused directory, a terminal receiver error):
//!
//! | Posture | Entered when | Ready |
//! |---|---|---|
//! | `connecting` | no session has applied anything since the store was attached | no |
//! | `recovering` | a bootstrap copy is downloading (`mode = "bootstrap"`) | no |
//! | `catching_up` | the first session after the store was attached has not reached its connect barrier yet | no |
//! | `streaming` | the current session's cursor reached the barrier the primary reported (`CaughtUp`, or an `Applying` of the same session at or past a barrier it reported earlier — its `CaughtUp` was overwritten before it was observed); kept while that session lives | yes |
//! | `reconnecting` | a streaming session was lost; kept until a new session catches up | while disconnected for at most `[replication] max_standby_lag` |
//! | `needs_recovery` | a refusal the receiver cannot resolve by itself (history mismatch, gap, poison, a directory it may not adopt); sticky until restart | no |
//!
//! Spec P3.2 §3: an ARCHIVE-ONLY standby (`--source archive`) has no receiver
//! and never streams. While it applies segments its posture is `recovering`
//! with mode `archive` and progress `"<cursor> of <newest>"`; it is ready
//! while the segment holding its cursor was observed (by the archiver that
//! wrote it) at most `max_standby_lag` ago, else `catching_up`. A gap in its
//! directories is `needs_recovery` naming the range — not sticky: it resumes
//! by itself when the missing segment appears ([`PostureTracker::archive_progress`]).
//!
//! Readiness also needs a serving store: a standby that is still installing
//! its first copy is never ready, whatever its receiver reports.
//!
//! `lag_secs` is the time since the standby last confirmed it held everything
//! its primary had (the last poll whose cursor reached the primary's barrier),
//! not the time since the last applied batch: a physical batch carries no
//! commit time, and an idle primary sends no batches, so "time since the last
//! apply" would grow without bound on a perfectly current standby.
//! `lag_bytes` is the sequence delta `primary_cursor - cursor` — "write
//! operations behind", monotone and comparable, as Proxy's `max_lag_bytes`
//! expects (§D8). `primary_cursor` is the last PHYSICAL barrier observed: a
//! session's connect barrier, a poll's announced durable barrier (reported
//! before its batches are applied, so it stays current through a long poll),
//! a caught-up poll's barrier. It never moves back, and it is kept across
//! reconnects until a new barrier arrives. Physical sequences only: since
//! spec P2.1 there is no other channel, and no logical WAL LSN, to confuse it
//! with.
//!
//! # Where it lives
//!
//! At the crate root, compiled in every build, because the SQL layer asks it
//! in every build; `replication::standby_posture` re-exports it for the HA
//! code. Only `PostureTracker::observe` (which reads the physical receiver's
//! status) needs `ha-tier1`.
//!
//! # Node status
//!
//! The server process installs what it is ([`install_standby`],
//! [`install_primary`]); the SQL functions and views of the PostgreSQL wire
//! read it through the façade at the bottom of this module, which works in
//! every build (a binary without `ha-tier1` is never in recovery).

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::sync::watch;

use crate::storage::StorageEngine;

/// The replication epoch every node reports until promotion makes it real
/// (plan §D7, phase P5): `pg_control_checkpoint().timeline_id` and the
/// `epoch` field of the replication block.
pub const EPOCH: u64 = 1;

/// The `recovering` mode of an archive-only standby (spec P3.2 §3).
pub const ARCHIVE_MODE: &str = "archive";

/// PostgreSQL's LSN text for a physical sequence: `HI/LO` in hex, `HI` the
/// upper and `LO` the lower 32 bits (§D8).
pub fn format_lsn(sequence: u64) -> String {
    format!("{:X}/{:X}", sequence >> 32, sequence & 0xFFFF_FFFF)
}

/// A physical standby's posture. See the module documentation for when each
/// is entered and which are ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StandbyPosture {
    Connecting,
    CatchingUp {
        cursor: u64,
        barrier: u64,
    },
    Streaming {
        cursor: u64,
    },
    Reconnecting {
        since: Instant,
        cursor: u64,
    },
    NeedsRecovery {
        reason: String,
        position: u64,
        since: Instant,
    },
    Recovering {
        mode: &'static str,
        progress: String,
    },
}

impl StandbyPosture {
    /// The state name every surface reports (`/health`, the view, the 55000
    /// message).
    pub fn state(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::CatchingUp { .. } => "catching_up",
            Self::Streaming { .. } => "streaming",
            Self::Reconnecting { .. } => "reconnecting",
            Self::NeedsRecovery { .. } => "needs_recovery",
            Self::Recovering { .. } => "recovering",
        }
    }
}

/// Everything a standby reports, as of one moment.
#[derive(Clone, Debug)]
pub struct StandbySnapshot {
    pub posture: StandbyPosture,
    /// The primary's sequence this standby has applied through (its durable
    /// physical cursor); 0 before its first copy is installed.
    pub cursor: u64,
    /// The last physical durable barrier the primary reported (module
    /// documentation); 0 until one is observed. Physical sequences only.
    pub primary_cursor: u64,
    /// When a poll last reached the primary's barrier.
    pub last_caught_up: Option<Instant>,
    /// The physical source this standby follows (`host:port`).
    pub primary: String,
    pub sync_mode: String,
    /// The sequence this standby last reported durable to its primary: the
    /// durable cursor its polls start from, which its heartbeats acknowledge
    /// (spec P2.1, plan §D3.6).
    pub last_ack_sequence: Option<u64>,
    pub epoch: u64,
    /// Whether a serving store is attached (`EmbeddedDatabase::with_physical_standby`
    /// over the receiver's store). Until then the listeners serve an empty
    /// placeholder and nothing is ready.
    pub store_attached: bool,
    pub max_standby_lag: Duration,
    /// Spec P2.2: the current (or last) replication session is on TLS, both
    /// ends said so.
    pub tls: bool,
    /// Spec P2.2: why the receiver's last connection attempt failed while it
    /// is connecting or reconnecting (a TLS failure reads `tls: …`); cleared
    /// once a session delivers again.
    pub last_error: Option<String>,
}

impl StandbySnapshot {
    /// Whether this standby may serve data reads: a serving store, and
    /// `streaming`, or `reconnecting` for at most `max_standby_lag`.
    pub fn ready_at(&self, now: Instant) -> bool {
        self.store_attached
            && match &self.posture {
                StandbyPosture::Streaming { .. } => true,
                StandbyPosture::Reconnecting { since, .. } => {
                    now.saturating_duration_since(*since) <= self.max_standby_lag
                }
                // Spec P3.2 §3: an archive-only standby is as current as the
                // segment holding its cursor.
                StandbyPosture::Recovering { mode, .. } if *mode == ARCHIVE_MODE => self
                    .last_caught_up
                    .is_some_and(|at| now.saturating_duration_since(at) <= self.max_standby_lag),
                _ => false,
            }
    }

    pub fn ready(&self) -> bool {
        self.ready_at(Instant::now())
    }

    /// `primary_cursor - cursor`, the sequence delta (module documentation).
    pub fn lag_bytes(&self) -> u64 {
        self.primary_cursor.saturating_sub(self.cursor)
    }

    /// Seconds since the standby last confirmed it was current; `None` while
    /// it never has (module documentation).
    pub fn lag_secs(&self) -> Option<f64> {
        self.last_caught_up
            .map(|at| (at.elapsed().as_secs_f64() * 1000.0).round() / 1000.0)
    }

    /// The refusal a not-ready standby answers: SQLSTATE 55000 on the wire
    /// ([`crate::error::STANDBY_NOT_READY`]). `Error::Generic`, whose text is
    /// the message alone, so the wire carries exactly
    /// [`Self::not_ready_message`] and no error-class prefix.
    pub fn not_ready_error(&self) -> crate::Error {
        crate::Error::Generic(self.not_ready_message())
    }

    /// `standby is not ready: <state> (cursor X, primary Y)`, then the
    /// reason of a `needs_recovery`, or the absence of a store.
    pub fn not_ready_message(&self) -> String {
        let mut message = format!(
            "{}: {} (cursor {}, primary {})",
            crate::error::STANDBY_NOT_READY,
            self.posture.state(),
            format_lsn(self.cursor),
            format_lsn(self.primary_cursor)
        );
        match &self.posture {
            StandbyPosture::NeedsRecovery { reason, .. } => {
                message.push_str(": ");
                message.push_str(reason);
            }
            _ if !self.store_attached => message.push_str(": no data store is attached yet"),
            _ => {}
        }
        message
    }

    /// The frozen replication block (`/health`, `/ready`, the view).
    pub fn block(&self) -> ReplicationBlock {
        ReplicationBlock {
            role: "standby".to_string(),
            state: self.posture.state().to_string(),
            ready: self.ready(),
            cursor: format_lsn(self.cursor),
            primary_cursor: format_lsn(self.primary_cursor),
            lag_bytes: self.lag_bytes(),
            lag_secs: self.lag_secs(),
            primary: Some(self.primary.clone()),
            sync_mode: self.sync_mode.clone(),
            last_ack_sequence: self.last_ack_sequence,
            epoch: self.epoch,
            tls: self.tls,
            reason: match &self.posture {
                StandbyPosture::NeedsRecovery { reason, .. } => Some(reason.clone()),
                StandbyPosture::Connecting | StandbyPosture::Reconnecting { .. } => self.last_error.clone(),
                _ => None,
            },
            progress: match &self.posture {
                StandbyPosture::Recovering { progress, .. } => Some(progress.clone()),
                _ => None,
            },
        }
    }
}

/// The one field set `/health`'s `replication` object, `/ready`'s body and
/// the `heliosdb_standby_status` view share (§D8). The first eleven fields
/// are frozen for HeliosDB Proxy (names and types); spec P2.2 ADDED `tls`,
/// `reason` and `progress` after them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReplicationBlock {
    pub role: String,
    pub state: String,
    pub ready: bool,
    /// PostgreSQL LSN text of the applied (standby) or written (primary)
    /// physical sequence.
    pub cursor: String,
    pub primary_cursor: String,
    pub lag_bytes: u64,
    pub lag_secs: Option<f64>,
    pub primary: Option<String>,
    pub sync_mode: String,
    pub last_ack_sequence: Option<u64>,
    pub epoch: u64,
    /// Spec P2.2: the replication session is on TLS (a standby: its session
    /// to the primary; a primary: its replication port serves TLS).
    pub tls: bool,
    /// Spec P2.2: why a standby is not streaming — the reason of a
    /// `needs_recovery`, or the last connection failure while `connecting` /
    /// `reconnecting` (e.g. `tls: invalid peer certificate: UnknownIssuer`).
    pub reason: Option<String>,
    /// Spec P2.2: a `recovering` standby's download progress,
    /// `"<bytes held>/<bytes total> bytes"`.
    pub progress: Option<String>,
}

/// A live reader of the standby's applied cursor, exact to the batch (the
/// receiver's status is only sampled per poll). Set once the store exists.
pub type LiveCursor = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// A clonable reader of a standby's posture.
#[derive(Clone)]
pub struct PostureHandle {
    rx: watch::Receiver<StandbySnapshot>,
    live: Arc<RwLock<Option<LiveCursor>>>,
}

impl PostureHandle {
    /// The current snapshot, its cursor read from the store when attached.
    pub fn snapshot(&self) -> StandbySnapshot {
        let mut snapshot = self.rx.borrow().clone();
        if let Some(live) = self.live.read().as_ref() {
            if let Some(cursor) = live() {
                snapshot.cursor = snapshot.cursor.max(cursor);
            }
        }
        snapshot
    }

    pub fn ready(&self) -> bool {
        self.readiness().0
    }

    /// `(ready, store_attached)` without reading the store's cursor: what the
    /// PostgreSQL wire's guard asks before EVERY statement on a standby, so it
    /// must not wait on the applier's writer, whose cursor lock an fsync holds.
    pub fn readiness(&self) -> (bool, bool) {
        let snapshot = self.rx.borrow();
        (snapshot.ready(), snapshot.store_attached)
    }

    /// Wake on every posture change (the start path's readiness gate).
    pub fn subscribe(&self) -> watch::Receiver<StandbySnapshot> {
        self.rx.clone()
    }
}

/// The receiver progress the tracker understands, decoupled from
/// `ReceiverStatus` so the state machine needs no physical types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiverEvent {
    Connecting {
        after: Option<u64>,
    },
    Downloading {
        received: u64,
        total: u64,
    },
    Applying {
        cursor: u64,
        barrier: Option<u64>,
        session: u64,
    },
    CaughtUp {
        cursor: u64,
        barrier: u64,
        session: u64,
    },
    Installed {
        cursor: u64,
    },
    /// `Stopped` / `Failed`: the start path classifies a failure from the
    /// receiver's typed error ([`PostureTracker::needs_recovery`]).
    Ended,
}

#[derive(Default)]
struct Machine {
    /// A session applied something since the store was attached.
    ever_connected: bool,
    /// The receiver session that reached its barrier, while it lives.
    streaming_session: Option<u64>,
    /// When the last streaming session was lost, until a new one catches up.
    reconnect_since: Option<Instant>,
    /// The session the last `Applying` / `CaughtUp` came from, and the
    /// highest barrier it reported before that event.
    session: Option<u64>,
    session_barrier: Option<u64>,
    /// Spec P3.2 §3: the `needs_recovery` posture is an archive-only
    /// standby's gap, which a new segment resolves (not a sticky refusal).
    archive_blocked: bool,
}

impl Machine {
    /// Note that `session` reported `barrier`; returns the highest barrier
    /// that session reported BEFORE this event (`None` for a new session).
    fn note_barrier(&mut self, session: u64, barrier: Option<u64>) -> Option<u64> {
        if self.session != Some(session) {
            self.session = Some(session);
            self.session_barrier = None;
        }
        let before = self.session_barrier;
        if let Some(barrier) = barrier {
            self.session_barrier = Some(before.map_or(barrier, |known| known.max(barrier)));
        }
        before
    }
}

/// The one writer of a standby's posture.
pub struct PostureTracker {
    tx: watch::Sender<StandbySnapshot>,
    live: Arc<RwLock<Option<LiveCursor>>>,
    machine: Mutex<Machine>,
}

impl PostureTracker {
    pub fn new(primary: String, sync_mode: String, max_standby_lag: Duration) -> Self {
        let (tx, _) = watch::channel(StandbySnapshot {
            posture: StandbyPosture::Connecting,
            cursor: 0,
            primary_cursor: 0,
            last_caught_up: None,
            primary,
            sync_mode,
            last_ack_sequence: None,
            epoch: EPOCH,
            store_attached: false,
            max_standby_lag,
            tls: false,
            last_error: None,
        });
        Self {
            tx,
            live: Arc::new(RwLock::new(None)),
            machine: Mutex::new(Machine::default()),
        }
    }

    /// Spec P2.2: what the receiver reports about its transport — why its
    /// last connection attempt failed (`Some` while it retries), and whether
    /// the current session is on TLS (`Some` once a session delivers).
    pub fn note_transport(&self, last_error: Option<String>, tls: Option<bool>) {
        self.tx.send_if_modified(|snapshot| {
            let before = (snapshot.last_error.clone(), snapshot.tls);
            if let Some(error) = last_error {
                snapshot.last_error = Some(error);
            }
            if let Some(tls) = tls {
                snapshot.tls = tls;
            }
            (snapshot.last_error.clone(), snapshot.tls) != before
        });
    }

    pub fn handle(&self) -> PostureHandle {
        PostureHandle {
            rx: self.tx.subscribe(),
            live: Arc::clone(&self.live),
        }
    }

    /// A serving store now exists; `live` reads its applied cursor. The
    /// first session over it must catch up before the standby is ready, so
    /// the session history is forgotten: a copy installed by a bootstrap was
    /// current when it was installed, not when the store is attached.
    pub fn attach_store(&self, cursor: u64, live: LiveCursor) {
        let mut machine = self.machine.lock();
        *machine = Machine::default();
        *self.live.write() = Some(live);
        self.tx.send_modify(|snapshot| {
            snapshot.store_attached = true;
            snapshot.cursor = snapshot.cursor.max(cursor);
            if !matches!(snapshot.posture, StandbyPosture::NeedsRecovery { .. }) {
                snapshot.posture = StandbyPosture::Connecting;
            }
        });
    }

    /// A refusal the receiver cannot resolve by itself. Sticky: only a
    /// restart (or a later recovery mode) leaves it.
    pub fn needs_recovery(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut machine = self.machine.lock();
        // A refusal replaces an archive-only standby's gap (spec P3.2 §3),
        // which is not sticky; it then is.
        let archive_gap = std::mem::replace(&mut machine.archive_blocked, false);
        self.tx.send_modify(|snapshot| {
            if archive_gap || !matches!(snapshot.posture, StandbyPosture::NeedsRecovery { .. }) {
                snapshot.posture = StandbyPosture::NeedsRecovery {
                    reason,
                    position: snapshot.cursor,
                    since: Instant::now(),
                };
            }
        });
    }

    /// Spec P3.2 §3: an archive-only standby has applied its directories'
    /// segments through `cursor`, of the `newest` sequence they hold; the
    /// segment holding `cursor` was last observed at `observed_at` (unix
    /// seconds, the clock of the archiver that wrote it; `None` when no
    /// segment holds it) and it is `now`. Its lag is the time since then:
    /// within `max_standby_lag` the posture is `recovering` (mode `archive`,
    /// progress `"<cursor> of <newest>"`) and ready, beyond it
    /// `catching_up`. Leaves an archive gap (a new segment closed it), never a
    /// refusal ([`Self::needs_recovery`] stays sticky).
    pub fn archive_progress(&self, cursor: u64, newest: u64, observed_at: Option<i64>, now: i64) {
        let mut machine = self.machine.lock();
        let archive_gap = machine.archive_blocked;
        self.tx.send_modify(|snapshot| {
            if matches!(snapshot.posture, StandbyPosture::NeedsRecovery { .. }) && !archive_gap {
                return;
            }
            snapshot.cursor = snapshot.cursor.max(cursor);
            snapshot.primary_cursor = snapshot.primary_cursor.max(newest).max(snapshot.cursor);
            snapshot.last_ack_sequence = Some(snapshot.cursor);
            snapshot.last_caught_up = observed_at.and_then(|at| {
                let age = u64::try_from(now.saturating_sub(at)).unwrap_or(0);
                Instant::now().checked_sub(Duration::from_secs(age))
            });
            snapshot.last_error = None;
            let ready = snapshot
                .last_caught_up
                .is_some_and(|at| at.elapsed() <= snapshot.max_standby_lag);
            snapshot.posture = if ready {
                StandbyPosture::Recovering {
                    mode: ARCHIVE_MODE,
                    progress: format!("{} of {}", snapshot.cursor, snapshot.primary_cursor),
                }
            } else {
                StandbyPosture::CatchingUp {
                    cursor: snapshot.cursor,
                    barrier: snapshot.primary_cursor,
                }
            };
        });
        machine.archive_blocked = false;
    }

    /// Spec P3.2 §3: an archive-only standby cannot continue — a gap in its
    /// directories, or a segment it may not apply — named by `reason`. It
    /// stays up in `needs_recovery` and resumes by itself once the directories
    /// change ([`Self::archive_progress`]). A refusal posted by
    /// [`Self::needs_recovery`] is never replaced.
    pub fn archive_blocked(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut machine = self.machine.lock();
        let archive_gap = machine.archive_blocked;
        let mut blocked = false;
        self.tx.send_if_modified(|snapshot| {
            match &snapshot.posture {
                StandbyPosture::NeedsRecovery { .. } if !archive_gap => return false,
                StandbyPosture::NeedsRecovery { reason: current, .. } if *current == reason => {
                    blocked = true;
                    return false;
                }
                _ => {}
            }
            blocked = true;
            snapshot.posture = StandbyPosture::NeedsRecovery {
                reason,
                position: snapshot.cursor,
                since: Instant::now(),
            };
            true
        });
        if blocked {
            machine.archive_blocked = true;
        }
    }

    /// The posture as of now (for the start path's own decisions).
    pub fn snapshot(&self) -> StandbySnapshot {
        self.handle().snapshot()
    }

    /// Fold one receiver progress report into the posture.
    #[cfg(feature = "ha-tier1")]
    pub fn observe(&self, status: &crate::replication::physical_receiver::ReceiverStatus) {
        use crate::replication::physical_receiver::ReceiverStatus as Status;
        match status {
            Status::Connecting { last_error, .. } => self.note_transport(last_error.clone(), None),
            Status::Applying { tls, .. } | Status::CaughtUp { tls, .. } => self.note_transport(None, Some(*tls)),
            _ => {}
        }
        let event = match status {
            Status::Connecting { after, .. } => ReceiverEvent::Connecting { after: *after },
            Status::Downloading { received, total, .. } => ReceiverEvent::Downloading {
                received: *received,
                total: *total,
            },
            Status::Applying {
                cursor,
                source_barrier,
                session,
                ..
            } => ReceiverEvent::Applying {
                cursor: cursor.source_sequence(),
                barrier: *source_barrier,
                session: *session,
            },
            Status::CaughtUp {
                cursor,
                source_barrier,
                session,
                ..
            } => ReceiverEvent::CaughtUp {
                cursor: cursor.source_sequence(),
                barrier: *source_barrier,
                session: *session,
            },
            Status::Installed { cursor, .. } => ReceiverEvent::Installed {
                cursor: cursor.source_sequence(),
            },
            Status::Stopped { .. } | Status::Failed { .. } => ReceiverEvent::Ended,
        };
        self.apply_event(event, Instant::now());
    }

    /// The state machine of the module documentation.
    pub fn apply_event(&self, event: ReceiverEvent, now: Instant) {
        let mut machine = self.machine.lock();
        self.tx.send_if_modified(|snapshot| {
            // A late report from a dying receiver must not hide the refusal.
            if matches!(snapshot.posture, StandbyPosture::NeedsRecovery { .. }) {
                return false;
            }
            let before = (snapshot.posture.clone(), snapshot.cursor, snapshot.primary_cursor);
            match event {
                ReceiverEvent::Connecting { after } => {
                    if let Some(after) = after {
                        snapshot.cursor = snapshot.cursor.max(after);
                    }
                    machine.streaming_session = None;
                    snapshot.posture = if machine.ever_connected {
                        StandbyPosture::Reconnecting {
                            since: *machine.reconnect_since.get_or_insert(now),
                            cursor: snapshot.cursor,
                        }
                    } else {
                        StandbyPosture::Connecting
                    };
                }
                ReceiverEvent::Downloading { received, total } => {
                    // A session delivers: the connection failure is history.
                    snapshot.last_error = None;
                    snapshot.posture = StandbyPosture::Recovering {
                        mode: "bootstrap",
                        progress: format!("{received}/{total} bytes"),
                    };
                }
                ReceiverEvent::Installed { cursor } => {
                    snapshot.cursor = snapshot.cursor.max(cursor);
                    snapshot.posture = StandbyPosture::CatchingUp {
                        cursor: snapshot.cursor,
                        barrier: snapshot.primary_cursor.max(snapshot.cursor),
                    };
                }
                ReceiverEvent::Applying {
                    cursor,
                    barrier,
                    session,
                } => {
                    machine.ever_connected = true;
                    snapshot.last_error = None;
                    // The session's cursor already reached a barrier the
                    // same session reported: it caught up, and its
                    // `CaughtUp` was overwritten (the receiver polls again at
                    // once after a poll that carried groups) before it was
                    // observed.
                    let reached = machine
                        .note_barrier(session, barrier)
                        .is_some_and(|known| cursor >= known);
                    snapshot.cursor = snapshot.cursor.max(cursor);
                    snapshot.last_ack_sequence = Some(snapshot.cursor);
                    if let Some(barrier) = barrier {
                        snapshot.primary_cursor = snapshot.primary_cursor.max(barrier);
                    }
                    if reached && machine.streaming_session != Some(session) {
                        machine.streaming_session = Some(session);
                        machine.reconnect_since = None;
                        snapshot.last_caught_up = Some(now);
                    }
                    if machine.streaming_session == Some(session) {
                        // Every poll of a streaming session starts with an
                        // `Applying`; it does not leave `streaming`.
                        snapshot.posture = StandbyPosture::Streaming {
                            cursor: snapshot.cursor,
                        };
                    } else {
                        if machine.streaming_session.take().is_some() && machine.reconnect_since.is_none() {
                            // A new session whose `Connecting` was not
                            // observed: the streaming one was lost.
                            machine.reconnect_since = Some(now);
                        }
                        snapshot.posture = match machine.reconnect_since {
                            Some(since) => StandbyPosture::Reconnecting {
                                since,
                                cursor: snapshot.cursor,
                            },
                            None => StandbyPosture::CatchingUp {
                                cursor: snapshot.cursor,
                                barrier: snapshot.primary_cursor.max(snapshot.cursor),
                            },
                        };
                    }
                }
                ReceiverEvent::CaughtUp {
                    cursor,
                    barrier,
                    session,
                } => {
                    machine.ever_connected = true;
                    snapshot.last_error = None;
                    machine.note_barrier(session, Some(barrier));
                    machine.streaming_session = Some(session);
                    machine.reconnect_since = None;
                    snapshot.cursor = snapshot.cursor.max(cursor);
                    snapshot.primary_cursor = snapshot.primary_cursor.max(barrier).max(snapshot.cursor);
                    snapshot.last_ack_sequence = Some(snapshot.cursor);
                    snapshot.last_caught_up = Some(now);
                    snapshot.posture = StandbyPosture::Streaming {
                        cursor: snapshot.cursor,
                    };
                    return true;
                }
                ReceiverEvent::Ended => {}
            }
            (snapshot.posture.clone(), snapshot.cursor, snapshot.primary_cursor) != before
        });
    }
}

// -------------------------------------------------------------------------
// Node status: what this process is, for the SQL surface and HTTP.
// -------------------------------------------------------------------------

/// What a primary reports.
#[derive(Clone)]
pub struct PrimaryStatus {
    /// `"primary"`.
    pub role: &'static str,
    /// The engine whose physical sequence is this node's cursor.
    pub engine: Weak<StorageEngine>,
    /// This node's replication port (`NPHYS003`), when it serves one.
    pub endpoint: Option<String>,
    pub sync_mode: String,
    /// Spec P2.2: the replication port serves TLS.
    pub tls: bool,
}

impl PrimaryStatus {
    pub fn block(&self) -> ReplicationBlock {
        let cursor = self.engine.upgrade().map_or(0, |engine| engine_sequence(&engine));
        primary_block(
            self.role,
            cursor,
            self.endpoint.clone(),
            self.sync_mode.clone(),
            self.tls,
        )
    }
}

#[derive(Clone)]
enum NodeStatus {
    Standby(PostureHandle),
    Primary(PrimaryStatus),
}

/// Process-global, like `ha_state()`: a server process is one node.
static NODE: RwLock<Option<NodeStatus>> = RwLock::new(None);

/// This process is a physical standby reporting `handle`.
pub fn install_standby(handle: PostureHandle) {
    *NODE.write() = Some(NodeStatus::Standby(handle));
}

/// This process is a primary reporting `status`.
pub fn install_primary(status: PrimaryStatus) {
    *NODE.write() = Some(NodeStatus::Primary(status));
}

/// The standby posture of this process, if it is a physical standby.
pub fn standby() -> Option<PostureHandle> {
    match NODE.read().as_ref() {
        Some(NodeStatus::Standby(handle)) => Some(handle.clone()),
        _ => None,
    }
}

/// The replication block of this process and whether it is ready, for
/// `/health` and `/ready`; `None` when no replication role was installed.
pub fn node_block() -> Option<ReplicationBlock> {
    let node = NODE.read().clone();
    match node? {
        NodeStatus::Standby(handle) => Some(handle.snapshot().block()),
        NodeStatus::Primary(status) => Some(status.block()),
    }
}

fn engine_sequence(engine: &StorageEngine) -> u64 {
    engine.db().latest_sequence_number()
}

fn primary_block(role: &str, cursor: u64, primary: Option<String>, sync_mode: String, tls: bool) -> ReplicationBlock {
    ReplicationBlock {
        role: role.to_string(),
        state: "streaming".to_string(),
        ready: true,
        cursor: format_lsn(cursor),
        primary_cursor: format_lsn(cursor),
        lag_bytes: 0,
        lag_secs: Some(0.0),
        primary,
        sync_mode,
        last_ack_sequence: None,
        epoch: EPOCH,
        tls,
        reason: None,
        progress: None,
    }
}

/// An `EmbeddedDatabase::with_physical_standby` engine that no server
/// installed a posture for (the embedded API): its cursor, read from its
/// writer. `None` for every other engine.
fn embedded_standby_cursor(engine: Option<&StorageEngine>) -> Option<u64> {
    #[cfg(feature = "ha-tier1")]
    {
        engine
            .and_then(StorageEngine::physical_standby_writer)
            .map(|writer| writer.cursor().source_sequence())
    }
    #[cfg(not(feature = "ha-tier1"))]
    {
        let _ = engine;
        None
    }
}

// -------------------------------------------------------------------------
// The SQL façade (every build).
// -------------------------------------------------------------------------

/// `pg_is_in_recovery()` and `in_hot_standby`: true on a physical standby, in
/// every posture (including a placeholder session that has no store yet).
pub fn in_recovery(engine: Option<&StorageEngine>) -> bool {
    standby().is_some() || engine.is_some_and(StorageEngine::is_physical_standby)
}

/// `pg_last_wal_replay_lsn()`: the applied physical sequence on a standby,
/// `None` (SQL NULL) on any other node.
pub fn replay_sequence(engine: Option<&StorageEngine>) -> Option<u64> {
    match standby() {
        Some(handle) => Some(handle.snapshot().cursor),
        None => embedded_standby_cursor(engine),
    }
}

/// `pg_current_wal_lsn()`: the physical sequence this node has written — on
/// a standby the same as its replay position (it writes nothing of its own
/// to the replicated keyspace).
pub fn current_sequence(engine: Option<&StorageEngine>) -> u64 {
    if let Some(replayed) = replay_sequence(engine) {
        return replayed;
    }
    match engine {
        Some(engine) => engine_sequence(engine),
        None => match NODE.read().as_ref() {
            Some(NodeStatus::Primary(status)) => status.engine.upgrade().map_or(0, |engine| engine_sequence(&engine)),
            _ => 0,
        },
    }
}

/// `heliosdb_require_ready()`: `Ok(true)` on a primary or a ready standby,
/// else the 55000 refusal. `engine` is the session's: a session served by the
/// placeholder of a standby that had no store when it connected (an engine
/// that is not a physical standby's) is never ready, whatever the posture.
pub fn require_ready(engine: Option<&StorageEngine>) -> crate::Result<bool> {
    match standby() {
        Some(handle) => {
            let mut snapshot = handle.snapshot();
            if engine.is_some_and(|engine| !engine.is_physical_standby()) {
                snapshot.store_attached = false;
            }
            if snapshot.ready() {
                Ok(true)
            } else {
                Err(snapshot.not_ready_error())
            }
        }
        None if engine.is_some_and(StorageEngine::is_physical_standby) => Err(crate::Error::Generic(format!(
            "{}: no replication receiver reports this standby's posture",
            crate::error::STANDBY_NOT_READY
        ))),
        None => Ok(true),
    }
}

/// The `heliosdb_standby_status` row of the node serving `engine`.
pub fn status_block(engine: &StorageEngine) -> ReplicationBlock {
    let node = NODE.read().clone();
    match node {
        Some(NodeStatus::Standby(handle)) => handle.snapshot().block(),
        Some(NodeStatus::Primary(status)) => primary_block(
            status.role,
            engine_sequence(engine),
            status.endpoint,
            status.sync_mode,
            status.tls,
        ),
        None => match embedded_standby_cursor(Some(engine)) {
            // An embedded standby: no receiver, so no posture to report.
            Some(cursor) => ReplicationBlock {
                role: "standby".to_string(),
                state: "connecting".to_string(),
                ready: false,
                cursor: format_lsn(cursor),
                primary_cursor: format_lsn(cursor),
                lag_bytes: 0,
                lag_secs: None,
                primary: None,
                sync_mode: "async".to_string(),
                last_ack_sequence: None,
                epoch: EPOCH,
                tls: false,
                reason: None,
                progress: None,
            },
            None => primary_block("standalone", engine_sequence(engine), None, "async".to_string(), false),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tracker() -> PostureTracker {
        PostureTracker::new("127.0.0.1:5440".into(), "async".into(), Duration::from_secs(30))
    }

    #[test]
    fn lsn_text_is_hi_lo_hex() {
        assert_eq!(format_lsn(0), "0/0");
        assert_eq!(format_lsn(0x1A2B), "0/1A2B");
        assert_eq!(format_lsn((3_u64 << 32) | 0xDEAD_BEEF), "3/DEADBEEF");
        assert_eq!(format_lsn(u64::MAX), "FFFFFFFF/FFFFFFFF");
    }

    #[test]
    fn nothing_is_ready_without_a_store() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 9,
                barrier: 9,
                session: 1,
            },
            t0,
        );
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture.state(), "streaming");
        assert!(!snapshot.ready(), "a bootstrap's catch-up is not a serving store");
        assert!(snapshot
            .not_ready_error()
            .to_string()
            .contains("standby is not ready: streaming (cursor 0/9, primary 0/9)"));
    }

    #[test]
    fn first_session_over_a_store_must_reach_its_barrier() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.attach_store(5, Arc::new(|| None));
        assert_eq!(posture.snapshot().posture, StandbyPosture::Connecting);
        posture.apply_event(ReceiverEvent::Connecting { after: Some(5) }, t0);
        assert_eq!(posture.snapshot().posture.state(), "connecting");
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 5,
                barrier: Some(12),
                session: 1,
            },
            t0,
        );
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture, StandbyPosture::CatchingUp { cursor: 5, barrier: 12 });
        assert_eq!(snapshot.lag_bytes(), 7);
        assert!(!snapshot.ready());
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 12,
                barrier: 12,
                session: 1,
            },
            t0,
        );
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture, StandbyPosture::Streaming { cursor: 12 });
        assert!(snapshot.ready());
        assert_eq!(snapshot.block().cursor, "0/C");
        assert_eq!(snapshot.block().lag_bytes, 0);
        // The next poll of the same session starts with an `Applying`.
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 12,
                barrier: None,
                session: 1,
            },
            t0,
        );
        assert!(
            posture.snapshot().ready(),
            "a streaming session's next poll stays streaming"
        );
    }

    #[test]
    fn a_lost_session_is_ready_only_within_max_standby_lag() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.attach_store(1, Arc::new(|| None));
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 3,
                barrier: 3,
                session: 1,
            },
            t0,
        );
        posture.apply_event(ReceiverEvent::Connecting { after: Some(3) }, t0);
        let snapshot = posture.snapshot();
        assert!(matches!(snapshot.posture, StandbyPosture::Reconnecting { since, cursor: 3 } if since == t0));
        assert!(snapshot.ready_at(t0 + Duration::from_secs(30)));
        assert!(!snapshot.ready_at(t0 + Duration::from_secs(31)));
        // Retried connections keep the original loss time.
        posture.apply_event(
            ReceiverEvent::Connecting { after: Some(3) },
            t0 + Duration::from_secs(20),
        );
        assert!(matches!(posture.snapshot().posture, StandbyPosture::Reconnecting { since, .. } if since == t0));
        // A new session catching up, then reaching its barrier.
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 3,
                barrier: Some(8),
                session: 2,
            },
            t0 + Duration::from_secs(25),
        );
        assert_eq!(posture.snapshot().posture.state(), "reconnecting");
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 8,
                barrier: 8,
                session: 2,
            },
            t0 + Duration::from_secs(26),
        );
        assert_eq!(posture.snapshot().posture, StandbyPosture::Streaming { cursor: 8 });
    }

    #[test]
    fn an_unobserved_reconnect_is_still_a_reconnect() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.attach_store(1, Arc::new(|| None));
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 3,
                barrier: 3,
                session: 1,
            },
            t0,
        );
        // Session 2's `Connecting` was overwritten before it was observed.
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 3,
                barrier: Some(9),
                session: 2,
            },
            t0,
        );
        assert_eq!(posture.snapshot().posture.state(), "reconnecting");
    }

    #[test]
    fn needs_recovery_is_sticky_and_names_its_reason() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.attach_store(4, Arc::new(|| Some(6)));
        posture.needs_recovery("physical replication history mismatch");
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 9,
                barrier: 9,
                session: 1,
            },
            t0,
        );
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture.state(), "needs_recovery");
        assert_eq!(snapshot.cursor, 6, "the live cursor of the store is reported");
        assert!(!snapshot.ready());
        let message = snapshot.not_ready_error().to_string();
        assert!(
            message.contains(
                "standby is not ready: needs_recovery (cursor 0/6, primary 0/0): physical replication history mismatch"
            ),
            "{message}"
        );
    }

    /// Field finding: a reconnecting standby reported `primary 0/C97` — the
    /// native v3 channel's LOGICAL LSN — while its primary's physical sequence
    /// was 0/46EB. `primary_cursor` is the last PHYSICAL barrier: a reconnect
    /// keeps it, and only a physical barrier advances it (spec P2.1 removed
    /// the channel that reported the other number space).
    #[test]
    fn primary_cursor_is_the_last_physical_barrier() {
        let posture = tracker();
        let t0 = Instant::now();
        assert_eq!(posture.snapshot().primary_cursor, 0);
        assert_eq!(posture.snapshot().block().primary_cursor, "0/0");

        posture.attach_store(0x4000, Arc::new(|| None));
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 0x46EB,
                barrier: 0x46EB,
                session: 1,
            },
            t0,
        );
        posture.apply_event(ReceiverEvent::Connecting { after: Some(0x46EB) }, t0);
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture.state(), "reconnecting");
        assert_eq!(snapshot.primary_cursor, 0x46EB, "kept across the reconnect");
        assert_eq!(snapshot.cursor, 0x46EB);
        assert_eq!(snapshot.block().primary_cursor, "0/46EB");
        assert_eq!(snapshot.lag_bytes(), 0);

        // A physical barrier (the new session's, or a poll's) advances it ...
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 0x46EB,
                barrier: Some(0x5000),
                session: 2,
            },
            t0,
        );
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.primary_cursor, 0x5000);
        assert_eq!(snapshot.block().primary_cursor, "0/5000");
        assert_eq!(snapshot.lag_bytes(), 0x5000 - 0x46EB);
        // ... and an older one never takes it back.
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 0x46EB,
                barrier: Some(0x4800),
                session: 2,
            },
            t0,
        );
        assert_eq!(posture.snapshot().primary_cursor, 0x5000);
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 0x4900,
                barrier: 0x4900,
                session: 2,
            },
            t0,
        );
        assert_eq!(posture.snapshot().primary_cursor, 0x5000);
    }

    /// The receiver polls again at once after a poll that carried groups, so
    /// a `CaughtUp` can be overwritten by the next poll's `Applying` before
    /// it is observed. That `Applying` (same session, cursor at the barrier
    /// the session reported) still makes the standby streaming; a new
    /// session's first report never does.
    #[test]
    fn an_overwritten_caught_up_still_reaches_streaming() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.attach_store(5, Arc::new(|| None));
        posture.apply_event(ReceiverEvent::Connecting { after: Some(5) }, t0);
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 5,
                barrier: Some(12),
                session: 1,
            },
            t0,
        );
        assert_eq!(posture.snapshot().posture.state(), "catching_up");
        // `CaughtUp { cursor: 12, barrier: 12 }` was never observed.
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 12,
                barrier: Some(20),
                session: 1,
            },
            t0,
        );
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture, StandbyPosture::Streaming { cursor: 12 });
        assert!(snapshot.ready());
        assert_eq!(snapshot.last_caught_up, Some(t0));
        assert_eq!(snapshot.primary_cursor, 20);
        assert_eq!(snapshot.lag_bytes(), 8);

        // A new session must reach a barrier it reported itself.
        posture.apply_event(ReceiverEvent::Connecting { after: Some(12) }, t0);
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 20,
                barrier: Some(20),
                session: 2,
            },
            t0,
        );
        assert_eq!(posture.snapshot().posture.state(), "reconnecting");
        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 20,
                barrier: Some(25),
                session: 2,
            },
            t0,
        );
        assert_eq!(posture.snapshot().posture, StandbyPosture::Streaming { cursor: 20 });
    }

    #[test]
    fn bootstrap_download_is_recovering() {
        let posture = tracker();
        posture.apply_event(
            ReceiverEvent::Downloading {
                received: 10,
                total: 40,
            },
            Instant::now(),
        );
        assert_eq!(
            posture.snapshot().posture,
            StandbyPosture::Recovering {
                mode: "bootstrap",
                progress: "10/40 bytes".into()
            }
        );
        assert_eq!(posture.snapshot().block().progress.as_deref(), Some("10/40 bytes"));
    }

    /// Spec P2.2: the block gains `tls`, `reason` and `progress` after the
    /// frozen fields. A failed connection's reason shows while the standby
    /// (re)connects and goes once a session delivers; a session's TLS shows.
    #[test]
    fn the_block_reports_tls_and_why_the_standby_is_not_streaming() {
        let posture = tracker();
        let t0 = Instant::now();
        posture.attach_store(3, Arc::new(|| None));
        posture.apply_event(
            ReceiverEvent::CaughtUp {
                cursor: 3,
                barrier: 3,
                session: 1,
            },
            t0,
        );
        posture.note_transport(None, Some(true));
        let block = posture.snapshot().block();
        assert!(
            block.tls && block.reason.is_none() && block.progress.is_none(),
            "{block:?}"
        );

        posture.apply_event(ReceiverEvent::Connecting { after: Some(3) }, t0);
        posture.note_transport(Some("tls: invalid peer certificate: UnknownIssuer".into()), None);
        let snapshot = posture.snapshot();
        assert_eq!(
            snapshot.posture.state(),
            "reconnecting",
            "a TLS failure is not needs_recovery"
        );
        assert_eq!(
            snapshot.block().reason.as_deref(),
            Some("tls: invalid peer certificate: UnknownIssuer")
        );
        assert!(snapshot.ready_at(t0), "within max_standby_lag it still serves");

        posture.apply_event(
            ReceiverEvent::Applying {
                cursor: 3,
                barrier: Some(5),
                session: 2,
            },
            t0,
        );
        assert!(
            posture.snapshot().block().reason.is_none(),
            "a delivering session clears it"
        );

        posture.needs_recovery("physical replication history mismatch");
        assert_eq!(
            posture.snapshot().block().reason.as_deref(),
            Some("physical replication history mismatch")
        );
        let primary = primary_block("primary", 7, Some("127.0.0.1:5433".into()), "async".into(), true);
        assert!(primary.tls && primary.reason.is_none());
    }

    /// Spec P3.2 §3: an archive-only standby is `recovering` (mode
    /// `archive`) and ready while the segment holding its cursor is recent,
    /// `catching_up` and not ready once it is older than `max_standby_lag`; a
    /// gap is `needs_recovery` naming the range and resolves by itself, while
    /// a refusal stays.
    #[test]
    fn an_archive_only_standby_is_ready_while_its_segment_is_recent_and_a_gap_is_not_sticky() {
        let posture = PostureTracker::new("archive:/srv/segments".into(), "async".into(), Duration::from_secs(30));
        posture.attach_store(10, Arc::new(|| None));
        let now = 1_000_000_i64;
        posture.archive_progress(20, 40, Some(now - 5), now);
        let snapshot = posture.snapshot();
        assert_eq!(
            snapshot.posture,
            StandbyPosture::Recovering {
                mode: ARCHIVE_MODE,
                progress: "20 of 40".into()
            }
        );
        assert!(snapshot.ready());
        let block = snapshot.block();
        assert_eq!(block.state, "recovering");
        assert_eq!(block.progress.as_deref(), Some("20 of 40"));
        assert_eq!(block.primary.as_deref(), Some("archive:/srv/segments"));
        assert!(
            block.lag_secs.is_some_and(|lag| (5.0..30.0).contains(&lag)),
            "{block:?}"
        );
        assert_eq!(block.lag_bytes, 20);

        // The cursor's segment is older than max_standby_lag: catching up.
        posture.archive_progress(40, 40, Some(now - 600), now);
        let snapshot = posture.snapshot();
        assert_eq!(
            snapshot.posture,
            StandbyPosture::CatchingUp {
                cursor: 40,
                barrier: 40
            }
        );
        assert!(!snapshot.ready());

        // A gap: needs_recovery naming it, until the segment appears.
        posture.archive_blocked("archive gap 41..50: no segment in /srv/segments holds it");
        let snapshot = posture.snapshot();
        assert_eq!(snapshot.posture.state(), "needs_recovery");
        assert!(snapshot.not_ready_message().contains("archive gap 41..50"));
        posture.archive_progress(60, 60, Some(now), now);
        assert!(posture.snapshot().ready(), "resumed once the segment appeared");

        // A refusal is sticky: archive progress does not leave it.
        posture.needs_recovery("the physical writer is poisoned");
        posture.archive_progress(61, 61, Some(now), now);
        assert_eq!(posture.snapshot().posture.state(), "needs_recovery");
        posture.archive_blocked("archive gap 62..70");
        assert!(posture
            .snapshot()
            .not_ready_message()
            .contains("the physical writer is poisoned"));
    }
}
