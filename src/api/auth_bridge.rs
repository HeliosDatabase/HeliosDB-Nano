//! Auth Bridge - Database-persisted authentication
//!
//! Provides user signup/signin with real DB table persistence, JWT access tokens,
//! and refresh token rotation. This is the BaaS Phase 1 auth layer that persists
//! to `_auth_users` and `_auth_refresh_tokens` tables in the embedded database.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{EmbeddedDatabase, Value};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Auth-specific error returned by all `AuthBridge` operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthError {
    /// Machine-readable error code (e.g. `"invalid_credentials"`)
    pub error: String,
    /// Optional human-readable description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)?;
        if let Some(desc) = &self.error_description {
            write!(f, ": {}", desc)?;
        }
        Ok(())
    }
}

impl std::error::Error for AuthError {}

// ---------------------------------------------------------------------------
// JWT Claims
// ---------------------------------------------------------------------------

/// Claims embedded inside every access-token JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject - the user id (UUID string)
    pub sub: String,
    /// User email
    pub email: String,
    /// User role (e.g. `"authenticated"`)
    pub role: String,
    /// Audience
    pub aud: String,
    /// Expiry (unix timestamp seconds)
    pub exp: u64,
    /// Issued-at (unix timestamp seconds)
    pub iat: u64,
    /// JWT ID - unique per token, ensures distinct tokens even within the same second
    pub jti: String,
}

// ---------------------------------------------------------------------------
// Public request / response types
// ---------------------------------------------------------------------------

/// Body for `POST /auth/signup`
#[derive(Debug, Clone, Deserialize)]
pub struct SignUpRequest {
    pub email: String,
    pub password: String,
}

/// Body for `POST /auth/signin`
#[derive(Debug, Clone, Deserialize)]
pub struct SignInRequest {
    pub email: String,
    pub password: String,
}

/// Body for `POST /auth/logout`
#[derive(Debug, Clone, Deserialize)]
pub struct LogoutRequest {
    pub refresh_token: String,
}

/// Body for `POST /auth/refresh`
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Successful auth response containing tokens and user info.
#[derive(Debug, Clone, Serialize)]
pub struct AuthSession {
    pub access_token: String,
    pub refresh_token: String,
    pub user: AuthUser,
    pub expires_in: u64,
}

/// Public user record returned in auth responses.
#[derive(Debug, Clone, Serialize)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    pub role: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// AuthBridge
// ---------------------------------------------------------------------------

/// Database-backed authentication service.
///
/// All user credentials and refresh tokens are stored in real database tables
/// (`_auth_users`, `_auth_refresh_tokens`) via parameterised queries.
pub struct AuthBridge {
    db: Arc<EmbeddedDatabase>,
    jwt_secret: String,
    access_token_expiry: u64,
    refresh_token_expiry: u64,
}

impl AuthBridge {
    /// Create a new `AuthBridge` with default token lifetimes.
    ///
    /// - access token: 3 600 s (1 hour)
    /// - refresh token: 604 800 s (7 days)
    pub fn new(db: Arc<EmbeddedDatabase>, jwt_secret: &str) -> Self {
        Self {
            db,
            jwt_secret: jwt_secret.to_string(),
            access_token_expiry: 3600,
            refresh_token_expiry: 604_800,
        }
    }

    // -- bootstrap --------------------------------------------------------

    /// Create auth tables if they don't exist.
    ///
    /// Safe to call on every startup; `CREATE TABLE IF NOT EXISTS` is a no-op
    /// when the tables already exist.
    pub fn bootstrap(&self) -> crate::Result<()> {
        self.db.execute(
            "CREATE TABLE IF NOT EXISTS _auth_users (\
                id TEXT PRIMARY KEY, \
                email TEXT NOT NULL, \
                encrypted_password TEXT NOT NULL, \
                role TEXT NOT NULL, \
                created_at TEXT NOT NULL\
            )",
        )?;

        self.db.execute(
            "CREATE TABLE IF NOT EXISTS _auth_refresh_tokens (\
                token TEXT PRIMARY KEY, \
                user_id TEXT NOT NULL, \
                expires_at BIGINT NOT NULL, \
                revoked SMALLINT NOT NULL\
            )",
        )?;

        Ok(())
    }

    // -- sign_up ----------------------------------------------------------

