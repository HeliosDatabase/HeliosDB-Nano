//! Supabase Auth Compatible API
//!
//! JWT-based authentication compatible with Supabase GoTrue auth.
//! Implements real JWT validation, password hashing, and user store.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// JWT Claims structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject (user ID)
    pub sub: String,
    /// Audience
    pub aud: Option<String>,
    /// Expiration time (Unix timestamp)
    pub exp: u64,
    /// Issued at (Unix timestamp)
    pub iat: u64,
    /// User role
    pub role: String,
    /// User email
    pub email: Option<String>,
    /// Phone number
    pub phone: Option<String>,
    /// App metadata
    pub app_metadata: Option<serde_json::Value>,
    /// User metadata
    pub user_metadata: Option<serde_json::Value>,
}

/// User session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Access token (JWT)
    pub access_token: String,
    /// Refresh token
    pub refresh_token: String,
    /// Token type (always "bearer")
    pub token_type: String,
    /// Expires in (seconds)
    pub expires_in: u64,
    /// Expires at (Unix timestamp)
    pub expires_at: u64,
    /// User info
    pub user: User,
}

/// User structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub aud: String,
    pub role: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub email_confirmed_at: Option<String>,
    pub phone_confirmed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub app_metadata: serde_json::Value,
    pub user_metadata: serde_json::Value,
}

/// Internal user record with password hash
#[derive(Debug, Clone)]
struct UserRecord {
    user: User,
    password_hash: String,
}

/// Sign up request
#[derive(Debug, Clone, Deserialize)]
pub struct SignUpRequest {
    pub email: Option<String>,
    pub phone: Option<String>,
    pub password: String,
    pub data: Option<serde_json::Value>,
}

/// Sign in request
#[derive(Debug, Clone, Deserialize)]
pub struct SignInRequest {
    pub email: Option<String>,
    pub phone: Option<String>,
    pub password: String,
}

/// Token refresh request
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshTokenRequest {
    pub refresh_token: String,
}

/// Auth error response
#[derive(Debug, Clone, Serialize)]
pub struct AuthError {
    pub error: String,
    pub error_description: Option<String>,
}

/// Auth configuration
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// JWT secret key
    pub jwt_secret: String,
    /// Token expiry in seconds
    pub token_expiry: u64,
    /// Refresh token expiry in seconds
    pub refresh_token_expiry: u64,
    /// Site URL for email links
    pub site_url: String,
    /// Require email confirmation
    pub require_confirmation: bool,
    /// Allow signup
    pub enable_signup: bool,
    /// Enable anonymous sign-in
    pub enable_anonymous: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: std::env::var("JWT_SECRET")
                .unwrap_or_else(|_| "heliosdb-jwt-secret-change-in-production".to_string()),
            token_expiry: 3600, // 1 hour
            refresh_token_expiry: 604800, // 7 days
            site_url: "http://localhost:3000".to_string(),
            require_confirmation: false,
            enable_signup: true,
            enable_anonymous: false,
        }
    }
}

/// Refresh token record
#[derive(Debug, Clone)]
struct RefreshTokenRecord {
    user_id: String,
    expires_at: u64,
    revoked: bool,
}

/// Auth service with real user store and JWT validation
pub struct AuthService {
    config: AuthConfig,
    /// User store: email/phone -> UserRecord
    users_by_email: Arc<RwLock<HashMap<String, UserRecord>>>,
    users_by_phone: Arc<RwLock<HashMap<String, UserRecord>>>,
    users_by_id: Arc<RwLock<HashMap<String, UserRecord>>>,
    /// Refresh tokens: token -> RefreshTokenRecord
    refresh_tokens: Arc<RwLock<HashMap<String, RefreshTokenRecord>>>,
    /// Revoked access tokens (blacklist)
    revoked_tokens: Arc<RwLock<HashMap<String, u64>>>,
}

