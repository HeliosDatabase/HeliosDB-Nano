//! Authentication models for REST API
//!
//! Data transfer objects for authentication and authorization.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// API Key authentication request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyAuth {
    /// API key
    pub api_key: String,

    /// Optional description
    pub description: Option<String>,
}

impl ApiKeyAuth {
    /// Create a new API key authentication request
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            description: None,
        }
    }

    /// Create with description
    pub fn with_description(api_key: String, description: String) -> Self {
        Self {
            api_key,
            description: Some(description),
        }
    }
}

/// JWT authentication request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtAuth {
    /// JWT Bearer token
    pub token: String,

    /// Token type (should be "Bearer")
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl JwtAuth {
    /// Create a new JWT authentication request
    pub fn new(token: String) -> Self {
        Self {
            token,
            token_type: "Bearer".to_string(),
        }
    }

    /// Get the authorization header value
    pub fn authorization_header(&self) -> String {
        format!("{} {}", self.token_type, self.token)
    }
}

/// User context response (safe to expose to client)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserContextResponse {
    /// User ID
    pub user_id: String,

    /// Tenant ID
    pub tenant_id: String,

    /// Client ID
    pub client_id: Uuid,

    /// Authentication method
    pub auth_method: String,

    /// Scopes/permissions
    pub scopes: Vec<String>,
}

impl UserContextResponse {
    /// Create from middleware UserContext
    pub fn from_user_context(ctx: &crate::api::middleware::UserContext) -> Self {
        Self {
            user_id: ctx.user_id.clone(),
            tenant_id: ctx.tenant_id.clone(),
            client_id: ctx.client_id,
            auth_method: format!("{:?}", ctx.auth_method),
            scopes: ctx.scopes.clone(),
        }
    }
}

/// Rate limit information response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfoResponse {
    /// Maximum requests allowed in window
    pub limit: u32,

    /// Remaining requests in current window
    pub remaining: u32,

    /// Time when the rate limit resets (Unix timestamp)
    pub reset_at: u64,

    /// Retry after seconds (only present when rate limited)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

impl From<crate::api::middleware::rate_limit::RateLimitInfo> for RateLimitInfoResponse {
    fn from(info: crate::api::middleware::rate_limit::RateLimitInfo) -> Self {
        Self {
            limit: info.limit,
            remaining: info.remaining,
            reset_at: info.reset_at,
            retry_after: info.retry_after,
        }
    }
}

/// Login request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    /// Username or email
    pub username: String,

    /// Password
    pub password: String,

    /// Optional tenant ID
    pub tenant_id: Option<String>,
}

/// Login response with token pair
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    /// Access token
    pub access_token: String,

    /// Refresh token
    pub refresh_token: String,

    /// Token type (Bearer)
    pub token_type: String,

    /// Expires in seconds
    pub expires_in: u64,

    /// User context
    pub user: UserContextResponse,
}

/// Refresh token request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenRequest {
    /// Refresh token
    pub refresh_token: String,
}

/// Refresh token response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenResponse {
    /// New access token
    pub access_token: String,

    /// Token type (Bearer)
    pub token_type: String,

    /// Expires in seconds
    pub expires_in: u64,
}

/// API key creation request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyRequest {
    /// Key name/description
    pub name: String,

    /// Scopes to grant
    pub scopes: Vec<String>,

    /// Expiration in days (optional)
    pub expires_in_days: Option<u32>,
}

/// API key creation response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    /// Generated API key (only shown once)
    pub api_key: String,

    /// Key name
    pub name: String,

    /// Key ID
    pub key_id: Uuid,

    /// Scopes granted
    pub scopes: Vec<String>,

    /// Creation timestamp
    pub created_at: i64,

    /// Expiration timestamp (if set)
    pub expires_at: Option<i64>,
}

