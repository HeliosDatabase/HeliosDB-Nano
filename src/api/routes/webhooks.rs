//! Webhook routes for Git provider integration
//!
//! Exposes endpoints for GitHub, GitLab, and generic webhooks to automate
//! PR/MR preview environments.
//!
//! ## Endpoints
//!
//! - `POST /api/webhooks/github` - GitHub webhook
//! - `POST /api/webhooks/gitlab` - GitLab webhook
//! - `POST /api/webhooks/generic` - Provider-agnostic webhook

use axum::{
    extract::{State, Json},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};

use crate::api::server::AppState;
use crate::git_integration::webhooks::{
    WebhookHandler, WebhookConfig, WebhookResult, WebhookEvent,
    GitProvider, WebhookEventType, StorageWebhookHandler,
};

/// Webhook response
#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

impl From<WebhookResult> for WebhookResponse {
    fn from(result: WebhookResult) -> Self {
        Self {
            success: result.success,
            message: result.message,
            branch_id: result.branch_id,
            action: result.action,
        }
    }
}

/// Error response
#[derive(Debug, Serialize)]
pub struct WebhookError {
    pub error: String,
    pub code: String,
}

impl WebhookError {
    fn new(error: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: code.into(),
        }
    }
}

/// Create webhook routes
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/github", post(handle_github_webhook))
        .route("/gitlab", post(handle_gitlab_webhook))
        .route("/generic", post(handle_generic_webhook))
        .route("/health", axum::routing::get(health_check))
}

/// Health check for webhook endpoints
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "webhooks"
    }))
}

/// Handle GitHub webhook
///
/// Expects:
/// - Header `X-GitHub-Event`: Event type (e.g., "pull_request")
/// - Header `X-Hub-Signature-256`: HMAC signature (optional, for validation)
/// - Body: JSON payload
async fn handle_github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    // Get event type header
    let event_type = match headers.get("X-GitHub-Event") {
        Some(v) => v.to_str().unwrap_or("unknown"),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(WebhookError::new(
                    "Missing X-GitHub-Event header",
                    "MISSING_EVENT_TYPE",
                )),
            ).into_response();
        }
    };

    // Get webhook config from state (if available)
    let config = get_webhook_config(&state);
    let handler = WebhookHandler::new(config);

    // Validate signature if configured
    if let Some(signature) = headers.get("X-Hub-Signature-256") {
        let sig_str = signature.to_str().unwrap_or("");
        match handler.validate_github_signature(body.as_bytes(), sig_str) {
            Ok(false) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        "Invalid webhook signature",
                        "INVALID_SIGNATURE",
                    )),
                ).into_response();
            }
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        format!("Signature validation error: {}", e),
                        "SIGNATURE_ERROR",
                    )),
                ).into_response();
            }
            Ok(true) => {}
        }
    }

    // Parse the webhook payload
    let event = match handler.parse_github(&body, event_type) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(WebhookError::new(
                    format!("Failed to parse webhook: {}", e),
                    "PARSE_ERROR",
                )),
            ).into_response();
        }
    };

    // Handle the event - prefer storage-aware handler if available
    let result = handle_event_with_storage(&state, &event);
    match result {
        Ok(result) => {
            let status = if result.success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(WebhookResponse::from(result))).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WebhookError::new(
                    format!("Handler error: {}", e),
                    "HANDLER_ERROR",
                )),
            ).into_response()
        }
    }
}

/// Handle GitLab webhook
///
/// Expects:
/// - Header `X-Gitlab-Event`: Event type
/// - Header `X-Gitlab-Token`: Secret token (optional, for validation)
/// - Body: JSON payload
async fn handle_gitlab_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    // Get webhook config from state
    let config = get_webhook_config(&state);
    let handler = WebhookHandler::new(config);

    // Validate token if provided
    if let Some(token) = headers.get("X-Gitlab-Token") {
        let token_str = token.to_str().unwrap_or("");
        match handler.validate_gitlab_token(token_str) {
            Ok(false) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        "Invalid GitLab token",
                        "INVALID_TOKEN",
                    )),
                ).into_response();
            }
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        format!("Token validation error: {}", e),
                        "TOKEN_ERROR",
                    )),
                ).into_response();
            }
            Ok(true) => {}
        }
    }

    // Parse the webhook payload
    let event = match handler.parse_gitlab(&body) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(WebhookError::new(
                    format!("Failed to parse webhook: {}", e),
                    "PARSE_ERROR",
                )),
            ).into_response();
        }
    };

    // Handle the event - prefer storage-aware handler if available
    let result = handle_event_with_storage(&state, &event);
    match result {
        Ok(result) => {
            let status = if result.success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(WebhookResponse::from(result))).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WebhookError::new(
                    format!("Handler error: {}", e),
                    "HANDLER_ERROR",
                )),
            ).into_response()
        }
    }
}

/// Generic webhook payload
#[derive(Debug, Deserialize)]
pub struct GenericWebhookPayload {
    /// Event type (pr_opened, pr_closed, pr_merged, etc.)
    pub event_type: String,
    /// Source branch name
    pub source_branch: String,
    /// Target branch name (for PRs)
    #[serde(default)]
    pub target_branch: Option<String>,
    /// PR/MR number
    #[serde(default)]
    pub pr_number: Option<u64>,
    /// Commit SHA
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Provider name (github, gitlab, bitbucket, custom)
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Repository identifier
    #[serde(default)]
    pub repository: Option<String>,
}

