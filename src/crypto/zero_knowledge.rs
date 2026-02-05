//! Zero-Knowledge Encryption (ZKE) implementation
//!
//! Zero-Knowledge Encryption ensures that encryption keys **never leave the client**.
//! The server only ever sees encrypted data and cannot decrypt it without the client
//! providing the key for each request.
//!
//! # Architecture
//!
//! ```text
//! Client                              Server
//!   |                                   |
//!   | key = derive(password, salt)      |
//!   | encrypted_query = encrypt(sql)    |
//!   |                                   |
//!   |--- encrypted_query + key_hash --->|
//!   |                                   |
//!   |                    Validate key_hash
//!   |                    Execute on encrypted data
//!   |                    Encrypt response
//!   |                                   |
//!   |<--- encrypted_response -----------|
//!   |                                   |
//!   | plaintext = decrypt(response)     |
//! ```
//!
//! # Security Properties
//!
//! - **Client-Side Encryption**: All data encrypted before transmission
//! - **Per-Request Keys**: Keys provided with each request, never stored
//! - **Key Hash Validation**: Server validates key via SHA-256 hash
//! - **Nonce-Based Replay Protection**: Each request has unique nonce
//! - **Memory Zeroization**: Keys zeroed immediately after use
//!
//! # Example
//!
//! ```rust,ignore
//! use heliosdb_lite::crypto::{ZeroKnowledgeSession, ZkeKeyDerivation};
//!
//! // Client-side: Derive separate keys for auth and encryption
//! let keys = ZkeKeyDerivation::derive_keys("password", "user@example.com")?;
//!
//! // Create session with encryption key (deref Zeroizing wrapper)
//! let session = ZeroKnowledgeSession::new(*keys.encryption_key)?;
//!
//! // Encrypt data before sending
//! let encrypted = session.encrypt(b"SELECT * FROM users")?;
//!
//! // Server validates and processes, returns encrypted_response...
//! // Client decrypts response
//! let decrypted = session.decrypt(&encrypted)?;
//! # Ok::<(), heliosdb_lite::Error>(())
//! ```

use crate::crypto::{derive_key_from_password, encrypt, decrypt, EncryptionKey};
use crate::{Error, Result};
use sha2::{Sha256, Digest};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

/// Zero-Knowledge Encryption mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZkeMode {
    /// Full zero-knowledge: server never sees plaintext
    /// - Client encrypts data before sending
    /// - Server stores only ciphertext
    /// - No server-side search capabilities
    Full,

    /// Hybrid mode: metadata unencrypted, data encrypted
    /// - Table/column names visible to server
    /// - Row data encrypted
    /// - Basic metadata search possible
    Hybrid,

    /// Per-request decryption: key provided per-request
    /// - Server temporarily decrypts for query execution
    /// - Key immediately zeroized after use
    /// - Full SQL capabilities
    PerRequest,
}

impl Default for ZkeMode {
    fn default() -> Self {
        Self::PerRequest
    }
}

/// Zero-Knowledge Encryption configuration
#[derive(Debug, Clone)]
pub struct ZkeConfig {
    /// ZKE mode
    pub mode: ZkeMode,
    /// Require key hash validation
    pub require_key_hash: bool,
    /// Enable replay protection
    pub replay_protection: bool,
    /// Nonce validity window (seconds)
    pub nonce_window_secs: u64,
    /// Maximum cached nonces (for replay protection)
    pub max_cached_nonces: usize,
}

impl Default for ZkeConfig {
    fn default() -> Self {
        Self {
            mode: ZkeMode::PerRequest,
            require_key_hash: true,
            replay_protection: true,
            nonce_window_secs: 300, // 5 minutes
            max_cached_nonces: 10000,
        }
    }
}

/// Derived keys for Zero-Knowledge operations
///
/// Separates authentication from encryption:
/// - `auth_key`: Used for login/authentication (can be sent to server as hash)
/// - `encryption_key`: Used for data encryption (NEVER sent to server)
#[derive(Clone)]
pub struct ZkeDerivedKeys {
    /// Authentication key (for server authentication)
    pub auth_key: Zeroizing<EncryptionKey>,
    /// Encryption key (never leaves client)
    pub encryption_key: Zeroizing<EncryptionKey>,
    /// Key hash for validation
    pub encryption_key_hash: [u8; 32],
}

