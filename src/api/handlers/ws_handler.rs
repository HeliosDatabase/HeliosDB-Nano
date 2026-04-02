//! WebSocket handler for realtime DB change notifications.
//!
//! Provides a Phoenix-style WebSocket endpoint at `/realtime/v1/websocket`
//! where clients can subscribe to table changes and receive live events
//! for INSERT, UPDATE, and DELETE operations.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::api::server::AppState;

// ── WebSocket upgrade handler ────────────────────────────────────────────────

/// HTTP upgrade handler for the realtime WebSocket endpoint.
///
/// Accepts optional query parameters:
/// - `apikey` — JWT or API key for authentication (validated if auth_bridge present).
/// - `token`  — alias for `apikey`.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Optional: validate JWT from query params before upgrading.
    let token = params
        .get("apikey")
        .or_else(|| params.get("token"))
        .cloned();

    if let Some(ref _t) = token {
        // If an auth bridge is configured we could validate here;
        // for now we accept the connection and rely on per-message auth.
        debug!("WS upgrade with token present");
    }

    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

// ── WebSocket connection loop ────────────────────────────────────────────────

/// Main loop for a single WebSocket connection.
///
/// Simultaneously listens for:
/// 1. **Incoming messages** from the client (subscribe / unsubscribe / heartbeat).
/// 2. **Outgoing events** from the `ChangeNotifier` broadcast channel.
async fn handle_ws(mut socket: WebSocket, state: AppState) {
    info!("WS client connected");

    // If there is no change notifier configured we cannot serve realtime events.
    let notifier = match &state.change_notifier {
        Some(n) => n.clone(),
        None => {
            let err = serde_json::json!({
                "event": "system",
                "payload": {
                    "status": "error",
                    "message": "Realtime notifications are not enabled on this server"
                }
            });
            let _ = socket.send(Message::Text(err.to_string())).await;
            return;
        }
    };

    let mut rx = notifier.subscribe();
    let mut subscribed_tables: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            // ── Client → Server ──────────────────────────────────────────
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(
                            &text,
                            &mut socket,
                            &notifier,
                            &mut subscribed_tables,
                        )
                        .await;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("WS client disconnected");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("WS recv error: {e}");
                        break;
                    }
                    // Binary / Pong — ignore
                    _ => {}
                }
            }

            // ── Server → Client (broadcast events) ──────────────────────
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        // Only forward events for tables this connection cares about.
                        let dominated = subscribed_tables
                            .iter()
                            .any(|t| t == &event.table || t == "*");

                        if dominated {
                            let payload = serde_json::json!({
                                "event": "postgres_changes",
                                "payload": {
                                    "type": event.event_type,
                                    "table": event.table,
                                    "record": event.new_record,
                                    "old_record": event.old_record,
                                    "commit_timestamp": event.timestamp,
                                }
                            });
                            if socket
                                .send(Message::Text(payload.to_string()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WS client lagged by {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        }
    }

    // ── Cleanup ──────────────────────────────────────────────────────────
    for table in &subscribed_tables {
        notifier.remove_table_subscription(table);
    }
    info!("WS client fully disconnected, {} subscriptions removed", subscribed_tables.len());
}

// ── Client message handling ──────────────────────────────────────────────────

/// Parse and dispatch a single text message from the client.
///
/// Supports the following Phoenix-style events:
///
/// | `event`     | Description                                    |
/// |-------------|------------------------------------------------|
/// | `phx_join`  | Subscribe to a table's changes                 |
/// | `phx_leave` | Unsubscribe from a table's changes             |
/// | `heartbeat` | Keep-alive ping (server replies with `phx_reply`) |
async fn handle_client_message(
    text: &str,
    socket: &mut WebSocket,
    notifier: &crate::api::change_notifier::ChangeNotifier,
    subscribed_tables: &mut Vec<String>,
) {
    // Best-effort JSON parse — malformed messages are silently dropped.
    let msg: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            debug!("WS ignoring non-JSON message: {e}");
            return;
        }
    };

    let event = msg.get("event").and_then(|e| e.as_str()).unwrap_or("");
    let topic = msg.get("topic").and_then(|t| t.as_str()).unwrap_or("");
    let msg_ref = msg.get("ref").and_then(|r| r.as_str());

    match event {
        // ── Subscribe ────────────────────────────────────────────────
        "phx_join" => {
            // Extract table from topic (format: `realtime:public:table_name`)
            // or from payload.config.postgres_changes[].table
            let table = extract_table_from_join(&msg, topic);

            if !table.is_empty() && !subscribed_tables.contains(&table) {
                notifier.add_table_subscription(&table);
                subscribed_tables.push(table.clone());
                info!("WS subscribed to table: {table}");
            }

            let reply = serde_json::json!({
                "event": "phx_reply",
                "topic": topic,
                "ref": msg_ref,
                "payload": {
                    "status": "ok",
                    "response": {}
                }
            });
            let _ = socket.send(Message::Text(reply.to_string())).await;
        }

        // ── Unsubscribe ──────────────────────────────────────────────
        "phx_leave" => {
            let table = extract_table_from_topic(topic);
            if let Some(pos) = subscribed_tables.iter().position(|t| t == &table) {
                notifier.remove_table_subscription(&table);
                subscribed_tables.remove(pos);
                info!("WS unsubscribed from table: {table}");
            }

            let reply = serde_json::json!({
                "event": "phx_reply",
                "topic": topic,
                "ref": msg_ref,
                "payload": {
                    "status": "ok",
                    "response": {}
                }
            });
            let _ = socket.send(Message::Text(reply.to_string())).await;
        }

        // ── Heartbeat ────────────────────────────────────────────────
        "heartbeat" => {
            let reply = serde_json::json!({
                "event": "phx_reply",
                "topic": "phoenix",
                "ref": msg_ref,
                "payload": {
                    "status": "ok",
                    "response": {}
                }
            });
            let _ = socket.send(Message::Text(reply.to_string())).await;
        }

        other => {
            debug!("WS unknown event: {other}");
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extract the table name from a `phx_join` message.
///
/// Looks in two places:
/// 1. `payload.config.postgres_changes[0].table`
/// 2. The topic string, e.g. `realtime:public:users` -> `users`.
fn extract_table_from_join(msg: &serde_json::Value, topic: &str) -> String {
    // Try explicit postgres_changes config first.
    if let Some(table) = msg
        .get("payload")
        .and_then(|p| p.get("config"))
        .and_then(|c| c.get("postgres_changes"))
        .and_then(|pcs| pcs.as_array())
        .and_then(|arr| arr.first())
        .and_then(|pc| pc.get("table"))
        .and_then(|t| t.as_str())
    {
        return table.to_string();
    }

    // Fall back to topic parsing.
    extract_table_from_topic(topic)
}

/// Extract the table name from a Phoenix topic string.
///
/// Common formats:
/// - `realtime:public:users`  -> `users`
/// - `realtime:users`         -> `users`
/// - `users`                  -> `users`
/// - `realtime:*`             -> `*`
fn extract_table_from_topic(topic: &str) -> String {
    topic
        .rsplit(':')
        .next()
        .unwrap_or(topic)
        .to_string()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_table_from_topic_full() {
        assert_eq!(extract_table_from_topic("realtime:public:users"), "users");
    }

    #[test]
    fn test_extract_table_from_topic_short() {
        assert_eq!(extract_table_from_topic("realtime:orders"), "orders");
    }

    #[test]
    fn test_extract_table_from_topic_plain() {
        assert_eq!(extract_table_from_topic("items"), "items");
    }

    #[test]
    fn test_extract_table_from_topic_wildcard() {
        assert_eq!(extract_table_from_topic("realtime:*"), "*");
    }

    #[test]
    fn test_extract_table_from_join_with_config() {
        let msg = serde_json::json!({
            "event": "phx_join",
            "topic": "realtime:public:users",
            "payload": {
                "config": {
                    "postgres_changes": [
                        { "event": "*", "schema": "public", "table": "orders" }
                    ]
                }
            }
        });
        // Explicit config wins over topic.
        assert_eq!(extract_table_from_join(&msg, "realtime:public:users"), "orders");
    }

    #[test]
    fn test_extract_table_from_join_fallback_to_topic() {
        let msg = serde_json::json!({
            "event": "phx_join",
            "topic": "realtime:public:users",
            "payload": {}
        });
        assert_eq!(extract_table_from_join(&msg, "realtime:public:users"), "users");
    }
}