impl AuthService {
    pub fn new(config: AuthConfig) -> Self {
        Self {
            config,
            users_by_email: Arc::new(RwLock::new(HashMap::new())),
            users_by_phone: Arc::new(RwLock::new(HashMap::new())),
            users_by_id: Arc::new(RwLock::new(HashMap::new())),
            refresh_tokens: Arc::new(RwLock::new(HashMap::new())),
            revoked_tokens: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Hash password using Argon2id
    fn hash_password(&self, password: &str) -> Result<String, AuthError> {
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();

        argon2
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|e| AuthError {
                error: "password_hash_error".to_string(),
                error_description: Some(format!("Failed to hash password: {}", e)),
            })
    }

    /// Verify password against stored hash
    fn verify_password(&self, password: &str, hash: &str) -> bool {
        match PasswordHash::new(hash) {
            Ok(parsed_hash) => {
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed_hash)
                    .is_ok()
            }
            Err(_) => false,
        }
    }

    /// Sign up a new user
    pub fn sign_up(&self, request: SignUpRequest) -> Result<Session, AuthError> {
        if !self.config.enable_signup {
            return Err(AuthError {
                error: "signup_disabled".to_string(),
                error_description: Some("Sign ups are disabled".to_string()),
            });
        }

        // Validate email or phone
        if request.email.is_none() && request.phone.is_none() {
            return Err(AuthError {
                error: "validation_failed".to_string(),
                error_description: Some("Email or phone required".to_string()),
            });
        }

        // Validate password strength
        if request.password.len() < 6 {
            return Err(AuthError {
                error: "weak_password".to_string(),
                error_description: Some("Password must be at least 6 characters".to_string()),
            });
        }

        // Check if user already exists
        if let Some(ref email) = request.email {
            let email_lower = email.to_lowercase();
            if self.users_by_email.read().contains_key(&email_lower) {
                return Err(AuthError {
                    error: "user_already_exists".to_string(),
                    error_description: Some("User with this email already exists".to_string()),
                });
            }
        }

        if let Some(ref phone) = request.phone {
            if self.users_by_phone.read().contains_key(phone) {
                return Err(AuthError {
                    error: "user_already_exists".to_string(),
                    error_description: Some("User with this phone already exists".to_string()),
                });
            }
        }

        // Hash password
        let password_hash = self.hash_password(&request.password)?;

        // Create user
        let user_id = uuid::Uuid::new_v4().to_string();
        let now = current_timestamp();

        let user = User {
            id: user_id.clone(),
            aud: "authenticated".to_string(),
            role: "authenticated".to_string(),
            email: request.email.clone(),
            phone: request.phone.clone(),
            email_confirmed_at: if self.config.require_confirmation {
                None
            } else {
                Some(format_timestamp(now))
            },
            phone_confirmed_at: None,
            created_at: format_timestamp(now),
            updated_at: format_timestamp(now),
            app_metadata: serde_json::json!({"provider": "email", "providers": ["email"]}),
            user_metadata: request.data.unwrap_or(serde_json::json!({})),
        };

        let record = UserRecord {
            user: user.clone(),
            password_hash,
        };

        // Store user
        {
            let mut users_by_id = self.users_by_id.write();
            users_by_id.insert(user_id.clone(), record.clone());
        }

        if let Some(ref email) = request.email {
            let mut users_by_email = self.users_by_email.write();
            users_by_email.insert(email.to_lowercase(), record.clone());
        }

        if let Some(ref phone) = request.phone {
            let mut users_by_phone = self.users_by_phone.write();
            users_by_phone.insert(phone.clone(), record);
        }

        // Generate tokens
        self.create_session(user)
    }

