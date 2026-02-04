//! Zero-Knowledge Encryption (ZKE) integration tests
//!
//! Tests for client-side encryption with per-request keys.

use heliosdb_lite::{
    ZkeKeyDerivation, ZeroKnowledgeSession, ZkeConfig, ZkeRequestContext,
    NonceTracker, TimestampValidator, ZkeDerivedKeys,
};
use std::sync::Arc;

/// Test client-side key derivation
#[test]
fn test_zke_key_derivation_basic() {
    let keys = ZkeKeyDerivation::derive_keys("my_password", "user@example.com")
        .expect("Key derivation should succeed");

    // Keys should be 32 bytes
    assert_eq!(keys.auth_key.len(), 32);
    assert_eq!(keys.encryption_key.len(), 32);
    assert_eq!(keys.encryption_key_hash.len(), 32);

    // Auth and encryption keys should be different
    assert_ne!(*keys.auth_key, *keys.encryption_key);
}

/// Test that key derivation is deterministic
#[test]
fn test_zke_key_derivation_deterministic() {
    let keys1 = ZkeKeyDerivation::derive_keys("password123", "test@test.com").unwrap();
    let keys2 = ZkeKeyDerivation::derive_keys("password123", "test@test.com").unwrap();

    // Same inputs should produce same outputs
    assert_eq!(*keys1.auth_key, *keys2.auth_key);
    assert_eq!(*keys1.encryption_key, *keys2.encryption_key);
    assert_eq!(keys1.encryption_key_hash, keys2.encryption_key_hash);
}

/// Test different passwords produce different keys
#[test]
fn test_zke_different_passwords() {
    let keys1 = ZkeKeyDerivation::derive_keys("password1", "user@test.com").unwrap();
    let keys2 = ZkeKeyDerivation::derive_keys("password2", "user@test.com").unwrap();

    assert_ne!(*keys1.auth_key, *keys2.auth_key);
    assert_ne!(*keys1.encryption_key, *keys2.encryption_key);
}

/// Test different identifiers produce different keys
#[test]
fn test_zke_different_identifiers() {
    let keys1 = ZkeKeyDerivation::derive_keys("password", "user1@test.com").unwrap();
    let keys2 = ZkeKeyDerivation::derive_keys("password", "user2@test.com").unwrap();

    assert_ne!(*keys1.auth_key, *keys2.auth_key);
    assert_ne!(*keys1.encryption_key, *keys2.encryption_key);
}

/// Test ZKE session encrypt/decrypt
#[test]
fn test_zke_session_encrypt_decrypt() {
    let keys = ZkeKeyDerivation::derive_keys("test", "test@test.com").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    let plaintext = b"SELECT * FROM secret_table WHERE id = 1";
    let ciphertext = session.encrypt(plaintext).expect("Encryption should succeed");

    // Ciphertext should be different from plaintext
    assert_ne!(&ciphertext[..], plaintext);

    // Decryption should recover plaintext
    let decrypted = session.decrypt(&ciphertext).expect("Decryption should succeed");
    assert_eq!(&decrypted[..], plaintext);
}

/// Test ZKE session from hex key
#[test]
fn test_zke_session_from_hex_key() {
    let hex_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let session = ZeroKnowledgeSession::from_hex_key(hex_key)
        .expect("Should create session from hex key");

    let plaintext = b"test data for hex key session";
    let ciphertext = session.encrypt(plaintext).unwrap();
    let decrypted = session.decrypt(&ciphertext).unwrap();

    assert_eq!(&decrypted[..], plaintext);
}

/// Test invalid hex key is rejected
#[test]
fn test_zke_session_invalid_hex_key() {
    // Too short
    let result = ZeroKnowledgeSession::from_hex_key("0123456789abcdef");
    assert!(result.is_err());

    // Invalid characters
    let result = ZeroKnowledgeSession::from_hex_key("ghijklmnopqrstuv0123456789abcdef0123456789abcdef0123456789abcdef");
    assert!(result.is_err());
}

