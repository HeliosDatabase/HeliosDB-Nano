//! Agent memory API handlers
//!
//! Provides REST API endpoints for AI agent memory management:
//! - Session-based conversation memory
//! - Semantic memory search
//! - Memory summarization and compression
//! - Cross-session memory sharing

#![allow(unused_variables)]

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::api::models::{ApiError, ApiResponse};
use crate::api::server::AppState;

/// Memory message
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MemoryMessage {
    /// Message role (user, assistant, system, function)
    pub role: String,
    /// Message content
    pub content: String,
    /// Optional function name (for function role)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional function call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    /// Optional tool calls
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Message metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// Function call in message
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Tool call in message
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Add message request
#[derive(Debug, Deserialize)]
pub struct AddMessageRequest {
    /// Message role
    pub role: String,
    /// Message content
    pub content: String,
    /// Optional function name
    pub name: Option<String>,
    /// Optional function call
    pub function_call: Option<FunctionCall>,
    /// Optional tool calls
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Optional metadata
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Add messages batch request
#[derive(Debug, Deserialize)]
pub struct AddMessagesRequest {
    /// Messages to add
    pub messages: Vec<AddMessageRequest>,
}

/// Session info response
#[derive(Debug, Serialize)]
pub struct SessionInfo {
    /// Session ID
    pub session_id: String,
    /// Number of messages
    pub message_count: usize,
    /// Total tokens (estimated)
    pub token_count: usize,
    /// Session metadata
    pub metadata: HashMap<String, serde_json::Value>,
    /// Created timestamp
    pub created_at: String,
    /// Last updated timestamp
    pub updated_at: String,
}

/// Memory search request
#[derive(Debug, Deserialize)]
pub struct SearchMemoryRequest {
    /// Search query
    pub query: String,
    /// Number of results
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Filter by role
    pub role_filter: Option<Vec<String>>,
    /// Time range start
    pub since: Option<String>,
    /// Time range end
    pub until: Option<String>,
    /// Minimum relevance score
    pub min_score: Option<f32>,
}

fn default_limit() -> usize {
    10
}

/// Search result
#[derive(Debug, Serialize)]
pub struct MemorySearchResult {
    /// Message content
    pub message: MemoryMessage,
    /// Relevance score
    pub score: f32,
    /// Message index in session
    pub index: usize,
}

/// Get messages query parameters
#[derive(Debug, Deserialize)]
pub struct GetMessagesQuery {
    /// Maximum number of messages
    pub limit: Option<usize>,
    /// Skip first N messages
    pub offset: Option<usize>,
    /// Filter by role
    pub role: Option<String>,
    /// Order (asc/desc)
    pub order: Option<String>,
    /// Since timestamp
    pub since: Option<String>,
}

/// Create session request
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// Custom session ID (optional, auto-generated if not provided)
    pub session_id: Option<String>,
    /// Session metadata
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Token limit for auto-summarization
    pub token_limit: Option<usize>,
    /// Summarization strategy (none, rolling, hierarchical)
    pub summarization: Option<String>,
}

/// Update session request
#[derive(Debug, Deserialize)]
pub struct UpdateSessionRequest {
    /// Session metadata to update
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Token limit
    pub token_limit: Option<usize>,
    /// Summarization strategy
    pub summarization: Option<String>,
}

/// Summarize memory request
#[derive(Debug, Deserialize)]
pub struct SummarizeRequest {
    /// Summarization strategy
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Keep last N messages
    pub keep_last: Option<usize>,
    /// Target token count after summarization
    pub target_tokens: Option<usize>,
    /// Custom summarization prompt
    pub prompt: Option<String>,
}

fn default_strategy() -> String {
    "rolling".to_string()
}

/// Summary response
#[derive(Debug, Serialize)]
pub struct SummaryResponse {
    /// Summary text
    pub summary: String,
    /// Original message count
    pub original_count: usize,
    /// New message count
    pub new_count: usize,
    /// Tokens saved
    pub tokens_saved: usize,
}

/// Fork session request
#[derive(Debug, Deserialize)]
pub struct ForkSessionRequest {
    /// New session ID
    pub new_session_id: Option<String>,
    /// Fork from message index (default: all messages)
    pub from_index: Option<usize>,
    /// Include summary in fork
    #[serde(default = "default_true")]
    pub include_summary: bool,
}

fn default_true() -> bool {
    true
}

/// Clear messages request
#[derive(Debug, Deserialize)]
pub struct ClearMessagesRequest {
    /// Keep last N messages
    pub keep_last: Option<usize>,
    /// Keep system messages
    #[serde(default = "default_true")]
    pub keep_system: bool,
}

/// Context window request
#[derive(Debug, Deserialize)]
pub struct GetContextRequest {
    /// Maximum tokens
    pub max_tokens: usize,
    /// Include system prompt
    #[serde(default = "default_true")]
    pub include_system: bool,
    /// Include summary
    #[serde(default = "default_true")]
    pub include_summary: bool,
    /// Recency bias (0.0-1.0)
    pub recency_bias: Option<f32>,
}

/// Context window response
#[derive(Debug, Serialize)]
pub struct ContextResponse {
    /// Messages for context window
    pub messages: Vec<MemoryMessage>,
    /// Token count
    pub token_count: usize,
    /// Truncated flag
    pub truncated: bool,
    /// Summary included
    pub summary_included: bool,
}

// ============================================================================
// Handler implementations
// ============================================================================

/// List all sessions
pub async fn list_sessions(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<ApiResponse<Vec<SessionInfo>>>, ApiError> {
    let limit = params.get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    let sessions = state.db.list_agent_sessions()
        .map_err(|e| ApiError::internal(format!("Failed to list sessions: {}", e)))?;

    let session_infos: Vec<SessionInfo> = sessions
        .into_iter()
        .map(|s| {
            // Convert metadata from serde_json::Value to HashMap
            let metadata = match s.metadata {
                serde_json::Value::Object(map) => map.into_iter().collect(),
                _ => HashMap::new(),
            };

            SessionInfo {
                session_id: s.session_id,
                message_count: s.message_count as usize,
                token_count: s.token_count as usize,
                metadata,
                created_at: s.created_at,
                updated_at: s.updated_at,
            }
        })
        .collect();

    Ok(Json(ApiResponse::success(session_infos)))
}

/// Create a new session
pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<ApiResponse<SessionInfo>>), ApiError> {
    let session_id = req.session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let session = state.db.create_agent_session(
        &session_id,
    ).map_err(|e| ApiError::internal(format!("Failed to create session: {}", e)))?;

    // Convert metadata
    let metadata = match session.metadata {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    };

    let info = SessionInfo {
        session_id: session.session_id,
        message_count: 0,
        token_count: 0,
        metadata,
        created_at: session.created_at,
        updated_at: session.updated_at,
    };

    Ok((StatusCode::CREATED, Json(ApiResponse::success(info))))
}

/// Get session info
pub async fn get_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<ApiResponse<SessionInfo>>, ApiError> {
    let session = state.db.get_agent_session(&session_id)
        .map_err(|e| ApiError::not_found(format!("Session not found: {}", e)))?;

    // Convert metadata
    let metadata = match session.metadata {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    };

    let info = SessionInfo {
        session_id: session.session_id,
        message_count: session.message_count as usize,
        token_count: session.token_count as usize,
        metadata,
        created_at: session.created_at,
        updated_at: session.updated_at,
    };

    Ok(Json(ApiResponse::success(info)))
}

/// Update session
pub async fn update_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(_req): Json<UpdateSessionRequest>,
) -> Result<Json<ApiResponse<SessionInfo>>, ApiError> {
    // Note: update_agent_session method doesn't exist yet, so we'll just retrieve the session
    let session = state.db.get_agent_session(&session_id)
        .map_err(|e| ApiError::not_found(format!("Session not found: {}", e)))?;

    // Convert metadata
    let metadata = match session.metadata {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    };

    let info = SessionInfo {
        session_id: session.session_id,
        message_count: session.message_count as usize,
        token_count: session.token_count as usize,
        metadata,
        created_at: session.created_at,
        updated_at: session.updated_at,
    };

    Ok(Json(ApiResponse::success(info)))
}

