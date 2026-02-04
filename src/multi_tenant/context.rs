//! Tenant Context Management
//!
//! Request-scoped tenant context for multi-tenant operations.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use super::{Tenant, TenantManager, TenantError, TenantStatus};

thread_local! {
    static CURRENT_TENANT: RefCell<Option<TenantContext>> = RefCell::new(None);
}

/// Tenant context for current request/operation
#[derive(Debug, Clone)]
pub struct TenantContext {
    /// Tenant information
    pub tenant: Tenant,
    /// User ID (if authenticated)
    pub user_id: Option<String>,
    /// User email
    pub user_email: Option<String>,
    /// User role
    pub role: TenantRole,
    /// Request ID for tracing
    pub request_id: Option<String>,
    /// Additional context metadata
    pub metadata: HashMap<String, String>,
    /// JWT claims (if resolved from JWT)
    pub jwt_claims: Option<JwtClaims>,
}

/// JWT claims for tenant resolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject (usually user ID)
    pub sub: Option<String>,
    /// Tenant ID
    pub tenant_id: Option<String>,
    /// Issuer
    pub iss: Option<String>,
    /// Audience
    pub aud: Option<OneOrMany<String>>,
    /// Expiration time
    pub exp: Option<u64>,
    /// Issued at
    pub iat: Option<u64>,
    /// Not before
    pub nbf: Option<u64>,
    /// JWT ID
    pub jti: Option<String>,
    /// User email
    pub email: Option<String>,
    /// User role
    pub role: Option<String>,
    /// Custom claims
    #[serde(flatten)]
    pub custom: HashMap<String, serde_json::Value>,
}

/// Helper for audience claim (can be string or array)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn contains(&self, value: &T) -> bool
    where
        T: PartialEq,
    {
        match self {
            OneOrMany::One(v) => v == value,
            OneOrMany::Many(vs) => vs.contains(value),
        }
    }
}

/// Tenant user role
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantRole {
    /// Full admin access
    Admin,
    /// Read-write access
    Member,
    /// Read-only access
    Viewer,
    /// Service account
    Service,
    /// Anonymous/public access
    Anonymous,
}

impl Default for TenantRole {
    fn default() -> Self {
        Self::Anonymous
    }
}

impl From<&str> for TenantRole {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "admin" | "administrator" | "owner" => TenantRole::Admin,
            "member" | "user" | "write" => TenantRole::Member,
            "viewer" | "reader" | "read" => TenantRole::Viewer,
            "service" | "service_account" | "api" => TenantRole::Service,
            _ => TenantRole::Anonymous,
        }
    }
}

impl TenantContext {
    /// Create new tenant context
    pub fn new(tenant: Tenant, user_id: Option<String>, role: TenantRole) -> Self {
        Self {
            tenant,
            user_id,
            user_email: None,
            role,
            request_id: None,
            metadata: HashMap::new(),
            jwt_claims: None,
        }
    }

    /// Create with JWT claims
    pub fn with_jwt_claims(mut self, claims: JwtClaims) -> Self {
        self.jwt_claims = Some(claims);
        self
    }

    /// Create with email
    pub fn with_email(mut self, email: String) -> Self {
        self.user_email = Some(email);
        self
    }

    /// Create with request ID
    pub fn with_request_id(mut self, request_id: String) -> Self {
        self.request_id = Some(request_id);
        self
    }

    /// Check if user has write permission
    pub fn can_write(&self) -> bool {
        matches!(self.role, TenantRole::Admin | TenantRole::Member | TenantRole::Service)
    }

    /// Check if user has admin permission
    pub fn is_admin(&self) -> bool {
        matches!(self.role, TenantRole::Admin)
    }

    /// Get schema name for current tenant
    pub fn schema(&self) -> &str {
        &self.tenant.schema_name
    }

    /// Check if feature is enabled
    pub fn has_feature(&self, feature: &str) -> bool {
        self.tenant.quotas.features.get(feature).copied().unwrap_or(false)
    }

    /// Get a custom claim value
    pub fn get_claim(&self, key: &str) -> Option<&serde_json::Value> {
        self.jwt_claims.as_ref().and_then(|c| c.custom.get(key))
    }
}

/// Set current tenant context (thread-local)
pub fn set_tenant_context(ctx: TenantContext) {
    CURRENT_TENANT.with(|c| {
        *c.borrow_mut() = Some(ctx);
    });
}

/// Get current tenant context
pub fn get_tenant_context() -> Option<TenantContext> {
    CURRENT_TENANT.with(|c| c.borrow().clone())
}

