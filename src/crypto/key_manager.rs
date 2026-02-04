//! Encryption key management
//!
//! Provides secure key lifecycle management including:
//! - Loading keys from various sources (environment, file, password)
//! - Secure in-memory key storage (zeroed on drop)
//! - Key derivation from passwords
//! - Key rotation support (planned for phase 2)

use crate::config::KeySource;
use crate::crypto::{derive_key_from_password, EncryptionKey};
use crate::{Error, Result};
use std::path::Path;
use zeroize::Zeroizing;

/// Key rotation metadata
#[derive(Debug, Clone)]
pub struct KeyRotationMetadata {
    /// Timestamp of rotation
    pub rotated_at: String,
    /// Previous key ID (hash of old key)
    pub previous_key_id: String,
    /// Current key ID (hash of new key)
    pub current_key_id: String,
    /// Status (pending, completed, failed)
    pub status: String,
}

/// Key manager for secure encryption key handling
pub struct KeyManager {
    /// Active encryption key (zeroed on drop)
    key: Zeroizing<EncryptionKey>,
    /// Previous key (for decrypting legacy data during rotation)
    previous_key: Option<Zeroizing<EncryptionKey>>,
    /// Key source for auditing
    source: KeySource,
    /// Rotation metadata
    rotation_metadata: Option<KeyRotationMetadata>,
}

impl KeyManager {
    /// Create a new key manager from a key source
    pub fn from_source(source: &KeySource) -> Result<Self> {
        let key = Self::load_key_from_source(source)?;

        Ok(Self {
            key: Zeroizing::new(key),
            previous_key: None,
            source: source.clone(),
            rotation_metadata: None,
        })
    }

    /// Load key from the configured source
    fn load_key_from_source(source: &KeySource) -> Result<EncryptionKey> {
        match source {
            KeySource::Environment(var_name) => Self::load_from_env(var_name),
            KeySource::File(path) => Self::load_from_file(path),
            KeySource::Kms { provider, key_id } => {
                // KMS integration planned for future
                Err(Error::encryption(format!(
                    "KMS provider '{}' not yet supported (key_id: {})",
                    provider, key_id
                )))
            }
        }
    }

    /// Load key from environment variable
    fn load_from_env(var_name: &str) -> Result<EncryptionKey> {
        let key_str = std::env::var(var_name).map_err(|_| {
            Error::encryption(format!(
                "Environment variable '{}' not found. \
                Set it with a 64-character hex string (32 bytes).",
                var_name
            ))
        })?;

        Self::parse_hex_key(&key_str)
    }

