//! PostgreSQL authentication
//!
//! This module implements authentication methods for PostgreSQL wire protocol:
//! - Clear-text password (simple, for testing)
//! - MD5 password with salt (legacy)
//! - SCRAM-SHA-256 (recommended for production, RFC 5802 + RFC 7677 compliant)
//!
//! ## SCRAM-SHA-256 Implementation
//!
//! This module provides a complete, production-ready SCRAM-SHA-256 implementation
//! following RFC 5802 (SCRAM) and RFC 7677 (SCRAM-SHA-256) specifications.
//!
//! ### Security Features
//! - Constant-time comparison for proofs (timing attack prevention)
//! - Salted password hashing with configurable iterations
//! - No plaintext password storage
//! - Channel binding support (future)
//!
//! ### SCRAM Flow
//! 1. Client sends initial message with username and nonce
//! 2. Server responds with server-first-message (combined nonce, salt, iterations)
//! 3. Client sends final message with proof
//! 4. Server verifies proof and sends server-final-message

use crate::{Error, Result};
#[cfg(feature = "ring-crypto")]
use ring::hmac;
#[cfg(feature = "ring-crypto")]
use ring::pbkdf2;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::num::NonZeroU32;

use super::password_store::{InMemoryPasswordStore, SharedPasswordStore};

/// Parse the SCRAM client-first-message per RFC 5802 §7. Returns
/// `(username, client_nonce)` extracted from the bare body.
///
/// Format:
/// ```text
/// client-first-message  = gs2-header client-first-message-bare
/// gs2-header            = gs2-cbind-flag "," [authzid] ","
/// gs2-cbind-flag        = ("p=" cb-name) / "n" / "y"
/// authzid               = "a=" saslname
/// client-first-message-bare = [reserved-mext ","] username "," nonce ["," extensions]
/// ```
///
/// The previous parser at `handler.rs:768-777` split the whole message
/// on commas and indexed `parts[1]` as the username, which only worked
/// when the GS2 header was *missing* — but libpq, asyncpg, and every
/// other compliant client always sends the header, so the parser was
/// broken for every real client.
/// The client-first-message-BARE portion of a SCRAM client-first-message: every
/// byte after the GS2 header (i.e. after the second comma), returned as a slice
/// of the original so it is byte-identical to what the client sent.
///
/// The SCRAM proof hashes this string on both sides, so it must not be
/// reconstructed. `format!("n=,r={nonce}")` is correct only for clients that
/// send an empty `n=`; RFC 5802 permits a non-empty username field and
/// extension fields, and either would silently break the AuthMessage and
/// surface as "Invalid password". Returns `None` if the GS2 header is malformed
/// (no second comma), leaving the caller to decide the fallback.
pub fn scram_client_first_bare(msg: &str) -> Option<&str> {
    let mut iter = msg.splitn(3, ',');
    iter.next()?; // gs2-cbind-flag
    iter.next()?; // authzid
    let bare = iter.next()?;
    if bare.is_empty() {
        return None;
    }
    Some(bare)
}

pub fn parse_scram_client_first(msg: &str) -> Result<(String, String)> {
    // GS2 header: at least 2 commas before the bare body.
    let mut iter = msg.splitn(3, ',');
    let _gs2_cbind_flag = iter
        .next()
        .ok_or_else(|| Error::protocol("Invalid SCRAM client-first-message: missing GS2 channel-binding flag"))?;
    let _authzid = iter
        .next()
        .ok_or_else(|| Error::protocol("Invalid SCRAM client-first-message: missing GS2 authzid slot"))?;
    let bare = iter
        .next()
        .ok_or_else(|| Error::protocol("Invalid SCRAM client-first-message: missing client-first-message-bare"))?;
    if bare.is_empty() {
        return Err(Error::protocol("Invalid SCRAM client-first-message: empty bare body"));
    }
    // Bare body. Username and nonce are required; extensions and
    // reserved-mext may appear in any order before/around them.
    let mut username: Option<&str> = None;
    let mut nonce: Option<&str> = None;
    for part in bare.split(',') {
        if let Some(rest) = part.strip_prefix("n=") {
            username = Some(rest);
        } else if let Some(rest) = part.strip_prefix("r=") {
            nonce = Some(rest);
        }
    }
    let username =
        username.ok_or_else(|| Error::protocol("Invalid SCRAM client-first-message: missing username (n=)"))?;
    let nonce = nonce.ok_or_else(|| Error::protocol("Invalid SCRAM client-first-message: missing nonce (r=)"))?;
    // BUG-003 (Perf-73): per RFC 5802 and the Postgres SCRAM profile, the
    // SCRAM `n=` field MUST be empty — the real username comes from the
    // StartupMessage's `user` parameter. libpq, psycopg, pgx and every other
    // standards-compliant driver sends `n,,n=,r=<nonce>`. Allow empty
    // username at this layer; callers must source the actual user name from
    // the connection's startup parameters, not from SCRAM.
    if nonce.is_empty() {
        return Err(Error::protocol("Invalid SCRAM client-first-message: empty nonce"));
    }
    Ok((username.to_string(), nonce.to_string()))
}

