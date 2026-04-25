//! Session table for the MCP HTTP+SSE transport pairing.
//!
//! When a client opens `GET /mcp/sse?session=<id>` (or `?session=`
//! omitted, in which case the server mints a UUID and announces
//! it via the `endpoint` SSE event), the SSE handler registers an
//! `mpsc::UnboundedSender<sse::Event>` against the session id in
//! this process-static table.
//!
//! Subsequent `POST /mcp` requests carrying the same session id in
//! the `Mcp-Session-Id` header (and `_meta.progressToken` in the
//! body) get their `notifications/progress` events routed through
//! the matching sender.  The SSE GET stream is the receiver side.
//!
//! Sessions auto-expire 5 minutes after the last activity to keep
//! the table bounded.  The expiry sweep runs piggy-backed on
//! registration, no background task required.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::sse::Event;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use tokio::sync::mpsc;

const SESSION_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct Session {
    pub sender: mpsc::UnboundedSender<Event>,
    pub last_seen: Instant,
}

static SESSIONS: Lazy<Arc<DashMap<String, Session>>> = Lazy::new(|| Arc::new(DashMap::new()));

/// Register a fresh session with a new channel pair. Returns the
/// receiver half so the SSE handler can stream from it.
pub fn register(session_id: String) -> mpsc::UnboundedReceiver<Event> {
    sweep_expired();
    let (tx, rx) = mpsc::unbounded_channel();
    SESSIONS.insert(
        session_id,
        Session { sender: tx, last_seen: Instant::now() },
    );
    rx
}

/// Drop a session.  Called on SSE channel close + on TTL sweep.
pub fn drop_session(session_id: &str) {
    SESSIONS.remove(session_id);
}

/// Look up an active session's sender. Refreshes the
/// last-seen timestamp.
pub fn sender_for(session_id: &str) -> Option<mpsc::UnboundedSender<Event>> {
    let mut entry = SESSIONS.get_mut(session_id)?;
    entry.last_seen = Instant::now();
    Some(entry.sender.clone())
}

/// Number of live sessions. For tests / metrics.
pub fn session_count() -> usize {
    SESSIONS.len()
}

fn sweep_expired() {
    let now = Instant::now();
    SESSIONS.retain(|_, s| {
        now.duration_since(s.last_seen) < SESSION_TTL
            && !s.sender.is_closed()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let _rx = register(id.clone());
        assert!(sender_for(&id).is_some());
        drop_session(&id);
        assert!(sender_for(&id).is_none());
    }

    #[tokio::test]
    async fn closed_receiver_is_swept() {
        let id = format!("test-{}", uuid::Uuid::new_v4());
        {
            let _rx = register(id.clone());
            assert!(sender_for(&id).is_some());
        }
        // Receiver dropped; sender still in table but is_closed().
        // Force a sweep by registering another session.
        let _other = register(format!("test-other-{}", uuid::Uuid::new_v4()));
        // The original session's sender should now be cleared.
        assert!(sender_for(&id).is_none());
    }
}