    /// Sign in with email/phone and password
    pub fn sign_in(&self, request: SignInRequest) -> Result<Session, AuthError> {
        if request.email.is_none() && request.phone.is_none() {
            return Err(AuthError {
                error: "validation_failed".to_string(),
                error_description: Some("Email or phone required".to_string()),
            });
        }

        // Look up user
        let user_record = if let Some(ref email) = request.email {
            let email_lower = email.to_lowercase();
            self.users_by_email.read().get(&email_lower).cloned()
        } else if let Some(ref phone) = request.phone {
            self.users_by_phone.read().get(phone).cloned()
        } else {
            None
        };

        let user_record = user_record.ok_or_else(|| AuthError {
            error: "invalid_credentials".to_string(),
            error_description: Some("Invalid login credentials".to_string()),
        })?;

        // Verify password
        if !self.verify_password(&request.password, &user_record.password_hash) {
            return Err(AuthError {
                error: "invalid_credentials".to_string(),
                error_description: Some("Invalid login credentials".to_string()),
            });
        }

        // Create session
        self.create_session(user_record.user)
    }

    /// Sign in anonymously
    pub fn sign_in_anonymous(&self) -> Result<Session, AuthError> {
        if !self.config.enable_anonymous {
            return Err(AuthError {
                error: "anonymous_disabled".to_string(),
                error_description: Some("Anonymous sign-in is disabled".to_string()),
            });
        }

        let user_id = uuid::Uuid::new_v4().to_string();
        let now = current_timestamp();

        let user = User {
            id: user_id.clone(),
            aud: "authenticated".to_string(),
            role: "anon".to_string(),
            email: None,
            phone: None,
            email_confirmed_at: None,
            phone_confirmed_at: None,
            created_at: format_timestamp(now),
            updated_at: format_timestamp(now),
            app_metadata: serde_json::json!({"provider": "anonymous"}),
            user_metadata: serde_json::json!({}),
        };

        // Store anonymous user
        let record = UserRecord {
            user: user.clone(),
            password_hash: String::new(),
        };
        self.users_by_id.write().insert(user_id, record);

        self.create_session(user)
    }

    /// Refresh access token
    pub fn refresh_token(&self, request: RefreshTokenRequest) -> Result<Session, AuthError> {
        // Validate refresh token
        let refresh_record = {
            let tokens = self.refresh_tokens.read();
            tokens.get(&request.refresh_token).cloned()
        };

        let refresh_record = refresh_record.ok_or_else(|| AuthError {
            error: "invalid_refresh_token".to_string(),
            error_description: Some("Invalid or expired refresh token".to_string()),
        })?;

        // Check if revoked or expired
        let now = current_timestamp();
        if refresh_record.revoked || refresh_record.expires_at < now {
            return Err(AuthError {
                error: "invalid_refresh_token".to_string(),
                error_description: Some("Refresh token has been revoked or expired".to_string()),
            });
        }

        // Get user
        let user = {
            let users = self.users_by_id.read();
            users.get(&refresh_record.user_id).map(|r| r.user.clone())
        };

        let user = user.ok_or_else(|| AuthError {
            error: "user_not_found".to_string(),
            error_description: Some("User no longer exists".to_string()),
        })?;

        // Revoke old refresh token
        {
            let mut tokens = self.refresh_tokens.write();
            if let Some(record) = tokens.get_mut(&request.refresh_token) {
                record.revoked = true;
            }
        }

        // Create new session
        self.create_session(user)
    }

    /// Sign out (invalidate tokens)
    pub fn sign_out(&self, access_token: &str) -> Result<(), AuthError> {
        // Verify token first to get expiry
        let claims = self.verify_token(access_token)?;

        // Add to revoked tokens
        self.revoked_tokens.write().insert(access_token.to_string(), claims.exp);

        Ok(())
    }

    /// Get user from access token
    pub fn get_user(&self, access_token: &str) -> Result<User, AuthError> {
        let claims = self.verify_token(access_token)?;

        // Get user from store
        let user = self.users_by_id.read()
            .get(&claims.sub)
            .map(|r| r.user.clone());

        user.ok_or_else(|| AuthError {
            error: "user_not_found".to_string(),
            error_description: Some("User not found".to_string()),
        })
    }