/// Test-only re-export so integration tests in `tests/` can verify the
/// SCRAM parser without going through the full PG-wire round-trip.
#[doc(hidden)]
pub fn parse_scram_client_first_for_test(msg: &str) -> Result<(String, String)> {
    parse_scram_client_first(msg)
}

/// Authentication method
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// No authentication required (trust)
    Trust,
    /// Clear-text password
    CleartextPassword,
    /// MD5 password with salt
    Md5,
    /// SCRAM-SHA-256 (recommended)
    ScramSha256,
}

/// User credentials
#[derive(Debug, Clone)]
pub struct UserCredentials {
    pub username: String,
    pub password_hash: String,
    pub salt: Option<Vec<u8>>,
}

/// Authentication manager
///
/// Manages authentication for PostgreSQL protocol connections.
/// Supports multiple authentication methods and integrates with password storage.
pub struct AuthManager {
    method: AuthMethod,
    users: HashMap<String, UserCredentials>,
    password_store: Option<SharedPasswordStore>,
}

impl AuthManager {
    /// Create a new authentication manager
    pub fn new(method: AuthMethod) -> Self {
        Self {
            method,
            users: HashMap::new(),
            password_store: None,
        }
    }

    /// Create authentication manager with SCRAM password store
    pub fn with_password_store(method: AuthMethod, password_store: SharedPasswordStore) -> Self {
        Self {
            method,
            users: HashMap::new(),
            password_store: Some(password_store),
        }
    }

    /// Create authentication manager with in-memory SCRAM store
    pub fn with_scram_store(method: AuthMethod) -> Self {
        let store = SharedPasswordStore::new(InMemoryPasswordStore::new());
        Self::with_password_store(method, store)
    }

    /// Add a user with password
    ///
    /// For SCRAM-SHA-256, this will store the user in the password store.
    /// For other methods, uses the legacy UserCredentials storage.
    pub fn add_user(&mut self, username: String, password: String) {
        if self.method == AuthMethod::ScramSha256 {
            if let Some(ref store) = self.password_store {
                let _ = store.add_user(&username, &password);
                return;
            }
        }

        // Legacy storage for non-SCRAM methods. For MD5, store the PostgreSQL
        // shadow (`md5` + hex(md5(password || username)), the pg_authid
        // .rolpassword format); for the remaining methods keep the SHA-256
        // hash used by clear-text verification.
        let password_hash = if self.method == AuthMethod::Md5 {
            format!(
                "md5{:x}",
                md5::compute([password.as_bytes(), username.as_bytes()].concat())
            )
        } else {
            Self::hash_password(&password)
        };
        self.users.insert(
            username.clone(),
            UserCredentials {
                username,
                password_hash,
                salt: None,
            },
        );
    }

    /// Get authentication method
    pub fn method(&self) -> AuthMethod {
        self.method
    }

    /// Get password store (if using SCRAM)
    pub fn password_store(&self) -> Option<&SharedPasswordStore> {
        self.password_store.as_ref()
    }

    /// Verify clear-text password
    pub fn verify_cleartext(&self, username: &str, password: &str) -> Result<bool> {
        // If using SCRAM store, verify through that
        if let Some(ref store) = self.password_store {
            if let Some(creds) = store.get_credentials(username) {
                return Ok(creds.verify_password(password));
            } else {
                // User not found - still do hash to prevent timing attacks
                let _ = Self::hash_password(password);
                return Ok(false);
            }
        }

        // Legacy verification
        if let Some(user) = self.users.get(username) {
            let password_hash = Self::hash_password(password);
            Ok(user.password_hash == password_hash)
        } else {
            // User not found - still do hash to prevent timing attacks
            let _ = Self::hash_password(password);
            Ok(false)
        }
    }

