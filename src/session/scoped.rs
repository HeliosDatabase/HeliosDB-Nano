//! Per-connection state the storage-less expression evaluator must be able to
//! READ AND WRITE in the middle of a statement — sprinter 6dc0cc115db9
//! (`LASTVAL()`) and sprinter f4f5d450e816 (`pg_backend_pid()` /
//! `application_name`).
//!
//! # Why this is not a field on [`Session`](super::Session)
//!
//! Every other piece of session state is *read* by the engine before the
//! statement runs (`Session::search_path`, `Session::login_identity`, the GH#28
//! timeout overrides) and reaches SQL through a per-statement thread-local the
//! engine installs. That shape does not work for these three:
//!
//! * `lastval` is **written** from inside statement execution — by the
//!   `nextval()` evaluator arm AND by the SERIAL/IDENTITY fill deep in the
//!   insert funnels — and the write must land on the *session*, not on a
//!   snapshot the guard took.
//! * `application_name` is written by `SET` on one statement and read by
//!   `SHOW` / `current_setting()` on the next, from an evaluator that holds no
//!   session handle.
//!
//! So the session owns an `Arc<SessionScopedState>` and the per-statement guard
//! installs a *clone of the handle* (one atomic increment) rather than a copy
//! of the values. Writes through it reach the session directly.
//!
//! # Why it is not a process global
//!
//! This is the single most important design constraint on sprinter
//! 6dc0cc115db9. A process-global "last nextval" would answer connection B with
//! connection A's row id — a `cursor.lastrowid` that silently names someone
//! else's row is strictly worse than the `None` it replaces. The same reasoning
//! rules out putting `application_name` in the process-global
//! `sql::settings::SessionSettings` registry, exactly as GH#28 kept the
//! connection-lifetime timeouts off it.
//!
//! The one deliberately process-wide structure here is [`live_backends`] — the
//! registry `pg_stat_activity` scans. Listing every live connection IS that
//! view's purpose, and each entry is a `Weak` handle on the very state the
//! session holds, so the catalog can never disagree with what the connection
//! itself reports — and a closed connection drops out of it automatically.

use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Weak};

use dashmap::DashMap;

/// Sentinel stored in [`SessionScopedState::lastval`] for "no `nextval` has run
/// in this session". `i64::MIN` is not a value any sequence can serve (a
/// sequence's `min_value` is clamped well above it), so it cannot collide with
/// a legitimate id — and, unlike `0`, it can never be mistaken for one.
const LASTVAL_UNDEFINED: i64 = i64::MIN;

/// PostgreSQL's own message for `lastval()` before any `nextval()`
/// (`commands/sequence.c`), reported under SQLSTATE 55000
/// `object_not_in_prerequisite_state`.
///
/// Owned here, by the single emitter, so the wire layer's SQLSTATE mapping
/// (`protocol::postgres::handler::sqlstate_for_query_execution_message`) and
/// the message cannot drift — the same marker-const discipline GH#28 used for
/// `SET_LOCAL_TIMEOUT_GUC_UNSUPPORTED`.
pub const LASTVAL_UNDEFINED_MESSAGE: &str = "lastval is not yet defined in this session";

/// Allocate the next backend pid. Starts at 1 and never repeats within the
/// process, which is all `pg_backend_pid()` promises: unique among LIVE
/// sessions and stable within one. It is deliberately NOT the OS pid — every
/// connection in this process would then report the same number, which is the
/// one thing a pool's affinity check must not see.
fn next_backend_pid() -> i32 {
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    // Wrap back to 1 rather than overflowing (a 2-billion-connection process
    // would have recycled every earlier pid long ago).
    let pid = COUNTER.fetch_add(1, Ordering::Relaxed);
    if pid <= 0 {
        COUNTER.store(1, Ordering::Relaxed);
        return 1;
    }
    pid
}

/// The process-wide registry of LIVE backends, keyed by backend pid.
///
/// Populated by [`SessionScopedState::new`] and emptied by its `Drop`, so a
/// destroyed session leaves the view the moment its `Session` is dropped from
/// the `SessionManager` map. Entries are **`Weak`** on purpose: a strong `Arc`
/// here would be a reference the session's own `Drop` waits on, so the refcount
/// would never reach zero, `Drop` would never run, and every backend that ever
/// connected would stay in `pg_stat_activity` forever. The `Weak` also makes
/// the registry self-healing — an upgrade that fails is a backend that is gone.
///
/// A live entry still shares the SAME state the session holds, so
/// `pg_stat_activity.application_name` is the live value, never a stale copy.
pub fn live_backends() -> &'static DashMap<i32, Weak<SessionScopedState>> {
    static BACKENDS: std::sync::OnceLock<DashMap<i32, Weak<SessionScopedState>>> = std::sync::OnceLock::new();
    BACKENDS.get_or_init(DashMap::new)
}

