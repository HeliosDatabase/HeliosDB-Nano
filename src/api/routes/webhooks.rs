//! Webhook routes for Git provider integration
//!
//! Exposes endpoints for GitHub, GitLab, and generic webhooks to automate
//! PR/MR preview environments.
//!
//! ## Endpoints
//!
//! - `POST /v1/webhooks/github` - GitHub webhook
//! - `POST /v1/webhooks/gitlab` - GitLab webhook
//! - `POST /v1/webhooks/generic` - Provider-agnostic webhook
//!
//! ## Authentication
//!
//! The secrets come from the environment once, when the router is built
//! (`HELIOS_GITHUB_WEBHOOK_SECRET`, `HELIOS_GITLAB_WEBHOOK_TOKEN`,
//! `HELIOS_WEBHOOK_SECRET`; an empty value counts as unset). When a provider's
//! secret is set, a request without a valid signature or token is refused with
//! 401 before anything is parsed or any branch is touched. With no secret set
//! the endpoint accepts unsigned requests (as before) and says so in the log
//! once; the outer HTTP policy still applies.

use std::sync::Arc;

use axum::{
    extract::{Extension, Json, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};

use crate::api::server::AppState;
use crate::git_integration::webhooks::{
    ct_eq_secret, GitProvider, StorageWebhookHandler, WebhookConfig, WebhookEvent, WebhookEventType, WebhookHandler,
    WebhookResult,
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

/// Create webhook routes. The secrets are read from the environment here,
/// once, not on every request.
pub fn routes() -> Router<AppState> {
    routes_with_config(WebhookConfig::from_env())
}

/// Create webhook routes with an explicit configuration.
pub fn routes_with_config(config: WebhookConfig) -> Router<AppState> {
    for (name, secret, variable) in [
        ("github", &config.github_secret, "HELIOS_GITHUB_WEBHOOK_SECRET"),
        ("gitlab", &config.gitlab_token, "HELIOS_GITLAB_WEBHOOK_TOKEN"),
        ("generic", &config.generic_secret, "HELIOS_WEBHOOK_SECRET"),
    ] {
        if secret.is_none() {
            tracing::warn!("webhook endpoint /v1/webhooks/{name} accepts unsigned requests: {variable} is not set");
        }
    }
    Router::new()
        .route("/github", post(handle_github_webhook))
        .route("/gitlab", post(handle_gitlab_webhook))
        .route("/generic", post(handle_generic_webhook))
        .route("/health", axum::routing::get(health_check))
        .layer(Extension(Arc::new(config)))
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
/// - Header `X-Hub-Signature-256`: HMAC signature (required when a secret is configured)
/// - Body: JSON payload
async fn handle_github_webhook(
    State(state): State<AppState>,
    Extension(config): Extension<Arc<WebhookConfig>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    // Get event type header
    let event_type = match headers.get("X-GitHub-Event") {
        Some(v) => v.to_str().unwrap_or("unknown"),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(WebhookError::new("Missing X-GitHub-Event header", "MISSING_EVENT_TYPE")),
            )
                .into_response();
        }
    };

    let handler = WebhookHandler::new((*config).clone());

    // With a secret configured the signature is mandatory: a missing header
    // is refused, not skipped.
    if config.github_secret.is_some() {
        let Some(signature) = headers.get("X-Hub-Signature-256") else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(WebhookError::new("Missing webhook signature", "MISSING_SIGNATURE")),
            )
                .into_response();
        };
        let sig_str = signature.to_str().unwrap_or("");
        match handler.validate_github_signature(body.as_bytes(), sig_str) {
            Ok(false) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new("Invalid webhook signature", "INVALID_SIGNATURE")),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        format!("Signature validation error: {}", e),
                        "SIGNATURE_ERROR",
                    )),
                )
                    .into_response();
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
            )
                .into_response();
        }
    };

    // Handle the event - prefer storage-aware handler if available
    let result = handle_event_with_storage(&state, &config, &event);
    match result {
        Ok(result) => {
            let status = if result.success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(WebhookResponse::from(result))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(WebhookError::new(format!("Handler error: {}", e), "HANDLER_ERROR")),
        )
            .into_response(),
    }
}

/// Handle GitLab webhook
///
/// Expects:
/// - Header `X-Gitlab-Event`: Event type
/// - Header `X-Gitlab-Token`: Secret token (required when a token is configured)
/// - Body: JSON payload
async fn handle_gitlab_webhook(
    State(state): State<AppState>,
    Extension(config): Extension<Arc<WebhookConfig>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let handler = WebhookHandler::new((*config).clone());

    // With a token configured it is mandatory: a missing header is refused.
    if config.gitlab_token.is_some() {
        let Some(token) = headers.get("X-Gitlab-Token") else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(WebhookError::new("Missing GitLab token", "MISSING_TOKEN")),
            )
                .into_response();
        };
        let token_str = token.to_str().unwrap_or("");
        match handler.validate_gitlab_token(token_str) {
            Ok(false) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new("Invalid GitLab token", "INVALID_TOKEN")),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(WebhookError::new(
                        format!("Token validation error: {}", e),
                        "TOKEN_ERROR",
                    )),
                )
                    .into_response();
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
            )
                .into_response();
        }
    };

    // Handle the event - prefer storage-aware handler if available
    let result = handle_event_with_storage(&state, &config, &event);
    match result {
        Ok(result) => {
            let status = if result.success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(WebhookResponse::from(result))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(WebhookError::new(format!("Handler error: {}", e), "HANDLER_ERROR")),
        )
            .into_response(),
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
    Extension(config): Extension<Arc<WebhookConfig>>,
    headers: HeaderMap,
    Json(payload): Json<GenericWebhookPayload>,
) -> impl IntoResponse {
    // With a secret configured `Authorization: Bearer <secret>` is mandatory.
    if let Some(ref secret) = config.generic_secret {
        let Some(auth) = headers.get("Authorization") else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(WebhookError::new("Missing authorization", "MISSING_AUTH")),
            )
                .into_response();
        };
        let presented = auth.to_str().unwrap_or("").strip_prefix("Bearer ");
        if !presented.is_some_and(|token| ct_eq_secret(token, secret)) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(WebhookError::new("Invalid authorization", "INVALID_AUTH")),
            )
                .into_response();
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
            )
                .into_response();
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
    let result = handle_event_with_storage(&state, &config, &event);
    match result {
        Ok(result) => {
            let status = if result.success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(WebhookResponse::from(result))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(WebhookError::new(format!("Handler error: {}", e), "HANDLER_ERROR")),
        )
            .into_response(),
    }
}

/// Handle webhook event with storage integration if available
///
/// If the database has branching enabled, uses StorageWebhookHandler
/// to actually create/merge/drop branches. Otherwise falls back to
/// the basic WebhookHandler that just logs events.
fn handle_event_with_storage(
    state: &AppState,
    config: &WebhookConfig,
    event: &WebhookEvent,
) -> Result<WebhookResult, crate::Error> {
    let config = config.clone();

    // Try to use storage-aware handler if branching is enabled
    if let Some(branch_manager) = state.db.storage.branch_manager() {
        // B1 second fix pass: PR merges go through the engine's MERGE (see
        // `StorageWebhookHandler::with_engine`).
        let storage_handler =
            StorageWebhookHandler::new(config, branch_manager.as_ref()).with_engine(&state.db.storage);
        storage_handler.handle(event)
    } else {
        // Fall back to basic handler (just logging)
        let basic_handler = WebhookHandler::new(config);
        basic_handler.handle(event)
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