/// Clear current tenant context
pub fn clear_tenant_context() {
    CURRENT_TENANT.with(|c| {
        *c.borrow_mut() = None;
    });
}

/// Execute code within a tenant context
pub fn with_tenant<F, R>(ctx: TenantContext, f: F) -> R
where
    F: FnOnce() -> R,
{
    set_tenant_context(ctx);
    let result = f();
    clear_tenant_context();
    result
}

/// JWT configuration for tenant resolution
#[derive(Debug, Clone)]
pub struct JwtConfig {
    /// Secret key for HS256/HS384/HS512
    pub secret: Option<String>,
    /// Public key for RS256/RS384/RS512/ES256/ES384
    pub public_key: Option<String>,
    /// Algorithm to use
    pub algorithm: Algorithm,
    /// Expected issuer
    pub issuer: Option<String>,
    /// Expected audience
    pub audience: Option<String>,
    /// Claim name for tenant ID (default: "tenant_id")
    pub tenant_id_claim: String,
    /// Claim name for user role (default: "role")
    pub role_claim: String,
    /// Allow expired tokens (for testing)
    pub allow_expired: bool,
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            secret: None,
            public_key: None,
            algorithm: Algorithm::HS256,
            issuer: None,
            audience: None,
            tenant_id_claim: "tenant_id".to_string(),
            role_claim: "role".to_string(),
            allow_expired: false,
        }
    }
}

impl JwtConfig {
    /// Create with secret for HMAC algorithms
    pub fn with_secret(secret: impl Into<String>) -> Self {
        Self {
            secret: Some(secret.into()),
            ..Default::default()
        }
    }

    /// Create with public key for RSA/EC algorithms
    pub fn with_public_key(key: impl Into<String>, algorithm: Algorithm) -> Self {
        Self {
            public_key: Some(key.into()),
            algorithm,
            ..Default::default()
        }
    }

    /// Set issuer validation
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Set audience validation
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    /// Set custom tenant ID claim name
    pub fn with_tenant_claim(mut self, claim: impl Into<String>) -> Self {
        self.tenant_id_claim = claim.into();
        self
    }

    /// Set custom role claim name
    pub fn with_role_claim(mut self, claim: impl Into<String>) -> Self {
        self.role_claim = claim.into();
        self
    }
}

/// Tenant context resolver
pub struct TenantResolver {
    manager: Arc<TenantManager>,
    jwt_config: JwtConfig,
}

impl TenantResolver {
    pub fn new(manager: Arc<TenantManager>) -> Self {
        Self {
            manager,
            jwt_config: JwtConfig::default(),
        }
    }

    /// Create with JWT configuration
    pub fn with_jwt_config(mut self, config: JwtConfig) -> Self {
        self.jwt_config = config;
        self
    }

    /// Resolve tenant from API key
    pub fn resolve_from_api_key(&self, api_key: &str) -> Result<TenantContext, TenantError> {
        // API key format: tenant_id:secret
        let parts: Vec<&str> = api_key.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(TenantError::InvalidId("Invalid API key format".to_string()));
        }