/// A snapshot of one live backend, in `pg_stat_activity` column order.
#[derive(Debug, Clone)]
pub struct BackendSnapshot {
    pub pid: i32,
    pub username: String,
    pub application_name: String,
    pub client_addr: Option<String>,
    pub client_port: Option<i32>,
    pub backend_start: i64,
}

/// Snapshot every live backend, ordered by pid so the view is deterministic.
pub fn snapshot_live_backends() -> Vec<BackendSnapshot> {
    let mut out: Vec<BackendSnapshot> = live_backends()
        .iter()
        .filter_map(|e| e.value().upgrade().map(|s| s.snapshot()))
        .collect();
    out.sort_by_key(|b| b.pid);
    out
}

/// Session-scoped state shared between the session and the statement currently
/// running on its behalf. See the module docs for why it is an `Arc` and not a
/// set of plain `Session` fields.
#[derive(Debug)]
pub struct SessionScopedState {
    /// `pg_backend_pid()`. Immutable for the life of the connection.
    backend_pid: i32,
    /// Unix seconds at which this backend was created (`pg_stat_activity.backend_start`).
    backend_start: i64,
    /// The value most recently RETURNED by `nextval` in this session — whether
    /// the caller spelled it `nextval('s')` or let a SERIAL/IDENTITY column
    /// generate it. [`LASTVAL_UNDEFINED`] until the first one.
    ///
    /// Relaxed ordering throughout: a session is single-threaded with respect to
    /// its own statements (the wire handler runs one at a time), so there is no
    /// happens-before to establish; the atomic exists only so the evaluator can
    /// write through a shared `&`.
    lastval: AtomicI64,
    /// The session's `application_name` GUC. Empty string = PostgreSQL's
    /// default, which is a VALUE, not "unset" — `SHOW application_name` on a
    /// stock server prints an empty line, it does not error.
    application_name: parking_lot::RwLock<String>,
    /// `SET LOCAL application_name` support: the value to restore when the
    /// current transaction block ends, armed on the first `SET LOCAL` of the
    /// block and disarmed by [`Self::end_transaction`]. `None` = no `SET LOCAL`
    /// is in effect.
    application_name_saved: parking_lot::Mutex<Option<String>>,
    /// The login identity (`pg_stat_activity.usename`), republished by
    /// `Session::set_login` after authentication.
    username: parking_lot::RwLock<String>,
    /// Peer address, when the listener knows one (`None` on the embedded
    /// funnels and over an in-memory duplex stream).
    client_addr: parking_lot::RwLock<Option<String>>,
    client_port: AtomicI32,
}

impl SessionScopedState {
    /// Mint a fresh backend and register it in [`live_backends`].
    pub fn new() -> Arc<Self> {
        let state = Arc::new(Self {
            backend_pid: next_backend_pid(),
            backend_start: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            lastval: AtomicI64::new(LASTVAL_UNDEFINED),
            application_name: parking_lot::RwLock::new(String::new()),
            application_name_saved: parking_lot::Mutex::new(None),
            username: parking_lot::RwLock::new(String::new()),
            client_addr: parking_lot::RwLock::new(None),
            client_port: AtomicI32::new(0),
        });
        live_backends().insert(state.backend_pid, Arc::downgrade(&state));
        state
    }

    /// `pg_backend_pid()`.
    pub fn backend_pid(&self) -> i32 {
        self.backend_pid
    }

    /// Record the value `nextval` (or a SERIAL/IDENTITY fill) just produced.
    ///
    /// Deliberately unconditional: PostgreSQL's `lastval()` follows whichever
    /// sequence was advanced LAST, which is exactly what makes it usable by a
    /// driver that does not know the sequence name.
    pub fn note_lastval(&self, value: i64) {
        self.lastval.store(value, Ordering::Relaxed);
    }

    /// `lastval()` — `None` when no `nextval` has run in this session.
    ///
    /// Note the deliberate asymmetry with `currval`, which returns `0` for an
    /// unknown sequence (a documented Nano divergence, see
    /// `crate::sql::sequences`): a `0` here would be indistinguishable from a
    /// real row id to `cursor.lastrowid`, so this one fails closed.
    pub fn lastval(&self) -> Option<i64> {
        match self.lastval.load(Ordering::Relaxed) {
            LASTVAL_UNDEFINED => None,
            v => Some(v),
        }
    }

    /// The session's `application_name`.
    pub fn application_name(&self) -> String {
        self.application_name.read().clone()
    }

    /// `SET application_name = '…'` (session scope) — also the startup-packet
    /// path, which is where every driver actually sends it.
    ///
    /// Truncated to PostgreSQL's `NAMEDATALEN - 1` for the same reason
    /// `Session::set_login` truncates the login name: on a trust listener the
    /// value is chosen by the client, and it is copied into every
    /// `pg_stat_activity` scan.
    pub fn set_application_name(&self, value: &str) {
        let mut slot = self.application_name.write();
        slot.clear();
        slot.push_str(truncate_on_char_boundary(value, APPLICATION_NAME_MAX_BYTES));
    }

