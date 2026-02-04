//! Supabase Realtime Compatible API
//!
//! WebSocket-based realtime subscription API compatible with Supabase Realtime.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Realtime message types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum RealtimeMessage {
    /// Phoenix-style phx_join
    #[serde(rename = "phx_join")]
    Join(JoinPayload),

    /// Phoenix-style phx_leave
    #[serde(rename = "phx_leave")]
    Leave,

    /// Phoenix-style phx_reply
    #[serde(rename = "phx_reply")]
    Reply(ReplyPayload),

    /// Heartbeat (ping)
    #[serde(rename = "heartbeat")]
    Heartbeat,

    /// Presence state
    #[serde(rename = "presence_state")]
    PresenceState(PresenceStatePayload),

    /// Presence diff
    #[serde(rename = "presence_diff")]
    PresenceDiff(PresenceDiffPayload),

    /// Broadcast
    #[serde(rename = "broadcast")]
    Broadcast(BroadcastPayload),

    /// Postgres changes
    #[serde(rename = "postgres_changes")]
    PostgresChanges(PostgresChangesPayload),

    /// System event
    #[serde(rename = "system")]
    System(SystemPayload),
}

/// Join channel payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinPayload {
    pub config: ChannelConfig,
}

/// Channel configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Broadcast configuration
    pub broadcast: Option<BroadcastConfig>,
    /// Presence configuration
    pub presence: Option<PresenceConfig>,
    /// Postgres changes configuration
    pub postgres_changes: Option<Vec<PostgresChangeConfig>>,
}

/// Broadcast configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastConfig {
    /// Receive own broadcasts
    #[serde(rename = "self")]
    pub self_broadcast: Option<bool>,
    /// Acknowledge broadcasts
    pub ack: Option<bool>,
}

/// Presence configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceConfig {
    /// Presence key (usually user ID)
    pub key: Option<String>,
}

/// Postgres changes subscription config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresChangeConfig {
    /// Event type: INSERT, UPDATE, DELETE, *
    pub event: String,
    /// Schema name
    pub schema: String,
    /// Table name
    pub table: Option<String>,
    /// Filter expression
    pub filter: Option<String>,
}

/// Reply payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyPayload {
    pub status: String,
    pub response: serde_json::Value,
}

/// Presence state payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceStatePayload {
    #[serde(flatten)]
    pub presences: HashMap<String, Vec<PresenceMeta>>,
}

/// Presence metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceMeta {
    pub phx_ref: String,
    #[serde(flatten)]
    pub user_meta: serde_json::Value,
}

/// Presence diff payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceDiffPayload {
    pub joins: HashMap<String, Vec<PresenceMeta>>,
    pub leaves: HashMap<String, Vec<PresenceMeta>>,
}

/// Broadcast payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastPayload {
    pub event: String,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub payload: serde_json::Value,
}

/// Postgres changes payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresChangesPayload {
    pub data: PostgresChangeData,
}

/// Postgres change data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresChangeData {
    /// Schema name
    pub schema: String,
    /// Table name
    pub table: String,
    /// Commit timestamp
    pub commit_timestamp: String,
    /// Event type: INSERT, UPDATE, DELETE
    #[serde(rename = "eventType")]
    pub event_type: String,
    /// New row data
    pub new: Option<serde_json::Value>,
    /// Old row data
    pub old: Option<serde_json::Value>,
    /// Errors
    pub errors: Option<Vec<String>>,
}

/// System event payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPayload {
    pub channel: Option<String>,
    pub extension: Option<String>,
    pub message: Option<String>,
    pub status: Option<String>,
}

/// Incoming WebSocket message
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    pub topic: String,
    pub event: String,
    pub payload: serde_json::Value,
    #[serde(rename = "ref")]
    pub msg_ref: Option<String>,
}

/// Outgoing WebSocket message
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingMessage {
    pub topic: String,
    pub event: String,
    pub payload: serde_json::Value,
    #[serde(rename = "ref")]
    pub msg_ref: Option<String>,
}

