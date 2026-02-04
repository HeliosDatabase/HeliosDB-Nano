//! Authentication middleware for REST API
//!
//! Supports JWT Bearer token authentication and API key authentication via X-API-Key header.

use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::api::jwt::{Claims, JwtManager};
use crate::api::models::ApiError;

/// User context extracted from authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserContext {
    /// User ID
    pub user_id: String,

    /// Tenant ID for multi-tenancy
    pub tenant_id: String,

    /// Client ID
    pub client_id: Uuid,

    /// Authentication method used
    pub auth_method: AuthMethod,

    /// Scopes/permissions
    pub scopes: Vec<String>,
}

impl UserContext {
    /// Create from JWT claims
    pub fn from_claims(claims: Claims, auth_method: AuthMethod) -> Self {
        Self {
            user_id: claims.sub,
            tenant_id: claims.tenant_id,
            client_id: claims.client_id,
            auth_method,
            scopes: claims.scopes,
        }
    }

    /// Check if user has a specific scope
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Check if user has any of the given scopes
    pub fn has_any_scope(&self, scopes: &[&str]) -> bool {
        scopes.iter().any(|scope| self.has_scope(scope))
    }

    /// Check if user has all of the given scopes
    pub fn has_all_scopes(&self, scopes: &[&str]) -> bool {
        scopes.iter().all(|scope| self.has_scope(scope))
    }
}

/// Authentication method
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuthMethod {
    /// JWT Bearer token
    JwtBearer,
    /// API key
    ApiKey,
}

/// Authentication middleware state
#[derive(Clone)]
pub struct AuthMiddleware {
    jwt_manager: Arc<JwtManager>,
    api_keys: Arc<std::collections::HashMap<String, ApiKeyInfo>>,
}

/// API Key information
#[derive(Debug, Clone)]
struct ApiKeyInfo {
    user_id: String,
    tenant_id: String,
    client_id: Uuid,
    scopes: Vec<String>,
    name: String,
}

impl AuthMiddleware {
    /// Create new authentication middleware
    pub fn new(jwt_secret: &[u8]) -> Self {
        Self {
            jwt_manager: Arc::new(JwtManager::new(jwt_secret)),
            api_keys: Arc::new(std::collections::HashMap::new()),
        }
    }

    /// Create from environment variable or default
    pub fn from_env_or_default() -> Self {
        let secret = std::env::var("HELIOSDB_JWT_SECRET")
            .unwrap_or_else(|_| "default-secret-change-in-production".to_string());
        Self::new(secret.as_bytes())
    }

    /// Add an API key for authentication
    pub fn with_api_key(
        mut self,
        key: String,
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
        scopes: Vec<String>,
        name: String,
    ) -> Self {
        let mut keys = (*self.api_keys).clone();
        keys.insert(key, ApiKeyInfo {
            user_id,
            tenant_id,
            client_id,
            scopes,
            name,
        });
        self.api_keys = Arc::new(keys);
        self
    }

    /// Extract and validate authentication from request
    pub async fn authenticate(&self, req: &Request) -> Result<UserContext, ApiError> {
        // Try JWT Bearer token first
        if let Some(auth_header) = req.headers().get(header::AUTHORIZATION) {
            if let Ok(auth_str) = auth_header.to_str() {
                if let Some(token) = auth_str.strip_prefix("Bearer ") {
                    return self.authenticate_jwt(token).await;
                }
            }
        }

        // Try API key
        if let Some(api_key_header) = req.headers().get("x-api-key") {
            if let Ok(api_key) = api_key_header.to_str() {
                return self.authenticate_api_key(api_key).await;
            }
        }

        Err(ApiError::unauthorized("Missing or invalid authentication credentials"))
    }

    /// Authenticate using JWT token
    pub async fn authenticate_jwt(&self, token: &str) -> Result<UserContext, ApiError> {
        let claims = self.jwt_manager
            .validate_token(token)
            .map_err(|_| ApiError::unauthorized("Invalid or expired JWT token"))?;

        Ok(UserContext::from_claims(claims, AuthMethod::JwtBearer))
    }

    /// Authenticate using API key
    pub async fn authenticate_api_key(&self, api_key: &str) -> Result<UserContext, ApiError> {
        let key_info = self.api_keys
            .get(api_key)
            .ok_or_else(|| ApiError::unauthorized("Invalid API key"))?;

        Ok(UserContext {
            user_id: key_info.user_id.clone(),
            tenant_id: key_info.tenant_id.clone(),
            client_id: key_info.client_id,
            auth_method: AuthMethod::ApiKey,
            scopes: key_info.scopes.clone(),
        })
    }
}

