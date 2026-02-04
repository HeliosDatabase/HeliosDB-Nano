//! Encryption implementation
//!
//! This module provides comprehensive encryption support for HeliosDB-Lite:
//!
//! ## Transparent Data Encryption (TDE)
//!
//! TDE encrypts all data at rest using AES-256-GCM. The encryption key is
//! stored by the server and used automatically for all storage operations.
//!
//! ## Zero-Knowledge Encryption (ZKE)
//!
//! ZKE ensures that encryption keys **never leave the client**. The server
//! only ever sees encrypted data and cannot decrypt it without the client
//! providing the key for each request.
//!
//! ### ZKE Modes
//!
//! - **Full**: Client encrypts all data before transmission
//! - **Hybrid**: Metadata unencrypted, row data encrypted
//! - **PerRequest**: Key provided per-request for server-side operations
//!
//! ## Key Management
//!
//! - [`KeyManager`]: Server-side key management for TDE
//! - [`ZkeKeyDerivation`]: Client-side key derivation for ZKE
//! - [`ZeroKnowledgeSession`]: Per-request encryption session
//!
//! ## Cryptographic Providers
//!
//! HeliosDB-Lite supports two cryptographic providers via feature flags:
//!
//! - **ring-crypto** (default): Uses ring, BLAKE3, and Argon2id
//! - **fips**: FIPS 140-3 compliant using AWS-LC (Certificate #4816)
//!
//! See [`provider`] module for details.

mod key_manager;
pub mod provider;
mod zero_knowledge;

pub use key_manager::KeyManager;
pub use provider::{
    CryptoProvider, CryptoKey, HashOutput,
    init_provider, provider as get_provider, is_fips_build, provider_name,
    hash_content, derive_key, generate_random_key,
};
pub use zero_knowledge::{
    ZkeConfig, ZkeDerivedKeys, ZkeKeyDerivation, ZkeMode, ZkeRequestContext,
    ZeroKnowledgeSession, NonceTracker, TimestampValidator,
};

use crate::{Result, Error};

/// Encryption key (256 bits)
pub type EncryptionKey = [u8; 32];

/// Nonce for AES-GCM (96 bits)
pub type Nonce = [u8; 12];

/// Encrypt data using AES-256-GCM
pub fn encrypt(key: &EncryptionKey, plaintext: &[u8]) -> Result<Vec<u8>> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce as AesNonce,
    };

    let cipher = Aes256Gcm::new(key.into());

    // Generate random nonce
    let nonce_bytes: Nonce = rand::random();
    let nonce = AesNonce::from_slice(&nonce_bytes);

    // Encrypt
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| Error::encryption(format!("Encryption failed: {}", e)))?;

    // Prepend nonce to ciphertext
    let mut result = nonce_bytes.to_vec();
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

/// Decrypt data using AES-256-GCM
pub fn decrypt(key: &EncryptionKey, ciphertext_with_nonce: &[u8]) -> Result<Vec<u8>> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce as AesNonce,
    };

    if ciphertext_with_nonce.len() < 12 {
        return Err(Error::encryption("Ciphertext too short"));
    }

    // Extract nonce (first 12 bytes)
    let nonce_bytes = &ciphertext_with_nonce[0..12];
    let ciphertext = &ciphertext_with_nonce[12..];

    let cipher = Aes256Gcm::new(key.into());
    let nonce = AesNonce::from_slice(nonce_bytes);

    // Decrypt
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| Error::encryption(format!("Decryption failed: {}", e)))?;

    Ok(plaintext)
}

/// Generate encryption key from password
pub fn derive_key_from_password(password: &str, salt: &[u8]) -> Result<EncryptionKey> {
    use argon2::{Argon2, PasswordHasher};
    use argon2::password_hash::SaltString;

    // Use Argon2 for key derivation
    let salt_string = SaltString::encode_b64(salt)
        .map_err(|e| Error::encryption(format!("Salt encoding failed: {}", e)))?;

    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt_string)
        .map_err(|e| Error::encryption(format!("Key derivation failed: {}", e)))?;

    // Extract key from hash
    let hash_bytes = hash.hash.ok_or_else(|| Error::encryption("No hash generated"))?;
    let key_bytes = hash_bytes.as_bytes();

    if key_bytes.len() < 32 {
        return Err(Error::encryption("Derived key too short"));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes[0..32]);

    Ok(key)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let key: EncryptionKey = rand::random();
        let plaintext = b"Hello, HeliosDB Lite!";

        let ciphertext = encrypt(&key, plaintext)
            .expect("Failed to encrypt plaintext");
        let decrypted = decrypt(&key, &ciphertext)
            .expect("Failed to decrypt ciphertext");

        assert_eq!(plaintext, &decrypted[..]);
    }

    #[test]
    fn test_key_derivation() {
        let password = "supersecret";
        let salt = b"randomsalt123456";

        let key = derive_key_from_password(password, salt)
            .expect("Failed to derive key from password");
        assert_eq!(key.len(), 32);
    }
}