impl ZkeDerivedKeys {
    /// Get the encryption key hash as hex string
    pub fn key_hash_hex(&self) -> String {
        hex::encode(self.encryption_key_hash)
    }
}

/// Client-side key derivation for ZKE
pub struct ZkeKeyDerivation;

impl ZkeKeyDerivation {
    /// Derive separate authentication and encryption keys from password
    ///
    /// This implements the recommended pattern where:
    /// - Auth key: Derived with "auth" salt suffix (sent to server for login)
    /// - Encryption key: Derived with "encrypt" salt suffix (NEVER sent to server)
    ///
    /// # Arguments
    ///
    /// * `password` - User's password
    /// * `identifier` - Unique identifier (email, username, etc.) used as salt base
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use heliosdb_lite::crypto::ZkeKeyDerivation;
    ///
    /// let keys = ZkeKeyDerivation::derive_keys("my_password", "user@example.com")?;
    /// // keys.auth_key - for authentication
    /// // keys.encryption_key - for encryption (never send to server!)
    /// # Ok::<(), heliosdb_lite::Error>(())
    /// ```
    pub fn derive_keys(password: &str, identifier: &str) -> Result<ZkeDerivedKeys> {
        // Create deterministic salt from identifier
        let base_salt = Self::create_salt(identifier);

        // Derive auth key (salt + "auth")
        let mut auth_salt = base_salt.clone();
        auth_salt.extend_from_slice(b"auth");
        let auth_key = derive_key_from_password(password, &auth_salt)?;

        // Derive encryption key (salt + "encrypt")
        let mut encrypt_salt = base_salt;
        encrypt_salt.extend_from_slice(b"encrypt");
        let encryption_key = derive_key_from_password(password, &encrypt_salt)?;

        // Compute key hash for validation
        let encryption_key_hash = Self::compute_key_hash(&encryption_key);

        Ok(ZkeDerivedKeys {
            auth_key: Zeroizing::new(auth_key),
            encryption_key: Zeroizing::new(encryption_key),
            encryption_key_hash,
        })
    }

    /// Create salt from identifier using SHA-256
    fn create_salt(identifier: &str) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(identifier.as_bytes());
        hasher.update(b"heliosdb.zke.salt");
        hasher.finalize().to_vec()
    }

    /// Compute SHA-256 hash of encryption key
    pub fn compute_key_hash(key: &EncryptionKey) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(key);
        let result = hasher.finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&result);
        hash
    }
}

/// Per-request Zero-Knowledge session
///
/// Holds the encryption key for a single request/session.
/// Key is automatically zeroed when session is dropped.
pub struct ZeroKnowledgeSession {
    /// Encryption key (zeroed on drop)
    key: Zeroizing<EncryptionKey>,
    /// Key hash for validation
    key_hash: [u8; 32],
    /// Creation timestamp
    created_at: Instant,
    /// Request nonce (for replay protection)
    nonce: Option<[u8; 16]>,
}

impl ZeroKnowledgeSession {
    /// Create a new ZKE session with the provided key
    pub fn new(key: EncryptionKey) -> Result<Self> {
        let key_hash = ZkeKeyDerivation::compute_key_hash(&key);

        Ok(Self {
            key: Zeroizing::new(key),
            key_hash,
            created_at: Instant::now(),
            nonce: None,
        })
    }

    /// Create session from hex-encoded key
    pub fn from_hex_key(hex_key: &str) -> Result<Self> {
        let key = Self::parse_hex_key(hex_key)?;
        Self::new(key)
    }

    /// Create session from derived keys
    pub fn from_derived_keys(keys: &ZkeDerivedKeys) -> Result<Self> {
        Ok(Self {
            key: keys.encryption_key.clone(),
            key_hash: keys.encryption_key_hash,
            created_at: Instant::now(),
            nonce: None,
        })
    }

    /// Set request nonce for replay protection
    pub fn with_nonce(mut self, nonce: [u8; 16]) -> Self {
        self.nonce = Some(nonce);
        self
    }

    /// Generate and set a random nonce
    pub fn with_random_nonce(mut self) -> Self {
        self.nonce = Some(rand::random());
        self
    }

    /// Get the request nonce
    pub fn nonce(&self) -> Option<&[u8; 16]> {
        self.nonce.as_ref()
    }