    /// Load key from file
    fn load_from_file(path: &Path) -> Result<EncryptionKey> {
        let key_bytes = std::fs::read(path).map_err(|e| {
            Error::encryption(format!(
                "Failed to read key file '{}': {}",
                path.display(),
                e
            ))
        })?;

        // Support both raw binary (32 bytes) and hex-encoded files
        if key_bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&key_bytes);
            Ok(key)
        } else {
            // Try to parse as hex string
            let key_str = String::from_utf8(key_bytes).map_err(|_| {
                Error::encryption("Key file must contain either 32 raw bytes or 64 hex characters")
            })?;
            Self::parse_hex_key(key_str.trim())
        }
    }

    /// Parse a hex-encoded key string
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

    /// Create key manager from password with salt
    pub fn from_password(password: &str, salt: &[u8]) -> Result<Self> {
        if salt.len() < 16 {
            return Err(Error::encryption("Salt must be at least 16 bytes"));
        }

        let key = derive_key_from_password(password, salt)?;

        Ok(Self {
            key: Zeroizing::new(key),
            previous_key: None,
            source: KeySource::Environment("password-derived".to_string()),
            rotation_metadata: None,
        })
    }

    /// Get a reference to the encryption key
    pub fn key(&self) -> &EncryptionKey {
        &self.key
    }

    /// Get the key source (for auditing)
    pub fn source(&self) -> &KeySource {
        &self.source
    }

    /// Generate a new random key (for key rotation or initial setup)
    pub fn generate_random() -> Self {
        let key: EncryptionKey = rand::random();

        Self {
            key: Zeroizing::new(key),
            previous_key: None,
            source: KeySource::Environment("generated".to_string()),
            rotation_metadata: None,
        }
    }

    /// Export key as hex string (use carefully!)
    pub fn export_as_hex(&self) -> String {
        hex::encode(&*self.key)
    }

    /// Rotate to a new key
    ///
    /// This performs the following:
    /// 1. Stores current key as previous key (for legacy data decryption)
    /// 2. Sets the new key as active
    /// 3. Records rotation metadata with timestamp
    ///
    /// Note: The actual data re-encryption must be handled by the caller
    /// (requires iterating through all data and re-encrypting with new key)
    pub fn rotate(&mut self, new_key: EncryptionKey) -> Result<()> {
        use chrono::Utc;

        // Generate key IDs (SHA256 hashes of keys)
        let old_key_id = Self::compute_key_id(&*self.key);
        let new_key_id = Self::compute_key_id(&new_key);

        // Store current key as backup for legacy data decryption
        self.previous_key = Some(self.key.clone());

        // Activate new key
        self.key = Zeroizing::new(new_key);

        // Record rotation metadata
        self.rotation_metadata = Some(KeyRotationMetadata {
            rotated_at: Utc::now().to_rfc3339(),
            previous_key_id: old_key_id,
            current_key_id: new_key_id,
            status: "completed".to_string(),
        });

        Ok(())
    }

    /// Compute a key ID (hash) for audit purposes
    fn compute_key_id(key: &EncryptionKey) -> String {
        use sha2::{Sha256, Digest};

        let mut hasher = Sha256::new();
        hasher.update(key);
        let result = hasher.finalize();

        hex::encode(&result[0..8]) // Use first 64 bits for ID
    }

    /// Get previous key for legacy data decryption during rotation
    pub fn previous_key(&self) -> Option<&EncryptionKey> {
        self.previous_key.as_ref().map(|k| &**k)
    }

    /// Get rotation metadata
    pub fn rotation_metadata(&self) -> Option<&KeyRotationMetadata> {
        self.rotation_metadata.as_ref()
    }
}

// Ensure key is zeroed when dropped
impl Drop for KeyManager {
    fn drop(&mut self) {
        // Zeroizing handles zeroing automatically
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_random_key() {
        let km = KeyManager::generate_random();
        assert_eq!(km.key().len(), 32);
    }

    #[test]
    fn test_from_password() {
        let password = "test_password_123";
        let salt = b"random_salt_1234";

        let km = KeyManager::from_password(password, salt)
            .expect("Failed to create key manager from password");

        assert_eq!(km.key().len(), 32);
    }

    #[test]
    fn test_parse_hex_key() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let key = KeyManager::parse_hex_key(hex_key)
            .expect("Failed to parse hex key");

        assert_eq!(key.len(), 32);
        assert_eq!(key[0], 0x01);
        assert_eq!(key[1], 0x23);
    }

    #[test]
    fn test_export_as_hex() {
        let km = KeyManager::generate_random();
        let hex = km.export_as_hex();

        assert_eq!(hex.len(), 64);

        // Verify it's valid hex
        let _parsed = KeyManager::parse_hex_key(&hex)
            .expect("Failed to parse exported hex");
    }

    #[test]
    fn test_load_from_env() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::env::set_var("TEST_HELIOSDB_KEY", hex_key);

        let key = KeyManager::load_from_env("TEST_HELIOSDB_KEY")
            .expect("Failed to load key from env");

        assert_eq!(key.len(), 32);

        std::env::remove_var("TEST_HELIOSDB_KEY");
    }

    #[test]
    fn test_key_manager_from_env_source() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::env::set_var("TEST_HELIOSDB_KEY_2", hex_key);

        let source = KeySource::Environment("TEST_HELIOSDB_KEY_2".to_string());
        let km = KeyManager::from_source(&source)
            .expect("Failed to create key manager from source");

        assert_eq!(km.key().len(), 32);

        std::env::remove_var("TEST_HELIOSDB_KEY_2");
    }
}
