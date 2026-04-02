//! HTTP handlers for the BaaS authentication endpoints.
//!
//! These are thin axum handlers that delegate to [`AuthBridge`] for all logic.
//! They follow the same `State(state) + Json(body) -> Result<Json<_>, ApiError>`
//! pattern used by `query_handler.rs`.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use tracing::{info, warn};

use crate::api::{
    auth_bridge::{
        AuthError, AuthSession, AuthUser, LogoutRequest, RefreshRequest, SignInRequest,
        SignUpRequest,
    },
    models::ApiError,
    server::AppState,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert an `AuthError` into an `ApiError` with an appropriate HTTP status.
fn map_auth_err(e: AuthError) -> ApiError {
    let status = match e.error.as_str() {
        "validation_failed" | "weak_password" => StatusCode::BAD_REQUEST,
        "user_already_exists" => StatusCode::CONFLICT,
        "invalid_credentials" | "invalid_token" | "token_expired" | "token_error"
        | "invalid_signature" | "token_revoked" => StatusCode::UNAUTHORIZED,
        "invalid_refresh_token" => StatusCode::UNAUTHORIZED,
        "user_not_found" => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError::new(status, &e.error, e.error_description.as_deref().unwrap_or(&e.error))
}

/// Extract the bearer token from `Authorization: Bearer <token>`.
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /auth/signup`
///
/// Create a new user account and return a fresh session.
pub async fn signup(
    State(state): State<AppState>,
    Json(body): Json<SignUpRequest>,
) -> Result<Json<AuthSession>, ApiError> {
    info!("Auth signup request for email: {}", body.email);

    let auth = state
        .auth_bridge
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "auth_not_enabled", "Auth service is not configured"))?;

    let session = auth.sign_up(&body.email, &body.password).map_err(|e| {
        warn!("Signup failed for {}: {}", body.email, e);
        map_auth_err(e)
    })?;

    info!("User signed up: {}", session.user.id);
    Ok(Json(session))
}

/// `POST /auth/signin`
///
/// Authenticate with email + password and return a session.
pub async fn signin(
    State(state): State<AppState>,
    Json(body): Json<SignInRequest>,
) -> Result<Json<AuthSession>, ApiError> {
    info!("Auth signin request for email: {}", body.email);

    let auth = state
        .auth_bridge
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "auth_not_enabled", "Auth service is not configured"))?;

    let session = auth.sign_in(&body.email, &body.password).map_err(|e| {
        warn!("Signin failed for {}: {}", body.email, e);
        map_auth_err(e)
    })?;

    info!("User signed in: {}", session.user.id);
    Ok(Json(session))
}

/// `POST /auth/logout`
///
/// Revoke the given refresh token.
pub async fn logout(
    State(state): State<AppState>,
    Json(body): Json<LogoutRequest>,
) -> Result<StatusCode, ApiError> {
    let auth = state
        .auth_bridge
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "auth_not_enabled", "Auth service is not configured"))?;

    auth.sign_out(&body.refresh_token).map_err(|e| {
        warn!("Logout failed: {}", e);
        map_auth_err(e)
    })?;

    info!("User logged out (refresh token revoked)");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/refresh`
///
/// Exchange a refresh token for a new session.
pub async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<AuthSession>, ApiError> {
    let auth = state
        .auth_bridge
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "auth_not_enabled", "Auth service is not configured"))?;

    let session = auth.refresh(&body.refresh_token).map_err(|e| {
        warn!("Token refresh failed: {}", e);
        map_auth_err(e)
    })?;

    info!("Token refreshed for user: {}", session.user.id);
    Ok(Json(session))
}

/// `GET /auth/user`
///
/// Return the authenticated user from the `Authorization: Bearer <token>` header.
pub async fn get_user(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthUser>, ApiError> {
    let auth = state
        .auth_bridge
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "auth_not_enabled", "Auth service is not configured"))?;

    let token = extract_bearer(&headers).ok_or_else(|| {
        ApiError::unauthorized("Missing or invalid Authorization header")
    })?;

    let user = auth.get_user(&token).map_err(|e| {
        warn!("Get user failed: {}", e);
        map_auth_err(e)
    })?;

    Ok(Json(user))
}