        let tenant_id = parts[0];
        let tenant = self.manager.get_tenant(tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.to_string()))?;

        if tenant.status != TenantStatus::Active && tenant.status != TenantStatus::Trial {
            return Err(TenantError::NotActive(tenant_id.to_string()));
        }

        // Update last active
        self.manager.touch(tenant_id);

        Ok(TenantContext::new(tenant, None, TenantRole::Service))
    }

    /// Resolve tenant from JWT token
    pub fn resolve_from_jwt(&self, token: &str) -> Result<TenantContext, TenantError> {
        // Parse JWT header to determine algorithm
        let header = decode_header(token)
            .map_err(|e| TenantError::InvalidId(format!("Invalid JWT header: {}", e)))?;

        // Build validation configuration
        let mut validation = Validation::new(header.alg);

        if let Some(ref issuer) = self.jwt_config.issuer {
            validation.set_issuer(&[issuer]);
        }

        if let Some(ref audience) = self.jwt_config.audience {
            validation.set_audience(&[audience]);
        }

        if self.jwt_config.allow_expired {
            validation.validate_exp = false;
        }

        // Get decoding key based on algorithm
        let decoding_key = self.get_decoding_key(&header.alg)?;

        // Decode and validate JWT
        let token_data = decode::<JwtClaims>(token, &decoding_key, &validation)
            .map_err(|e| TenantError::InvalidId(format!("JWT validation failed: {}", e)))?;

        let claims = token_data.claims;

        // Extract tenant ID from claims
        let tenant_id = claims.tenant_id.clone()
            .or_else(|| {
                claims.custom.get(&self.jwt_config.tenant_id_claim)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                // Try to extract from 'aud' claim (common pattern)
                claims.aud.as_ref().and_then(|aud| match aud {
                    OneOrMany::One(s) => Some(s.clone()),
                    OneOrMany::Many(arr) => arr.first().cloned(),
                })
            })
            .ok_or_else(|| TenantError::InvalidId("No tenant ID in JWT claims".to_string()))?;

        // Get tenant
        let tenant = self.manager.get_tenant(&tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.clone()))?;

        if tenant.status != TenantStatus::Active && tenant.status != TenantStatus::Trial {
            return Err(TenantError::NotActive(tenant_id));
        }

        // Extract user ID
        let user_id = claims.sub.clone();

        // Extract role
        let role = claims.role.as_ref()
            .map(|r| TenantRole::from(r.as_str()))
            .or_else(|| {
                claims.custom.get(&self.jwt_config.role_claim)
                    .and_then(|v| v.as_str())
                    .map(|s| TenantRole::from(s))
            })
            .unwrap_or(TenantRole::Member);

        // Update last active
        self.manager.touch(&tenant_id);

        // Build context
        let mut ctx = TenantContext::new(tenant, user_id, role)
            .with_jwt_claims(claims.clone());

        if let Some(ref email) = claims.email {
            ctx = ctx.with_email(email.clone());
        }

        Ok(ctx)
    }

    /// Get decoding key based on algorithm
    fn get_decoding_key(&self, algorithm: &Algorithm) -> Result<DecodingKey, TenantError> {
        match algorithm {
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
                let secret = self.jwt_config.secret.as_ref()
                    .ok_or_else(|| TenantError::InvalidId("JWT secret not configured".to_string()))?;
                Ok(DecodingKey::from_secret(secret.as_bytes()))
            }
            Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
                let key = self.jwt_config.public_key.as_ref()
                    .ok_or_else(|| TenantError::InvalidId("RSA public key not configured".to_string()))?;

                // Try PEM format first
                DecodingKey::from_rsa_pem(key.as_bytes())
                    .or_else(|_| {
                        // Try DER format
                        let der_bytes = base64::Engine::decode(
                            &base64::engine::general_purpose::STANDARD,
                            key
                        ).map_err(|_| TenantError::InvalidId("Invalid RSA key format".to_string()))?;
                        // from_rsa_der returns DecodingKey directly in jsonwebtoken 9.x
                        Ok(DecodingKey::from_rsa_der(&der_bytes))
                    })
            }
            Algorithm::ES256 | Algorithm::ES384 => {
                let key = self.jwt_config.public_key.as_ref()
                    .ok_or_else(|| TenantError::InvalidId("EC public key not configured".to_string()))?;

                DecodingKey::from_ec_pem(key.as_bytes())
                    .map_err(|e| TenantError::InvalidId(format!("Invalid EC key: {}", e)))
            }
            Algorithm::EdDSA => {
                let key = self.jwt_config.public_key.as_ref()
                    .ok_or_else(|| TenantError::InvalidId("EdDSA public key not configured".to_string()))?;

                DecodingKey::from_ed_pem(key.as_bytes())
                    .map_err(|e| TenantError::InvalidId(format!("Invalid EdDSA key: {}", e)))
            }
            _ => Err(TenantError::InvalidId(format!("Unsupported algorithm: {:?}", algorithm))),
        }
    }

    /// Resolve tenant from subdomain
    pub fn resolve_from_subdomain(&self, host: &str) -> Result<TenantContext, TenantError> {
        // Extract subdomain from host (e.g., "tenant1.app.example.com" -> "tenant1")
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() < 3 {
            return Err(TenantError::InvalidId("Cannot determine tenant from host".to_string()));
        }

        let tenant_id = parts[0];
        let tenant = self.manager.get_tenant(tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.to_string()))?;

        Ok(TenantContext::new(tenant, None, TenantRole::Anonymous))
    }

    /// Resolve tenant from path prefix
    pub fn resolve_from_path(&self, path: &str) -> Result<TenantContext, TenantError> {
        // Path format: /tenants/{tenant_id}/...
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if parts.len() < 2 || parts[0] != "tenants" {
            return Err(TenantError::InvalidId("Invalid path format".to_string()));
        }

        let tenant_id = parts[1];
        let tenant = self.manager.get_tenant(tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.to_string()))?;

        Ok(TenantContext::new(tenant, None, TenantRole::Anonymous))
    }

    /// Resolve tenant from request header
    pub fn resolve_from_header(&self, tenant_id: &str) -> Result<TenantContext, TenantError> {
        let tenant = self.manager.get_tenant(tenant_id)
            .ok_or_else(|| TenantError::NotFound(tenant_id.to_string()))?;

        if tenant.status != TenantStatus::Active && tenant.status != TenantStatus::Trial {
            return Err(TenantError::NotActive(tenant_id.to_string()));
        }

        self.manager.touch(tenant_id);

        Ok(TenantContext::new(tenant, None, TenantRole::Service))
    }

    /// Create a JWT token for a tenant and user
    pub fn create_jwt(&self, tenant_id: &str, user_id: &str, role: TenantRole, expires_in_secs: u64) -> Result<String, TenantError> {
        use jsonwebtoken::{encode, EncodingKey, Header};

        let secret = self.jwt_config.secret.as_ref()
            .ok_or_else(|| TenantError::InvalidId("JWT secret not configured".to_string()))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_secs();

        let claims = JwtClaims {
            sub: Some(user_id.to_string()),
            tenant_id: Some(tenant_id.to_string()),
            iss: self.jwt_config.issuer.clone(),
            aud: self.jwt_config.audience.as_ref().map(|a| OneOrMany::One(a.clone())),
            exp: Some(now + expires_in_secs),
            iat: Some(now),
            nbf: Some(now),
            jti: Some(uuid::Uuid::new_v4().to_string()),
            email: None,
            role: Some(format!("{:?}", role).to_lowercase()),
            custom: HashMap::new(),
        };

        let header = Header::new(self.jwt_config.algorithm);
        let key = EncodingKey::from_secret(secret.as_bytes());

        encode(&header, &claims, &key)
            .map_err(|e| TenantError::InvalidId(format!("Failed to create JWT: {}", e)))
    }
}