    /// Update user metadata
    pub fn update_user(
        &self,
        access_token: &str,
        data: serde_json::Value,
    ) -> Result<User, AuthError> {
        let claims = self.verify_token(access_token)?;

        let mut users_by_id = self.users_by_id.write();
        let record = users_by_id.get_mut(&claims.sub).ok_or_else(|| AuthError {
            error: "user_not_found".to_string(),
            error_description: Some("User not found".to_string()),
        })?;

        record.user.user_metadata = data;
        record.user.updated_at = format_timestamp(current_timestamp());

        Ok(record.user.clone())
    }

    /// Create session with real JWT tokens
    fn create_session(&self, user: User) -> Result<Session, AuthError> {
        let now = current_timestamp();
        let expires_at = now + self.config.token_expiry;

        // Create JWT claims
        let claims = JwtClaims {
            sub: user.id.clone(),
            aud: Some("authenticated".to_string()),
            exp: expires_at,
            iat: now,
            role: user.role.clone(),
            email: user.email.clone(),
            phone: user.phone.clone(),
            app_metadata: Some(user.app_metadata.clone()),
            user_metadata: Some(user.user_metadata.clone()),
        };

        // Generate JWT using jsonwebtoken
        let access_token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.config.jwt_secret.as_bytes()),
        )
        .map_err(|e| AuthError {
            error: "token_generation_error".to_string(),
            error_description: Some(format!("Failed to generate access token: {}", e)),
        })?;

        // Generate secure refresh token
        let refresh_token = generate_secure_token();

        // Store refresh token
        {
            let mut tokens = self.refresh_tokens.write();
            tokens.insert(
                refresh_token.clone(),
                RefreshTokenRecord {
                    user_id: user.id.clone(),
                    expires_at: now + self.config.refresh_token_expiry,
                    revoked: false,
                },
            );
        }

        Ok(Session {
            access_token,
            refresh_token,
            token_type: "bearer".to_string(),
            expires_in: self.config.token_expiry,
            expires_at,
            user,
        })
    }

    /// Verify JWT token and extract claims
    pub fn verify_token(&self, token: &str) -> Result<JwtClaims, AuthError> {
        // Check if token is revoked
        if self.revoked_tokens.read().contains_key(token) {
            return Err(AuthError {
                error: "token_revoked".to_string(),
                error_description: Some("Token has been revoked".to_string()),
            });
        }

        // Decode and verify JWT
        let mut validation = Validation::default();
        validation.validate_exp = true;
        validation.set_audience(&["authenticated"]);

        let token_data = decode::<JwtClaims>(
            token,
            &DecodingKey::from_secret(self.config.jwt_secret.as_bytes()),
            &validation,
        )
        .map_err(|e| {
            let (error, description) = match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                    ("token_expired", "Token has expired")
                }
                jsonwebtoken::errors::ErrorKind::InvalidToken => {
                    ("invalid_token", "Token is invalid")
                }
                jsonwebtoken::errors::ErrorKind::InvalidSignature => {
                    ("invalid_signature", "Token signature is invalid")
                }
                _ => ("token_error", "Token validation failed"),
            };
            AuthError {
                error: error.to_string(),
                error_description: Some(description.to_string()),
            }
        })?;

        Ok(token_data.claims)
    }

    /// Change user password
    pub fn change_password(
        &self,
        access_token: &str,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        let claims = self.verify_token(access_token)?;

        // Validate new password
        if new_password.len() < 6 {
            return Err(AuthError {
                error: "weak_password".to_string(),
                error_description: Some("Password must be at least 6 characters".to_string()),
            });
        }

        let mut users_by_id = self.users_by_id.write();
        let record = users_by_id.get_mut(&claims.sub).ok_or_else(|| AuthError {
            error: "user_not_found".to_string(),
            error_description: Some("User not found".to_string()),
        })?;

        // Verify old password
        if !self.verify_password(old_password, &record.password_hash) {
            return Err(AuthError {
                error: "invalid_password".to_string(),
                error_description: Some("Current password is incorrect".to_string()),
            });
        }

        // Hash and update new password
        record.password_hash = self.hash_password(new_password)?;
        record.user.updated_at = format_timestamp(current_timestamp());

        Ok(())
    }

    /// Delete user account
    pub fn delete_user(&self, access_token: &str) -> Result<(), AuthError> {
        let claims = self.verify_token(access_token)?;

        // Get user to find email/phone
        let user = {
            let users = self.users_by_id.read();
            users.get(&claims.sub).map(|r| r.user.clone())
        };

        let user = user.ok_or_else(|| AuthError {
            error: "user_not_found".to_string(),
            error_description: Some("User not found".to_string()),
        })?;

        // Remove from all indexes
        self.users_by_id.write().remove(&claims.sub);

        if let Some(ref email) = user.email {
            self.users_by_email.write().remove(&email.to_lowercase());
        }

        if let Some(ref phone) = user.phone {
            self.users_by_phone.write().remove(phone);
        }

        // Revoke current token
        self.revoked_tokens.write().insert(access_token.to_string(), claims.exp);

        Ok(())
    }

    /// Clean up expired tokens (call periodically)
    pub fn cleanup_expired_tokens(&self) {
        let now = current_timestamp();

        // Clean expired refresh tokens
        {
            let mut tokens = self.refresh_tokens.write();
            tokens.retain(|_, record| record.expires_at > now);
        }

        // Clean expired revoked tokens
        {
            let mut revoked = self.revoked_tokens.write();
            revoked.retain(|_, exp| *exp > now);
        }
    }
}

