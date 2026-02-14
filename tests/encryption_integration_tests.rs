//! Integration tests for encryption and cryptography features
//!
//! Tests cover:
//! - AES-256-GCM encryption/decryption
//! - Argon2 key derivation
//! - Key management
//! - Large data encryption
//! - Performance overhead
//! - Concurrent encryption operations

use heliosdb_nano::crypto::{encrypt, decrypt, derive_key_from_password, EncryptionKey};
use std::time::Instant;

#[test]
fn test_encrypt_decrypt_basic() {
    let key: EncryptionKey = rand::random();
    let plaintext = b"Hello, HeliosDB Lite encryption!";

    let ciphertext = encrypt(&key, plaintext).expect("Encryption failed");
    assert_ne!(&ciphertext[12..], plaintext); // Verify it's encrypted

    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");
    assert_eq!(&decrypted[..], plaintext);
}

#[test]
fn test_encrypt_empty_data() {
    let key: EncryptionKey = rand::random();
    let plaintext = b"";

    let ciphertext = encrypt(&key, plaintext).expect("Encryption failed");
    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");

    assert_eq!(&decrypted[..], plaintext);
}

#[test]
fn test_encrypt_large_data() {
    let key: EncryptionKey = rand::random();

    // Test with 1MB of data
    let plaintext = vec![0x42u8; 1024 * 1024];

    let start = Instant::now();
    let ciphertext = encrypt(&key, &plaintext).expect("Encryption failed");
    let encrypt_duration = start.elapsed();

    let start = Instant::now();
    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");
    let decrypt_duration = start.elapsed();

    assert_eq!(decrypted, plaintext);

    // Performance assertions - relaxed for CI/VM environments
    // Local runs with AES-NI typically complete in <50ms
    // CI environments may be significantly slower due to virtualization
    assert!(
        encrypt_duration.as_millis() < 5000,
        "Encryption of 1MB took {:?}, expected <5s",
        encrypt_duration
    );
    assert!(
        decrypt_duration.as_millis() < 5000,
        "Decryption of 1MB took {:?}, expected <5s",
        decrypt_duration
    );

    println!("Encryption of 1MB: {:?}", encrypt_duration);
    println!("Decryption of 1MB: {:?}", decrypt_duration);
}

#[test]
fn test_decrypt_with_wrong_key() {
    let key1: EncryptionKey = rand::random();
    let key2: EncryptionKey = rand::random();
    let plaintext = b"Secret message";

    let ciphertext = encrypt(&key1, plaintext).expect("Encryption failed");

    // Should fail with wrong key
    let result = decrypt(&key2, &ciphertext);
    assert!(result.is_err(), "Decryption should fail with wrong key");
}

#[test]
fn test_decrypt_corrupted_ciphertext() {
    let key: EncryptionKey = rand::random();
    let plaintext = b"Original message";

    let mut ciphertext = encrypt(&key, plaintext).expect("Encryption failed");

    // Corrupt the ciphertext (after the nonce)
    if ciphertext.len() > 20 {
        ciphertext[20] ^= 0xFF;
    }

    // Should fail due to authentication tag mismatch
    let result = decrypt(&key, &ciphertext);
    assert!(result.is_err(), "Decryption should fail with corrupted ciphertext");
}

#[test]
fn test_decrypt_invalid_nonce() {
    let key: EncryptionKey = rand::random();

    // Ciphertext too short (no valid nonce)
    let invalid_ciphertext = vec![0u8; 10];

    let result = decrypt(&key, &invalid_ciphertext);
    assert!(result.is_err(), "Decryption should fail with invalid nonce");
}

#[test]
fn test_key_derivation_from_password() {
    let password = "supersecretpassword123";
    let salt = b"unique_salt_1234";

    let key = derive_key_from_password(password, salt)
        .expect("Key derivation failed");

    assert_eq!(key.len(), 32, "Key should be 256 bits (32 bytes)");
}

#[test]
fn test_key_derivation_deterministic() {
    let password = "mypassword";
    let salt = b"consistent_salt_";

    let key1 = derive_key_from_password(password, salt)
        .expect("Key derivation failed");
    let key2 = derive_key_from_password(password, salt)
        .expect("Key derivation failed");

    assert_eq!(key1, key2, "Same password and salt should produce same key");
}

#[test]
fn test_key_derivation_different_salts() {
    let password = "mypassword";
    let salt1 = b"salt_version_001";
    let salt2 = b"salt_version_002";

    let key1 = derive_key_from_password(password, salt1)
        .expect("Key derivation failed");
    let key2 = derive_key_from_password(password, salt2)
        .expect("Key derivation failed");

    assert_ne!(key1, key2, "Different salts should produce different keys");
}

#[test]
fn test_key_derivation_different_passwords() {
    let salt = b"same_salt_used__";
    let password1 = "password123";
    let password2 = "password456";

    let key1 = derive_key_from_password(password1, salt)
        .expect("Key derivation failed");
    let key2 = derive_key_from_password(password2, salt)
        .expect("Key derivation failed");

    assert_ne!(key1, key2, "Different passwords should produce different keys");
}

#[test]
fn test_key_derivation_performance() {
    let password = "testpassword";
    let salt = b"performance_salt";

    // Argon2 should take 50-500ms (intentionally slow for security)
    let start = Instant::now();
    let _key = derive_key_from_password(password, salt)
        .expect("Key derivation failed");
    let duration = start.elapsed();

    println!("Key derivation took: {:?}", duration);

    // Should complete in reasonable time (not timeout)
    // Relaxed for CI/VM environments which may be significantly slower
    assert!(
        duration.as_millis() < 5000,
        "Key derivation took too long: {:?}",
        duration
    );
}