    /// Get nonce as hex string
    pub fn nonce_hex(&self) -> Option<String> {
        self.nonce.map(|n| hex::encode(n))
    }

    /// Encrypt data using the session key
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        encrypt(&self.key, plaintext)
    }

    /// Decrypt data using the session key
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        decrypt(&self.key, ciphertext)
    }

    /// Get the encryption key (for internal use)
    pub fn key(&self) -> &EncryptionKey {
        &self.key
    }

    /// Get the key hash
    pub fn key_hash(&self) -> &[u8; 32] {
        &self.key_hash
    }

    /// Get key hash as hex string
    pub fn key_hash_hex(&self) -> String {
        hex::encode(self.key_hash)
    }

    /// Validate that provided hash matches the session key
    pub fn validate_key_hash(&self, expected_hash: &[u8; 32]) -> bool {
        // Constant-time comparison to prevent timing attacks
        constant_time_compare(&self.key_hash, expected_hash)
    }

    /// Validate hex-encoded hash
    pub fn validate_key_hash_hex(&self, expected_hash_hex: &str) -> Result<bool> {
        let expected = hex::decode(expected_hash_hex)
            .map_err(|e| Error::encryption(format!("Invalid hash hex: {}", e)))?;

        if expected.len() != 32 {
            return Err(Error::encryption("Hash must be 32 bytes"));
        }

        let mut hash = [0u8; 32];
        hash.copy_from_slice(&expected);

        Ok(self.validate_key_hash(&hash))
    }

    /// Session age in seconds
    pub fn age_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }

    /// Parse hex-encoded key
    fn parse_hex_key(hex_str: &str) -> Result<EncryptionKey> {
        let hex_str = hex_str.trim();

        if hex_str.len() != 64 {
            return Err(Error::encryption(format!(
                "Hex key must be 64 characters (32 bytes), got {}",
                hex_str.len()
            )));
        }

        let mut key = [0u8; 32];
        for (i, chunk) in hex_str.as_bytes().chunks(2).enumerate() {
            let hex_byte = std::str::from_utf8(chunk)
                .map_err(|_| Error::encryption("Invalid hex string"))?;
            key[i] = u8::from_str_radix(hex_byte, 16)
                .map_err(|_| Error::encryption(format!("Invalid hex byte: {}", hex_byte)))?;
        }

        Ok(key)
    }
}

/// Constant-time comparison to prevent timing attacks
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

/// Nonce tracker for replay protection
pub struct NonceTracker {
    /// Seen nonces with timestamps
    nonces: RwLock<HashSet<[u8; 16]>>,
    /// Nonce expiration timestamps
    expiry: RwLock<Vec<(Instant, [u8; 16])>>,
    /// Validity window
    window: Duration,
    /// Maximum cached nonces
    max_nonces: usize,
}

impl NonceTracker {
    /// Create a new nonce tracker
    pub fn new(window_secs: u64, max_nonces: usize) -> Self {
        Self {
            nonces: RwLock::new(HashSet::new()),
            expiry: RwLock::new(Vec::new()),
            window: Duration::from_secs(window_secs),
            max_nonces,
        }
    }

    /// Check if nonce is valid (not seen before)
    /// Returns true if valid, false if replay detected
    pub fn check_and_record(&self, nonce: &[u8; 16]) -> bool {
        // Clean up expired nonces first
        self.cleanup_expired();

        let mut nonces = self.nonces.write().unwrap_or_else(|e| e.into_inner());
        let mut expiry = self.expiry.write().unwrap_or_else(|e| e.into_inner());

        // Check if at capacity
        if nonces.len() >= self.max_nonces {
            // Force cleanup of oldest entries
            self.force_cleanup(&mut nonces, &mut expiry);
        }

        // Check if nonce was already seen
        if nonces.contains(nonce) {
            return false; // Replay attack!
        }

        // Record nonce
        nonces.insert(*nonce);
        expiry.push((Instant::now(), *nonce));

        true
    }

    /// Clean up expired nonces
    fn cleanup_expired(&self) {
        let now = Instant::now();
        let mut nonces = self.nonces.write().unwrap_or_else(|e| e.into_inner());
        let mut expiry = self.expiry.write().unwrap_or_else(|e| e.into_inner());

        expiry.retain(|(created, nonce)| {
            if now.duration_since(*created) > self.window {
                nonces.remove(nonce);
                false
            } else {
                true
            }
        });
    }