    /// Register a new user.
    ///
    /// 1. Validate email format (must contain `@`).
    /// 2. Reject if email already taken.
    /// 3. Hash password with Argon2id.
    /// 4. Insert user row.
    /// 5. Issue access + refresh tokens.
    pub fn sign_up(&self, email: &str, password: &str) -> Result<AuthSession, AuthError> {
        // 1. Basic email validation
        if !email.contains('@') || email.len() < 3 {
            return Err(AuthError {
                error: "validation_failed".into(),
                error_description: Some("Invalid email format".into()),
            });
        }

        // 2. Password strength
        if password.len() < 6 {
            return Err(AuthError {
                error: "weak_password".into(),
                error_description: Some("Password must be at least 6 characters".into()),
            });
        }

        // 3. Check if email already exists
        let email_lower = email.to_lowercase();
        let existing = self
            .db
            .query_params(
                "SELECT id FROM _auth_users WHERE email = $1",
                &[Value::String(email_lower.clone())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        if !existing.is_empty() {
            return Err(AuthError {
                error: "user_already_exists".into(),
                error_description: Some("User with this email already exists".into()),
            });
        }

        // 4. Hash password
        let password_hash = hash_password(password)?;

        // 5. Generate user id + timestamp
        let user_id = uuid::Uuid::new_v4().to_string();
        let now_ts = format_iso_now();

        // 6. Insert user (all columns explicit)
        self.db
            .execute_params(
                "INSERT INTO _auth_users (id, email, encrypted_password, role, created_at) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    Value::String(user_id.clone()),
                    Value::String(email_lower.clone()),
                    Value::String(password_hash),
                    Value::String("authenticated".into()),
                    Value::String(now_ts.clone()),
                ],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        // 7. Build tokens
        let user = AuthUser {
            id: user_id.clone(),
            email: email_lower,
            role: "authenticated".into(),
            created_at: now_ts,
        };
        self.create_session(&user)
    }

    // -- sign_in ----------------------------------------------------------

    /// Authenticate with email + password.
    pub fn sign_in(&self, email: &str, password: &str) -> Result<AuthSession, AuthError> {
        let email_lower = email.to_lowercase();

        // 1. Look up user (all columns in one query)
        let rows = self
            .db
            .query_params(
                "SELECT id, email, encrypted_password, role, created_at \
                 FROM _auth_users WHERE email = $1",
                &[Value::String(email_lower.clone())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        let row = rows.first().ok_or_else(|| AuthError {
            error: "invalid_credentials".into(),
            error_description: Some("Invalid login credentials".into()),
        })?;

        // Extract values: id(0), email(1), hash(2), role(3), created_at(4)
        let stored_id = value_to_string(row.values.get(0));
        let stored_email = value_to_string(row.values.get(1));
        let stored_hash = value_to_string(row.values.get(2));
        let stored_role = value_to_string(row.values.get(3));
        let created_at = value_to_string(row.values.get(4));

        // 2. Verify password
        if !verify_password(password, &stored_hash) {
            return Err(AuthError {
                error: "invalid_credentials".into(),
                error_description: Some("Invalid login credentials".into()),
            });
        }

        // 3. Build session
        let user = AuthUser {
            id: stored_id,
            email: stored_email,
            role: stored_role,
            created_at,
        };
        self.create_session(&user)
    }

    // -- refresh ----------------------------------------------------------

    /// Exchange a valid refresh token for a new session.
    ///
    /// The old refresh token is revoked atomically.
    pub fn refresh(&self, refresh_token: &str) -> Result<AuthSession, AuthError> {
        let now_secs = now_unix();

        // 1. Look up token
        let rows = self
            .db
            .query_params(
                "SELECT user_id, expires_at, revoked FROM _auth_refresh_tokens WHERE token = $1",
                &[Value::String(refresh_token.into())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        let row = rows.first().ok_or_else(|| AuthError {
            error: "invalid_refresh_token".into(),
            error_description: Some("Invalid or expired refresh token".into()),
        })?;

        let token_user_id = value_to_string(row.values.get(0));
        let expires_at = value_to_i64(row.values.get(1));
        let revoked = value_to_i64(row.values.get(2));

        // 2. Validate
        if revoked != 0 {
            return Err(AuthError {
                error: "invalid_refresh_token".into(),
                error_description: Some("Refresh token has been revoked".into()),
            });
        }
        if expires_at < now_secs as i64 {
            return Err(AuthError {
                error: "invalid_refresh_token".into(),
                error_description: Some("Refresh token has expired".into()),
            });
        }

        // 3. Revoke old token
        self.db
            .execute_params(
                "UPDATE _auth_refresh_tokens SET revoked = 1 WHERE token = $1",
                &[Value::String(refresh_token.into())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        // 4. Fetch user
        let user = self.fetch_user_by_id(&token_user_id)?;

        // 5. New session
        self.create_session(&user)
    }

    // -- get_user ---------------------------------------------------------

    /// Verify an access token and return the user record.
    pub fn get_user(&self, access_token: &str) -> Result<AuthUser, AuthError> {
        let claims = self.verify_token(access_token)?;
        self.fetch_user_by_id(&claims.sub)
    }

    // -- sign_out ---------------------------------------------------------

    /// Revoke a refresh token (sign-out).
    pub fn sign_out(&self, refresh_token: &str) -> Result<(), AuthError> {
        self.db
            .execute_params(
                "UPDATE _auth_refresh_tokens SET revoked = 1 WHERE token = $1",
                &[Value::String(refresh_token.into())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;
        Ok(())
    }

    // -- oauth_sign_in ----------------------------------------------------

    /// Upsert a user from an OAuth provider and return a fresh session.
    ///
    /// - If a user with the same email already exists the session is created
    ///   for that existing user (the provider metadata is *not* overwritten to
    ///   avoid leaking cross-provider info).
    /// - If no user exists a new one is created with a random 32-byte password
    ///   (the user will never need it because they authenticate via OAuth).
    pub fn oauth_sign_in(
        &self,
        info: &crate::api::oauth::OAuthUserInfo,
    ) -> Result<AuthSession, AuthError> {
        let email_lower = info.email.to_lowercase();

        // Check if user with this email already exists
        let existing = self
            .db
            .query_params(
                "SELECT id, email, role, created_at FROM _auth_users WHERE email = $1",
                &[Value::String(email_lower.clone())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        if let Some(row) = existing.first() {
            // User exists - create session for them
            let user = AuthUser {
                id: value_to_string(row.values.get(0)),
                email: value_to_string(row.values.get(1)),
                role: value_to_string(row.values.get(2)),
                created_at: value_to_string(row.values.get(3)),
            };
            return self.create_session(&user);
        }

        // New user - create with random password
        let random_password = generate_secure_token();
        let password_hash = hash_password(&random_password)?;

        let user_id = uuid::Uuid::new_v4().to_string();
        let now_ts = format_iso_now();

        self.db
            .execute_params(
                "INSERT INTO _auth_users (id, email, encrypted_password, role, created_at) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    Value::String(user_id.clone()),
                    Value::String(email_lower.clone()),
                    Value::String(password_hash),
                    Value::String("authenticated".into()),
                    Value::String(now_ts.clone()),
                ],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        let user = AuthUser {
            id: user_id,
            email: email_lower,
            role: "authenticated".into(),
            created_at: now_ts,
        };
        self.create_session(&user)
    }

    // -- internal helpers -------------------------------------------------

    /// Create a fresh JWT access token + refresh token pair and persist the
    /// refresh token to `_auth_refresh_tokens`.
    fn create_session(&self, user: &AuthUser) -> Result<AuthSession, AuthError> {
        let now = now_unix();
        let exp = now + self.access_token_expiry;

        // JWT
        let claims = Claims {
            sub: user.id.clone(),
            email: user.email.clone(),
            role: user.role.clone(),
            aud: "authenticated".into(),
            exp,
            iat: now,
            jti: uuid::Uuid::new_v4().to_string(),
        };

        let access_token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .map_err(|e| AuthError {
            error: "token_generation_error".into(),
            error_description: Some(format!("Failed to generate access token: {e}")),
        })?;

        // Refresh token - secure random
        let refresh_token = generate_secure_token();
        let refresh_expires = (now + self.refresh_token_expiry) as i64;

        // Persist refresh token
        self.db
            .execute_params(
                "INSERT INTO _auth_refresh_tokens (token, user_id, expires_at, revoked) \
                 VALUES ($1, $2, $3, $4)",
                &[
                    Value::String(refresh_token.clone()),
                    Value::String(user.id.clone()),
                    Value::Int8(refresh_expires),
                    Value::Int2(0),
                ],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        Ok(AuthSession {
            access_token,
            refresh_token,
            user: user.clone(),
            expires_in: self.access_token_expiry,
        })
    }

    /// Verify a JWT access token and return the decoded claims.
    fn verify_token(&self, token: &str) -> Result<Claims, AuthError> {
        let mut validation = Validation::default();
        validation.validate_exp = true;
        validation.set_audience(&["authenticated"]);

        let token_data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.jwt_secret.as_bytes()),
            &validation,
        )
        .map_err(|e| {
            let (code, desc) = match e.kind() {
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
                error: code.into(),
                error_description: Some(desc.into()),
            }
        })?;

        Ok(token_data.claims)
    }

    /// Fetch a user from `_auth_users` by primary key.
    fn fetch_user_by_id(&self, user_id: &str) -> Result<AuthUser, AuthError> {
        let rows = self
            .db
            .query_params(
                "SELECT id, email, role, created_at FROM _auth_users WHERE id = $1",
                &[Value::String(user_id.into())],
            )
            .map_err(|e| AuthError {
                error: "database_error".into(),
                error_description: Some(format!("{e}")),
            })?;

        let row = rows.first().ok_or_else(|| AuthError {
            error: "user_not_found".into(),
            error_description: Some("User not found".into()),
        })?;

        Ok(AuthUser {
            id: value_to_string(row.values.get(0)),
            email: value_to_string(row.values.get(1)),
            role: value_to_string(row.values.get(2)),
            created_at: value_to_string(row.values.get(3)),
        })
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Hash a password using Argon2id with a random salt.
fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();

    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AuthError {
            error: "password_hash_error".into(),
            error_description: Some(format!("Failed to hash password: {e}")),
        })
}

/// Verify a plaintext password against an Argon2id hash.
fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Current time as unix seconds.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current time as an ISO-8601 string (UTC).
fn format_iso_now() -> String {
    let secs = now_unix();
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

/// Generate a 32-byte URL-safe base64 token.
fn generate_secure_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen::<u8>()).collect();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &bytes)
}

/// Extract a `String` from an optional `Value`, falling back to `""`.
fn value_to_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(other) => format!("{other}"),
        None => String::new(),
    }
}

/// Extract an `i64` from an optional `Value`, falling back to `0`.
fn value_to_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Int8(n)) => *n,
        Some(Value::Int4(n)) => i64::from(*n),
        Some(Value::Int2(n)) => i64::from(*n),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Helper: create an in-memory DB, bootstrap auth tables, and return the bridge.
    fn setup() -> AuthBridge {
        let db = Arc::new(EmbeddedDatabase::new_in_memory().unwrap());
        let bridge = AuthBridge::new(db, "test-jwt-secret-key-at-least-32-bytes!");
        bridge.bootstrap().unwrap();
        bridge
    }

    // -- bootstrap --------------------------------------------------------

    #[test]
    fn test_bootstrap_creates_tables() {
        let bridge = setup();
        // Should be able to query both tables without error
        let users = bridge
            .db
            .query("SELECT id FROM _auth_users", &[])
            .unwrap();
        assert!(users.is_empty());

        let tokens = bridge
            .db
            .query("SELECT token FROM _auth_refresh_tokens", &[])
            .unwrap();
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_bootstrap_idempotent() {
        let bridge = setup();
        // Second bootstrap is a no-op
        bridge.bootstrap().unwrap();
    }

    // -- sign_up ----------------------------------------------------------

    #[test]
    fn test_signup_success() {
        let bridge = setup();
        let session = bridge.sign_up("alice@example.com", "password123").unwrap();

        assert!(!session.access_token.is_empty());
        assert!(!session.refresh_token.is_empty());
        assert_eq!(session.user.email, "alice@example.com");
        assert_eq!(session.user.role, "authenticated");
        assert_eq!(session.expires_in, 3600);
    }

    #[test]
    fn test_signup_normalises_email() {
        let bridge = setup();
        let session = bridge.sign_up("Alice@Example.COM", "password123").unwrap();
        assert_eq!(session.user.email, "alice@example.com");
    }

    #[test]
    fn test_signup_duplicate_email() {
        let bridge = setup();
        bridge.sign_up("dup@example.com", "password123").unwrap();
        let err = bridge.sign_up("dup@example.com", "password456").unwrap_err();
        assert_eq!(err.error, "user_already_exists");
    }

    #[test]
    fn test_signup_invalid_email() {
        let bridge = setup();
        let err = bridge.sign_up("not-an-email", "password123").unwrap_err();
        assert_eq!(err.error, "validation_failed");
    }

    #[test]
    fn test_signup_weak_password() {
        let bridge = setup();
        let err = bridge.sign_up("weak@example.com", "123").unwrap_err();
        assert_eq!(err.error, "weak_password");
    }

    // -- sign_in ----------------------------------------------------------

    #[test]
    fn test_signin_success() {
        let bridge = setup();
        bridge.sign_up("bob@example.com", "correct-horse").unwrap();
        let session = bridge.sign_in("bob@example.com", "correct-horse").unwrap();

        assert!(!session.access_token.is_empty());
        assert_eq!(session.user.email, "bob@example.com");
    }

    #[test]
    fn test_signin_wrong_password() {
        let bridge = setup();
        bridge.sign_up("carol@example.com", "right-pw").unwrap();
        let err = bridge.sign_in("carol@example.com", "wrong-pw").unwrap_err();
        assert_eq!(err.error, "invalid_credentials");
    }

    #[test]
    fn test_signin_unknown_email() {
        let bridge = setup();
        let err = bridge.sign_in("unknown@example.com", "pw").unwrap_err();
        assert_eq!(err.error, "invalid_credentials");
    }

    #[test]
    fn test_signin_case_insensitive_email() {
        let bridge = setup();
        bridge.sign_up("dave@example.com", "password").unwrap();
        let session = bridge.sign_in("DAVE@Example.COM", "password").unwrap();
        assert_eq!(session.user.email, "dave@example.com");
    }

    // -- get_user ---------------------------------------------------------

    #[test]
    fn test_get_user_from_access_token() {
        let bridge = setup();
        let session = bridge.sign_up("eve@example.com", "password123").unwrap();
        let user = bridge.get_user(&session.access_token).unwrap();
        assert_eq!(user.email, "eve@example.com");
        assert_eq!(user.id, session.user.id);
    }

    #[test]
    fn test_get_user_invalid_token() {
        let bridge = setup();
        let err = bridge.get_user("not.a.jwt").unwrap_err();
        assert!(
            err.error == "invalid_token" || err.error == "token_error",
            "unexpected error: {}",
            err.error
        );
    }

    // -- refresh ----------------------------------------------------------

    #[test]
    fn test_refresh_success() {
        let bridge = setup();
        let session = bridge.sign_up("frank@example.com", "password123").unwrap();
        let new_session = bridge.refresh(&session.refresh_token).unwrap();

        assert!(!new_session.access_token.is_empty());
        assert_ne!(new_session.access_token, session.access_token);
        assert_ne!(new_session.refresh_token, session.refresh_token);
        assert_eq!(new_session.user.email, "frank@example.com");
    }

    #[test]
    fn test_refresh_revoked_token() {
        let bridge = setup();
        let session = bridge.sign_up("grace@example.com", "password123").unwrap();
        // Use it once
        bridge.refresh(&session.refresh_token).unwrap();
        // Second use should fail (old token was revoked)
        let err = bridge.refresh(&session.refresh_token).unwrap_err();
        assert_eq!(err.error, "invalid_refresh_token");
    }

    #[test]
    fn test_refresh_invalid_token() {
        let bridge = setup();
        let err = bridge.refresh("bogus-token").unwrap_err();
        assert_eq!(err.error, "invalid_refresh_token");
    }

    // -- sign_out ---------------------------------------------------------

    #[test]
    fn test_sign_out_revokes_refresh() {
        let bridge = setup();
        let session = bridge.sign_up("heidi@example.com", "password123").unwrap();
        bridge.sign_out(&session.refresh_token).unwrap();

        let err = bridge.refresh(&session.refresh_token).unwrap_err();
        assert_eq!(err.error, "invalid_refresh_token");
    }

    // -- data persistence -------------------------------------------------

    #[test]
    fn test_user_persisted_in_table() {
        let bridge = setup();
        bridge.sign_up("persist@example.com", "password123").unwrap();

        let rows = bridge
            .db
            .query_params(
                "SELECT id, email, role FROM _auth_users WHERE email = $1",
                &[Value::String("persist@example.com".into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_refresh_token_persisted_in_table() {
        let bridge = setup();
        let session = bridge.sign_up("token@example.com", "password123").unwrap();

        let rows = bridge
            .db
            .query_params(
                "SELECT token, user_id FROM _auth_refresh_tokens WHERE user_id = $1",
                &[Value::String(session.user.id.clone())],
            )
            .unwrap();
        assert!(!rows.is_empty());
    }

    // -- password hashing -------------------------------------------------

    #[test]
    fn test_password_hash_and_verify() {
        let hash = hash_password("my-secret").unwrap();
        assert!(verify_password("my-secret", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    // -- JWT round-trip ---------------------------------------------------

    #[test]
    fn test_jwt_roundtrip() {
        let bridge = setup();
        let session = bridge.sign_up("jwt@example.com", "password123").unwrap();
        let claims = bridge.verify_token(&session.access_token).unwrap();
        assert_eq!(claims.email, "jwt@example.com");
        assert_eq!(claims.role, "authenticated");
        assert_eq!(claims.aud, "authenticated");
        assert_eq!(claims.sub, session.user.id);
    }

    // -- full flow --------------------------------------------------------

    #[test]
    fn test_full_signup_signin_refresh_logout_flow() {
        let bridge = setup();

        // 1. Sign up
        let s1 = bridge.sign_up("flow@example.com", "password123").unwrap();
        assert_eq!(s1.user.email, "flow@example.com");

        // 2. Sign in
        let s2 = bridge.sign_in("flow@example.com", "password123").unwrap();
        assert_eq!(s2.user.id, s1.user.id);

        // 3. Verify access token
        let user = bridge.get_user(&s2.access_token).unwrap();
        assert_eq!(user.id, s1.user.id);

        // 4. Refresh
        let s3 = bridge.refresh(&s2.refresh_token).unwrap();
        assert_ne!(s3.access_token, s2.access_token);

        // 5. Old refresh token is dead
        assert!(bridge.refresh(&s2.refresh_token).is_err());

        // 6. Sign out
        bridge.sign_out(&s3.refresh_token).unwrap();
        assert!(bridge.refresh(&s3.refresh_token).is_err());
    }

    // -- oauth_sign_in ----------------------------------------------------

    /// Build an `OAuthUserInfo` for testing.
    fn make_oauth_info(email: &str, provider: &str) -> crate::api::oauth::OAuthUserInfo {
        crate::api::oauth::OAuthUserInfo {
            email: email.to_string(),
            name: Some("Test User".to_string()),
            avatar_url: None,
            provider: provider.to_string(),
            provider_id: "provider-id-12345".to_string(),
        }
    }

    #[test]
    fn test_oauth_sign_in_new_user() {
        let bridge = setup();
        let info = make_oauth_info("oauth-new@example.com", "google");
        let session = bridge.oauth_sign_in(&info).unwrap();

        assert_eq!(session.user.email, "oauth-new@example.com");
        assert_eq!(session.user.role, "authenticated");
        assert!(!session.access_token.is_empty());
        assert!(!session.refresh_token.is_empty());
    }

    #[test]
    fn test_oauth_sign_in_existing_user() {
        let bridge = setup();

        // First sign up with password
        let s1 = bridge.sign_up("existing@example.com", "password123").unwrap();

        // Then OAuth sign in with same email
        let info = make_oauth_info("existing@example.com", "github");
        let session = bridge.oauth_sign_in(&info).unwrap();

        // Should be the same user
        assert_eq!(session.user.id, s1.user.id);
        assert_eq!(session.user.email, "existing@example.com");
    }

    #[test]
    fn test_oauth_sign_in_case_insensitive() {
        let bridge = setup();
        let info = make_oauth_info("OAuth@Example.COM", "google");
        let session = bridge.oauth_sign_in(&info).unwrap();
        assert_eq!(session.user.email, "oauth@example.com");
    }

    #[test]
    fn test_oauth_sign_in_creates_valid_jwt() {
        let bridge = setup();
        let info = make_oauth_info("jwt-oauth@example.com", "google");
        let session = bridge.oauth_sign_in(&info).unwrap();

        // Verify the JWT is valid
        let user = bridge.get_user(&session.access_token).unwrap();
        assert_eq!(user.email, "jwt-oauth@example.com");
    }

    #[test]
    fn test_oauth_sign_in_twice_same_user() {
        let bridge = setup();
        let info = make_oauth_info("twice@example.com", "github");

        let s1 = bridge.oauth_sign_in(&info).unwrap();
        let s2 = bridge.oauth_sign_in(&info).unwrap();

        // Same user, different tokens
        assert_eq!(s1.user.id, s2.user.id);
        assert_ne!(s1.access_token, s2.access_token);
    }
}
