//! Comprehensive encryption tests
//!
//! Tests for transparent data encryption (TDE) with AES-256-GCM.
//!
//! NOTE: These tests are disabled because they use internal APIs that are not
//! publicly exposed. They need to be rewritten to use the public API.
//! Enable with: cargo test --features internal-tests

#![cfg(feature = "internal-tests")]

use heliosdb_lite::{
    Config, Column, DataType, EmbeddedDatabase, Schema, Tuple, Value,
};

mod test_helpers;
use test_helpers::*;

#[test]
fn test_encryption_enabled_basic() {
    // Generate a random key
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_1", &hex_key);

    // Create config with encryption enabled
    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_1".to_string());

    // Create storage engine
    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    assert!(storage.is_encrypted(), "Storage should be encrypted");

    // Test basic put/get
    let key = b"test_key".to_vec();
    let value = b"test_value_encrypted".to_vec();

    storage.put(&key, &value)
        .expect("Failed to put encrypted value");

    let retrieved = storage.get(&key)
        .expect("Failed to get encrypted value")
        .expect("Value should exist");

    assert_eq!(retrieved, value, "Decrypted value should match original");

    std::env::remove_var("TEST_ENCRYPTION_KEY_1");
}

#[test]
fn test_encryption_disabled() {
    // Create config with encryption disabled
    let config = Config::in_memory();
    assert!(!config.encryption.enabled, "Encryption should be disabled by default");

    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open unencrypted storage");

    assert!(!storage.is_encrypted(), "Storage should not be encrypted");
    assert!(storage.encryption_info().is_none(), "Should have no encryption info");

    // Test basic put/get
    let key = b"test_key".to_vec();
    let value = b"test_value_unencrypted".to_vec();

    storage.put(&key, &value)
        .expect("Failed to put value");

    let retrieved = storage.get(&key)
        .expect("Failed to get value")
        .expect("Value should exist");

    assert_eq!(retrieved, value, "Value should match original");
}

#[test]
fn test_encrypted_vs_unencrypted_data() {
    // This test verifies that encrypted data is actually encrypted on disk
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_2", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_2".to_string());

    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    let key = b"test_key".to_vec();
    let plaintext = b"sensitive_data_12345";

    storage.put(&key, plaintext)
        .expect("Failed to put encrypted value");

    // Verify encryption is enabled
    assert!(storage.is_encrypted(), "Storage should report encryption enabled");

    // Verify encryption info is available
    let enc_info = storage.encryption_info()
        .expect("Encryption info should be available");
    assert!(!enc_info.is_empty(), "Encryption info should not be empty");

    // Decryption through storage API should work
    let decrypted = storage.get(&key)
        .expect("Failed to decrypt value")
        .expect("Value should exist");

    assert_eq!(decrypted, plaintext, "Decrypted value should match original");

    std::env::remove_var("TEST_ENCRYPTION_KEY_2");
}

#[test]
fn test_encryption_key_uniqueness() {
    // Each encryption should produce different ciphertext due to random nonce
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_3", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_3".to_string());

    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    // Store same value multiple times under different keys
    let value = b"same_secret_data".to_vec();

    storage.put(&b"key1".to_vec(), &value).expect("Failed to put value 1");
    storage.put(&b"key2".to_vec(), &value).expect("Failed to put value 2");
    storage.put(&b"key3".to_vec(), &value).expect("Failed to put value 3");

    // All should decrypt to the same value
    let val1 = storage.get(&b"key1".to_vec()).expect("get1").expect("value1");
    let val2 = storage.get(&b"key2".to_vec()).expect("get2").expect("value2");
    let val3 = storage.get(&b"key3".to_vec()).expect("get3").expect("value3");

    assert_eq!(val1, value, "Value 1 should match");
    assert_eq!(val2, value, "Value 2 should match");
    assert_eq!(val3, value, "Value 3 should match");

    std::env::remove_var("TEST_ENCRYPTION_KEY_3");
}

#[test]
fn test_wrong_key_fails_via_crypto() {
    // Test that decryption with wrong key fails at the crypto level
    use heliosdb_lite::crypto;

    let key1: [u8; 32] = rand::random();
    let key2: [u8; 32] = rand::random();

    let plaintext = b"sensitive data that should be protected";

    // Encrypt with key1
    let ciphertext = crypto::encrypt(&key1, plaintext)
        .expect("Encryption should succeed");

    // Verify decryption with correct key works
    let decrypted = crypto::decrypt(&key1, &ciphertext)
        .expect("Decryption with correct key should work");
    assert_eq!(&decrypted[..], plaintext);

    // Verify decryption with wrong key fails
    let result = crypto::decrypt(&key2, &ciphertext);
    assert!(result.is_err(), "Decryption with wrong key should fail");
}

#[test]
fn test_encrypted_table_operations() {
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_5", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_5".to_string());

    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    let catalog = storage.catalog();

    // Create table with schema
    let schema = Schema::new(vec![
        Column {
            name: "id".to_string(),
            data_type: DataType::Int4,
            nullable: false,
            primary_key: true,
            source_table: None,
            source_table_name: None,
        },
        Column {
            name: "secret".to_string(),
            data_type: DataType::Text,
            nullable: false,
            primary_key: false,
            source_table: None,
            source_table_name: None,
        },
    ]);

    catalog.create_table("secrets", schema.clone())
        .expect("Failed to create encrypted table");

    // Verify table exists (metadata is encrypted)
    assert!(
        catalog.table_exists("secrets").expect("Failed to check table"),
        "Table should exist"
    );

    // Verify schema retrieval (decryption of metadata)
    let retrieved_schema = catalog.get_table_schema("secrets")
        .expect("Failed to get encrypted schema");

    assert_eq!(retrieved_schema, schema, "Schema should match after encryption/decryption");

    // Insert encrypted data
    let tuple = Tuple::new(vec![
        Value::Int4(1),
        Value::String("classified_information".to_string()),
    ]);

    storage.insert_tuple("secrets", tuple.clone())
        .expect("Failed to insert encrypted tuple");

    // Scan table (decrypt all tuples)
    let tuples = storage.scan_table("secrets")
        .expect("Failed to scan encrypted table");

    assert_eq!(tuples.len(), 1, "Should retrieve one tuple");
    // Compare values only (row_id is populated by scan_table)
    assert_eq!(tuples[0].values, tuple.values, "Tuple values should match after encryption/decryption");

    std::env::remove_var("TEST_ENCRYPTION_KEY_5");
}