/// Test key hash validation
#[test]
fn test_zke_key_hash_validation() {
    let keys = ZkeKeyDerivation::derive_keys("test_pass", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    // Valid hash
    assert!(session.validate_key_hash(&keys.encryption_key_hash));

    // Invalid hash
    let mut wrong_hash = keys.encryption_key_hash;
    wrong_hash[0] ^= 0xFF;
    assert!(!session.validate_key_hash(&wrong_hash));
}

/// Test key hash hex validation
#[test]
fn test_zke_key_hash_hex_validation() {
    let keys = ZkeKeyDerivation::derive_keys("test_pass", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    // Valid hash hex
    let hash_hex = keys.key_hash_hex();
    assert!(session.validate_key_hash_hex(&hash_hex).unwrap());

    // Wrong hash hex
    let wrong_hex = "0000000000000000000000000000000000000000000000000000000000000000";
    assert!(!session.validate_key_hash_hex(wrong_hex).unwrap());
}

/// Test nonce tracker basic functionality
#[test]
fn test_nonce_tracker_basic() {
    let tracker = NonceTracker::new(300, 100);

    let nonce1: [u8; 16] = rand::random();
    let nonce2: [u8; 16] = rand::random();

    // First use should succeed
    assert!(tracker.check_and_record(&nonce1));
    assert!(tracker.check_and_record(&nonce2));

    // Replay should fail
    assert!(!tracker.check_and_record(&nonce1));
    assert!(!tracker.check_and_record(&nonce2));

    assert_eq!(tracker.len(), 2);
}

/// Test nonce tracker detects replay attacks
#[test]
fn test_nonce_tracker_replay_detection() {
    let tracker = NonceTracker::new(300, 100);
    let nonce: [u8; 16] = rand::random();

    // First request should succeed
    assert!(tracker.check_and_record(&nonce));

    // Immediate replay should fail
    assert!(!tracker.check_and_record(&nonce));

    // Another replay should also fail
    assert!(!tracker.check_and_record(&nonce));
}

/// Test nonce tracker capacity limit
#[test]
fn test_nonce_tracker_capacity() {
    let max_nonces = 50;
    let tracker = NonceTracker::new(300, max_nonces);

    // Fill beyond capacity
    for _ in 0..100 {
        let nonce: [u8; 16] = rand::random();
        tracker.check_and_record(&nonce);
    }

    // Should have cleaned up some entries
    assert!(tracker.len() <= max_nonces);
}

/// Test timestamp validator
#[test]
fn test_timestamp_validator() {
    let validator = TimestampValidator::new(60); // 1 minute tolerance

    let now = TimestampValidator::current_timestamp();

    // Current time should be valid
    assert!(validator.validate(now));

    // 30 seconds ago should be valid
    assert!(validator.validate(now.saturating_sub(30)));

    // 30 seconds in future should be valid
    assert!(validator.validate(now + 30));

    // 2 minutes ago should be invalid
    assert!(!validator.validate(now.saturating_sub(120)));

    // 2 minutes in future should be invalid
    assert!(!validator.validate(now + 120));
}

/// Test ZKE session with nonce
#[test]
fn test_zke_session_with_nonce() {
    let key: [u8; 32] = rand::random();
    let session = ZeroKnowledgeSession::new(key)
        .unwrap()
        .with_random_nonce();

    assert!(session.nonce().is_some());
    assert!(session.nonce_hex().is_some());
    assert_eq!(session.nonce_hex().unwrap().len(), 32); // 16 bytes = 32 hex chars
}

/// Test ZKE request context validation
#[test]
fn test_zke_request_context_validation() {
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

    // Valid request should pass
    assert!(context.validate(Some(&hash_hex), Some(&nonce), Some(timestamp)).is_ok());
}

/// Test ZKE request context rejects invalid key hash
#[test]
fn test_zke_request_context_invalid_hash() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys)
        .unwrap()
        .with_random_nonce();

    let nonce = *session.nonce().unwrap();
    let nonce_tracker = Arc::new(NonceTracker::default());
    let config = ZkeConfig::default();

    let context = ZkeRequestContext::new(session, nonce_tracker, config);

    let timestamp = TimestampValidator::current_timestamp();
    let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";

    // Should fail with wrong hash
    assert!(context.validate(Some(wrong_hash), Some(&nonce), Some(timestamp)).is_err());
}