/// RLS (Row Level Security) Policy evaluation
#[derive(Debug, Clone)]
pub struct RlsPolicy {
    pub name: String,
    pub table: String,
    pub operation: RlsOperation,
    pub check: String,
    pub using: Option<String>,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RlsOperation {
    Select,
    Insert,
    Update,
    Delete,
    All,
}

/// Evaluate RLS policy for a user
pub fn evaluate_rls(
    policy: &RlsPolicy,
    user: &JwtClaims,
    row: &HashMap<String, serde_json::Value>,
) -> bool {
    // Check if user role matches policy roles
    if !policy.roles.is_empty() && !policy.roles.contains(&user.role) {
        return false;
    }

    // Evaluate check expression
    // Supports common patterns like: auth.uid() = user_id
    if policy.check.contains("auth.uid()") {
        // Look for pattern: auth.uid() = column_name
        if let Some(column) = extract_uid_column(&policy.check) {
            if let Some(value) = row.get(&column) {
                if let Some(row_user_id) = value.as_str() {
                    return row_user_id == user.sub;
                }
            }
            return false;
        }
    }

    // Check for role-based policies
    if policy.check.contains("auth.role()") {
        if let Some(required_role) = extract_role_value(&policy.check) {
            return user.role == required_role;
        }
    }

    // Check for email-based policies
    if policy.check.contains("auth.email()") {
        if let Some(column) = extract_email_column(&policy.check) {
            if let Some(value) = row.get(&column) {
                if let Some(row_email) = value.as_str() {
                    return user.email.as_deref() == Some(row_email);
                }
            }
            return false;
        }
    }

    // Default: allow if no specific check matched (policy exists = enabled)
    true
}

// Helper to extract column name from "auth.uid() = column_name" pattern
fn extract_uid_column(check: &str) -> Option<String> {
    let patterns = [
        ("auth.uid() = ", ""),
        ("auth.uid() == ", ""),
        ("= auth.uid()", ""),
        ("== auth.uid()", ""),
    ];

    for (prefix, suffix) in patterns {
        if check.contains(prefix) {
            let remaining = if suffix.is_empty() {
                check.split(prefix).nth(1)?
            } else {
                check.split(suffix).next()?
            };
            return Some(remaining.trim().trim_matches('"').to_string());
        }
    }
    None
}

// Helper to extract role value from "auth.role() = 'role_name'" pattern
fn extract_role_value(check: &str) -> Option<String> {
    if let Some(start) = check.find("auth.role()") {
        let after = &check[start + 11..];
        if let Some(quote_start) = after.find('\'') {
            let rest = &after[quote_start + 1..];
            if let Some(quote_end) = rest.find('\'') {
                return Some(rest[..quote_end].to_string());
            }
        }
    }
    None
}

// Helper to extract column name from "auth.email() = column_name" pattern
fn extract_email_column(check: &str) -> Option<String> {
    if let Some(pos) = check.find("auth.email() = ") {
        let remaining = &check[pos + 15..];
        return Some(remaining.trim().trim_matches('"').to_string());
    }
    None
}

// Helper functions

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_timestamp(ts: u64) -> String {
    use chrono::{DateTime, Utc};
    DateTime::<Utc>::from_timestamp(ts as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("1970-01-01T00:00:00Z"))
}

fn generate_secure_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signup_and_signin() {
        let service = AuthService::new(AuthConfig::default());

        // Sign up
        let signup_result = service.sign_up(SignUpRequest {
            email: Some("test@example.com".to_string()),
            phone: None,
            password: "password123".to_string(),
            data: None,
        });
        assert!(signup_result.is_ok());
        let session = signup_result.expect("signup should succeed");
        assert!(!session.access_token.is_empty());

        // Sign in
        let signin_result = service.sign_in(SignInRequest {
            email: Some("test@example.com".to_string()),
            phone: None,
            password: "password123".to_string(),
        });
        assert!(signin_result.is_ok());
    }

