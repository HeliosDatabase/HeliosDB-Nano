//! GH#40: PostgreSQL's asynchronous notifications — `LISTEN`, `UNLISTEN`,
//! `NOTIFY` and `pg_notify()`.
//!
//! One process-wide registry ([`hub`]) keyed by the backend pid of the
//! sending/listening session (`SessionScopedState::backend_pid`), filtered by
//! the database it belongs to (`SessionScopedState::engine_instance`), so two
//! databases opened in one process never hear each other.
//!
//! * **Listening is PostgreSQL-wire only.** A wire session is registered here
//!   when it is created (`EmbeddedDatabase::create_wire_session`) and released
//!   when it is destroyed, so its listens end when the client disconnects.
//!   `LISTEN` from any other session (embedded API, REPL, MySQL wire) has no
//!   connection to deliver to and is refused with `0A000`.
//! * **Sending works everywhere.** `NOTIFY` / `pg_notify()` from any session of
//!   a database reach that database's wire listeners.
//! * **Transactional.** Inside a transaction block the notifications are queued
//!   (identical channel + payload pairs folded, as PostgreSQL does), delivered
//!   when it COMMITs and dropped when it rolls back. `ROLLBACK TO SAVEPOINT`
//!   drops the ones queued after the savepoint.
//! * **Node-local.** Like PostgreSQL's, notifications are not replicated to HA
//!   standbys.
//!
//! The wire handler drains a listener's inbox before every ReadyForQuery it
//! sends outside a transaction block and whenever it sits idle.

use crate::{Error, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// PostgreSQL's `NOTIFY_PAYLOAD_MAX_LENGTH`: a payload must be SHORTER than this
/// many bytes.
pub const NOTIFY_PAYLOAD_MAX_LENGTH: usize = 8000;

/// PostgreSQL's `NAMEDATALEN`: a channel name must be shorter than this.
const NAMEDATALEN: usize = 64;

/// PostgreSQL's message for an over-long payload (SQLSTATE 22023).
pub const PAYLOAD_TOO_LONG: &str = "payload string too long";
/// PostgreSQL's message for an empty (or NULL) channel (SQLSTATE 22023).
pub const CHANNEL_EMPTY: &str = "channel name cannot be empty";
/// PostgreSQL's message for an over-long channel name (SQLSTATE 22023).
pub const CHANNEL_TOO_LONG: &str = "channel name too long";

/// Is `message` one of the three refusals above? The wire maps them to
/// `22023 invalid_parameter_value`.
pub fn is_invalid_parameter_message(message: &str) -> bool {
    message.contains(PAYLOAD_TOO_LONG) || message.contains(CHANNEL_EMPTY) || message.contains(CHANNEL_TOO_LONG)
}

/// One notification as a listener receives it (`NotificationResponse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Backend pid of the session that sent it.
    pub sender_pid: i32,
    /// Channel name.
    pub channel: String,
    /// Payload (empty when none was given).
    pub payload: String,
}

/// The statement family the planner hands the executor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NotifyCommand {
    /// `LISTEN channel`
    Listen(String),
    /// `UNLISTEN channel`, or `UNLISTEN *` (`None`)
    Unlisten(Option<String>),
    /// `NOTIFY channel [, 'payload']`
    Notify {
        /// Channel name.
        channel: String,
        /// Payload, if one was given.
        payload: Option<String>,
    },
}

/// A wire session's mailbox. The handler keeps an `Arc` of it, so draining
/// before each ReadyForQuery is one atomic load when nothing arrived.
#[derive(Default)]
pub struct Mailbox {
    has_mail: AtomicBool,
    inbox: Mutex<VecDeque<Notification>>,
    wake: tokio::sync::Notify,
}

impl Mailbox {
    /// Take every notification waiting here.
    pub fn take(&self) -> Vec<Notification> {
        if !self.has_mail.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut inbox = self.inbox.lock();
        self.has_mail.store(false, Ordering::Release);
        inbox.drain(..).collect()
    }

    /// Resolves when a notification arrives (a permit is stored if one
    /// arrived while nobody waited).
    pub async fn wait(&self) {
        self.wake.notified().await;
    }