fn default_provider() -> String {
    "generic".to_string()
}

/// Handle generic webhook
///
/// Provider-agnostic webhook endpoint that accepts a standardized payload format.
/// This allows integration with any CI/CD system or custom tooling.
async fn handle_generic_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<GenericWebhookPayload>,
) -> impl IntoResponse {
    // Get webhook config from state
    let config = get_webhook_config(&state);

    // Validate secret if provided in Authorization header
    if let Some(auth) = headers.get("Authorization") {
        if let Some(ref secret) = config.generic_secret {
            let auth_str = auth.to_str().unwrap_or("");
            let expected = format!("Bearer {}", secret);
            if auth_str != expected {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        "Invalid authorization",
                        "INVALID_AUTH",
                    )),
                ).into_response();
            }
        }
    }

    // Parse event type
    let event_type = match payload.event_type.as_str() {
        "pr_opened" | "pull_request_opened" | "mr_opened" => WebhookEventType::PrOpened,
        "pr_updated" | "pull_request_updated" | "mr_updated" => WebhookEventType::PrUpdated,
        "pr_merged" | "pull_request_merged" | "mr_merged" => WebhookEventType::PrMerged,
        "pr_closed" | "pull_request_closed" | "mr_closed" => WebhookEventType::PrClosed,
        "push" => WebhookEventType::Push,
        "branch_created" => WebhookEventType::BranchCreated,
        "branch_deleted" => WebhookEventType::BranchDeleted,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(WebhookError::new(
                    format!("Unknown event type: {}", payload.event_type),
                    "UNKNOWN_EVENT_TYPE",
                )),
            ).into_response();
        }
    };

    // Parse provider
    let provider = match payload.provider.to_lowercase().as_str() {
        "github" => GitProvider::GitHub,
        "gitlab" => GitProvider::GitLab,
        "bitbucket" => GitProvider::Bitbucket,
        _ => GitProvider::Generic,
    };

    // Build event
    let event = WebhookEvent {
        event_type,
        source_branch: payload.source_branch,
        target_branch: payload.target_branch,
        pr_number: payload.pr_number,
        commit_sha: payload.commit_sha,
        provider,
        repository: payload.repository,
        timestamp: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        ),
        raw_payload: None,
    };

    // Handle the event - prefer storage-aware handler if available
    let result = handle_event_with_storage(&state, &event);
    match result {
        Ok(result) => {
            let status = if result.success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(WebhookResponse::from(result))).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WebhookError::new(
                    format!("Handler error: {}", e),
                    "HANDLER_ERROR",
                )),
            ).into_response()
        }
    }
}

/// Handle webhook event with storage integration if available
///
/// If the database has branching enabled, uses StorageWebhookHandler
/// to actually create/merge/drop branches. Otherwise falls back to
/// the basic WebhookHandler that just logs events.
fn handle_event_with_storage(
    state: &AppState,
    event: &WebhookEvent,
) -> Result<WebhookResult, crate::Error> {
    let config = get_webhook_config(state);

    // Try to use storage-aware handler if branching is enabled
    if let Some(branch_manager) = state.db.storage.branch_manager() {
        let storage_handler = StorageWebhookHandler::new(config, branch_manager.as_ref());
        storage_handler.handle(event)
    } else {
        // Fall back to basic handler (just logging)
        let basic_handler = WebhookHandler::new(config);
        basic_handler.handle(event)
    }
}

/// Get webhook configuration from app state
///
/// Reads webhook secrets from environment variables:
/// - HELIOS_GITHUB_WEBHOOK_SECRET: GitHub webhook signature secret
/// - HELIOS_GITLAB_WEBHOOK_TOKEN: GitLab webhook token
/// - HELIOS_WEBHOOK_SECRET: Generic webhook secret
fn get_webhook_config(_state: &AppState) -> WebhookConfig {
    WebhookConfig {
        github_secret: std::env::var("HELIOS_GITHUB_WEBHOOK_SECRET").ok(),
        gitlab_token: std::env::var("HELIOS_GITLAB_WEBHOOK_TOKEN").ok(),
        generic_secret: std::env::var("HELIOS_WEBHOOK_SECRET").ok(),
        allowed_ips: Vec::new(),
        rate_limit: 60, // requests per minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_payload_parsing() {
        let json = r#"{
            "event_type": "pr_opened",
            "source_branch": "feature/test",
            "target_branch": "main",
            "pr_number": 123
        }"#;

        let payload: GenericWebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.event_type, "pr_opened");
        assert_eq!(payload.source_branch, "feature/test");
        assert_eq!(payload.target_branch, Some("main".to_string()));
        assert_eq!(payload.pr_number, Some(123));
        assert_eq!(payload.provider, "generic");
    }

    #[test]
    fn test_webhook_response_from_result() {
        let result = WebhookResult::success("Test success")
            .with_branch(42)
            .with_action("test");

        let response: WebhookResponse = result.into();
        assert!(response.success);
        assert_eq!(response.message, "Test success");
        assert_eq!(response.branch_id, Some(42));
        assert_eq!(response.action, Some("test".to_string()));
    }
}
