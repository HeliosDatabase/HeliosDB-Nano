//! Session table for the MCP HTTP+SSE transport pairing.
//!
//! When a client opens `GET /mcp/sse?session=<id>` (or `?session=`
//! omitted, in which case the server mints a UUID), the SSE handler
//! registers an `mpsc::UnboundedSender<sse::Event>` for that session
//! and announces the id it was ACTUALLY granted via the `endpoint`
//! SSE event.
//!
//! Subsequent `POST /mcp` requests carrying the same session id in
//! the `Mcp-Session-Id` header (and `_meta.progressToken` in the
//! body) get their `notifications/progress` events routed through
//! the matching sender.  The SSE GET stream is the receiver side.
//!
//! Sessions auto-expire 5 minutes after the last activity to keep
//! the table bounded.  The expiry sweep runs piggy-backed on
//! registration, no background task required.
//!
//! # The table is keyed by (DATABASE, session id) — sprinter `f469f178aa29`
//!
//! Through v4.40.0 this was one `DashMap<String, Session>` keyed by the
//! session id ALONE, and that id is CLIENT-SUPPLIED: `handle_sse` takes it
//! straight off `?session=<id>`. A process can hold several MCP mounts —
//! two `mcp_router()`s, or an `McpServer` beside an HTTP route — each with
//! its own `McpState`, its own database and possibly its own authenticator,
//! and they all shared that one namespace. Two consequences, both real:
//!
//! * `register` was a bare `insert`, so a second registration of an id
//!   DROPPED the incumbent `Session` and with it the only sender feeding
//!   that client's SSE body. The reader's `rx.recv()` returned `None`, the
//!   stream ended, and one router's client could silently terminate another
//!   router's client's open connection.
//! * whichever registration survived, BOTH mounts' [`sender_for`] resolved
//!   to it, so `dispatch_streaming_post` forwarded one database's
//!   `notifications/progress` into the other database's client — and those
//!   events are not empty: `helios_graphrag_search` puts the caller's query
//!   text and its hit count in the `message` field.
//!
//! The namespace is `StorageEngine::instance_id` — the same per-engine
//! identity `mcp::result_cache` and the code-graph AST-index registry use,
//! a counter that is never reused (see the "process-global `static`s"
//! design rule in `src/lib.rs`). Keying on the router's database, rather
//! than moving the table onto the PUBLIC `McpState`, keeps the mount API
//! source-compatible; the cost is that two mounts over the SAME database
//! still share one namespace, which the anti-seizure rule below covers.
//!
//! # A live session id is never displaced
//!
//! The id is client-chosen and `GET /mcp/sse` is only `Scope::Read`-gated,
//! so within one namespace any authenticated reader could previously name
//! another client's id and take its stream over. [`register`] now honours a
//! requested id only when nobody LIVE holds it; otherwise the newcomer is
//! minted a fresh one, which the handshake announces in its `endpoint` event
//! exactly as it does for a client that sent no `?session=` at all. Liveness,
//! not mere prior use, is the test — a client reconnecting after its stream
//! died gets its own id back.

use std::time::{Duration, Instant};

use axum::response::sse::Event;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use tokio::sync::mpsc;

const SESSION_TTL: Duration = Duration::from_secs(5 * 60);

/// The MCP mount a session belongs to: `StorageEngine::instance_id` of the
/// database its `McpState` serves.
pub type Namespace = u64;

#[derive(Debug, Clone)]
pub struct Session {
    pub sender: mpsc::UnboundedSender<Event>,
    pub last_seen: Instant,
}

static SESSIONS: Lazy<DashMap<(Namespace, String), Session>> = Lazy::new(DashMap::new);