    fn post(&self, notification: Notification) {
        self.inbox.lock().push_back(notification);
        self.has_mail.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

struct Listener {
    engine: u64,
    channels: BTreeSet<String>,
    mailbox: Arc<Mailbox>,
}

#[derive(Default)]
struct Pending {
    /// (engine, channel, payload) in send order, duplicates folded.
    queue: Vec<(u64, String, String)>,
    seen: HashSet<(u64, String, String)>,
}

/// The process-wide registry. See the module documentation.
#[derive(Default)]
pub struct NotifyHub {
    listeners: Mutex<HashMap<i32, Listener>>,
    /// One entry per open transaction block, keyed by backend pid. Sharded:
    /// every BEGIN and COMMIT touches it.
    open_txns: DashMap<i32, Pending>,
}

/// The one registry of this process.
pub fn hub() -> &'static NotifyHub {
    static HUB: OnceLock<NotifyHub> = OnceLock::new();
    HUB.get_or_init(NotifyHub::default)
}

fn validate(channel: &str, payload: &str) -> Result<()> {
    if channel.is_empty() {
        return Err(Error::query_execution(CHANNEL_EMPTY));
    }
    if channel.len() >= NAMEDATALEN {
        return Err(Error::query_execution(CHANNEL_TOO_LONG));
    }
    if payload.len() >= NOTIFY_PAYLOAD_MAX_LENGTH {
        return Err(Error::query_execution(PAYLOAD_TOO_LONG));
    }
    Ok(())
}

impl NotifyHub {
    fn deliver(&self, engine: u64, sender_pid: i32, channel: &str, payload: &str) {
        for listener in self.listeners.lock().values() {
            if listener.engine == engine && listener.channels.contains(channel) {
                listener.mailbox.post(Notification {
                    sender_pid,
                    channel: channel.to_string(),
                    payload: payload.to_string(),
                });
            }
        }
    }

    /// Register a PostgreSQL wire session as able to LISTEN.
    pub fn attach(&self, pid: i32, engine: u64) {
        self.listeners.lock().insert(
            pid,
            Listener {
                engine,
                channels: BTreeSet::new(),
                mailbox: Arc::new(Mailbox::default()),
            },
        );
    }

    /// The session is gone: its listens end and anything it had queued is dropped.
    pub fn release(&self, pid: i32) {
        self.listeners.lock().remove(&pid);
        self.open_txns.remove(&pid);
    }

    /// The mailbox of a wire session; `None` for any other session.
    pub fn mailbox(&self, pid: i32) -> Option<Arc<Mailbox>> {
        self.listeners.lock().get(&pid).map(|l| Arc::clone(&l.mailbox))
    }