/// Middleware context extractor
pub struct TenantMiddleware {
    resolver: Arc<TenantResolver>,
    /// Resolution strategy order
    strategies: Vec<ResolutionStrategy>,
}

/// Strategy for resolving tenant
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResolutionStrategy {
    /// X-Tenant-ID header
    Header,
    /// API key in Authorization header
    ApiKey,
    /// JWT token
    Jwt,
    /// Subdomain
    Subdomain,
    /// URL path
    Path,
}

impl TenantMiddleware {
    pub fn new(resolver: Arc<TenantResolver>) -> Self {
        Self {
            resolver,
            strategies: vec![
                ResolutionStrategy::Jwt,
                ResolutionStrategy::Header,
                ResolutionStrategy::ApiKey,
                ResolutionStrategy::Path,
            ],
        }
    }

    pub fn with_strategies(mut self, strategies: Vec<ResolutionStrategy>) -> Self {
        self.strategies = strategies;
        self
    }

    /// Resolve tenant from request
    pub fn resolve(&self, request: &RequestInfo) -> Result<TenantContext, TenantError> {
        let mut last_error = None;

        for strategy in &self.strategies {
            let result = match strategy {
                ResolutionStrategy::Header => {
                    if let Some(ref tenant_id) = request.tenant_header {
                        self.resolver.resolve_from_header(tenant_id)
                    } else {
                        continue;
                    }
                }
                ResolutionStrategy::ApiKey => {
                    if let Some(ref api_key) = request.api_key {
                        self.resolver.resolve_from_api_key(api_key)
                    } else {
                        continue;
                    }
                }
                ResolutionStrategy::Jwt => {
                    if let Some(ref token) = request.jwt_token {
                        self.resolver.resolve_from_jwt(token)
                    } else {
                        continue;
                    }
                }
                ResolutionStrategy::Subdomain => {
                    if let Some(ref host) = request.host {
                        self.resolver.resolve_from_subdomain(host)
                    } else {
                        continue;
                    }
                }
                ResolutionStrategy::Path => {
                    self.resolver.resolve_from_path(&request.path)
                }
            };

            match result {
                Ok(ctx) => return Ok(ctx),
                Err(e) => last_error = Some(e),
            }
        }

        Err(last_error.unwrap_or_else(|| TenantError::NotFound("Could not resolve tenant".to_string())))
    }
}

/// Request information for tenant resolution
#[derive(Debug, Clone, Default)]
pub struct RequestInfo {
    pub path: String,
    pub host: Option<String>,
    pub tenant_header: Option<String>,
    pub api_key: Option<String>,
    pub jwt_token: Option<String>,
}