impl OutgoingMessage {
    pub fn reply(topic: &str, msg_ref: Option<&str>, status: &str, response: serde_json::Value) -> Self {
        Self {
            topic: topic.to_string(),
            event: "phx_reply".to_string(),
            payload: serde_json::json!({
                "status": status,
                "response": response
            }),
            msg_ref: msg_ref.map(|s| s.to_string()),
        }
    }

    pub fn postgres_changes(topic: &str, change: PostgresChangeData) -> Self {
        Self {
            topic: topic.to_string(),
            event: "postgres_changes".to_string(),
            payload: serde_json::json!({
                "data": change
            }),
            msg_ref: None,
        }
    }

    pub fn broadcast(topic: &str, event: &str, payload: serde_json::Value) -> Self {
        Self {
            topic: topic.to_string(),
            event: "broadcast".to_string(),
            payload: serde_json::json!({
                "event": event,
                "payload": payload
            }),
            msg_ref: None,
        }
    }

    pub fn presence_state(topic: &str, state: HashMap<String, Vec<PresenceMeta>>) -> Self {
        Self {
            topic: topic.to_string(),
            event: "presence_state".to_string(),
            payload: serde_json::to_value(state).unwrap_or_default(),
            msg_ref: None,
        }
    }

    pub fn presence_diff(topic: &str, joins: HashMap<String, Vec<PresenceMeta>>, leaves: HashMap<String, Vec<PresenceMeta>>) -> Self {
        Self {
            topic: topic.to_string(),
            event: "presence_diff".to_string(),
            payload: serde_json::json!({
                "joins": joins,
                "leaves": leaves
            }),
            msg_ref: None,
        }
    }

    pub fn system(topic: &str, message: &str, status: &str) -> Self {
        Self {
            topic: topic.to_string(),
            event: "system".to_string(),
            payload: serde_json::json!({
                "message": message,
                "status": status
            }),
            msg_ref: None,
        }
    }

    pub fn heartbeat(msg_ref: Option<&str>) -> Self {
        Self {
            topic: "phoenix".to_string(),
            event: "phx_reply".to_string(),
            payload: serde_json::json!({
                "status": "ok",
                "response": {}
            }),
            msg_ref: msg_ref.map(|s| s.to_string()),
        }
    }
}

/// Channel subscription
#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: String,
    pub topic: String,
    pub config: ChannelConfig,
    pub created_at: u64,
}

/// Realtime server state
pub struct RealtimeServer {
    subscriptions: HashMap<String, Vec<Subscription>>, // connection_id -> subscriptions
    presence: HashMap<String, HashMap<String, Vec<PresenceMeta>>>, // topic -> presence state
}