    /// The channels `pid` listens on, sorted.
    pub fn listening_channels(&self, pid: i32) -> Vec<String> {
        self.listeners
            .lock()
            .get(&pid)
            .map(|l| l.channels.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// `LISTEN channel` for `pid`.
    pub fn listen(&self, pid: i32, channel: &str) -> Result<()> {
        validate(channel, "")?;
        match self.listeners.lock().get_mut(&pid) {
            Some(listener) => {
                listener.channels.insert(channel.to_string());
                Ok(())
            }
            None => Err(Error::query_execution(format!(
                "LISTEN \"{channel}\" {} outside the PostgreSQL wire protocol: notifications are \
                 delivered to PostgreSQL wire connections only (NOTIFY and pg_notify() work from here)",
                crate::error::UNSUPPORTED_STATEMENT_KIND_MARKER
            ))),
        }
    }

    /// `UNLISTEN channel` / `UNLISTEN *` (`None`). A no-op for a session that
    /// does not listen, as in PostgreSQL.
    pub fn unlisten(&self, pid: i32, channel: Option<&str>) {
        if let Some(listener) = self.listeners.lock().get_mut(&pid) {
            match channel {
                Some(channel) => {
                    listener.channels.remove(channel);
                }
                None => listener.channels.clear(),
            }
        }
    }

    /// A transaction block opened for `pid`: its notifications wait for COMMIT.
    pub fn txn_begin(&self, pid: i32) {
        self.open_txns.insert(pid, Pending::default());
    }

    /// The transaction block of `pid` ended: deliver its queue on `commit`,
    /// drop it otherwise.
    pub fn txn_end(&self, pid: i32, commit: bool) {
        if let Some((_, pending)) = self.open_txns.remove(&pid) {
            if commit {
                for (engine, channel, payload) in pending.queue {
                    self.deliver(engine, pid, &channel, &payload);
                }
            }
        }
    }

    /// `NOTIFY` / `pg_notify()` from `pid` of database `engine`.
    pub fn notify(&self, pid: i32, engine: u64, channel: &str, payload: &str) -> Result<()> {
        validate(channel, payload)?;
        if let Some(mut pending) = self.open_txns.get_mut(&pid) {
            let key = (engine, channel.to_string(), payload.to_string());
            if pending.seen.insert(key.clone()) {
                pending.queue.push(key);
            }
            return Ok(());
        }
        self.deliver(engine, pid, channel, payload);
        Ok(())
    }

    /// How many notifications `pid`'s open block has queued (savepoint mark).
    pub fn pending_len(&self, pid: i32) -> usize {
        self.open_txns.get(&pid).map_or(0, |p| p.queue.len())
    }

    /// `ROLLBACK TO SAVEPOINT`: drop what was queued after the mark.
    pub fn truncate_pending(&self, pid: i32, len: usize) {
        if let Some(mut pending) = self.open_txns.get_mut(&pid) {
            pending.queue.truncate(len);
            let seen = pending.queue.iter().cloned().collect();
            pending.seen = seen;
        }
    }
}

/// The calling statement's (pid, engine) — every session and embedded funnel
/// installs its backend state before a statement runs.
fn current_backend() -> Result<(i32, u64)> {
    crate::session_scoped_state_tls()
        .map(|state| (state.backend_pid(), state.engine_instance()))
        .ok_or_else(|| Error::internal("NOTIFY/LISTEN ran outside every session"))
}

/// Run one planned `LISTEN` / `UNLISTEN` / `NOTIFY` for the calling session.
pub(crate) fn run_command(command: &NotifyCommand) -> Result<()> {
    let (pid, engine) = current_backend()?;
    match command {
        NotifyCommand::Listen(channel) => hub().listen(pid, channel),
        NotifyCommand::Unlisten(channel) => {
            hub().unlisten(pid, channel.as_deref());
            Ok(())
        }
        NotifyCommand::Notify { channel, payload } => {
            hub().notify(pid, engine, channel, payload.as_deref().unwrap_or(""))
        }
    }
}

/// `pg_notify(channel, payload)` for the calling session.
pub(crate) fn pg_notify(channel: &str, payload: &str) -> Result<()> {
    let (pid, engine) = current_backend()?;
    hub().notify(pid, engine, channel, payload)
}

/// The calling statement's backend pid, if any (savepoint marks).
pub(crate) fn current_pid() -> Option<i32> {
    crate::session_scoped_state_tls().map(|state| state.backend_pid())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn folds_duplicates_and_honours_rollback() {
        let hub = NotifyHub::default();
        hub.attach(-901, 77);
        hub.listen(-901, "c").unwrap();
        hub.txn_begin(-902);
        hub.notify(-902, 77, "c", "x").unwrap();
        hub.notify(-902, 77, "c", "x").unwrap();
        hub.notify(-902, 77, "c", "y").unwrap();
        let mailbox = hub.mailbox(-901).unwrap();
        assert_eq!(mailbox.take(), Vec::new());
        hub.txn_end(-902, true);
        let got: Vec<_> = mailbox.take().into_iter().map(|n| n.payload).collect();
        assert_eq!(got, vec!["x".to_string(), "y".to_string()]);
        hub.txn_begin(-902);
        hub.notify(-902, 77, "c", "z").unwrap();
        hub.txn_end(-902, false);
        assert_eq!(mailbox.take(), Vec::new());
        // Another database does not hear it.
        hub.notify(-902, 78, "c", "w").unwrap();
        assert_eq!(mailbox.take(), Vec::new());
    }

    #[test]
    fn limits_follow_postgresql() {
        let hub = NotifyHub::default();
        assert!(hub.notify(1, 1, "c", &"a".repeat(7999)).is_ok());
        let err = hub.notify(1, 1, "c", &"a".repeat(8000)).unwrap_err().to_string();
        assert!(err.contains(PAYLOAD_TOO_LONG), "{err}");
        assert!(hub.notify(1, 1, "", "").is_err());
        assert!(hub.notify(1, 1, &"c".repeat(64), "").is_err());
        assert!(hub
            .listen(12345, "c")
            .unwrap_err()
            .to_string()
            .contains("is not supported"));
    }
}
