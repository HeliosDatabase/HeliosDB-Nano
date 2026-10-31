//! Encryption key management
//!
//! Provides secure key lifecycle management including:
//! - Loading keys from various sources (environment, file, password)
//! - Secure in-memory key storage (zeroed on drop)
//! - Key derivation from passwords
//! - Key rotation support (planned for phase 2; refused while it would strand a
//!   physical standby, spec R1 §3 — [`KeyRotationStanding`])

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

/// Spec R1 §3 (plan `PHYSICAL_STANDBY_REPLICATION.md` §D5): where the
/// dataset a key seals stands in physical replication — what
/// [`KeyManager::rotate`] must know before it may change the key.
///
/// A physical standby applies its primary's write groups verbatim, sealed
/// values included, so it holds the primary's dataset key; it could follow a
/// primary that re-keys only by holding both keys while the rewritten values
/// arrive, and no standby can (there is no such capability on the wire).
/// Rotating the key therefore strands every standby of the dataset, and a
/// standby's own key is its primary's, never rotated on its own. Built only
/// by the store whose key it is (`StorageEngine::rotate_encryption_key`,
/// from its open mode and the census of the replication source serving it)
/// — its constructors are the crate's, so no caller can claim a dataset is
/// unreplicated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRotationStanding {
    kind: Standing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Standing {
    /// No physical standby holds this key: rotation is not refused here.
    Unreplicated,
    /// This dataset IS a physical standby: its key is its primary's.
    PhysicalStandby,
    /// A primary with these standbys registered (attached sessions).
    PrimaryWithStandbys(Vec<String>),
    /// A primary serving its replication port with no standby attached right
    /// now: one that is away holds the key all the same.
    PrimaryServingReplication,
}

/// What every refusal says to do instead.
const ROTATION_PROCEDURE: &str = "rotate the key on a primary that serves no physical standby (stop its \
     standbys; start it with --physical-replication=false), then rebuild each standby from it, configured with the \
     new key (`heliosdb-nano recover --from-primary --full --replace`)";

impl KeyRotationStanding {
    pub(crate) fn unreplicated() -> Self {
        Self {
            kind: Standing::Unreplicated,
        }
    }

    pub(crate) fn physical_standby() -> Self {
        Self {
            kind: Standing::PhysicalStandby,
        }
    }

    /// A primary and the standbys it has registered, as the operator reads
    /// them (node id, address, state). An empty list still refuses: the
    /// primary serves its replication port.
    pub(crate) fn primary_with_standbys(standbys: Vec<String>) -> Self {
        if standbys.is_empty() {
            return Self::primary_serving_replication();
        }
        Self {
            kind: Standing::PrimaryWithStandbys(standbys),
        }
    }

    pub(crate) fn primary_serving_replication() -> Self {
        Self {
            kind: Standing::PrimaryServingReplication,
        }
    }

    /// Why rotating the key now would strand a physical standby, naming what
    /// to do instead; `None` when it would strand none.
    pub fn refusal(&self) -> Option<String> {
        match &self.kind {
            Standing::Unreplicated => None,
            Standing::PhysicalStandby => Some(format!(
                "encryption key rotation refused: this dataset is a physical standby; its key is its primary's (it \
                 applies the primary's sealed values verbatim), so it is never rotated here: {ROTATION_PROCEDURE}"
            )),
            Standing::PrimaryWithStandbys(standbys) => Some(format!(
                "encryption key rotation refused: this primary serves {} physical standby(s) — {} — which hold its \
                 current key and apply its sealed values verbatim; a rotated key would strand them (no standby can \
                 hold two keys): {ROTATION_PROCEDURE}",
                standbys.len(),
                standbys.join("; ")
            )),
            Standing::PrimaryServingReplication => Some(format!(
                "encryption key rotation refused: this primary serves its replication port, so physical standbys \
                 — attached now or away — may hold its current key, and a rotated key would strand them: \
                 {ROTATION_PROCEDURE}"
            )),
        }
    }
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
        let key_bytes = std::fs::read(path)
            .map_err(|e| Error::encryption(format!("Failed to read key file '{}': {}", path.display(), e)))?;

