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
//! ### TDE per-key invocation limit
//!
//! [`encrypt`] uses a fresh, fully RANDOM 96-bit nonce for every invocation
//! (`rand::random()`), under one static key for the lifetime of the database.
//! NIST SP 800-38D §8.3 therefore caps a single key at **2^32 invocations**
//! (~4.29 × 10^9): with random 96-bit nonces the birthday bound puts the
//! probability of a nonce COLLISION at roughly 2^-32 at that point, and a GCM
//! nonce collision leaks the XOR of the two plaintexts and compromises the
//! authentication subkey.
//!
//! Count invocations, not rows. On the current storage boundary an INSERT of one
//! row costs ONE invocation for its `data:` image on the default autocommit and
//! transaction-commit paths, because the MVCC `v:` twin reuses those same sealed
//! bytes; it costs two where the two images genuinely differ (a column with a
//! non-default `STORAGE` mode). An UPDATE adds one for the preserved pre-image,
//! each logical-WAL entry (`storage.wal_enabled`, on by default) adds one, and
//! each row-counter flush (every 64 inserts) adds one. A database sustaining
//! 10,000 row writes per second reaches 2^32 in roughly five days.
//!
//! `[encryption] rotation_interval_days` is parsed into `Config` but is read by
//! nothing in this build, and `KeyManager::previous_key` has no callers: there
//! is no automatic rotation. Re-keying today means dumping and restoring under a
//! new key.
//!
//! The structural fix is a nonce that cannot collide by construction rather than
//! by probability — an 8-byte per-process random prefix concatenated with a
//! 4-byte per-process monotonic counter, or a 4-byte prefix with an 8-byte
//! counter persisted alongside the key. Either shape keeps the frame layout
//! (nonce ‖ ciphertext ‖ tag) and [`MIN_CIPHERTEXT_LEN`] exactly as they are, so
//! it stays readable by the tolerant decode in `storage::tde` and by every
//! database already written. That change is deliberately NOT made here; it is
//! tracked as its own piece of work.
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
    derive_key, generate_random_key, hash_content, init_provider, is_fips_build, provider as get_provider,
    provider_name, CryptoKey, CryptoProvider, HashOutput,
};
pub use zero_knowledge::{
    NonceTracker, TimestampValidator, ZeroKnowledgeSession, ZkeConfig, ZkeDerivedKeys, ZkeKeyDerivation, ZkeMode,
    ZkeRequestContext,
};

use crate::{Error, Result};

/// Encryption key (256 bits)
pub type EncryptionKey = [u8; 32];

/// Nonce for AES-GCM (96 bits)
pub type Nonce = [u8; 12];

/// Encrypt data using AES-256-GCM.
///
/// Emits `nonce(12) ‖ ciphertext ‖ tag(16)`.
///
/// The nonce is drawn fresh from the CSPRNG on every call, so no two calls
/// reuse one by construction — but a fully random 96-bit nonce is subject to
/// the birthday bound, which caps a single key at ~2^32 invocations (NIST
/// SP 800-38D §8.3). See this module's "TDE per-key invocation limit" section
/// for how invocations accrue and what the structural fix looks like.
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

/// Smallest buffer [`encrypt`] can possibly emit: nonce(12) ‖ tag(16), for an
/// empty plaintext. A shorter buffer cannot be AES-256-GCM output from this
/// module, so it can be rejected without touching the cipher.
pub const MIN_CIPHERTEXT_LEN: usize = 12 + 16;

/// Outcome of an authenticated-decryption attempt that is permitted to
/// conclude "this buffer is not ciphertext under this key" instead of raising
/// an error.
///
/// This type exists so that a caller which must tolerate a MIXTURE of
/// ciphertext and plaintext under one key prefix can distinguish the AEAD
/// authentication failure — the only failure mode that may legitimately be
/// swallowed — from every other kind of crypto or key error. Note that it has
/// no error case at all: a missing key, an unreadable key source or any other
/// configuration failure happens *before* this function is reachable (the
/// caller must already hold a key to call it), so there is no way to lose one
/// of those through this path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecryptAttempt {
    /// The AES-256-GCM tag verified under the supplied key; carries the
    /// authenticated plaintext.
    Authenticated(Vec<u8>),
    /// The buffer is not a valid AES-256-GCM frame under the supplied key:
    /// either it is too short to be one at all, or the tag check failed.
    Unauthenticated,
}

/// Attempt AES-256-GCM decryption, reporting an authentication failure as a
/// VALUE rather than an error.
///
/// Use this only where "the stored bytes may not be ciphertext" is a
/// legitimate state; use [`decrypt`] where ciphertext is required. A false
/// [`DecryptAttempt::Authenticated`] on non-ciphertext would require forging a
/// GCM tag (probability 2^-128), so `Unauthenticated` is a sound — not merely
/// heuristic — statement that the buffer was not produced by [`encrypt`] under
/// this key.
#[must_use]
pub fn try_decrypt(key: &EncryptionKey, ciphertext_with_nonce: &[u8]) -> DecryptAttempt {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce as AesNonce,
    };

    // Cheap structural reject: below the minimum frame size the cipher cannot
    // succeed, so skip constructing it at all.
    if ciphertext_with_nonce.len() < MIN_CIPHERTEXT_LEN {
        return DecryptAttempt::Unauthenticated;
    }

    // Split nonce (first 12 bytes) from ciphertext‖tag. Both are guaranteed
    // present by the length check above; the `else` arm is unreachable and is
    // written this way only to keep the function panic- and index-free.
    let (Some(nonce_bytes), Some(ciphertext)) = (ciphertext_with_nonce.get(0..12), ciphertext_with_nonce.get(12..))
    else {
        return DecryptAttempt::Unauthenticated;
    };

    let cipher = Aes256Gcm::new(key.into());
    let nonce = AesNonce::from_slice(nonce_bytes);

    match cipher.decrypt(nonce, ciphertext) {
        Ok(plaintext) => DecryptAttempt::Authenticated(plaintext),
        Err(_) => DecryptAttempt::Unauthenticated,
    }
}