impl RealtimeServer {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
            presence: HashMap::new(),
        }
    }

    /// Handle incoming message
    pub fn handle_message(
        &mut self,
        connection_id: &str,
        message: IncomingMessage,
    ) -> Vec<OutgoingMessage> {
        let mut responses = Vec::new();

        match message.event.as_str() {
            "phx_join" => {
                let config: ChannelConfig = serde_json::from_value(
                    message.payload.get("config").cloned().unwrap_or_default()
                ).unwrap_or(ChannelConfig {
                    broadcast: None,
                    presence: None,
                    postgres_changes: None,
                });

                // Create subscription
                let sub = Subscription {
                    id: generate_ref(),
                    topic: message.topic.clone(),
                    config: config.clone(),
                    created_at: current_timestamp(),
                };

                self.subscriptions
                    .entry(connection_id.to_string())
                    .or_default()
                    .push(sub);

                // Reply with OK
                responses.push(OutgoingMessage::reply(
                    &message.topic,
                    message.msg_ref.as_deref(),
                    "ok",
                    serde_json::json!({
                        "postgres_changes": config.postgres_changes.map(|pcs| {
                            pcs.iter().map(|pc| {
                                serde_json::json!({
                                    "id": generate_ref(),
                                    "event": pc.event,
                                    "schema": pc.schema,
                                    "table": pc.table,
                                    "filter": pc.filter
                                })
                            }).collect::<Vec<_>>()
                        })
                    }),
                ));

                // Send presence state if presence is configured
                if config.presence.is_some() {
                    let state = self.presence.get(&message.topic).cloned().unwrap_or_default();
                    responses.push(OutgoingMessage::presence_state(&message.topic, state));
                }
            }

            "phx_leave" => {
                // Remove subscription
                if let Some(subs) = self.subscriptions.get_mut(connection_id) {
                    subs.retain(|s| s.topic != message.topic);
                }

                responses.push(OutgoingMessage::reply(
                    &message.topic,
                    message.msg_ref.as_deref(),
                    "ok",
                    serde_json::json!({}),
                ));
            }

            "heartbeat" => {
                responses.push(OutgoingMessage::heartbeat(message.msg_ref.as_deref()));
            }

            "broadcast" => {
                // Forward broadcast to all subscribers
                if let Some(payload) = message.payload.get("payload") {
                    let event = message.payload.get("event")
                        .and_then(|e| e.as_str())
                        .unwrap_or("broadcast");

                    responses.push(OutgoingMessage::broadcast(
                        &message.topic,
                        event,
                        payload.clone(),
                    ));
                }
            }

            "presence" => {
                // Handle presence update
                if let Some(key) = message.payload.get("key").and_then(|k| k.as_str()) {
                    let meta = PresenceMeta {
                        phx_ref: generate_ref(),
                        user_meta: message.payload.get("meta").cloned().unwrap_or_default(),
                    };

                    let mut joins = HashMap::new();
                    joins.insert(key.to_string(), vec![meta]);

                    self.presence
                        .entry(message.topic.clone())
                        .or_default()
                        .insert(key.to_string(), joins.get(key).cloned().unwrap_or_default());

                    responses.push(OutgoingMessage::presence_diff(
                        &message.topic,
                        joins,
                        HashMap::new(),
                    ));
                }
            }

            _ => {
                // Unknown event - send error reply
                responses.push(OutgoingMessage::reply(
                    &message.topic,
                    message.msg_ref.as_deref(),
                    "error",
                    serde_json::json!({
                        "reason": format!("Unknown event: {}", message.event)
                    }),
                ));
            }
        }

        responses
    }

    /// Notify postgres change to subscribers
    pub fn notify_postgres_change(&self, change: &PostgresChangeData) -> Vec<(String, OutgoingMessage)> {
        let mut notifications = Vec::new();

        for (conn_id, subs) in &self.subscriptions {
            for sub in subs {
                if let Some(ref pcs) = sub.config.postgres_changes {
                    for pc in pcs {
                        // Check if change matches subscription filter
                        if pc.schema == change.schema
                            && (pc.event == "*" || pc.event.to_uppercase() == change.event_type)
                            && (pc.table.is_none() || pc.table.as_deref() == Some(&change.table))
                        {
                            notifications.push((
                                conn_id.clone(),
                                OutgoingMessage::postgres_changes(&sub.topic, change.clone()),
                            ));
                        }
                    }
                }
            }
        }

        notifications
    }

    /// Remove connection and clean up presence
    pub fn disconnect(&mut self, connection_id: &str) -> Vec<OutgoingMessage> {
        let mut responses = Vec::new();

        if let Some(subs) = self.subscriptions.remove(connection_id) {
            for sub in subs {
                // Clean up presence
                if let Some(presence) = self.presence.get_mut(&sub.topic) {
                    // Would identify user from connection and remove
                    if !presence.is_empty() {
                        let leaves: HashMap<String, Vec<PresenceMeta>> = presence.drain().collect();
                        responses.push(OutgoingMessage::presence_diff(&sub.topic, HashMap::new(), leaves));
                    }
                }
            }
        }

        responses
    }

    /// Get all connections subscribed to a topic
    pub fn get_topic_subscribers(&self, topic: &str) -> Vec<&str> {
        self.subscriptions
            .iter()
            .filter(|(_, subs)| subs.iter().any(|s| s.topic == topic))
            .map(|(conn_id, _)| conn_id.as_str())
            .collect()
    }
}

impl Default for RealtimeServer {
    fn default() -> Self {
        Self::new()
    }
}

// Helper functions

fn generate_ref() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);

    format!("{}", hasher.finish())
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