/// API key list item (without the actual key)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyListItem {
    /// Key ID
    pub key_id: Uuid,

    /// Key name
    pub name: String,

    /// Scopes granted
    pub scopes: Vec<String>,

    /// Last used timestamp
    pub last_used_at: Option<i64>,

    /// Creation timestamp
    pub created_at: i64,

    /// Expiration timestamp (if set)
    pub expires_at: Option<i64>,

    /// Whether the key is active
    pub is_active: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_api_key_auth() {
        let auth = ApiKeyAuth::new("test-key-123".to_string());
        assert_eq!(auth.api_key, "test-key-123");
        assert!(auth.description.is_none());

        let auth_with_desc = ApiKeyAuth::with_description(
            "test-key-456".to_string(),
            "Test key".to_string(),
        );
        assert_eq!(auth_with_desc.api_key, "test-key-456");
        assert_eq!(auth_with_desc.description, Some("Test key".to_string()));
    }

    #[test]
    fn test_jwt_auth() {
        let auth = JwtAuth::new("eyJhbGc...".to_string());
        assert_eq!(auth.token, "eyJhbGc...");
        assert_eq!(auth.token_type, "Bearer");

        let header = auth.authorization_header();
        assert_eq!(header, "Bearer eyJhbGc...");
    }

    #[test]
    fn test_jwt_auth_deserialization() {
        let json = r#"{"token": "abc123"}"#;
        let auth: JwtAuth = serde_json::from_str(json).unwrap();
        assert_eq!(auth.token, "abc123");
        assert_eq!(auth.token_type, "Bearer");
    }

    #[test]
    fn test_user_context_response() {
        let client_id = Uuid::new_v4();
        let ctx = crate::api::middleware::UserContext {
            user_id: "user123".to_string(),
            tenant_id: "tenant456".to_string(),
            client_id,
            auth_method: crate::api::middleware::AuthMethod::JwtBearer,
            scopes: vec!["read".to_string(), "write".to_string()],
        };

        let response = UserContextResponse::from_user_context(&ctx);
        assert_eq!(response.user_id, "user123");
        assert_eq!(response.tenant_id, "tenant456");
        assert_eq!(response.client_id, client_id);
        assert_eq!(response.scopes.len(), 2);
    }

    #[test]
    fn test_login_request() {
        let request = LoginRequest {
            username: "user@example.com".to_string(),
            password: "secret123".to_string(),
            tenant_id: Some("tenant1".to_string()),
        };

        assert_eq!(request.username, "user@example.com");
        assert_eq!(request.password, "secret123");
        assert_eq!(request.tenant_id, Some("tenant1".to_string()));
    }

    #[test]
    fn test_login_response() {
        let user_ctx = UserContextResponse {
            user_id: "user123".to_string(),
            tenant_id: "tenant456".to_string(),
            client_id: Uuid::new_v4(),
            auth_method: "JwtBearer".to_string(),
            scopes: vec!["read".to_string()],
        };

        let response = LoginResponse {
            access_token: "access123".to_string(),
            refresh_token: "refresh456".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            user: user_ctx,
        };

        assert_eq!(response.access_token, "access123");
        assert_eq!(response.refresh_token, "refresh456");
        assert_eq!(response.expires_in, 3600);
    }

    #[test]
    fn test_create_api_key_request() {
        let request = CreateApiKeyRequest {
            name: "Production API Key".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
            expires_in_days: Some(90),
        };

        assert_eq!(request.name, "Production API Key");
        assert_eq!(request.scopes.len(), 2);
        assert_eq!(request.expires_in_days, Some(90));
    }

    #[test]
    fn test_api_key_list_item() {
        let item = ApiKeyListItem {
            key_id: Uuid::new_v4(),
            name: "Test Key".to_string(),
            scopes: vec!["read".to_string()],
            last_used_at: Some(1234567890),
            created_at: 1234567890,
            expires_at: None,
            is_active: true,
        };

        assert_eq!(item.name, "Test Key");
        assert!(item.is_active);
        assert!(item.expires_at.is_none());
    }

    #[test]
    fn test_refresh_token_request() {
        let request = RefreshTokenRequest {
            refresh_token: "refresh_token_123".to_string(),
        };

        assert_eq!(request.refresh_token, "refresh_token_123");
    }

    #[test]
    fn test_refresh_token_response() {
        let response = RefreshTokenResponse {
            access_token: "new_access_token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
        };

        assert_eq!(response.access_token, "new_access_token");
        assert_eq!(response.token_type, "Bearer");
        assert_eq!(response.expires_in, 3600);
    }
}