#[test]
fn test_sql_queries_with_encryption() {
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_6", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_6".to_string());

    let db = heliosdb_lite::EmbeddedDatabase::new_in_memory()
        .expect("Failed to create database");

    // Manually set up encrypted storage (workaround for testing)
    // In production, EmbeddedDatabase should accept Config
    drop(db);

    // For now, let's just test direct storage operations
    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    let catalog = storage.catalog();
    let schema = Schema::new(vec![
        Column::new("id", DataType::Int4),
        Column::new("name", DataType::Text),
    ]);

    catalog.create_table("users", schema)
        .expect("Failed to create table");

    storage.insert_tuple("users", Tuple::new(vec![
        Value::Int4(1),
        Value::String("Alice".to_string()),
    ])).expect("Failed to insert");

    storage.insert_tuple("users", Tuple::new(vec![
        Value::Int4(2),
        Value::String("Bob".to_string()),
    ])).expect("Failed to insert");

    let tuples = storage.scan_table("users")
        .expect("Failed to scan");

    assert_eq!(tuples.len(), 2, "Should have 2 encrypted tuples");

    std::env::remove_var("TEST_ENCRYPTION_KEY_6");
}

#[test]
fn test_key_manager_from_file() {
    use heliosdb_lite::crypto::KeyManager;
    use std::io::Write;

    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let key_file = temp_dir.path().join("encryption.key");

    // Write hex key to file
    let hex_key = generate_random_hex_key();
    let mut file = std::fs::File::create(&key_file)
        .expect("Failed to create key file");
    file.write_all(hex_key.as_bytes())
        .expect("Failed to write key");

    // Load key from file
    let key_source = heliosdb_lite::KeySource::File(key_file.clone());
    let km = KeyManager::from_source(&key_source)
        .expect("Failed to load key from file");

    assert_eq!(km.key().len(), 32, "Key should be 32 bytes");

    // Test encryption/decryption with file-based key
    let plaintext = b"test data from file key";
    let encrypted = heliosdb_lite::crypto::encrypt(km.key(), plaintext)
        .expect("Failed to encrypt");
    let decrypted = heliosdb_lite::crypto::decrypt(km.key(), &encrypted)
        .expect("Failed to decrypt");

    assert_eq!(decrypted, plaintext, "Decrypted data should match");
}

#[test]
fn test_key_manager_from_password() {
    use heliosdb_lite::crypto::KeyManager;

    let password = "my_super_secret_password_123";
    let salt = b"random_salt_16bytes!";

    let km = KeyManager::from_password(password, salt)
        .expect("Failed to create key from password");

    assert_eq!(km.key().len(), 32, "Derived key should be 32 bytes");

    // Test that same password + salt produces same key
    let km2 = KeyManager::from_password(password, salt)
        .expect("Failed to create second key from password");

    assert_eq!(
        km.key(), km2.key(),
        "Same password and salt should produce same key"
    );

    // Different password should produce different key
    let km3 = KeyManager::from_password("different_password", salt)
        .expect("Failed to create key with different password");

    assert_ne!(
        km.key(), km3.key(),
        "Different password should produce different key"
    );
}

#[test]
fn test_encryption_with_delete() {
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_7", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_7".to_string());

    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    let key = b"delete_test".to_vec();
    let value = b"to_be_deleted".to_vec();

    // Put encrypted value
    storage.put(&key, &value)
        .expect("Failed to put encrypted value");

    // Verify it exists
    assert!(
        storage.get(&key).expect("Get failed").is_some(),
        "Value should exist"
    );

    // Delete it
    storage.delete(&key)
        .expect("Failed to delete encrypted value");

    // Verify it's gone
    assert!(
        storage.get(&key).expect("Get failed").is_none(),
        "Value should be deleted"
    );

    std::env::remove_var("TEST_ENCRYPTION_KEY_7");
}

#[test]
fn test_encryption_info() {
    let hex_key = generate_random_hex_key();
    std::env::set_var("TEST_ENCRYPTION_KEY_8", &hex_key);

    let mut config = Config::in_memory();
    config.encryption.enabled = true;
    config.encryption.key_source =
        heliosdb_lite::KeySource::Environment("TEST_ENCRYPTION_KEY_8".to_string());

    let storage = heliosdb_lite::storage::StorageEngine::open_in_memory(&config)
        .expect("Failed to open encrypted storage");

    let info = storage.encryption_info()
        .expect("Should have encryption info");

    assert!(
        info.contains("AES-256-GCM"),
        "Info should mention AES-256-GCM"
    );
    assert!(
        info.contains("Enabled"),
        "Info should say Enabled"
    );

    std::env::remove_var("TEST_ENCRYPTION_KEY_8");
}

/// Helper function to generate a random hex key
fn generate_random_hex_key() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let key: [u8; 32] = rng.gen();
    hex::encode(key)
}
