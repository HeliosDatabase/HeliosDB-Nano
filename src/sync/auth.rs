//! JWT Authentication for Sync Protocol
//!
//! Provides secure authentication and authorization for sync operations
//! using JSON Web Tokens (JWT).

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::SyncError;

/// JWT Claims structure
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    /// Subject (user ID)
    pub sub: String,

    /// Tenant ID for multi-tenancy support
    pub tenant_id: String,

    /// Client ID
    pub client_id: Uuid,

    /// Expiration timestamp (Unix timestamp)
    pub exp: u64,

    /// Issued at timestamp (Unix timestamp)
    pub iat: u64,

    /// Not before timestamp (Unix timestamp)
    pub nbf: u64,

    /// JWT ID (unique identifier for this token)
    pub jti: String,

    /// Issuer
    pub iss: String,

    /// Audience
    pub aud: String,

    /// Scopes/Permissions
    pub scopes: Vec<String>,
}

impl Claims {
    /// Create new claims with default values
    pub fn new(
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
        expires_in: Duration,
    ) -> Self {
        let now = Utc::now();
        let exp = (now + expires_in).timestamp() as u64;
        let iat = now.timestamp() as u64;
        let nbf = iat;

        Self {
            sub: user_id,
            tenant_id,
            client_id,
            exp,
            iat,
            nbf,
            jti: Uuid::new_v4().to_string(),
            iss: "heliosdb-sync".to_string(),
            aud: "heliosdb-client".to_string(),
            scopes: vec!["sync:read".to_string(), "sync:write".to_string()],
        }
    }

    /// Check if token is expired
    pub fn is_expired(&self) -> bool {
        let now = Utc::now().timestamp() as u64;
        self.exp < now
    }

    /// Check if token is active (not before time has passed)
    pub fn is_active(&self) -> bool {
        let now = Utc::now().timestamp() as u64;
        self.nbf <= now
    }

    /// Check if token has a specific scope
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Validate all time-based constraints
    pub fn validate_time(&self) -> Result<(), SyncError> {
        if !self.is_active() {
            return Err(SyncError::Authentication);
        }
        if self.is_expired() {
            return Err(SyncError::Authentication);
        }
        Ok(())
    }
}

/// JWT Token Manager
pub struct JwtManager {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    validation: Validation,
    default_expiry: Duration,
    refresh_expiry: Duration,
}

impl JwtManager {
    /// Create a new JWT manager with a secret key
    pub fn new(secret: &[u8]) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&["heliosdb-sync"]);
        validation.set_audience(&["heliosdb-client"]);
        validation.validate_exp = true;
        validation.validate_nbf = true;

        Self {
            encoding_key: EncodingKey::from_secret(secret),
            decoding_key: DecodingKey::from_secret(secret),
            validation,
            default_expiry: Duration::hours(1),
            refresh_expiry: Duration::days(7),
        }
    }

    /// Create a JWT manager from an environment variable or config
    pub fn from_env_or_default() -> Self {
        let secret = std::env::var("HELIOSDB_JWT_SECRET")
            .unwrap_or_else(|_| "default-secret-change-in-production".to_string());
        Self::new(secret.as_bytes())
    }

    /// Set custom expiry durations
    pub fn with_expiry(mut self, default: Duration, refresh: Duration) -> Self {
        self.default_expiry = default;
        self.refresh_expiry = refresh;
        self
    }

    /// Generate an access token
    pub fn generate_token(
        &self,
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
    ) -> Result<String, SyncError> {
        let claims = Claims::new(user_id, tenant_id, client_id, self.default_expiry);

        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| SyncError::Authentication)
    }

    /// Generate a refresh token with longer expiry
    pub fn generate_refresh_token(
        &self,
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
    ) -> Result<String, SyncError> {
        let mut claims = Claims::new(user_id, tenant_id, client_id, self.refresh_expiry);
        claims.scopes = vec!["refresh".to_string()];

        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| SyncError::Authentication)
    }

    /// Validate and decode a token
    pub fn validate_token(&self, token: &str) -> Result<Claims, SyncError> {
        let token_data = decode::<Claims>(token, &self.decoding_key, &self.validation)
            .map_err(|e| {
                tracing::warn!("JWT validation failed: {}", e);
                SyncError::Authentication
            })?;

        let claims = token_data.claims;

        // Additional time validation
        claims.validate_time()?;

        Ok(claims)
    }

    /// Validate token and check specific scope
    pub fn validate_with_scope(&self, token: &str, required_scope: &str) -> Result<Claims, SyncError> {
        let claims = self.validate_token(token)?;

        if !claims.has_scope(required_scope) {
            tracing::warn!("Token missing required scope: {}", required_scope);
            return Err(SyncError::Authentication);
        }

        Ok(claims)
    }

    /// Refresh an access token using a refresh token
    pub fn refresh_access_token(&self, refresh_token: &str) -> Result<String, SyncError> {
        // Validate refresh token
        let claims = self.validate_with_scope(refresh_token, "refresh")?;

        // Generate new access token with same user/tenant/client
        self.generate_token(claims.sub, claims.tenant_id, claims.client_id)
    }
}