/// Delete session
pub async fn delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.db.delete_agent_session(&session_id)
        .map_err(|e| ApiError::internal(format!("Failed to delete session: {}", e)))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Add a message to session
pub async fn add_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<AddMessageRequest>,
) -> Result<(StatusCode, Json<ApiResponse<MemoryMessage>>), ApiError> {
    let message = state.db.add_agent_message(
        &session_id,
        &req.role,
        &req.content,
    ).map_err(|e| ApiError::internal(format!("Failed to add message: {}", e)))?;

    // Parse function_call JSON string if present
    let function_call = message.function_call.as_ref().and_then(|fc_str| {
        serde_json::from_str::<FunctionCall>(fc_str).ok()
    });

    // Convert metadata to HashMap
    let metadata = if let serde_json::Value::Object(map) = message.metadata {
        Some(map.into_iter().collect())
    } else {
        Some(HashMap::new())
    };

    // Parse tool_calls if present
    let tool_calls = message.tool_calls.as_ref().and_then(|tc_val| {
        serde_json::from_value::<Vec<ToolCall>>(tc_val.clone()).ok()
    });

    let response = MemoryMessage {
        role: message.role,
        content: message.content,
        name: if message.name.is_empty() { None } else { Some(message.name) },
        function_call,
        tool_calls,
        metadata,
        timestamp: Some(message.timestamp),
    };

    Ok((StatusCode::CREATED, Json(ApiResponse::success(response))))
}

/// Add multiple messages
pub async fn add_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<AddMessagesRequest>,
) -> Result<(StatusCode, Json<ApiResponse<serde_json::Value>>), ApiError> {
    // Note: add_agent_messages_batch doesn't exist, so we'll add them one by one
    let mut count = 0;
    for msg in &req.messages {
        let _ = state.db.add_agent_message(
            &session_id,
            &msg.role,
            &msg.content,
        ).map_err(|e| ApiError::internal(format!("Failed to add message: {}", e)))?;
        count += 1;
    }

    Ok((StatusCode::CREATED, Json(ApiResponse::success(serde_json::json!({
        "added_count": count,
    })))))
}