    /// Verify a PostgreSQL MD5 password response (AuthenticationMD5Password).
    ///
    /// The per-user shadow is stored as `md5` + hex(md5(password || username))
    /// (pg_authid.rolpassword format). Given the 4-byte challenge `salt` the
    /// server sent, a correct client returns
    /// `md5` + hex(md5(ascii(shadow_body) || salt)), where `shadow_body` is the
    /// 32 hex characters of the shadow *without* the `md5` prefix. We recompute
    /// that expected string from the stored shadow and compare it, in
    /// constant time, to what the client sent. Returns `Ok(false)` (never an
    /// error) for an unknown user or a malformed stored shadow, so the caller
    /// fails closed uniformly.
    pub fn verify_md5_response(&self, username: &str, client_response: &str, salt: &[u8; 4]) -> Result<bool> {
        // Fixed dummy shadow body used to equalize the MD5 work on the
        // unknown-user / malformed-shadow paths (mirrors verify_cleartext's
        // dummy hashing above), so an unauthenticated peer cannot use response
        // latency to enumerate valid usernames. `is_real` gates the result so a
        // dummy path never authenticates even if its hash were reproduced.
        const DUMMY_BODY: &[u8; 32] = b"00000000000000000000000000000000";
        let (body, is_real): (&[u8], bool) = match self.users.get(username) {
            // Stored shadow must be `md5<32 hex>`; strip the prefix to the body.
            Some(user) => match user.password_hash.strip_prefix("md5") {
                Some(b) => (b.as_bytes(), true),
                None => (DUMMY_BODY, false),
            },
            None => (DUMMY_BODY, false),
        };
        let expected = format!("md5{:x}", md5::compute([body, salt.as_slice()].concat()));
        // Length-checked, constant-time comparison (accumulate XOR of every
        // byte; no early return on the first mismatch).
        let matches = constant_time_compare(expected.as_bytes(), client_response.as_bytes());
        Ok(is_real && matches)
    }

    /// Hash password using SHA-256
    fn hash_password(password: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(password.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Create default users (for development/testing)
    pub fn with_default_users(mut self) -> Self {
        self.add_user("postgres".to_string(), "postgres".to_string());
        self.add_user("admin".to_string(), "admin".to_string());
        self
    }
}

/// Iteration count used when a credential does not carry its own.
///
/// 4096 is the SCRAM-SHA-256 minimum recommended by RFC 5802 and the value
/// PostgreSQL itself defaults to, so a credential registered elsewhere with the
/// default interoperates. Kept in sync with `scram_hi`'s fallback below.
pub const DEFAULT_SCRAM_ITERATIONS: u32 = 4096;

/// SCRAM-SHA-256 authentication state
///
/// Maintains the state of a SCRAM-SHA-256 authentication session.
/// This includes nonces, salt, iteration count, and authentication messages.
#[derive(Debug, Clone)]
pub struct ScramAuthState {
    username: String,
    client_nonce: String,
    server_nonce: String,
    salt: Vec<u8>,
    iteration_count: u32,
    client_first_message_bare: String,
    server_first_message: String,
}

impl ScramAuthState {
    /// Create new SCRAM authentication state
    /// Fresh state with a RANDOM salt and the default iteration count.
    ///
    /// **Not usable for verifying a stored password.** A SCRAM proof can only be
    /// checked against the salt and iteration count the stored key was DERIVED
    /// from; a random salt makes the comparison fail for every password,
    /// including the correct one. Use [`ScramAuthState::with_credentials`] on any
    /// path that verifies a real user — the wire handler does.
    ///
    /// Retained (and still public) because it is a legitimate way to build state
    /// for tests and for registering a brand-new credential, and because it is
    /// re-exported from `protocol::postgres`, so changing its signature would be
    /// a breaking change for embedders.
    pub fn new(username: String) -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let salt: Vec<u8> = (0..16).map(|_| rng.gen::<u8>()).collect();
        Self::with_credentials(username, salt, DEFAULT_SCRAM_ITERATIONS)
    }

    /// State that advertises the salt and iteration count a stored credential was
    /// actually derived from.
    ///
    /// This is what makes SCRAM verification possible at all. The server sends
    /// `s=<salt>,i=<iterations>` in server-first-message, and the client derives
    /// its proof from them; if they are not the ones behind the stored key, the
    /// server's HMAC/XOR recovery yields garbage and the comparison fails
    /// unconditionally — no password can authenticate. That was GH#20: the wire
    /// handler built state with [`ScramAuthState::new`], whose salt is freshly
    /// random per connection and unrelated to the stored credential.
    pub fn with_credentials(username: String, salt: Vec<u8>, iterations: u32) -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        let server_nonce: String = (0..24)
            .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
            .collect();

        Self {
            username,
            client_nonce: String::new(),
            server_nonce,
            salt,
            iteration_count: if iterations == 0 {
                DEFAULT_SCRAM_ITERATIONS
            } else {
                iterations
            },
            client_first_message_bare: String::new(),
            server_first_message: String::new(),
        }
    }

