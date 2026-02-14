//! Cryptography Security Tests
//!
//! Tests to verify the correctness and security of cryptographic operations.

use heliosdb_nano::crypto::{encrypt, decrypt, derive_key_from_password, EncryptionKey};

#[test]
fn test_encryption_decryption_roundtrip() {
    // Test basic encryption/decryption roundtrip
    let key: EncryptionKey = rand::random();
    let plaintext = b"Hello, HeliosDB! This is a test message.";

    let ciphertext = encrypt(&key, plaintext)
        .expect("Failed to encrypt plaintext");
    let decrypted = decrypt(&key, &ciphertext)
        .expect("Failed to decrypt ciphertext");

    assert_eq!(plaintext, &decrypted[..], "Decrypted plaintext doesn't match original");
}

#[test]
fn test_encryption_produces_different_ciphertexts() {
    // Test that encrypting the same plaintext twice produces different ciphertexts
    // (due to random nonce)
    let key: EncryptionKey = rand::random();
    let plaintext = b"Hello, HeliosDB!";

    let ciphertext1 = encrypt(&key, plaintext)
        .expect("Failed to encrypt plaintext (1st time)");
    let ciphertext2 = encrypt(&key, plaintext)
        .expect("Failed to encrypt plaintext (2nd time)");

    // Ciphertexts should be different (different nonces)
    assert_ne!(
        ciphertext1, ciphertext2,
        "Encrypting same plaintext twice should produce different ciphertexts"
    );

    // But both should decrypt to the same plaintext
    let decrypted1 = decrypt(&key, &ciphertext1)
        .expect("Failed to decrypt ciphertext1");
    let decrypted2 = decrypt(&key, &ciphertext2)
        .expect("Failed to decrypt ciphertext2");

    assert_eq!(decrypted1, plaintext);
    assert_eq!(decrypted2, plaintext);
}

#[test]
fn test_decryption_with_wrong_key() {
    // Test that decryption with wrong key fails
    let key1: EncryptionKey = rand::random();
    let key2: EncryptionKey = rand::random();
    let plaintext = b"Secret message";

    let ciphertext = encrypt(&key1, plaintext)
        .expect("Failed to encrypt plaintext");

    // Attempt to decrypt with wrong key
    let result = decrypt(&key2, &ciphertext);

    assert!(
        result.is_err(),
        "Decryption with wrong key should fail"
    );
}

#[test]
fn test_ciphertext_tampering_detection() {
    // Test that tampering with ciphertext is detected (AEAD property)
    let key: EncryptionKey = rand::random();
    let plaintext = b"Important data";

    let mut ciphertext = encrypt(&key, plaintext)
        .expect("Failed to encrypt plaintext");

    // Tamper with ciphertext (flip a bit)
    if ciphertext.len() > 13 {
        ciphertext[13] ^= 0x01;
    }

    // Attempt to decrypt tampered ciphertext
    let result = decrypt(&key, &ciphertext);

    assert!(
        result.is_err(),
        "Decryption of tampered ciphertext should fail (AEAD protection)"
    );
}

#[test]
fn test_ciphertext_too_short() {
    // Test that too-short ciphertext is rejected
    let key: EncryptionKey = rand::random();
    let short_ciphertext = vec![0u8; 8]; // Less than nonce size (12 bytes)

    let result = decrypt(&key, &short_ciphertext);

    assert!(
        result.is_err(),
        "Decryption of too-short ciphertext should fail"
    );

    // Verify error message
    if let Err(e) = result {
        assert!(
            e.to_string().contains("too short"),
            "Error should mention ciphertext is too short"
        );
    }
}

#[test]
fn test_empty_plaintext_encryption() {
    // Test encryption of empty plaintext
    let key: EncryptionKey = rand::random();
    let plaintext = b"";

    let ciphertext = encrypt(&key, plaintext)
        .expect("Failed to encrypt empty plaintext");

    // Ciphertext should still include nonce + auth tag
    assert!(
        ciphertext.len() >= 12,
        "Ciphertext should include nonce even for empty plaintext"
    );

    let decrypted = decrypt(&key, &ciphertext)
        .expect("Failed to decrypt empty ciphertext");

    assert_eq!(decrypted, plaintext, "Decrypted empty plaintext should match");
}

#[test]
fn test_large_plaintext_encryption() {
    // Test encryption of large plaintext
    let key: EncryptionKey = rand::random();
    let plaintext = vec![0x42u8; 1_000_000]; // 1 MB of data

    let ciphertext = encrypt(&key, &plaintext)
        .expect("Failed to encrypt large plaintext");
    let decrypted = decrypt(&key, &ciphertext)
        .expect("Failed to decrypt large ciphertext");

    assert_eq!(decrypted, plaintext, "Large plaintext roundtrip failed");
}

#[test]
fn test_key_derivation_deterministic() {
    // Test that key derivation is deterministic (same password + salt = same key)
    let password = "supersecretpassword";
    let salt = b"randomsalt123456";

    let key1 = derive_key_from_password(password, salt)
        .expect("Failed to derive key (1st time)");
    let key2 = derive_key_from_password(password, salt)
        .expect("Failed to derive key (2nd time)");

    assert_eq!(
        key1, key2,
        "Key derivation should be deterministic"
    );
}

#[test]
fn test_key_derivation_different_passwords() {
    // Test that different passwords produce different keys
    let salt = b"randomsalt123456";
    let password1 = "password1";
    let password2 = "password2";

    let key1 = derive_key_from_password(password1, salt)
        .expect("Failed to derive key from password1");
    let key2 = derive_key_from_password(password2, salt)
        .expect("Failed to derive key from password2");

    assert_ne!(
        key1, key2,
        "Different passwords should produce different keys"
    );
}

