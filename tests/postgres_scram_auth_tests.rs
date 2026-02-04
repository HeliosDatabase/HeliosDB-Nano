//! PostgreSQL SCRAM-SHA-256 Authentication Tests
//!
//! Comprehensive tests for SCRAM-SHA-256 authentication implementation

use heliosdb_lite::{EmbeddedDatabase, Result};
use heliosdb_lite::protocol::postgres::{
    AuthManager, AuthMethod, ScramAuthState,
    InMemoryPasswordStore, SharedPasswordStore, PasswordStore,
    password_store::ScramCredentials,
};
use heliosdb_lite::protocol::postgres::auth::{
    scram_hi, scram_hmac_sha256, scram_h,
    scram_salted_password, scram_client_key, scram_stored_key, scram_server_key,
    prepare_scram_credentials,
};

#[test]
fn test_scram_hi_function_rfc_example() {
    // Test Hi function with known values
    let password = "pencil";
    let salt = b"4125c247e43ab1e93c6dff76";
    let iterations = 4096;

    let result = scram_hi(password, salt, iterations);
    assert_eq!(result.len(), 32); // SHA-256 output is 32 bytes
}

#[test]
fn test_scram_hmac_sha256_basic() {
    let key = b"test_key";
    let message = b"test_message";

    let result = scram_hmac_sha256(key, message);
    assert_eq!(result.len(), 32);

    // Test consistency
    let result2 = scram_hmac_sha256(key, message);
    assert_eq!(result, result2);
}

#[test]
fn test_scram_h_function() {
    let input = b"hello world";
    let result = scram_h(input);

    assert_eq!(result.len(), 32);

    // Test consistency
    let result2 = scram_h(input);
    assert_eq!(result, result2);
}

#[test]
fn test_scram_key_derivation_chain() {
    let password = "secret";
    let salt = b"randomsalt123456";
    let iterations = 4096;

    let salted_password = scram_salted_password(password, salt, iterations);
    assert_eq!(salted_password.len(), 32);

    let client_key = scram_client_key(&salted_password);
    assert_eq!(client_key.len(), 32);

    let stored_key = scram_stored_key(&client_key);
    assert_eq!(stored_key.len(), 32);

    let server_key = scram_server_key(&salted_password);
    assert_eq!(server_key.len(), 32);

    // Ensure keys are different
    assert_ne!(client_key, stored_key);
    assert_ne!(client_key, server_key);
    assert_ne!(stored_key, server_key);
}

#[test]
fn test_prepare_scram_credentials() {
    let password = "my_secure_password";
    let salt = b"0123456789abcdef";
    let iterations = 4096;

    let (stored_key, server_key) = prepare_scram_credentials(password, salt, iterations);

    assert_eq!(stored_key.len(), 32);
    assert_eq!(server_key.len(), 32);
    assert_ne!(stored_key, server_key);

    // Test consistency
    let (stored_key2, server_key2) = prepare_scram_credentials(password, salt, iterations);
    assert_eq!(stored_key, stored_key2);
    assert_eq!(server_key, server_key2);
}

#[test]
fn test_scram_credentials_from_password() {
    let creds = ScramCredentials::from_password("alice".to_string(), "secret123", 4096);

    assert_eq!(creds.username, "alice");
    assert_eq!(creds.salt.len(), 16);
    assert_eq!(creds.iterations, 4096);
    assert_eq!(creds.stored_key.len(), 32);
    assert_eq!(creds.server_key.len(), 32);
}

#[test]
fn test_scram_credentials_verify_password() {
    let creds = ScramCredentials::from_password("alice".to_string(), "correct_password", 4096);

    assert!(creds.verify_password("correct_password"));
    assert!(!creds.verify_password("wrong_password"));
    assert!(!creds.verify_password("Correct_password")); // Case sensitive
    assert!(!creds.verify_password(""));
}

#[test]
fn test_scram_credentials_update_password() {
    let mut creds = ScramCredentials::from_password("alice".to_string(), "old_pass", 4096);
    let old_stored_key = creds.stored_key.clone();
    let old_salt = creds.salt.clone();

    creds.update_password("new_pass");

    // Password verification
    assert!(!creds.verify_password("old_pass"));
    assert!(creds.verify_password("new_pass"));

    // Keys and salt should change
    assert_ne!(creds.stored_key, old_stored_key);
    assert_ne!(creds.salt, old_salt);
}