#[test]
fn test_encrypt_decrypt_with_derived_key() {
    let password = "user_password_123";
    let salt = b"user_salt_value_";

    let key = derive_key_from_password(password, salt)
        .expect("Key derivation failed");

    let plaintext = b"User data to be encrypted";
    let ciphertext = encrypt(&key, plaintext).expect("Encryption failed");
    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");

    assert_eq!(&decrypted[..], plaintext);
}

#[test]
fn test_encryption_unique_nonces() {
    let key: EncryptionKey = rand::random();
    let plaintext = b"Same plaintext";

    // Encrypt the same plaintext twice
    let ciphertext1 = encrypt(&key, plaintext).expect("Encryption failed");
    let ciphertext2 = encrypt(&key, plaintext).expect("Encryption failed");

    // Ciphertexts should be different (due to different nonces)
    assert_ne!(ciphertext1, ciphertext2, "Nonces should be unique");

    // But both should decrypt to the same plaintext
    let decrypted1 = decrypt(&key, &ciphertext1).expect("Decryption failed");
    let decrypted2 = decrypt(&key, &ciphertext2).expect("Decryption failed");

    assert_eq!(&decrypted1[..], plaintext);
    assert_eq!(&decrypted2[..], plaintext);
}

#[test]
fn test_encryption_various_data_sizes() {
    let key: EncryptionKey = rand::random();

    let test_sizes = vec![
        1,      // 1 byte
        16,     // AES block size
        100,    // Small data
        1024,   // 1KB
        10240,  // 10KB
        102400, // 100KB
    ];

    for size in test_sizes {
        let plaintext = vec![0x5A; size];

        let ciphertext = encrypt(&key, &plaintext)
            .expect(&format!("Encryption failed for {} bytes", size));
        let decrypted = decrypt(&key, &ciphertext)
            .expect(&format!("Decryption failed for {} bytes", size));

        assert_eq!(
            decrypted, plaintext,
            "Encryption/decryption failed for {} bytes",
            size
        );

        // Ciphertext should be larger (nonce + auth tag)
        assert!(
            ciphertext.len() > size,
            "Ciphertext should include overhead"
        );
    }
}

#[test]
fn test_concurrent_encryption() {
    use std::sync::Arc;
    use std::thread;

    let key = Arc::new(rand::random::<EncryptionKey>());
    let mut handles = vec![];

    // Spawn 10 threads doing encryption concurrently
    for i in 0..10 {
        let key_clone = Arc::clone(&key);
        let handle = thread::spawn(move || {
            let plaintext = format!("Message from thread {}", i);
            let ciphertext = encrypt(&key_clone, plaintext.as_bytes())
                .expect("Encryption failed");
            let decrypted = decrypt(&key_clone, &ciphertext)
                .expect("Decryption failed");

            assert_eq!(decrypted, plaintext.as_bytes());
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}

#[test]
fn test_encryption_special_characters() {
    let key: EncryptionKey = rand::random();

    let special_data = "Hello 世界! 🚀 \n\t\r\0 UTF-8: üñíçödé";
    let plaintext = special_data.as_bytes();

    let ciphertext = encrypt(&key, plaintext).expect("Encryption failed");
    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");

    assert_eq!(&decrypted[..], plaintext);
    assert_eq!(
        String::from_utf8(decrypted).unwrap(),
        special_data
    );
}

#[test]
fn test_encryption_binary_data() {
    let key: EncryptionKey = rand::random();

    // Binary data with all byte values
    let plaintext: Vec<u8> = (0..=255).collect();

    let ciphertext = encrypt(&key, &plaintext).expect("Encryption failed");
    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_encryption_performance_overhead() {
    let key: EncryptionKey = rand::random();
    let data_size = 100 * 1024; // 100KB
    let plaintext = vec![0x42; data_size];

    // Measure encryption time
    let iterations = 100;
    let start = Instant::now();
    for _ in 0..iterations {
        let _ = encrypt(&key, &plaintext).expect("Encryption failed");
    }
    let total_duration = start.elapsed();
    let avg_duration = total_duration / iterations;

    println!(
        "Average encryption time for 100KB: {:?} ({:.2} MB/s)",
        avg_duration,
        (data_size as f64 / 1024.0 / 1024.0) / avg_duration.as_secs_f64()
    );

    // Should process at least 0.5 MB/s (very conservative for CI/VM environments)
    // Local runs with AES-NI typically achieve >100 MB/s
    let throughput_mbps = (data_size as f64 / 1024.0 / 1024.0) / avg_duration.as_secs_f64();
    assert!(
        throughput_mbps > 0.5,
        "Encryption throughput too low: {:.2} MB/s",
        throughput_mbps
    );
}

#[test]
fn test_key_derivation_weak_password() {
    // Even weak passwords should produce valid keys
    let weak_password = "123";
    let salt = b"salt____________";

    let key = derive_key_from_password(weak_password, salt)
        .expect("Key derivation failed");

    assert_eq!(key.len(), 32);

    // Key should still work for encryption
    let plaintext = b"test data";
    let ciphertext = encrypt(&key, plaintext).expect("Encryption failed");
    let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");

    assert_eq!(&decrypted[..], plaintext);
}

#[test]
fn test_encrypt_decrypt_roundtrip_stress() {
    let key: EncryptionKey = rand::random();

    // Perform many encrypt/decrypt cycles
    for i in 0..100 {
        let plaintext = format!("Message number {}", i);
        let bytes = plaintext.as_bytes();

        let ciphertext = encrypt(&key, bytes).expect("Encryption failed");
        let decrypted = decrypt(&key, &ciphertext).expect("Decryption failed");

        assert_eq!(decrypted, bytes);
    }
}