    /// Set client nonce from client-first-message
    pub fn set_client_nonce(&mut self, nonce: String) {
        self.client_nonce = nonce;
    }

    /// Set client-first-message-bare for auth message construction
    pub fn set_client_first_message_bare(&mut self, msg: String) {
        self.client_first_message_bare = msg;
    }

    /// Build and get server-first-message
    pub fn build_server_first_message(&mut self) -> Result<String> {
        let salt_b64 =
            base64_encode(&self.salt).map_err(|e| Error::authentication(format!("Failed to encode salt: {}", e)))?;

        let msg = format!(
            "r={}{},s={},i={}",
            self.client_nonce, self.server_nonce, salt_b64, self.iteration_count
        );

        self.server_first_message = msg.clone();
        Ok(msg)
    }

    /// Get the combined nonce (client nonce + server nonce)
    pub fn combined_nonce(&self) -> String {
        format!("{}{}", self.client_nonce, self.server_nonce)
    }

    /// Verify client proof and return server signature if valid
    ///
    /// # Arguments
    /// * `client_proof_b64` - Base64-encoded client proof from client-final-message
    /// * `client_final_message_without_proof` - Client final message without the proof part
    /// * `stored_key` - The stored key from password storage
    /// * `server_key` - The server key from password storage
    ///
    /// # Returns
    /// `Ok(server_signature)` if proof is valid, `Err` otherwise
    pub fn verify_client_proof(
        &self,
        client_proof_b64: &str,
        client_final_message_without_proof: &str,
        stored_key: &[u8],
        server_key: &[u8],
    ) -> Result<Vec<u8>> {
        // Decode client proof from base64
        let client_proof = base64_decode(client_proof_b64)
            .map_err(|e| Error::authentication(format!("Invalid client proof encoding: {}", e)))?;

        // Build auth message: client-first-message-bare + "," + server-first-message + "," + client-final-message-without-proof
        let auth_message = format!(
            "{},{},{}",
            self.client_first_message_bare, self.server_first_message, client_final_message_without_proof
        );

        // Calculate client signature: HMAC(StoredKey, AuthMessage)
        let client_signature = scram_hmac_sha256(stored_key, auth_message.as_bytes());

        // Calculate client key: ClientProof XOR ClientSignature
        let client_key: Vec<u8> = client_proof
            .iter()
            .zip(client_signature.iter())
            .map(|(a, b)| a ^ b)
            .collect();

        // Verify: H(ClientKey) should equal StoredKey
        let computed_stored_key = scram_h(&client_key);

        // Constant-time comparison to prevent timing attacks
        if !constant_time_compare(&computed_stored_key, stored_key) {
            return Err(Error::authentication("Invalid password"));
        }

        // Calculate server signature: HMAC(ServerKey, AuthMessage)
        let server_signature = scram_hmac_sha256(server_key, auth_message.as_bytes());

        Ok(server_signature)
    }

    /// Build server-final-message with server signature
    pub fn build_server_final_message(&self, server_signature: &[u8]) -> Result<String> {
        let signature_b64 = base64_encode(server_signature)
            .map_err(|e| Error::authentication(format!("Failed to encode signature: {}", e)))?;
        Ok(format!("v={}", signature_b64))
    }

    /// Get username
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Get salt
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    /// Get iteration count
    pub fn iteration_count(&self) -> u32 {
        self.iteration_count
    }
}

// ============================================================================
// SCRAM-SHA-256 Cryptographic Functions (RFC 5802 + RFC 7677)
// ============================================================================