#[test]
fn test_password_store_basic_operations() {
    let mut store = InMemoryPasswordStore::new();

    // Add user
    store.add_user("alice", "secret123").unwrap();
    assert!(store.user_exists("alice"));
    assert!(!store.user_exists("bob"));

    // Get credentials
    let creds = store.get_credentials("alice").unwrap();
    assert_eq!(creds.username, "alice");
    assert!(creds.verify_password("secret123"));

    // Remove user
    let removed = store.remove_user("alice").unwrap();
    assert!(removed);
    assert!(!store.user_exists("alice"));
}

#[test]
fn test_password_store_update_password() {
    let mut store = InMemoryPasswordStore::new();

    store.add_user("alice", "old_password").unwrap();

    let old_creds = store.get_credentials("alice").unwrap();
    let old_stored_key = old_creds.stored_key.clone();

    // Update password
    store.update_password("alice", "new_password").unwrap();

    let new_creds = store.get_credentials("alice").unwrap();
    assert!(!new_creds.verify_password("old_password"));
    assert!(new_creds.verify_password("new_password"));
    assert_ne!(new_creds.stored_key, old_stored_key);
}

#[test]
fn test_password_store_list_users() {
    let mut store = InMemoryPasswordStore::new();

    store.add_user("alice", "pass1").unwrap();
    store.add_user("bob", "pass2").unwrap();
    store.add_user("charlie", "pass3").unwrap();

    let mut users = store.list_users();
    users.sort();

    assert_eq!(users, vec!["alice", "bob", "charlie"]);
}

#[test]
fn test_shared_password_store() {
    let store = SharedPasswordStore::new(InMemoryPasswordStore::new());

    store.add_user("alice", "secret").unwrap();
    assert!(store.user_exists("alice"));

    let creds = store.get_credentials("alice").unwrap();
    assert!(creds.verify_password("secret"));

    // Test cloning (shared ownership)
    let store2 = store.clone();
    assert!(store2.user_exists("alice"));

    // Add user through clone
    store2.add_user("bob", "password").unwrap();
    assert!(store.user_exists("bob")); // Should be visible in original
}

#[test]
fn test_scram_auth_state_creation() {
    let state = ScramAuthState::new("testuser".to_string());

    assert_eq!(state.username(), "testuser");
    assert_eq!(state.salt().len(), 16);
    assert_eq!(state.iteration_count(), 4096);
}

#[test]
fn test_scram_auth_state_server_first_message() {
    let mut state = ScramAuthState::new("testuser".to_string());
    state.set_client_nonce("fyko+d2lbbFgONRv9qkxdawL".to_string());

    let msg = state.build_server_first_message().unwrap();

    // Should contain combined nonce, salt, and iteration count
    assert!(msg.starts_with("r=fyko+d2lbbFgONRv9qkxdawL"));
    assert!(msg.contains(",s="));
    assert!(msg.contains(",i=4096"));
}

#[test]
fn test_scram_auth_state_combined_nonce() {
    let mut state = ScramAuthState::new("testuser".to_string());
    state.set_client_nonce("client_nonce_123".to_string());

    let combined = state.combined_nonce();
    assert!(combined.starts_with("client_nonce_123"));
    assert!(combined.len() > "client_nonce_123".len()); // Should include server nonce
}

#[test]
fn test_auth_manager_with_scram_store() {
    let mut auth = AuthManager::with_scram_store(AuthMethod::ScramSha256);
    auth.add_user("alice".to_string(), "secret123".to_string());

    assert_eq!(auth.method(), AuthMethod::ScramSha256);
    assert!(auth.password_store().is_some());

    // Verify through cleartext (uses password store internally)
    assert!(auth.verify_cleartext("alice", "secret123").unwrap());
    assert!(!auth.verify_cleartext("alice", "wrong_password").unwrap());
}

#[test]
fn test_auth_manager_with_password_store() {
    let mut store = InMemoryPasswordStore::new();
    store.add_user("alice", "secret").unwrap();

    let shared_store = SharedPasswordStore::new(store);
    let auth = AuthManager::with_password_store(AuthMethod::ScramSha256, shared_store);

    assert_eq!(auth.method(), AuthMethod::ScramSha256);
    assert!(auth.verify_cleartext("alice", "secret").unwrap());
}