/// Test ZKE request context detects replay
#[test]
fn test_zke_request_context_replay_detection() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let nonce_tracker = Arc::new(NonceTracker::default());
    let config = ZkeConfig::default();

    // First request
    let session1 = ZeroKnowledgeSession::from_derived_keys(&keys)
        .unwrap()
        .with_random_nonce();
    let nonce = *session1.nonce().unwrap();
    let context1 = ZkeRequestContext::new(session1, Arc::clone(&nonce_tracker), config.clone());

    let timestamp = TimestampValidator::current_timestamp();
    let hash_hex = keys.key_hash_hex();

    // First request should pass
    assert!(context1.validate(Some(&hash_hex), Some(&nonce), Some(timestamp)).is_ok());

    // Second request with same nonce (replay attack)
    let session2 = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();
    let context2 = ZkeRequestContext::new(session2, Arc::clone(&nonce_tracker), config);

    // Should fail - nonce already used
    assert!(context2.validate(Some(&hash_hex), Some(&nonce), Some(timestamp)).is_err());
}

/// Test ZKE context encrypt/decrypt
#[test]
fn test_zke_request_context_encrypt_decrypt() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();
    let nonce_tracker = Arc::new(NonceTracker::default());
    let config = ZkeConfig::default();

    let context = ZkeRequestContext::new(session, nonce_tracker, config);

    let plaintext = b"SELECT * FROM users";
    let ciphertext = context.encrypt(plaintext).unwrap();
    let decrypted = context.decrypt(&ciphertext).unwrap();

    assert_eq!(&decrypted[..], plaintext);
}

/// Test that wrong key cannot decrypt data
#[test]
fn test_zke_wrong_key_fails_decrypt() {
    let keys1 = ZkeKeyDerivation::derive_keys("password1", "user1").unwrap();
    let keys2 = ZkeKeyDerivation::derive_keys("password2", "user2").unwrap();

    let session1 = ZeroKnowledgeSession::from_derived_keys(&keys1).unwrap();
    let session2 = ZeroKnowledgeSession::from_derived_keys(&keys2).unwrap();

    let plaintext = b"sensitive data";
    let ciphertext = session1.encrypt(plaintext).unwrap();

    // Same session should decrypt
    let decrypted = session1.decrypt(&ciphertext).unwrap();
    assert_eq!(&decrypted[..], plaintext);

    // Different session should fail
    let result = session2.decrypt(&ciphertext);
    assert!(result.is_err());
}

/// Test large data encryption
#[test]
fn test_zke_large_data() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    // 1MB of data
    let large_data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();

    let ciphertext = session.encrypt(&large_data).expect("Should encrypt large data");
    let decrypted = session.decrypt(&ciphertext).expect("Should decrypt large data");

    assert_eq!(decrypted, large_data);
}

/// Test multiple encrypt/decrypt cycles
#[test]
fn test_zke_multiple_operations() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    let queries = vec![
        b"SELECT * FROM users".to_vec(),
        b"INSERT INTO orders VALUES (1, 'item')".to_vec(),
        b"UPDATE products SET price = 99.99 WHERE id = 5".to_vec(),
        b"DELETE FROM sessions WHERE expired = true".to_vec(),
    ];

    for query in queries {
        let ciphertext = session.encrypt(&query).unwrap();
        let decrypted = session.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, query);
    }
}

/// Test key hash is consistent
#[test]
fn test_zke_key_hash_consistency() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();

    // Create multiple sessions from same keys
    let session1 = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();
    let session2 = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    // Key hashes should be identical
    assert_eq!(session1.key_hash(), session2.key_hash());
    assert_eq!(session1.key_hash_hex(), session2.key_hash_hex());
}

/// Test empty data encryption
#[test]
fn test_zke_empty_data() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    let empty: &[u8] = b"";
    let ciphertext = session.encrypt(empty).unwrap();
    let decrypted = session.decrypt(&ciphertext).unwrap();

    assert_eq!(&decrypted[..], empty);
}

/// Test session age tracking
#[test]
fn test_zke_session_age() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();
    let session = ZeroKnowledgeSession::from_derived_keys(&keys).unwrap();

    // Session should be very new
    assert!(session.age_secs() < 1);
}

/// Test derived keys key_hash_hex
#[test]
fn test_derived_keys_hash_hex() {
    let keys = ZkeKeyDerivation::derive_keys("test", "user").unwrap();

    let hash_hex = keys.key_hash_hex();
    assert_eq!(hash_hex.len(), 64); // 32 bytes = 64 hex chars

    // Should be valid hex
    for c in hash_hex.chars() {
        assert!(c.is_ascii_hexdigit());
    }
}