/// Register a session in `namespace` with a new channel pair, and return the
/// id it was granted alongside the receiver half the SSE handler streams from.
///
/// `requested_id` is the client's `?session=<id>`. It is granted only when no
/// LIVE session in this namespace already holds it; otherwise a fresh UUID is
/// minted, so one client can never seize another's stream. The caller MUST
/// announce the returned id (not the requested one) in the `endpoint` event.
pub fn register(namespace: Namespace, requested_id: Option<String>) -> (String, mpsc::UnboundedReceiver<Event>) {
    sweep_expired();
    let session_id = match requested_id {
        Some(id) if !is_live(namespace, &id) => id,
        _ => uuid::Uuid::new_v4().to_string(),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    SESSIONS.insert(
        (namespace, session_id.clone()),
        Session {
            sender: tx,
            last_seen: Instant::now(),
        },
    );
    (session_id, rx)
}

/// Drop a session.  Called on SSE channel close + on TTL sweep.
pub fn drop_session(namespace: Namespace, session_id: &str) {
    SESSIONS.remove(&(namespace, session_id.to_string()));
}

/// Look up an active session's sender WITHIN `namespace`. Refreshes the
/// last-seen timestamp.
///
/// The namespace is not decoration: without it a POST to one router resolved
/// a session another router's client had opened (see the module docs).
pub fn sender_for(namespace: Namespace, session_id: &str) -> Option<mpsc::UnboundedSender<Event>> {
    let mut entry = SESSIONS.get_mut(&(namespace, session_id.to_string()))?;
    entry.last_seen = Instant::now();
    Some(entry.sender.clone())
}

/// Number of live sessions across every namespace. For process-level
/// metrics; use [`session_count_in`] to ask about one mount.
pub fn session_count() -> usize {
    SESSIONS.len()
}

/// Number of sessions registered against one mount's database.
pub fn session_count_in(namespace: Namespace) -> usize {
    SESSIONS.iter().filter(|e| e.key().0 == namespace).count()
}

/// Is `session_id` held by a session in `namespace` whose reader is still
/// attached? A closed sender means the SSE stream it fed is gone, so the id
/// is free for a reconnecting client.
fn is_live(namespace: Namespace, session_id: &str) -> bool {
    SESSIONS
        .get(&(namespace, session_id.to_string()))
        .is_some_and(|s| !s.sender.is_closed())
}

fn sweep_expired() {
    let now = Instant::now();
    SESSIONS.retain(|_, s| now.duration_since(s.last_seen) < SESSION_TTL && !s.sender.is_closed());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A namespace nobody else in this binary uses. The table is shared by
    /// the whole test binary and tests run concurrently, so each test needs
    /// its own. Counting up from `u64::MAX / 2` keeps these clear of any real
    /// `StorageEngine::instance_id`, which counts up from 1.
    fn fresh_namespace() -> Namespace {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(u64::MAX / 2);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn register_and_lookup() {
        let ns = fresh_namespace();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let (granted, _rx) = register(ns, Some(id.clone()));
        assert_eq!(granted, id);
        assert!(sender_for(ns, &id).is_some());
        drop_session(ns, &id);
        assert!(sender_for(ns, &id).is_none());
    }

    #[test]
    fn a_session_is_not_visible_from_another_namespace() {
        let (a, b) = (fresh_namespace(), fresh_namespace());
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let (_granted, _rx) = register(a, Some(id.clone()));
        assert!(sender_for(a, &id).is_some(), "the owning mount resolves it");
        assert!(sender_for(b, &id).is_none(), "another mount must not");
    }

    #[test]
    fn a_live_id_is_not_handed_to_a_second_caller() {
        let ns = fresh_namespace();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let (_first, _rx) = register(ns, Some(id.clone()));
        let (second, _rx2) = register(ns, Some(id.clone()));
        assert_ne!(second, id, "the incumbent keeps the id; the newcomer is minted one");
        assert_eq!(session_count_in(ns), 2, "both sessions exist");
    }

    #[tokio::test]
    async fn closed_receiver_is_swept() {
        let ns = fresh_namespace();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        {
            let (_granted, _rx) = register(ns, Some(id.clone()));
            assert!(sender_for(ns, &id).is_some());
        }
        // Receiver dropped; sender still in table but is_closed().
        // Force a sweep by registering another session.
        let _other = register(ns, Some(format!("test-other-{}", uuid::Uuid::new_v4())));
        // The original session's sender should now be cleared.
        assert!(sender_for(ns, &id).is_none());
    }
}