    /// `SET LOCAL application_name = '…'` inside an open transaction block.
    ///
    /// Arms the restore slot on the FIRST `SET LOCAL` of the block only, so two
    /// `SET LOCAL`s in one transaction both revert to the value the block
    /// started with (PostgreSQL semantics), not to each other.
    pub fn set_local_application_name(&self, value: &str) {
        let mut saved = self.application_name_saved.lock();
        if saved.is_none() {
            *saved = Some(self.application_name.read().clone());
        }
        drop(saved);
        self.set_application_name(value);
    }

    /// End of a transaction block — COMMIT *and* ROLLBACK alike. PostgreSQL
    /// reverts a `SET LOCAL` in both cases, so this is called from both.
    ///
    /// A no-op (one uncontended mutex probe) when no `SET LOCAL` is armed,
    /// which is every transaction in practice.
    pub fn end_transaction(&self) {
        let restore = self.application_name_saved.lock().take();
        if let Some(previous) = restore {
            let mut slot = self.application_name.write();
            *slot = previous;
        }
    }

    /// Republish the login identity (`pg_stat_activity.usename`). Called by
    /// `Session::set_login`, the single writer of the session's identity, so
    /// the view and `current_user` can never disagree.
    pub fn set_username(&self, name: &str) {
        let mut slot = self.username.write();
        slot.clear();
        slot.push_str(name);
    }

    /// Stamp the peer address a listener accepted the connection from.
    pub fn set_client_address(&self, addr: Option<&str>, port: i32) {
        *self.client_addr.write() = addr.map(str::to_string);
        self.client_port.store(port, Ordering::Relaxed);
    }

    /// One row of `pg_stat_activity`.
    pub fn snapshot(&self) -> BackendSnapshot {
        let port = self.client_port.load(Ordering::Relaxed);
        BackendSnapshot {
            pid: self.backend_pid,
            username: self.username.read().clone(),
            application_name: self.application_name.read().clone(),
            client_addr: self.client_addr.read().clone(),
            client_port: (port != 0).then_some(port),
            backend_start: self.backend_start,
        }
    }
}

impl Drop for SessionScopedState {
    fn drop(&mut self) {
        // The registry holds only a `Weak`, so this runs as soon as the
        // `Session` (and every clone of it) is gone. Removing here is what keeps
        // `pg_stat_activity` free of pids a client would try to join against
        // forever.
        live_backends().remove(&self.backend_pid);
    }
}

/// PostgreSQL's `NAMEDATALEN - 1`: where a stock server truncates
/// `application_name`.
const APPLICATION_NAME_MAX_BYTES: usize = 63;

/// Truncate to at most `max` bytes without splitting a UTF-8 sequence.
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // `get` rather than `s[..end]`: the loop has already proved `end` is a char
    // boundary, and the fallback keeps this panic-free by construction rather
    // than by argument.
    s.get(..end).unwrap_or(s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lastval_is_undefined_until_a_nextval_lands() {
        let s = SessionScopedState::new();
        assert!(s.lastval().is_none());
        s.note_lastval(7);
        assert_eq!(s.lastval(), Some(7));
        // It follows the LAST value, whichever sequence produced it.
        s.note_lastval(500);
        assert_eq!(s.lastval(), Some(500));
    }

    #[test]
    fn two_backends_never_share_a_pid_or_a_lastval() {
        let a = SessionScopedState::new();
        let b = SessionScopedState::new();
        assert_ne!(a.backend_pid(), b.backend_pid());
        a.note_lastval(1);
        assert!(b.lastval().is_none(), "backend B saw backend A's lastval");
    }

    #[test]
    fn set_local_reverts_to_the_value_the_block_started_with() {
        let s = SessionScopedState::new();
        s.set_application_name("outer");
        s.set_local_application_name("inner");
        // A second SET LOCAL in the same block still reverts to `outer`.
        s.set_local_application_name("inner2");
        assert_eq!(s.application_name(), "inner2");
        s.end_transaction();
        assert_eq!(s.application_name(), "outer");
        // Disarmed: a later transaction end must not re-restore.
        s.set_application_name("later");
        s.end_transaction();
        assert_eq!(s.application_name(), "later");
    }

    #[test]
    fn a_dropped_backend_leaves_the_live_registry() {
        let pid = {
            let s = SessionScopedState::new();
            let pid = s.backend_pid();
            assert!(live_backends().get(&pid).is_some_and(|w| w.upgrade().is_some()));
            pid
        };
        assert!(!live_backends().contains_key(&pid), "a dropped backend stayed listed");
    }

    #[test]
    fn application_name_is_truncated_on_a_char_boundary() {
        let s = SessionScopedState::new();
        let long = "é".repeat(100); // 200 bytes
        s.set_application_name(&long);
        let got = s.application_name();
        assert!(got.len() <= APPLICATION_NAME_MAX_BYTES);
        assert!(got.chars().all(|c| c == 'é'), "truncation split a UTF-8 sequence");
    }
}
