//! Realtime DB Change Notifications
//!
//! Tracks which tables have active realtime subscriptions and manages
//! the notification pipeline from DML operations (INSERT/UPDATE/DELETE)
//! to WebSocket subscribers via a `tokio::sync::broadcast` channel.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::EmbeddedDatabase;

// ── ChangeEvent ──────────────────────────────────────────────────────────────

/// A single DML change event that is broadcast to WebSocket subscribers.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ChangeEvent {
    /// The table affected by the DML statement.
    pub table: String,
    /// The type of DML operation: `"INSERT"`, `"UPDATE"`, or `"DELETE"`.
    pub event_type: String,
    /// The row *after* the operation (present for INSERT and UPDATE).
    pub new_record: Option<serde_json::Value>,
    /// The row *before* the operation (present for UPDATE and DELETE).
    pub old_record: Option<serde_json::Value>,
    /// ISO-8601 timestamp of the change.
    pub timestamp: String,
}

// ── ChangeNotifier ───────────────────────────────────────────────────────────

/// Manages table-level subscriptions and fans out `ChangeEvent`s to all
/// active WebSocket receivers via a broadcast channel.
pub struct ChangeNotifier {
    /// Reference to the database (kept for possible future enrichment).
    #[allow(dead_code)]
    db: Arc<EmbeddedDatabase>,

    /// `table_name -> subscriber_count`.
    ///
    /// Only tables with at least one subscriber will have events sent.
    subscriptions: RwLock<HashMap<String, usize>>,

    /// Broadcast sender shared by all WebSocket connections.
    sender: tokio::sync::broadcast::Sender<ChangeEvent>,
}

impl ChangeNotifier {
    /// Create a new `ChangeNotifier` with a broadcast buffer of 1 024 events.
    pub fn new(db: Arc<EmbeddedDatabase>) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(1024);
        Self {
            db,
            subscriptions: RwLock::new(HashMap::new()),
            sender,
        }
    }

    /// Obtain a new broadcast receiver.
    ///
    /// Each WebSocket connection should call this once to get its own
    /// receiver handle.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ChangeEvent> {
        self.sender.subscribe()
    }

    // ── DML hook ─────────────────────────────────────────────────────────

    /// Called by the REST handlers after a successful DML operation.
    ///
    /// If the table has at least one active subscription, the event is
    /// pushed into the broadcast channel.  If no subscribers are listening,
    /// the send is silently dropped.
    pub fn notify(
        &self,
        table: &str,
        event_type: &str,
        new_record: Option<serde_json::Value>,
        old_record: Option<serde_json::Value>,
    ) {
        let subs = self.subscriptions.read();
        if subs.contains_key(table) || subs.contains_key("*") {
            let event = ChangeEvent {
                table: table.to_string(),
                event_type: event_type.to_string(),
                new_record,
                old_record,
                timestamp: chrono::Utc::now().to_rfc3339(),
            };
            // `send` returns Err only when there are zero receivers, which is fine.
            let _ = self.sender.send(event);
        }
    }

    // ── Subscription bookkeeping ─────────────────────────────────────────

    /// Increment the subscriber count for `table`.
    pub fn add_table_subscription(&self, table: &str) {
        let mut subs = self.subscriptions.write();
        *subs.entry(table.to_string()).or_insert(0) += 1;
    }

    /// Decrement the subscriber count for `table`.
    ///
    /// When the count reaches zero the entry is removed so that
    /// [`notify()`](Self::notify) can skip serialisation entirely.
    pub fn remove_table_subscription(&self, table: &str) {
        let mut subs = self.subscriptions.write();
        if let Some(count) = subs.get_mut(table) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                subs.remove(table);
            }
        }
    }

    /// Return the current subscriber count for a table (mainly for tests).
    #[cfg(test)]
    pub fn subscriber_count(&self, table: &str) -> usize {
        let subs = self.subscriptions.read();
        subs.get(table).copied().unwrap_or(0)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_notifier() -> ChangeNotifier {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        ChangeNotifier::new(db)
    }

    #[test]
    fn test_subscribe_and_unsubscribe() {
        let n = make_notifier();
        assert_eq!(n.subscriber_count("users"), 0);

        n.add_table_subscription("users");
        assert_eq!(n.subscriber_count("users"), 1);

        n.add_table_subscription("users");
        assert_eq!(n.subscriber_count("users"), 2);

        n.remove_table_subscription("users");
        assert_eq!(n.subscriber_count("users"), 1);

        n.remove_table_subscription("users");
        assert_eq!(n.subscriber_count("users"), 0);
    }

    #[test]
    fn test_remove_below_zero_is_safe() {
        let n = make_notifier();
        // Should not panic on an unknown table.
        n.remove_table_subscription("unknown");
        assert_eq!(n.subscriber_count("unknown"), 0);
    }

    #[tokio::test]
    async fn test_notify_sends_to_subscriber() {
        let n = make_notifier();
        n.add_table_subscription("orders");

        let mut rx = n.subscribe();

        n.notify(
            "orders",
            "INSERT",
            Some(serde_json::json!({"id": 1})),
            None,
        );

        let event = rx.recv().await.unwrap();
        assert_eq!(event.table, "orders");
        assert_eq!(event.event_type, "INSERT");
        assert!(event.new_record.is_some());
        assert!(event.old_record.is_none());
    }

    #[tokio::test]
    async fn test_notify_skips_unsubscribed_table() {
        let n = make_notifier();
        // "orders" has subscribers but "other" does not.
        n.add_table_subscription("orders");

        let mut rx = n.subscribe();

        // This should be silently dropped since "other" has no subscribers.
        n.notify("other", "INSERT", Some(serde_json::json!({"id": 1})), None);

        // Nothing in channel — try_recv should fail.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_wildcard_subscription() {
        let n = make_notifier();
        n.add_table_subscription("*");

        let mut rx = n.subscribe();

        n.notify("any_table", "DELETE", None, Some(serde_json::json!({"id": 99})));

        let event = rx.recv().await.unwrap();
        assert_eq!(event.table, "any_table");
        assert_eq!(event.event_type, "DELETE");
    }
}