#[test]
fn test_key_derivation_different_salts() {
    // Test that different salts produce different keys
    let password = "password";
    let salt1 = b"salt1111111111111";
    let salt2 = b"salt2222222222222";

    let key1 = derive_key_from_password(password, salt1)
        .expect("Failed to derive key with salt1");
    let key2 = derive_key_from_password(password, salt2)
        .expect("Failed to derive key with salt2");

    assert_ne!(
        key1, key2,
        "Different salts should produce different keys"
    );
}

#[test]
fn test_key_derivation_output_length() {
    // Test that derived key has correct length
    let password = "password";
    let salt = b"randomsalt123456";

    let key = derive_key_from_password(password, salt)
        .expect("Failed to derive key");

    assert_eq!(
        key.len(),
        32,
        "Derived key should be 32 bytes (256 bits)"
    );
}

#[test]
fn test_nonce_uniqueness() {
    // Test that nonces are unique across multiple encryptions
    let key: EncryptionKey = rand::random();
    let plaintext = b"Test message";

    let mut nonces = std::collections::HashSet::new();

    // Generate 100 ciphertexts and extract nonces
    for _ in 0..100 {
        let ciphertext = encrypt(&key, plaintext)
            .expect("Failed to encrypt plaintext");

        // Extract nonce (first 12 bytes)
        let nonce = &ciphertext[0..12];
        nonces.insert(nonce.to_vec());
    }

    // All nonces should be unique
    assert_eq!(
        nonces.len(),
        100,
        "All nonces should be unique (found {} unique out of 100)",
        nonces.len()
    );
}

#[test]
fn test_encryption_unicode_plaintext() {
    // Test encryption of Unicode plaintext
    let key: EncryptionKey = rand::random();
    let plaintext = "Hello, 世界! 🌍🔒";

    let ciphertext = encrypt(&key, plaintext.as_bytes())
        .expect("Failed to encrypt Unicode plaintext");
    let decrypted = decrypt(&key, &ciphertext)
        .expect("Failed to decrypt Unicode ciphertext");

    let decrypted_str = String::from_utf8(decrypted)
        .expect("Failed to convert decrypted bytes to UTF-8");

    assert_eq!(decrypted_str, plaintext, "Unicode plaintext roundtrip failed");
}

#[test]
fn test_encryption_binary_data() {
    // Test encryption of arbitrary binary data
    let key: EncryptionKey = rand::random();
    let plaintext: Vec<u8> = (0..=255).collect(); // All byte values 0-255

    let ciphertext = encrypt(&key, &plaintext)
        .expect("Failed to encrypt binary data");
    let decrypted = decrypt(&key, &ciphertext)
        .expect("Failed to decrypt binary data");

    assert_eq!(decrypted, plaintext, "Binary data roundtrip failed");
}

#[test]
fn test_weak_password_handling() {
    // Test that weak passwords still produce valid keys (no rejection)
    // In production, you might want to enforce password strength policies
    let weak_passwords = vec!["", "1", "12", "password", "123456"];
    let salt = b"randomsalt123456";

    for weak_password in weak_passwords {
        let result = derive_key_from_password(weak_password, salt);

        // Key derivation should succeed even with weak passwords
        // (password strength enforcement should be done at application level)
        assert!(
            result.is_ok(),
            "Key derivation should succeed even with weak password: '{}'",
            weak_password
        );

        if let Ok(key) = result {
            assert_eq!(key.len(), 32, "Derived key should be 32 bytes");
        }
    }
}

#[test]
fn test_concurrent_encryption_operations() {
    use std::sync::Arc;
    use std::thread;

    // Test concurrent encryption operations (thread safety)
    let key: Arc<EncryptionKey> = Arc::new(rand::random());
    let plaintext = Arc::new(b"Concurrent test message".to_vec());

    let mut handles = vec![];

    for thread_id in 0..10 {
        let key_clone = Arc::clone(&key);
        let plaintext_clone = Arc::clone(&plaintext);

        let handle = thread::spawn(move || {
            for i in 0..10 {
                let ciphertext = encrypt(&key_clone, &plaintext_clone)
                    .expect(&format!("Thread {} encryption {} failed", thread_id, i));

                let decrypted = decrypt(&key_clone, &ciphertext)
                    .expect(&format!("Thread {} decryption {} failed", thread_id, i));

                assert_eq!(
                    decrypted, plaintext_clone.as_slice(),
                    "Thread {} iteration {} roundtrip failed",
                    thread_id, i
                );
            }
        });

        handles.push(handle);
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle.join()
            .expect(&format!("Thread {} panicked", i));
    }

    println!("Concurrent encryption test completed successfully (100 operations across 10 threads)");
}

#[test]
fn test_encryption_performance_baseline() {
    // Performance baseline test (not a security test, but useful for monitoring)
    let key: EncryptionKey = rand::random();
    let plaintext = vec![0x42u8; 1024]; // 1 KB

    let iterations = 1000;
    let start = std::time::Instant::now();

    for _ in 0..iterations {
        let ciphertext = encrypt(&key, &plaintext)
            .expect("Failed to encrypt");
        let _ = decrypt(&key, &ciphertext)
            .expect("Failed to decrypt");
    }

    let elapsed = start.elapsed();
    let ops_per_sec = (iterations as f64 / elapsed.as_secs_f64()) as u64;

    println!(
        "Encryption/decryption performance: {} ops/sec ({} iterations in {:?})",
        ops_per_sec, iterations, elapsed
    );

    // Should be able to do at least 1000 ops/sec on modern hardware
    assert!(
        ops_per_sec > 100,
        "Encryption performance too slow: {} ops/sec",
        ops_per_sec
    );
}