        // Support both raw binary (32 bytes) and hex-encoded files
        if key_bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&key_bytes);
            Ok(key)
        } else {
            // Try to parse as hex string
            let key_str = String::from_utf8(key_bytes)
                .map_err(|_| Error::encryption("Key file must contain either 32 raw bytes or 64 hex characters"))?;
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
            let hex_byte = std::str::from_utf8(chunk).map_err(|_| Error::encryption("Invalid hex string"))?;
            let dest = key
                .get_mut(i)
                .ok_or_else(|| Error::encryption("Key index out of bounds"))?;
            *dest = u8::from_str_radix(hex_byte, 16)
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
        hex::encode(&self.key)
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
    ///
    /// Spec R1 §3 (plan §D5): crate-private — a store's key is rotated only
    /// through `StorageEngine::rotate_encryption_key`, which computes the
    /// store's own [`KeyRotationStanding`] (and refuses while nothing
    /// re-encrypts the sealed values, so nothing calls this yet). It fails
    /// closed on that standing too, and nothing changes when it refuses.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rotate(&mut self, new_key: EncryptionKey, standing: &KeyRotationStanding) -> Result<()> {
        use chrono::Utc;

        if let Some(reason) = standing.refusal() {
            return Err(Error::encryption(reason));
        }

        // Generate key IDs (SHA256 hashes of keys)
        let old_key_id = Self::compute_key_id(&self.key);
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
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(key);
        let result = hasher.finalize();

        hex::encode(result.get(0..8).unwrap_or(&result)) // Use first 64 bits for ID
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

        let km = KeyManager::from_password(password, salt).expect("Failed to create key manager from password");

        assert_eq!(km.key().len(), 32);
    }

    #[test]
    fn test_parse_hex_key() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let key = KeyManager::parse_hex_key(hex_key).expect("Failed to parse hex key");

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
        let _parsed = KeyManager::parse_hex_key(&hex).expect("Failed to parse exported hex");
    }

    /// Spec R1 §3: rotation is refused on a physical standby, on a primary
    /// with standbys registered (naming them) and on a primary serving its
    /// replication port; the key is unchanged by a refusal. An unreplicated
    /// dataset rotates as before.
    #[test]
    fn rotation_is_refused_while_it_would_strand_a_physical_standby() {
        let new_key: EncryptionKey = [7; 32];
        for (standing, needle) in [
            (KeyRotationStanding::physical_standby(), "is a physical standby"),
            (
                KeyRotationStanding::primary_with_standbys(vec![
                    "node a at 10.0.0.2:41000 (streaming)".to_string(),
                    "node b at 10.0.0.3:41000 (bootstrapping)".to_string(),
                ]),
                "serves 2 physical standby(s) — node a at 10.0.0.2:41000 (streaming); node b",
            ),
            (
                KeyRotationStanding::primary_with_standbys(Vec::new()),
                "serves its replication port",
            ),
            (
                KeyRotationStanding::primary_serving_replication(),
                "serves its replication port",
            ),
        ] {
            let mut km = KeyManager::generate_random();
            let before = *km.key();
            let error = km.rotate(new_key, &standing).unwrap_err().to_string();
            assert!(error.contains(needle), "{error}");
            assert!(error.contains("recover --from-primary --full --replace"), "{error}");
            assert_eq!(*km.key(), before, "a refused rotation changes nothing");
            assert!(km.previous_key().is_none());
            assert!(km.rotation_metadata().is_none());
        }
        let mut km = KeyManager::generate_random();
        let before = *km.key();
        assert!(KeyRotationStanding::unreplicated().refusal().is_none());
        km.rotate(new_key, &KeyRotationStanding::unreplicated()).unwrap();
        assert_eq!(*km.key(), new_key);
        assert_eq!(km.previous_key(), Some(&before));
    }

    #[test]
    fn test_load_from_env() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::env::set_var("TEST_HELIOSDB_KEY", hex_key);

        let key = KeyManager::load_from_env("TEST_HELIOSDB_KEY").expect("Failed to load key from env");

        assert_eq!(key.len(), 32);

        std::env::remove_var("TEST_HELIOSDB_KEY");
    }

    #[test]
    fn test_key_manager_from_env_source() {
        let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::env::set_var("TEST_HELIOSDB_KEY_2", hex_key);

        let source = KeySource::Environment("TEST_HELIOSDB_KEY_2".to_string());
        let km = KeyManager::from_source(&source).expect("Failed to create key manager from source");

        assert_eq!(km.key().len(), 32);

        std::env::remove_var("TEST_HELIOSDB_KEY_2");
    }
}
