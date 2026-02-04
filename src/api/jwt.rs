//! JWT Authentication for REST API
//!
//! Provides secure authentication using JSON Web Tokens (JWT).

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Error;

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
            iss: "heliosdb-api".to_string(),
            aud: "heliosdb-client".to_string(),
            scopes: vec!["api:read".to_string(), "api:write".to_string()],
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
}

/// JWT Token Manager
pub struct JwtManager {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    validation: Validation,
    default_expiry: Duration,
}

impl JwtManager {
    /// Create a new JWT manager with a secret key
    pub fn new(secret: &[u8]) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&["heliosdb-api"]);
        validation.set_audience(&["heliosdb-client"]);
        validation.validate_exp = true;
        validation.validate_nbf = true;

        Self {
            encoding_key: EncodingKey::from_secret(secret),
            decoding_key: DecodingKey::from_secret(secret),
            validation,
            default_expiry: Duration::hours(1),
        }
    }

    /// Generate an access token
    pub fn generate_token(
        &self,
        user_id: String,
        tenant_id: String,
        client_id: Uuid,
    ) -> Result<String, Error> {
        let claims = Claims::new(user_id, tenant_id, client_id, self.default_expiry);
        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| Error::Generic(format!("JWT encode error: {}", e)))
    }

    /// Validate and decode a token
    pub fn validate_token(&self, token: &str) -> Result<Claims, Error> {
        let token_data = decode::<Claims>(token, &self.decoding_key, &self.validation)
            .map_err(|e| Error::Generic(format!("JWT decode error: {}", e)))?;
        Ok(token_data.claims)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_jwt_roundtrip() {
        let manager = JwtManager::new(b"test-secret-key");
        let token = manager
            .generate_token(
                "user123".to_string(),
                "tenant456".to_string(),
                Uuid::new_v4(),
            )
            .unwrap();

        let claims = manager.validate_token(&token).unwrap();
        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.tenant_id, "tenant456");
    }
}