/// Token pair (access + refresh)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u64,
}

impl TokenPair {
    pub fn new(access_token: String, refresh_token: String, expires_in: u64) -> Self {
        Self {
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in,
        }
    }
}

/// Authorization checker for tenant-based access control
pub struct Authorizer {
    allowed_tenants: Vec<String>,
}

impl Authorizer {
    /// Create a new authorizer
    pub fn new() -> Self {
        Self {
            allowed_tenants: vec![],
        }
    }

    /// Create authorizer with specific allowed tenants
    pub fn with_tenants(tenants: Vec<String>) -> Self {
        Self {
            allowed_tenants: tenants,
        }
    }

    /// Check if a tenant is authorized
    pub fn is_authorized(&self, tenant_id: &str) -> bool {
        // If no tenants configured, allow all (for backward compatibility)
        if self.allowed_tenants.is_empty() {
            return true;
        }

        self.allowed_tenants.iter().any(|t| t == tenant_id)
    }

    /// Add an allowed tenant
    pub fn add_tenant(&mut self, tenant_id: String) {
        if !self.allowed_tenants.contains(&tenant_id) {
            self.allowed_tenants.push(tenant_id);
        }
    }

    /// Remove an allowed tenant
    pub fn remove_tenant(&mut self, tenant_id: &str) -> bool {
        if let Some(pos) = self.allowed_tenants.iter().position(|t| t == tenant_id) {
            self.allowed_tenants.remove(pos);
            true
        } else {
            false
        }
    }

    /// Validate claims against authorization rules
    pub fn validate_claims(&self, claims: &Claims) -> Result<(), SyncError> {
        if !self.is_authorized(&claims.tenant_id) {
            tracing::warn!("Unauthorized tenant: {}", claims.tenant_id);
            return Err(SyncError::Authentication);
        }

        Ok(())
    }
}