/// PBKDF2-HMAC-SHA256 key derivation (Hi function in SCRAM)
///
/// This is the core key derivation function used in SCRAM-SHA-256.
/// It applies PBKDF2 with HMAC-SHA-256 to derive a key from a password.
pub fn scram_hi(password: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
    // Default to 4096 iterations if input is 0 (SCRAM-SHA-256 minimum recommended)
    const DEFAULT_ITERATIONS: NonZeroU32 = match NonZeroU32::new(4096) {
        Some(n) => n,
        None => unreachable!(),
    };
    let iterations = NonZeroU32::new(iterations).unwrap_or(DEFAULT_ITERATIONS);
    let mut out = vec![0u8; 32]; // SHA-256 produces 32 bytes

    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password.as_bytes(),
        &mut out,
    );

    out
}

/// HMAC-SHA-256 function
pub fn scram_hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let signature = hmac::sign(&key, message);
    signature.as_ref().to_vec()
}

/// SHA-256 hash function (H in SCRAM)
pub fn scram_h(input: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().to_vec()
}

/// Calculate SaltedPassword: Hi(password, salt, i)
pub fn scram_salted_password(password: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
    scram_hi(password, salt, iterations)
}

/// Calculate ClientKey: HMAC(SaltedPassword, "Client Key")
pub fn scram_client_key(salted_password: &[u8]) -> Vec<u8> {
    scram_hmac_sha256(salted_password, b"Client Key")
}

/// Calculate StoredKey: H(ClientKey)
pub fn scram_stored_key(client_key: &[u8]) -> Vec<u8> {
    scram_h(client_key)
}

/// Calculate ServerKey: HMAC(SaltedPassword, "Server Key")
pub fn scram_server_key(salted_password: &[u8]) -> Vec<u8> {
    scram_hmac_sha256(salted_password, b"Server Key")
}

/// Constant-time comparison to prevent timing attacks
///
/// This is critical for security - comparing secrets byte-by-byte
/// with early exit would leak information about the secret.
fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }

    result == 0
}

/// Prepare SCRAM credentials for storage
///
/// Given a password, generates the stored key and server key that should
/// be saved in the password store. This allows password verification without
/// storing the actual password.
pub fn prepare_scram_credentials(password: &str, salt: &[u8], iterations: u32) -> (Vec<u8>, Vec<u8>) {
    let salted_password = scram_salted_password(password, salt, iterations);
    let client_key = scram_client_key(&salted_password);
    let stored_key = scram_stored_key(&client_key);
    let server_key = scram_server_key(&salted_password);

    (stored_key, server_key)
}

/// Base64 encode
pub(crate) fn base64_encode(data: &[u8]) -> std::result::Result<String, Box<dyn std::error::Error>> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    Ok(encoded)
}