    #[test]
    fn test_invalid_credentials() {
        let service = AuthService::new(AuthConfig::default());

        // Sign up
        let _ = service.sign_up(SignUpRequest {
            email: Some("test2@example.com".to_string()),
            phone: None,
            password: "password123".to_string(),
            data: None,
        });

        // Sign in with wrong password
        let signin_result = service.sign_in(SignInRequest {
            email: Some("test2@example.com".to_string()),
            phone: None,
            password: "wrongpassword".to_string(),
        });
        assert!(signin_result.is_err());
        let err = signin_result.expect_err("should fail");
        assert_eq!(err.error, "invalid_credentials");
    }

    #[test]
    fn test_jwt_verification() {
        let service = AuthService::new(AuthConfig::default());

        // Sign up
        let session = service.sign_up(SignUpRequest {
            email: Some("jwt_test@example.com".to_string()),
            phone: None,
            password: "password123".to_string(),
            data: None,
        }).expect("signup should succeed");

        // Verify token
        let claims = service.verify_token(&session.access_token);
        assert!(claims.is_ok());
        let claims = claims.expect("verification should succeed");
        assert_eq!(claims.email, Some("jwt_test@example.com".to_string()));
    }

    #[test]
    fn test_refresh_token() {
        let service = AuthService::new(AuthConfig::default());

        // Sign up
        let session = service.sign_up(SignUpRequest {
            email: Some("refresh_test@example.com".to_string()),
            phone: None,
            password: "password123".to_string(),
            data: None,
        }).expect("signup should succeed");

        // Refresh token
        let new_session = service.refresh_token(RefreshTokenRequest {
            refresh_token: session.refresh_token,
        });
        assert!(new_session.is_ok());
    }

    #[test]
    fn test_rls_evaluation() {
        let claims = JwtClaims {
            sub: "user-123".to_string(),
            aud: Some("authenticated".to_string()),
            exp: current_timestamp() + 3600,
            iat: current_timestamp(),
            role: "authenticated".to_string(),
            email: Some("test@example.com".to_string()),
            phone: None,
            app_metadata: None,
            user_metadata: None,
        };

        let policy = RlsPolicy {
            name: "own_rows".to_string(),
            table: "posts".to_string(),
            operation: RlsOperation::Select,
            check: "auth.uid() = user_id".to_string(),
            using: None,
            roles: vec![],
        };

        // User can see their own rows
        let mut row1 = HashMap::new();
        row1.insert("user_id".to_string(), serde_json::json!("user-123"));
        assert!(evaluate_rls(&policy, &claims, &row1));

        // User cannot see other users' rows
        let mut row2 = HashMap::new();
        row2.insert("user_id".to_string(), serde_json::json!("other-user"));
        assert!(!evaluate_rls(&policy, &claims, &row2));
    }
}