impl RequestInfo {
    pub fn from_headers(headers: &HashMap<String, String>, path: &str) -> Self {
        Self {
            path: path.to_string(),
            host: headers.get("Host").cloned().or_else(|| headers.get("host").cloned()),
            tenant_header: headers.get("X-Tenant-ID").cloned().or_else(|| headers.get("x-tenant-id").cloned()),
            api_key: headers.get("X-API-Key").cloned()
                .or_else(|| headers.get("x-api-key").cloned()),
            jwt_token: Self::extract_jwt_token(headers),
        }
    }

    /// Extract JWT token from headers
    fn extract_jwt_token(headers: &HashMap<String, String>) -> Option<String> {
        headers.get("Authorization")
            .or_else(|| headers.get("authorization"))
            .and_then(|h| {
                if let Some(token) = h.strip_prefix("Bearer ") {
                    // Check if it looks like a JWT (has 2 dots)
                    if token.matches('.').count() == 2 {
                        return Some(token.to_string());
                    }
                }
                None
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_tenant::{TenantManager, TenantPlan};

    #[test]
    fn test_tenant_context() {
        let manager = TenantManager::new("test");
        let tenant = manager.create_tenant("t1", "Test", TenantPlan::Pro).unwrap();

        let ctx = TenantContext::new(tenant, Some("user1".to_string()), TenantRole::Admin);

        assert!(ctx.can_write());
        assert!(ctx.is_admin());
    }

    #[test]
    fn test_thread_local_context() {
        let manager = TenantManager::new("test");
        let tenant = manager.create_tenant("t1", "Test", TenantPlan::Free).unwrap();
        let ctx = TenantContext::new(tenant, None, TenantRole::Member);

        let result = with_tenant(ctx.clone(), || {
            let current = get_tenant_context().unwrap();
            current.tenant.id.clone()
        });

        assert_eq!(result, "t1");
        assert!(get_tenant_context().is_none());
    }

    #[test]
    fn test_jwt_resolution() {
        let manager = Arc::new(TenantManager::new("test"));
        let _tenant = manager.create_tenant("test-tenant", "Test", TenantPlan::Pro).unwrap();
        manager.update_status("test-tenant", TenantStatus::Active).unwrap();

        let jwt_config = JwtConfig::with_secret("test-secret-key-that-is-long-enough")
            .with_issuer("heliosdb")
            .with_audience("heliosdb-api");

        let resolver = Arc::new(TenantResolver::new(manager.clone()).with_jwt_config(jwt_config));

        // Create a JWT token
        let token = resolver.create_jwt("test-tenant", "user123", TenantRole::Admin, 3600).unwrap();

        // Resolve tenant from JWT
        let ctx = resolver.resolve_from_jwt(&token).unwrap();

        assert_eq!(ctx.tenant.id, "test-tenant");
        assert_eq!(ctx.user_id, Some("user123".to_string()));
        assert_eq!(ctx.role, TenantRole::Admin);
    }

    #[test]
    fn test_role_parsing() {
        assert_eq!(TenantRole::from("admin"), TenantRole::Admin);
        assert_eq!(TenantRole::from("ADMIN"), TenantRole::Admin);
        assert_eq!(TenantRole::from("member"), TenantRole::Member);
        assert_eq!(TenantRole::from("viewer"), TenantRole::Viewer);
        assert_eq!(TenantRole::from("service"), TenantRole::Service);
        assert_eq!(TenantRole::from("unknown"), TenantRole::Anonymous);
    }

    #[test]
    fn test_middleware_resolution() {
        let manager = Arc::new(TenantManager::new("test"));
        let _tenant = manager.create_tenant("tenant-abc", "ABC Corp", TenantPlan::Starter).unwrap();
        manager.update_status("tenant-abc", TenantStatus::Active).unwrap();

        let resolver = Arc::new(TenantResolver::new(manager));
        let middleware = TenantMiddleware::new(resolver)
            .with_strategies(vec![ResolutionStrategy::Header]);

        let request = RequestInfo {
            path: "/api/data".to_string(),
            host: None,
            tenant_header: Some("tenant-abc".to_string()),
            api_key: None,
            jwt_token: None,
        };

        let ctx = middleware.resolve(&request).unwrap();
        assert_eq!(ctx.tenant.id, "tenant-abc");
    }

    #[test]
    fn test_api_key_resolution() {
        let manager = Arc::new(TenantManager::new("test"));
        let _tenant = manager.create_tenant("api-tenant", "API Tenant", TenantPlan::Pro).unwrap();
        manager.update_status("api-tenant", TenantStatus::Active).unwrap();

        let resolver = TenantResolver::new(manager);
        let ctx = resolver.resolve_from_api_key("api-tenant:secret-key").unwrap();

        assert_eq!(ctx.tenant.id, "api-tenant");
        assert_eq!(ctx.role, TenantRole::Service);
    }
}