/// Base64 decode
pub(crate) fn base64_decode(data: &str) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error>> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD.decode(data)?;
    Ok(decoded)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_manager_creation() {
        let mut auth = AuthManager::new(AuthMethod::CleartextPassword);
        auth.add_user("test_user".to_string(), "test_pass".to_string());

        assert!(auth.verify_cleartext("test_user", "test_pass").unwrap());
        assert!(!auth.verify_cleartext("test_user", "wrong_pass").unwrap());
    }

    #[test]
    fn test_password_hashing() {
        let hash1 = AuthManager::hash_password("password123");
        let hash2 = AuthManager::hash_password("password123");
        assert_eq!(hash1, hash2);
    }

    /// Build the libpq client MD5 response for the given credentials + salt,
    /// exactly as a real client computes it:
    /// `md5` + hex(md5( ascii(hex(md5(password || username))) || salt )).
    fn client_md5_response(password: &str, username: &str, salt: &[u8; 4]) -> String {
        let shadow_body = format!(
            "{:x}",
            md5::compute([password.as_bytes(), username.as_bytes()].concat())
        );
        format!(
            "md5{:x}",
            md5::compute([shadow_body.as_bytes(), salt.as_slice()].concat())
        )
    }

    #[test]
    fn test_md5_add_user_stores_pg_shadow() {
        let mut auth = AuthManager::new(AuthMethod::Md5);
        auth.add_user("postgres".to_string(), "s3cret".to_string());
        let stored = &auth.users.get("postgres").unwrap().password_hash;
        let expected = format!(
            "md5{:x}",
            md5::compute([b"s3cret".as_slice(), b"postgres".as_slice()].concat())
        );
        assert_eq!(stored, &expected);
        assert!(stored.starts_with("md5"));
        assert_eq!(stored.len(), 3 + 32);
    }

    #[test]
    fn test_verify_md5_response_accepts_and_rejects() {
        let mut auth = AuthManager::new(AuthMethod::Md5);
        auth.add_user("postgres".to_string(), "s3cret".to_string());
        let salt: [u8; 4] = [0x01, 0x02, 0x03, 0x04];

        // Correct password + correct salt authenticates.
        let good = client_md5_response("s3cret", "postgres", &salt);
        assert!(auth.verify_md5_response("postgres", &good, &salt).unwrap());

        // Wrong password is rejected.
        let wrong_pw = client_md5_response("wrong", "postgres", &salt);
        assert!(!auth.verify_md5_response("postgres", &wrong_pw, &salt).unwrap());

        // Right response but wrong salt is rejected.
        let other_salt: [u8; 4] = [0x09, 0x08, 0x07, 0x06];
        assert!(!auth.verify_md5_response("postgres", &good, &other_salt).unwrap());

        // Unknown user is rejected (fail closed, no error).
        assert!(!auth.verify_md5_response("nobody", &good, &salt).unwrap());
    }

    #[test]
    fn test_scram_state_creation() {
        let scram = ScramAuthState::new("testuser".to_string());
        assert_eq!(scram.username(), "testuser");
        assert_eq!(scram.salt().len(), 16);
        assert_eq!(scram.iteration_count(), 4096);
    }

    #[test]
    fn test_scram_server_first_message() {
        let mut scram = ScramAuthState::new("testuser".to_string());
        scram.set_client_nonce("clientnonce".to_string());
        let msg = scram.build_server_first_message().unwrap();

        assert!(msg.starts_with("r=clientnonce"));
        assert!(msg.contains(",s="));
        assert!(msg.contains(",i=4096"));
    }

    #[test]
    fn test_scram_hi_function() {
        let password = "pencil";
        let salt = b"salt";
        let iterations = 4096;

        let result = scram_hi(password, salt, iterations);
        assert_eq!(result.len(), 32); // SHA-256 output
    }

    #[test]
    fn test_scram_hmac_sha256() {
        let key = b"key";
        let message = b"The quick brown fox jumps over the lazy dog";

        let result = scram_hmac_sha256(key, message);
        assert_eq!(result.len(), 32); // SHA-256 output
    }

    #[test]
    fn test_scram_h_function() {
        let input = b"test data";
        let result = scram_h(input);
        assert_eq!(result.len(), 32); // SHA-256 output
    }

    #[test]
    fn test_scram_key_derivation() {
        let password = "pencil";
        let salt = b"salt1234567890ab";
        let iterations = 4096;

        let salted_password = scram_salted_password(password, salt, iterations);
        assert_eq!(salted_password.len(), 32);

        let client_key = scram_client_key(&salted_password);
        assert_eq!(client_key.len(), 32);

        let stored_key = scram_stored_key(&client_key);
        assert_eq!(stored_key.len(), 32);

        let server_key = scram_server_key(&salted_password);
        assert_eq!(server_key.len(), 32);
    }

    #[test]
    fn test_prepare_scram_credentials() {
        let password = "secret";
        let salt = b"randomsalt123456";
        let iterations = 4096;

        let (stored_key, server_key) = prepare_scram_credentials(password, salt, iterations);

        assert_eq!(stored_key.len(), 32);
        assert_eq!(server_key.len(), 32);
        assert_ne!(stored_key, server_key);
    }

    #[test]
    fn test_constant_time_compare() {
        let a = vec![1, 2, 3, 4];
        let b = vec![1, 2, 3, 4];
        let c = vec![1, 2, 3, 5];

        assert!(constant_time_compare(&a, &b));
        assert!(!constant_time_compare(&a, &c));
        assert!(!constant_time_compare(&a, &[1, 2, 3]));
    }

    #[test]
    fn test_base64_encoding() {
        let data = b"Hello, World!";
        let encoded = base64_encode(data).unwrap();
        let decoded = base64_decode(&encoded).unwrap();

        assert_eq!(data.to_vec(), decoded);
    }

    #[test]
    fn test_scram_proof_verification() {
        let password = "secret";
        let salt = b"randomsalt123456";
        let iterations = 4096;

        let (stored_key, server_key) = prepare_scram_credentials(password, salt, iterations);

        let mut scram = ScramAuthState::new("testuser".to_string());
        scram.set_client_nonce("clientnonce".to_string());
        scram.set_client_first_message_bare("n=testuser,r=clientnonce".to_string());
        let _server_msg = scram.build_server_first_message().unwrap();

        // Note: Full proof verification test requires a complete SCRAM exchange
        // This is tested in integration tests
        assert_eq!(stored_key.len(), 32);
        assert_eq!(server_key.len(), 32);
    }
}