    /// Force cleanup when at capacity
    fn force_cleanup(
        &self,
        nonces: &mut HashSet<[u8; 16]>,
        expiry: &mut Vec<(Instant, [u8; 16])>,
    ) {
        // Remove oldest 10%
        let remove_count = self.max_nonces / 10;
        for _ in 0..remove_count {
            if let Some((_, nonce)) = expiry.first() {
                nonces.remove(nonce);
            }
            if !expiry.is_empty() {
                expiry.remove(0);
            }
        }
    }

    /// Get number of tracked nonces
    pub fn len(&self) -> usize {
        self.nonces.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Check if tracker is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for NonceTracker {
    fn default() -> Self {
        Self::new(300, 10000) // 5 minute window, 10k max
    }
}

/// Request timestamp validator
pub struct TimestampValidator {
    /// Maximum allowed clock skew (seconds)
    max_skew_secs: u64,
}

impl TimestampValidator {
    /// Create with specified max skew
    pub fn new(max_skew_secs: u64) -> Self {
        Self { max_skew_secs }
    }

    /// Validate request timestamp is within acceptable range
    pub fn validate(&self, request_timestamp_secs: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let diff = if request_timestamp_secs > now {
            request_timestamp_secs - now
        } else {
            now - request_timestamp_secs
        };

        diff <= self.max_skew_secs
    }

    /// Get current timestamp
    pub fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

impl Default for TimestampValidator {
    fn default() -> Self {
        Self::new(300) // 5 minute tolerance
    }
}

/// ZKE request context (for server-side validation)
pub struct ZkeRequestContext {
    /// Session with encryption key
    session: ZeroKnowledgeSession,
    /// Nonce tracker
    nonce_tracker: Arc<NonceTracker>,
    /// Timestamp validator
    timestamp_validator: TimestampValidator,
    /// Configuration
    config: ZkeConfig,
}

impl ZkeRequestContext {
    /// Create new request context
    pub fn new(
        session: ZeroKnowledgeSession,
        nonce_tracker: Arc<NonceTracker>,
        config: ZkeConfig,
    ) -> Self {
        Self {
            session,
            nonce_tracker,
            timestamp_validator: TimestampValidator::new(config.nonce_window_secs),
            config,
        }
    }

    /// Validate the request
    pub fn validate(
        &self,
        expected_key_hash: Option<&str>,
        nonce: Option<&[u8; 16]>,
        timestamp: Option<u64>,
    ) -> Result<()> {
        // Validate key hash if required
        if self.config.require_key_hash {
            if let Some(hash) = expected_key_hash {
                if !self.session.validate_key_hash_hex(hash)? {
                    return Err(Error::encryption("Key hash validation failed"));
                }
            } else {
                return Err(Error::encryption("Key hash required but not provided"));
            }
        }

        // Validate replay protection
        if self.config.replay_protection {
            // Check nonce
            if let Some(n) = nonce {
                if !self.nonce_tracker.check_and_record(n) {
                    return Err(Error::encryption("Replay attack detected: nonce already used"));
                }
            } else {
                return Err(Error::encryption("Nonce required for replay protection"));
            }

            // Check timestamp
            if let Some(ts) = timestamp {
                if !self.timestamp_validator.validate(ts) {
                    return Err(Error::encryption("Request timestamp out of valid range"));
                }
            }
        }

        Ok(())
    }

    /// Get the session
    pub fn session(&self) -> &ZeroKnowledgeSession {
        &self.session
    }

    /// Encrypt data
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.session.encrypt(plaintext)
    }

    /// Decrypt data
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        self.session.decrypt(ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_keys() {
        let keys = ZkeKeyDerivation::derive_keys("password123", "user@example.com")
            .expect("Key derivation failed");

        assert_eq!(keys.auth_key.len(), 32);
        assert_eq!(keys.encryption_key.len(), 32);
        assert_eq!(keys.encryption_key_hash.len(), 32);

        // Auth and encryption keys should be different
        assert_ne!(*keys.auth_key, *keys.encryption_key);
    }

    #[test]
    fn test_derive_keys_deterministic() {
        let keys1 = ZkeKeyDerivation::derive_keys("password", "user@test.com").unwrap();
        let keys2 = ZkeKeyDerivation::derive_keys("password", "user@test.com").unwrap();

        // Same inputs should produce same outputs
        assert_eq!(*keys1.auth_key, *keys2.auth_key);
        assert_eq!(*keys1.encryption_key, *keys2.encryption_key);
        assert_eq!(keys1.encryption_key_hash, keys2.encryption_key_hash);
    }

    #[test]
    fn test_derive_keys_different_passwords() {
        let keys1 = ZkeKeyDerivation::derive_keys("password1", "user@test.com").unwrap();
        let keys2 = ZkeKeyDerivation::derive_keys("password2", "user@test.com").unwrap();

        // Different passwords should produce different keys
        assert_ne!(*keys1.encryption_key, *keys2.encryption_key);
    }

    #[test]
    fn test_zke_session_encrypt_decrypt() {
        let keys = ZkeKeyDerivation::derive_keys("test", "test@test.com").unwrap();
        let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

        let plaintext = b"SELECT * FROM secret_data";
        let ciphertext = session.encrypt(plaintext).unwrap();
        let decrypted = session.decrypt(&ciphertext).unwrap();

        assert_eq!(plaintext, &decrypted[..]);
    }

    #[test]
    fn test_zke_session_from_hex() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let session = ZeroKnowledgeSession::from_hex_key(hex_key).unwrap();

        let plaintext = b"test data";
        let ciphertext = session.encrypt(plaintext).unwrap();
        let decrypted = session.decrypt(&ciphertext).unwrap();

        assert_eq!(plaintext, &decrypted[..]);
    }

    #[test]
    fn test_key_hash_validation() {
        let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
        let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

        // Valid hash
        assert!(session.validate_key_hash(&keys.encryption_key_hash));

        // Invalid hash
        let mut wrong_hash = keys.encryption_key_hash;
        wrong_hash[0] ^= 0xFF;
        assert!(!session.validate_key_hash(&wrong_hash));
    }

    #[test]
    fn test_key_hash_hex_validation() {
        let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
        let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

        let hash_hex = keys.key_hash_hex();
        assert!(session.validate_key_hash_hex(&hash_hex).unwrap());
    }

    #[test]
    fn test_nonce_tracker() {
        let tracker = NonceTracker::new(300, 100);

        let nonce1: [u8; 16] = rand::random();
        let nonce2: [u8; 16] = rand::random();

        // First use should succeed
        assert!(tracker.check_and_record(&nonce1));
        assert!(tracker.check_and_record(&nonce2));

        // Replay should fail
        assert!(!tracker.check_and_record(&nonce1));
        assert!(!tracker.check_and_record(&nonce2));
    }

    #[test]
    fn test_timestamp_validator() {
        let validator = TimestampValidator::new(60); // 1 minute tolerance

        let now = TimestampValidator::current_timestamp();

        // Current time should be valid
        assert!(validator.validate(now));

        // 30 seconds ago should be valid
        assert!(validator.validate(now - 30));

        // 30 seconds in future should be valid
        assert!(validator.validate(now + 30));

        // 2 minutes ago should be invalid
        assert!(!validator.validate(now - 120));

        // 2 minutes in future should be invalid
        assert!(!validator.validate(now + 120));
    }

    #[test]
    fn test_session_with_nonce() {
        let key: EncryptionKey = rand::random();
        let session = ZeroKnowledgeSession::new(key)
            .unwrap()
            .with_random_nonce();

        assert!(session.nonce().is_some());
        assert!(session.nonce_hex().is_some());
    }

    #[test]
    fn test_request_context_validation() {
        let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
        let session = ZeroKnowledgeSession::from_derived_keys(&keys)
            .unwrap()
            .with_random_nonce();

        let nonce = *session.nonce().unwrap();
        let nonce_tracker = Arc::new(NonceTracker::default());
        let config = ZkeConfig::default();

        let context = ZkeRequestContext::new(session, nonce_tracker, config);

        let timestamp = TimestampValidator::current_timestamp();
        let hash_hex = keys.key_hash_hex();

        // Valid request
        assert!(context.validate(Some(&hash_hex), Some(&nonce), Some(timestamp)).is_ok());
    }

    #[test]
    fn test_constant_time_compare() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 3, 4];
        let c = [1u8, 2, 3, 5];

        assert!(constant_time_compare(&a, &b));
        assert!(!constant_time_compare(&a, &c));
    }
}