/// Authentication middleware handler
pub async fn auth_middleware(
    State(auth): State<Arc<AuthMiddleware>>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // Extract and validate authentication
    let user_ctx = auth.authenticate(&req).await?;

    // Attach user context to request extensions
    req.extensions_mut().insert(user_ctx);

    // Continue to next middleware/handler
    Ok(next.run(req).await)
}

/// Extract user context from request extensions
///
/// This is an extractor that can be used in handlers to get the authenticated user context.
#[derive(Debug, Clone)]
pub struct AuthUser(pub UserContext);

#[axum::async_trait]
impl<S> axum::extract::FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<UserContext>()
            .cloned()
            .map(AuthUser)
            .ok_or_else(|| ApiError::unauthorized("User context not found. Authentication middleware may not be configured."))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_user_context_from_claims() {
        let claims = Claims::new(
            "user123".to_string(),
            "tenant456".to_string(),
            Uuid::new_v4(),
            chrono::Duration::hours(1),
        );

        let ctx = UserContext::from_claims(claims.clone(), AuthMethod::JwtBearer);

        assert_eq!(ctx.user_id, "user123");
        assert_eq!(ctx.tenant_id, "tenant456");
        assert_eq!(ctx.auth_method, AuthMethod::JwtBearer);
        assert!(ctx.has_scope("api:read"));
        assert!(ctx.has_scope("api:write"));
    }

    #[test]
    fn test_user_context_scope_checks() {
        let mut claims = Claims::new(
            "user123".to_string(),
            "tenant456".to_string(),
            Uuid::new_v4(),
            chrono::Duration::hours(1),
        );
        claims.scopes = vec!["read".to_string(), "write".to_string()];

        let ctx = UserContext::from_claims(claims, AuthMethod::JwtBearer);

        assert!(ctx.has_scope("read"));
        assert!(ctx.has_scope("write"));
        assert!(!ctx.has_scope("admin"));

        assert!(ctx.has_any_scope(&["read", "admin"]));
        assert!(!ctx.has_any_scope(&["admin", "delete"]));

        assert!(ctx.has_all_scopes(&["read", "write"]));
        assert!(!ctx.has_all_scopes(&["read", "write", "admin"]));
    }

    #[tokio::test]
    async fn test_auth_middleware_jwt() {
        let jwt_manager = JwtManager::new(b"test-secret");
        let client_id = Uuid::new_v4();

        let token = jwt_manager
            .generate_token(
                "user123".to_string(),
                "tenant456".to_string(),
                client_id,
            )
            .unwrap();

        let auth = AuthMiddleware::new(b"test-secret");

        // Create mock request with JWT token
        let req = Request::builder()
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .body(axum::body::Body::empty())
            .unwrap();

        let result = auth.authenticate(&req).await;
        assert!(result.is_ok());

        let ctx = result.unwrap();
        assert_eq!(ctx.user_id, "user123");
        assert_eq!(ctx.tenant_id, "tenant456");
        assert_eq!(ctx.auth_method, AuthMethod::JwtBearer);
    }

    #[tokio::test]
    async fn test_auth_middleware_api_key() {
        let client_id = Uuid::new_v4();
        let auth = AuthMiddleware::new(b"test-secret")
            .with_api_key(
                "test-key-123".to_string(),
                "user123".to_string(),
                "tenant456".to_string(),
                client_id,
                vec!["read".to_string(), "write".to_string()],
                "Test Key".to_string(),
            );

        // Create mock request with API key
        let req = Request::builder()
            .header("x-api-key", "test-key-123")
            .body(axum::body::Body::empty())
            .unwrap();

        let result = auth.authenticate(&req).await;
        assert!(result.is_ok());

        let ctx = result.unwrap();
        assert_eq!(ctx.user_id, "user123");
        assert_eq!(ctx.tenant_id, "tenant456");
        assert_eq!(ctx.auth_method, AuthMethod::ApiKey);
        assert!(ctx.has_scope("read"));
    }

    #[tokio::test]
    async fn test_auth_middleware_missing_credentials() {
        let auth = AuthMiddleware::new(b"test-secret");

        // Create mock request without auth
        let req = Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();

        let result = auth.authenticate(&req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_auth_middleware_invalid_jwt() {
        let auth = AuthMiddleware::new(b"test-secret");

        // Create mock request with invalid JWT
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer invalid.token.here")
            .body(axum::body::Body::empty())
            .unwrap();

        let result = auth.authenticate(&req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_auth_middleware_invalid_api_key() {
        let auth = AuthMiddleware::new(b"test-secret");

        // Create mock request with invalid API key
        let req = Request::builder()
            .header("x-api-key", "invalid-key")
            .body(axum::body::Body::empty())
            .unwrap();

        let result = auth.authenticate(&req).await;
        assert!(result.is_err());
    }
}
