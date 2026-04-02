//! HTTP handlers for OAuth2 authentication (Google, GitHub).
//!
//! These handlers implement the Authorization Code + PKCE flow:
//! - `GET /auth/v1/authorize?provider=google` redirects to the provider's login page.
//! - `GET /auth/v1/callback?code=...&state=...` exchanges the code, upserts the
//!   user through [`AuthBridge::oauth_sign_in`], and returns a session.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Redirect,
    Json,
};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::api::{
    auth_bridge::AuthSession,
    models::ApiError,
    oauth::OAuthError,
    server::AppState,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map an [`OAuthError`] into an [`ApiError`] with a suitable HTTP status.
fn map_oauth_err(e: OAuthError) -> ApiError {
    match &e {
        OAuthError::ProviderNotFound(_) => {
            ApiError::bad_request(e.to_string())
        }
        OAuthError::InvalidState => {
            ApiError::new(StatusCode::UNAUTHORIZED, "invalid_state", e.to_string())
        }
        OAuthError::TokenExchange(_) => {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "token_exchange_failed",
                e.to_string(),
            )
        }
        OAuthError::UserInfoFetch(_) => {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "userinfo_fetch_failed",
                e.to_string(),
            )
        }
        OAuthError::ConfigError(_) => {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "oauth_config_error",
                e.to_string(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /auth/v1/authorize?provider=google`
///
/// Builds the OAuth authorization URL for the requested provider and returns
/// a 307 redirect so the user's browser navigates to the provider's login page.
pub async fn authorize(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Redirect, ApiError> {
    let provider = params.get("provider").ok_or_else(|| {
        ApiError::bad_request("Missing required query parameter: provider")
    })?;

    info!("OAuth authorize request for provider: {provider}");

    let registry = state
        .oauth_registry
        .as_ref()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "oauth_not_configured",
                "OAuth is not configured on this server",
            )
        })?;

    let (url, oauth_state) = registry.get_authorize_url(provider).map_err(|e| {
        warn!("OAuth authorize failed for {provider}: {e}");
        map_oauth_err(e)
    })?;

    info!("OAuth redirecting to provider (state={oauth_state})");
    Ok(Redirect::temporary(&url))
}

/// `GET /auth/v1/callback?code=...&state=...`
///
/// Called by the provider after the user authorizes. Exchanges the code for
/// tokens, fetches user info, upserts the user in the database, and returns
/// a full [`AuthSession`] (access token + refresh token).
pub async fn callback(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<AuthSession>, ApiError> {
    let code = params.get("code").ok_or_else(|| {
        ApiError::bad_request("Missing required query parameter: code")
    })?;
    let state_param = params.get("state").ok_or_else(|| {
        ApiError::bad_request("Missing required query parameter: state")
    })?;

    info!("OAuth callback received (state={state_param})");

    let registry = state
        .oauth_registry
        .as_ref()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "oauth_not_configured",
                "OAuth is not configured on this server",
            )
        })?;

    // Exchange code for user info (async - hits the provider's token + userinfo endpoints)
    let user_info = registry.exchange_code(code, state_param).await.map_err(|e| {
        warn!("OAuth code exchange failed: {e}");
        map_oauth_err(e)
    })?;

    info!(
        "OAuth user info received: email={}, provider={}",
        user_info.email, user_info.provider
    );

    // Upsert user through AuthBridge
    let auth = state
        .auth_bridge
        .as_ref()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_not_enabled",
                "Auth service is not configured",
            )
        })?;

    let session = auth.oauth_sign_in(&user_info).map_err(|e| {
        warn!("OAuth sign-in failed for {}: {e}", user_info.email);
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.error,
            e.error_description.as_deref().unwrap_or(&e.error),
        )
    })?;

    info!("OAuth sign-in successful: user_id={}", session.user.id);
    Ok(Json(session))
}