impl Default for Authorizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_claims_creation() {
        let claims = Claims::new(
            "user123".to_string(),
            "tenant456".to_string(),
            Uuid::new_v4(),
            Duration::hours(1),
        );

        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.tenant_id, "tenant456");
        assert_eq!(claims.iss, "heliosdb-sync");
        assert!(claims.has_scope("sync:read"));
        assert!(claims.has_scope("sync:write"));
    }

    #[test]
    fn test_claims_expiry() {
        let mut claims = Claims::new(
            "user123".to_string(),
            "tenant456".to_string(),
            Uuid::new_v4(),
            Duration::hours(1),
        );

        assert!(!claims.is_expired());
        assert!(claims.is_active());

        // Simulate expired token
        claims.exp = (Utc::now() - Duration::hours(2)).timestamp() as u64;
        assert!(claims.is_expired());
    }

    #[test]
    fn test_jwt_manager_generation_and_validation() {
        let manager = JwtManager::new(b"test-secret-key-for-testing");
        let client_id = Uuid::new_v4();

        // Generate token
        let token = manager
            .generate_token(
                "user123".to_string(),
                "tenant456".to_string(),
                client_id,
            )
            .unwrap();

        // Validate token
        let claims = manager.validate_token(&token).unwrap();
        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.tenant_id, "tenant456");
        assert_eq!(claims.client_id, client_id);
    }

    #[test]
    fn test_jwt_manager_invalid_token() {
        let manager = JwtManager::new(b"test-secret-key-for-testing");

        // Try to validate invalid token
        let result = manager.validate_token("invalid.token.here");
        assert!(result.is_err());
    }

    #[test]
    fn test_jwt_manager_wrong_secret() {
        let manager1 = JwtManager::new(b"secret1");
        let manager2 = JwtManager::new(b"secret2");

        let token = manager1
            .generate_token(
                "user123".to_string(),
                "tenant456".to_string(),
                Uuid::new_v4(),
            )
            .unwrap();

        // Should fail with different secret
        let result = manager2.validate_token(&token);
        assert!(result.is_err());
    }

    #[test]
    fn test_refresh_token_flow() {
        let manager = JwtManager::new(b"test-secret-key-for-testing");
        let client_id = Uuid::new_v4();

        // Generate refresh token
        let refresh_token = manager
            .generate_refresh_token(
                "user123".to_string(),
                "tenant456".to_string(),
                client_id,
            )
            .unwrap();

        // Validate refresh token has correct scope
        let claims = manager.validate_token(&refresh_token).unwrap();
        assert!(claims.has_scope("refresh"));
        assert!(!claims.has_scope("sync:read"));

        // Use refresh token to get new access token
        let new_access_token = manager.refresh_access_token(&refresh_token).unwrap();

        // Validate new access token
        let new_claims = manager.validate_token(&new_access_token).unwrap();
        assert_eq!(new_claims.sub, "user123");
        assert!(new_claims.has_scope("sync:read"));
    }

    #[test]
    fn test_scope_validation() {
        let manager = JwtManager::new(b"test-secret-key-for-testing");

        let token = manager
            .generate_token(
                "user123".to_string(),
                "tenant456".to_string(),
                Uuid::new_v4(),
            )
            .unwrap();

        // Should succeed with correct scope
        let result = manager.validate_with_scope(&token, "sync:read");
        assert!(result.is_ok());

        // Should fail with incorrect scope
        let result = manager.validate_with_scope(&token, "admin:write");
        assert!(result.is_err());
    }

    #[test]
    fn test_authorizer() {
        let mut authorizer = Authorizer::new();

        // Empty authorizer allows all
        assert!(authorizer.is_authorized("any-tenant"));

        // Add specific tenants
        authorizer.add_tenant("tenant1".to_string());
        authorizer.add_tenant("tenant2".to_string());

        // Now only allowed tenants work
        assert!(authorizer.is_authorized("tenant1"));
        assert!(authorizer.is_authorized("tenant2"));
        assert!(!authorizer.is_authorized("tenant3"));

        // Remove tenant
        assert!(authorizer.remove_tenant("tenant1"));
        assert!(!authorizer.is_authorized("tenant1"));
    }

    #[test]
    fn test_authorizer_with_claims() {
        let manager = JwtManager::new(b"test-secret-key-for-testing");
        let authorizer = Authorizer::with_tenants(vec!["allowed-tenant".to_string()]);

        // Create token for allowed tenant
        let token = manager
            .generate_token(
                "user123".to_string(),
                "allowed-tenant".to_string(),
                Uuid::new_v4(),
            )
            .unwrap();

        let claims = manager.validate_token(&token).unwrap();
        assert!(authorizer.validate_claims(&claims).is_ok());

        // Create token for disallowed tenant
        let token2 = manager
            .generate_token(
                "user456".to_string(),
                "forbidden-tenant".to_string(),
                Uuid::new_v4(),
            )
            .unwrap();

        let claims2 = manager.validate_token(&token2).unwrap();
        assert!(authorizer.validate_claims(&claims2).is_err());
    }

    #[test]
    fn test_token_pair() {
        let manager = JwtManager::new(b"test-secret-key-for-testing");
        let client_id = Uuid::new_v4();

        let access_token = manager
            .generate_token(
                "user123".to_string(),
                "tenant456".to_string(),
                client_id,
            )
            .unwrap();

        let refresh_token = manager
            .generate_refresh_token(
                "user123".to_string(),
                "tenant456".to_string(),
                client_id,
            )
            .unwrap();

        let token_pair = TokenPair::new(access_token.clone(), refresh_token, 3600);

        assert_eq!(token_pair.access_token, access_token);
        assert_eq!(token_pair.token_type, "Bearer");
        assert_eq!(token_pair.expires_in, 3600);
    }

    #[test]
    fn test_claims_not_before() {
        let mut claims = Claims::new(
            "user123".to_string(),
            "tenant456".to_string(),
            Uuid::new_v4(),
            Duration::hours(1),
        );

        // Token active now
        assert!(claims.is_active());

        // Token not active yet (future nbf)
        claims.nbf = (Utc::now() + Duration::hours(1)).timestamp() as u64;
        assert!(!claims.is_active());
    }
}