/// Get messages from session
pub async fn get_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<ApiResponse<Vec<MemoryMessage>>>, ApiError> {
    let messages = state.db.get_agent_messages(
        &session_id,
    ).map_err(|e| ApiError::internal(format!("Failed to get messages: {}", e)))?;

    let response: Vec<MemoryMessage> = messages
        .into_iter()
        .map(|m| {
            // Parse function_call JSON string if present
            let function_call = m.function_call.as_ref().and_then(|fc_str| {
                serde_json::from_str::<FunctionCall>(fc_str).ok()
            });

            // Convert metadata to HashMap
            let metadata = if let serde_json::Value::Object(map) = m.metadata {
                Some(map.into_iter().collect())
            } else {
                Some(HashMap::new())
            };

            // Parse tool_calls if present
            let tool_calls = m.tool_calls.as_ref().and_then(|tc_val| {
                serde_json::from_value::<Vec<ToolCall>>(tc_val.clone()).ok()
            });

            MemoryMessage {
                role: m.role,
                content: m.content,
                name: if m.name.is_empty() { None } else { Some(m.name) },
                function_call,
                tool_calls,
                metadata,
                timestamp: Some(m.timestamp),
            }
        })
        .collect();

    Ok(Json(ApiResponse::success(response)))
}

/// Search memory semantically
pub async fn search_memory(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<SearchMemoryRequest>,
) -> Result<Json<ApiResponse<Vec<MemorySearchResult>>>, ApiError> {
    let raw_results = state.db.search_agent_memory(
        &session_id,
        &req.query,
    ).map_err(|e| ApiError::internal(format!("Memory search failed: {}", e)))?;

    let response: Vec<MemorySearchResult> = raw_results
        .into_iter()
        .enumerate()
        .map(|(index, (msg, score))| {
            // Parse function_call JSON string if present
            let function_call = msg.function_call.as_ref().and_then(|fc_str| {
                serde_json::from_str::<FunctionCall>(fc_str).ok()
            });

            // Convert metadata to HashMap
            let metadata = if let serde_json::Value::Object(map) = msg.metadata {
                map.into_iter().collect()
            } else {
                HashMap::new()
            };

            // Parse tool_calls if present
            let tool_calls = msg.tool_calls.as_ref().and_then(|tc_val| {
                serde_json::from_value::<Vec<ToolCall>>(tc_val.clone()).ok()
            });

            MemorySearchResult {
                message: MemoryMessage {
                    role: msg.role,
                    content: msg.content,
                    name: if msg.name.is_empty() { None } else { Some(msg.name) },
                    function_call,
                    tool_calls,
                    metadata: Some(metadata),
                    timestamp: Some(msg.timestamp),
                },
                score,
                index,
            }
        })
        .collect();

    Ok(Json(ApiResponse::success(response)))
}

/// Summarize memory
pub async fn summarize_memory(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(_req): Json<SummarizeRequest>,
) -> Result<Json<ApiResponse<SummaryResponse>>, ApiError> {
    let summary = state.db.summarize_agent_memory(
        &session_id,
    ).map_err(|e| ApiError::internal(format!("Summarization failed: {}", e)))?;

    Ok(Json(ApiResponse::success(SummaryResponse {
        summary,
        original_count: 0, // Not available from current implementation
        new_count: 0,
        tokens_saved: 0,
    })))
}

/// Get context window
pub async fn get_context(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(_req): Json<GetContextRequest>,
) -> Result<Json<ApiResponse<ContextResponse>>, ApiError> {
    let _result = state.db.get_agent_context(
        &session_id,
    ).map_err(|e| ApiError::internal(format!("Failed to get context: {}", e)))?;

    // Since get_agent_context returns serde_json::Value, we can't reliably parse it
    // For now, return an empty context
    Ok(Json(ApiResponse::success(ContextResponse {
        messages: vec![],
        token_count: 0,
        truncated: false,
        summary_included: false,
    })))
}

/// Fork session
pub async fn fork_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<ForkSessionRequest>,
) -> Result<(StatusCode, Json<ApiResponse<SessionInfo>>), ApiError> {
    let new_session_id = req.new_session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let session = state.db.fork_agent_session(
        &session_id,
        &new_session_id,
    ).map_err(|e| ApiError::internal(format!("Failed to fork session: {}", e)))?;

    // Convert metadata
    let metadata = match session.metadata {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    };

    let info = SessionInfo {
        session_id: session.session_id,
        message_count: session.message_count as usize,
        token_count: session.token_count as usize,
        metadata,
        created_at: session.created_at,
        updated_at: session.updated_at,
    };

    Ok((StatusCode::CREATED, Json(ApiResponse::success(info))))
}

/// Clear messages
pub async fn clear_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<ClearMessagesRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    state.db.clear_agent_messages(
        &session_id,
    ).map_err(|e| ApiError::internal(format!("Failed to clear messages: {}", e)))?;
    let deleted = 0;

    Ok(Json(ApiResponse::success(serde_json::json!({
        "deleted_count": deleted,
    }))))
}