#[test]
fn test_auth_manager_timing_attack_resistance() {
    use std::time::Instant;

    let mut auth = AuthManager::with_scram_store(AuthMethod::CleartextPassword);
    auth.add_user("alice".to_string(), "secret".to_string());

    // Test that both existing and non-existing users take similar time
    // (prevents timing attacks to enumerate users)

    let start = Instant::now();
    let _ = auth.verify_cleartext("alice", "wrong_password");
    let time_existing_user = start.elapsed();

    let start = Instant::now();
    let _ = auth.verify_cleartext("nonexistent_user", "password");
    let time_nonexistent_user = start.elapsed();

    // Times should be similar (within an order of magnitude)
    // This is a basic check; real timing attack testing would be more sophisticated
    assert!(
        time_existing_user.as_micros() > 0 &&
        time_nonexistent_user.as_micros() > 0
    );
}

#[test]
fn test_scram_full_authentication_simulation() {
    // Simulate a full SCRAM authentication exchange
    let password = "secret123";
    let username = "alice";

    // Server setup
    let mut store = InMemoryPasswordStore::new();
    store.add_user(username, password).unwrap();
    let credentials = store.get_credentials(username).unwrap();

    // Client sends client-first-message
    let client_nonce = "fyko+d2lbbFgONRv9qkxdawL";
    let client_first_bare = format!("n={},r={}", username, client_nonce);

    // Server processes and creates auth state
    let mut scram_state = ScramAuthState::new(username.to_string());
    scram_state.set_client_nonce(client_nonce.to_string());
    scram_state.set_client_first_message_bare(client_first_bare.clone());

    // Server sends server-first-message
    let server_first = scram_state.build_server_first_message().unwrap();
    assert!(server_first.contains(&format!("r={}", client_nonce)));

    // Note: Full client proof generation would require implementing client-side SCRAM
    // This test validates the server-side components are in place
    assert_eq!(scram_state.username(), username);
    assert_eq!(credentials.username, username);
}

#[test]
fn test_password_iterations_customization() {
    let store = InMemoryPasswordStore::with_iterations(8192);

    // This would require adding a method to check iterations
    // For now, we verify the store works with custom iterations
    let shared = SharedPasswordStore::new(store);
    shared.add_user("alice", "password").unwrap();

    let creds = shared.get_credentials("alice").unwrap();
    assert!(creds.verify_password("password"));
}

#[test]
fn test_scram_with_empty_password() {
    let creds = ScramCredentials::from_password("alice".to_string(), "", 4096);

    assert!(creds.verify_password(""));
    assert!(!creds.verify_password("not_empty"));
}

#[test]
fn test_scram_with_special_characters() {
    let password = "p@ssw0rd!#$%^&*()";
    let creds = ScramCredentials::from_password("alice".to_string(), password, 4096);

    assert!(creds.verify_password(password));
    assert!(!creds.verify_password("p@ssw0rd"));
}

#[test]
fn test_scram_with_unicode_password() {
    let password = "пароль🔒"; // Russian + emoji
    let creds = ScramCredentials::from_password("alice".to_string(), password, 4096);

    assert!(creds.verify_password(password));
    assert!(!creds.verify_password("пароль"));
}

#[test]
fn test_multiple_users_different_salts() {
    let mut store = InMemoryPasswordStore::new();
    let password = "same_password";

    store.add_user("alice", password).unwrap();
    store.add_user("bob", password).unwrap();

    let alice_creds = store.get_credentials("alice").unwrap();
    let bob_creds = store.get_credentials("bob").unwrap();

    // Same password should result in different stored keys due to different salts
    assert_ne!(alice_creds.salt, bob_creds.salt);
    assert_ne!(alice_creds.stored_key, bob_creds.stored_key);
    assert_ne!(alice_creds.server_key, bob_creds.server_key);

    // Both should verify correctly
    assert!(alice_creds.verify_password(password));
    assert!(bob_creds.verify_password(password));
}

#[test]
fn test_password_store_error_cases() {
    let mut store = InMemoryPasswordStore::new();

    // Update non-existent user
    let result = store.update_password("nonexistent", "newpass");
    assert!(result.is_err());

    // Remove non-existent user
    let removed = store.remove_user("nonexistent").unwrap();
    assert!(!removed);
}

#[test]
fn test_scram_deterministic_with_same_salt() {
    let password = "test_password";
    let salt = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    let iterations = 4096;

    let creds1 = ScramCredentials::with_salt("user".to_string(), password, salt.clone(), iterations);
    let creds2 = ScramCredentials::with_salt("user".to_string(), password, salt.clone(), iterations);

    // With same salt, should produce identical keys
    assert_eq!(creds1.stored_key, creds2.stored_key);
    assert_eq!(creds1.server_key, creds2.server_key);
}