/// Decrypt data using AES-256-GCM, requiring the buffer to BE ciphertext.
///
/// Thin strict wrapper over [`try_decrypt`] so there is exactly one AES-GCM
/// open in this module; only the error policy differs between the two.
pub fn decrypt(key: &EncryptionKey, ciphertext_with_nonce: &[u8]) -> Result<Vec<u8>> {
    if ciphertext_with_nonce.len() < 12 {
        return Err(Error::encryption("Ciphertext too short"));
    }

    match try_decrypt(key, ciphertext_with_nonce) {
        DecryptAttempt::Authenticated(plaintext) => Ok(plaintext),
        // `aes_gcm::Error` is opaque and renders as exactly "aead::Error". The
        // string is spelled out here (rather than formatted from the error
        // value) so `try_decrypt` never has to allocate a diagnostic on a path
        // that can run once per row.
        DecryptAttempt::Unauthenticated => Err(Error::encryption("Decryption failed: aead::Error")),
    }
}

/// Generate encryption key from password
pub fn derive_key_from_password(password: &str, salt: &[u8]) -> Result<EncryptionKey> {
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};

    // Use Argon2 for key derivation
    let salt_string =
        SaltString::encode_b64(salt).map_err(|e| Error::encryption(format!("Salt encoding failed: {}", e)))?;

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
    key.copy_from_slice(
        key_bytes
            .get(0..32)
            .ok_or_else(|| Error::encryption("Derived key too short"))?,
    );

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

        let ciphertext = encrypt(&key, plaintext).expect("Failed to encrypt plaintext");
        let decrypted = decrypt(&key, &ciphertext).expect("Failed to decrypt ciphertext");

        assert_eq!(plaintext, &decrypted[..]);
    }

    #[test]
    fn try_decrypt_authenticates_real_ciphertext() {
        let key: EncryptionKey = rand::random();
        let ciphertext = encrypt(&key, b"row bytes").expect("encrypt");
        assert_eq!(
            try_decrypt(&key, &ciphertext),
            DecryptAttempt::Authenticated(b"row bytes".to_vec())
        );
    }

    #[test]
    fn try_decrypt_reports_non_ciphertext_as_unauthenticated() {
        let key: EncryptionKey = rand::random();

        // Plaintext, short and long — the shape a legacy stored value has.
        assert_eq!(try_decrypt(&key, b""), DecryptAttempt::Unauthenticated);
        assert_eq!(try_decrypt(&key, b"short"), DecryptAttempt::Unauthenticated);
        assert_eq!(try_decrypt(&key, &vec![0xABu8; 4096]), DecryptAttempt::Unauthenticated);

        // A real frame under a DIFFERENT key must not authenticate either:
        // "not ciphertext under THIS key" is the property, and a wrong-key
        // database must never silently read as plaintext-looking garbage that
        // then decodes.
        let other: EncryptionKey = rand::random();
        let foreign = encrypt(&other, b"row bytes").expect("encrypt");
        assert_eq!(try_decrypt(&key, &foreign), DecryptAttempt::Unauthenticated);

        // Tampering flips the tag check.
        let mut tampered = encrypt(&key, b"row bytes").expect("encrypt");
        if let Some(last) = tampered.last_mut() {
            *last ^= 0xFF;
        }
        assert_eq!(try_decrypt(&key, &tampered), DecryptAttempt::Unauthenticated);
    }

    #[test]
    fn decrypt_still_rejects_what_try_decrypt_only_reports() {
        let key: EncryptionKey = rand::random();

        // The strict wrapper keeps its historical messages.
        let too_short = decrypt(&key, &[0u8; 8]).expect_err("short buffer must error");
        assert!(
            too_short.to_string().contains("too short"),
            "short-ciphertext message must be preserved, got: {}",
            too_short
        );

        let not_a_frame = decrypt(&key, &vec![0xABu8; 64]).expect_err("plaintext must error in strict mode");
        assert!(
            not_a_frame.to_string().contains("Decryption failed"),
            "tag-failure message must be preserved, got: {}",
            not_a_frame
        );
    }

    #[test]
    fn min_ciphertext_len_matches_what_encrypt_emits() {
        let key: EncryptionKey = rand::random();
        let empty = encrypt(&key, b"").expect("encrypt empty");
        assert_eq!(
            empty.len(),
            MIN_CIPHERTEXT_LEN,
            "the structural reject in try_decrypt must not be able to discard a real frame"
        );
    }

    #[test]
    fn test_key_derivation() {
        let password = "supersecret";
        let salt = b"randomsalt123456";

        let key = derive_key_from_password(password, salt).expect("Failed to derive key from password");
        assert_eq!(key.len(), 32);
    }
}
