//! Password storage for SCRAM-SHA-256 authentication
//!
//! This module provides a secure way to store and verify user passwords
//! for SCRAM-SHA-256 authentication. Instead of storing plaintext passwords,
//! it stores the derived keys (stored_key and server_key) along with the salt
//! and iteration count.
//!
//! ## Security Model
//!
//! - Passwords are never stored in plaintext
//! - Each user has a unique random salt
//! - PBKDF2-HMAC-SHA256 with configurable iterations (default: 4096)
//! - Constant-time comparison for password verification
//!
//! ## Usage
//!
//! ```rust,no_run
//! use heliosdb_nano::protocol::postgres::password_store::{PasswordStore, InMemoryPasswordStore};
//!
//! let mut store = InMemoryPasswordStore::new();
//! store.add_user("alice", "secret123");
//!
//! // Later, during authentication
//! if let Some(credentials) = store.get_credentials("alice") {
//!     // Use credentials.stored_key and credentials.server_key for SCRAM verification
//! }
//! ```

use crate::{Result, Error};
use parking_lot::RwLock;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;

use super::auth::{prepare_scram_credentials, scram_salted_password, scram_client_key, scram_stored_key};

/// SCRAM-SHA-256 credentials stored for a user
#[derive(Debug, Clone)]
pub struct ScramCredentials {
    /// Username
    pub username: String,
    /// Random salt used for key derivation
    pub salt: Vec<u8>,
    /// PBKDF2 iteration count
    pub iterations: u32,
    /// Stored key: H(ClientKey)
    pub stored_key: Vec<u8>,
    /// Server key: HMAC(SaltedPassword, "Server Key")
    pub server_key: Vec<u8>,
}

impl ScramCredentials {
    /// Create new credentials from a password
    pub fn from_password(username: String, password: &str, iterations: u32) -> Self {
        let mut rng = rand::thread_rng();
        let salt: Vec<u8> = (0..16).map(|_| rng.gen::<u8>()).collect();

        let (stored_key, server_key) = prepare_scram_credentials(password, &salt, iterations);

        Self {
            username,
            salt,
            iterations,
            stored_key,
            server_key,
        }
    }

    /// Create credentials with a specific salt (for testing or migration)
    pub fn with_salt(username: String, password: &str, salt: Vec<u8>, iterations: u32) -> Self {
        let (stored_key, server_key) = prepare_scram_credentials(password, &salt, iterations);

        Self {
            username,
            salt,
            iterations,
            stored_key,
            server_key,
        }
    }

    /// Update password (generates new salt)
    pub fn update_password(&mut self, new_password: &str) {
        let mut rng = rand::thread_rng();
        self.salt = (0..16).map(|_| rng.gen::<u8>()).collect();

        let (stored_key, server_key) = prepare_scram_credentials(new_password, &self.salt, self.iterations);

        self.stored_key = stored_key;
        self.server_key = server_key;
    }

    /// Verify that a password matches these credentials
    pub fn verify_password(&self, password: &str) -> bool {
        let salted_password = scram_salted_password(password, &self.salt, self.iterations);
        let client_key = scram_client_key(&salted_password);
        let computed_stored_key = scram_stored_key(&client_key);

        // Constant-time comparison
        constant_time_compare(&computed_stored_key, &self.stored_key)
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

/// Password storage trait
///
/// Implement this trait to provide custom password storage backends
/// (e.g., database, file, LDAP, etc.)
pub trait PasswordStore: Send + Sync {
    /// Get SCRAM credentials for a user
    fn get_credentials(&self, username: &str) -> Option<ScramCredentials>;

    /// Add or update a user with a password
    fn add_user(&mut self, username: &str, password: &str) -> Result<()>;

    /// Remove a user
    fn remove_user(&mut self, username: &str) -> Result<bool>;

    /// Update a user's password
    fn update_password(&mut self, username: &str, new_password: &str) -> Result<()>;

    /// Check if a user exists
    fn user_exists(&self, username: &str) -> bool;

    /// List all usernames
    fn list_users(&self) -> Vec<String>;
}

/// In-memory password store implementation
///
/// This is suitable for development, testing, and small deployments.
/// For production use with persistence, implement a custom PasswordStore
/// backed by a database.
pub struct InMemoryPasswordStore {
    users: Arc<RwLock<HashMap<String, ScramCredentials>>>,
    default_iterations: u32,
}

impl InMemoryPasswordStore {
    /// Create a new empty in-memory password store
    pub fn new() -> Self {
        Self {
            users: Arc::new(RwLock::new(HashMap::new())),
            default_iterations: 4096,
        }
    }

    /// Create with custom iteration count
    pub fn with_iterations(iterations: u32) -> Self {
        Self {
            users: Arc::new(RwLock::new(HashMap::new())),
            default_iterations: iterations,
        }
    }

    /// Create with default test users
    pub fn with_test_users() -> Self {
        let mut store = Self::new();
        let _ = store.add_user("postgres", "postgres");
        let _ = store.add_user("admin", "admin");
        let _ = store.add_user("test", "test");
        store
    }

    /// Get iteration count for a user (for testing)
    pub fn get_iterations(&self, username: &str) -> Option<u32> {
        self.users.read().get(username).map(|cred| cred.iterations)
    }

    /// Get salt for a user (for testing)
    pub fn get_salt(&self, username: &str) -> Option<Vec<u8>> {
        self.users.read().get(username).map(|cred| cred.salt.clone())
    }
}

impl Default for InMemoryPasswordStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PasswordStore for InMemoryPasswordStore {
    fn get_credentials(&self, username: &str) -> Option<ScramCredentials> {
        self.users.read().get(username).cloned()
    }

    fn add_user(&mut self, username: &str, password: &str) -> Result<()> {
        let credentials = ScramCredentials::from_password(
            username.to_string(),
            password,
            self.default_iterations,
        );

        self.users.write().insert(username.to_string(), credentials);
        Ok(())
    }

    fn remove_user(&mut self, username: &str) -> Result<bool> {
        Ok(self.users.write().remove(username).is_some())
    }

    fn update_password(&mut self, username: &str, new_password: &str) -> Result<()> {
        let mut users = self.users.write();
        if let Some(credentials) = users.get_mut(username) {
            credentials.update_password(new_password);
            Ok(())
        } else {
            Err(Error::authentication(format!("User not found: {}", username)))
        }
    }

    fn user_exists(&self, username: &str) -> bool {
        self.users.read().contains_key(username)
    }

    fn list_users(&self) -> Vec<String> {
        self.users.read().keys().cloned().collect()
    }
}

// Thread-safe wrapper for Arc<RwLock<dyn PasswordStore>>
/// Shared password store that can be cloned and shared across threads
#[derive(Clone)]
pub struct SharedPasswordStore {
    inner: Arc<RwLock<Box<dyn PasswordStore>>>,
}

impl SharedPasswordStore {
    /// Create a new shared password store
    pub fn new<T: PasswordStore + 'static>(store: T) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Box::new(store))),
        }
    }

    /// Get credentials for a user
    pub fn get_credentials(&self, username: &str) -> Option<ScramCredentials> {
        self.inner.read().get_credentials(username)
    }

    /// Add a user
    pub fn add_user(&self, username: &str, password: &str) -> Result<()> {
        self.inner.write().add_user(username, password)
    }

    /// Remove a user
    pub fn remove_user(&self, username: &str) -> Result<bool> {
        self.inner.write().remove_user(username)
    }

    /// Update password
    pub fn update_password(&self, username: &str, new_password: &str) -> Result<()> {
        self.inner.write().update_password(username, new_password)
    }

    /// Check if user exists
    pub fn user_exists(&self, username: &str) -> bool {
        self.inner.read().user_exists(username)
    }

    /// List all users
    pub fn list_users(&self) -> Vec<String> {
        self.inner.read().list_users()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_scram_credentials_from_password() {
        let creds = ScramCredentials::from_password("alice".to_string(), "secret", 4096);

        assert_eq!(creds.username, "alice");
        assert_eq!(creds.salt.len(), 16);
        assert_eq!(creds.iterations, 4096);
        assert_eq!(creds.stored_key.len(), 32);
        assert_eq!(creds.server_key.len(), 32);
    }

    #[test]
    fn test_scram_credentials_verify_password() {
        let creds = ScramCredentials::from_password("alice".to_string(), "secret", 4096);

        assert!(creds.verify_password("secret"));
        assert!(!creds.verify_password("wrong"));
        assert!(!creds.verify_password("Secret")); // Case sensitive
    }

    #[test]
    fn test_scram_credentials_update_password() {
        let mut creds = ScramCredentials::from_password("alice".to_string(), "old_password", 4096);
        let old_salt = creds.salt.clone();

        creds.update_password("new_password");

        assert!(!creds.verify_password("old_password"));
        assert!(creds.verify_password("new_password"));
        assert_ne!(old_salt, creds.salt); // Salt should change
    }

    #[test]
    fn test_in_memory_store_basic() {
        let mut store = InMemoryPasswordStore::new();

        store.add_user("alice", "secret").unwrap();
        assert!(store.user_exists("alice"));
        assert!(!store.user_exists("bob"));

        let creds = store.get_credentials("alice").unwrap();
        assert_eq!(creds.username, "alice");
        assert!(creds.verify_password("secret"));
    }

    #[test]
    fn test_in_memory_store_update_password() {
        let mut store = InMemoryPasswordStore::new();

        store.add_user("alice", "old_password").unwrap();
        store.update_password("alice", "new_password").unwrap();

        let creds = store.get_credentials("alice").unwrap();
        assert!(!creds.verify_password("old_password"));
        assert!(creds.verify_password("new_password"));
    }

    #[test]
    fn test_in_memory_store_remove_user() {
        let mut store = InMemoryPasswordStore::new();

        store.add_user("alice", "secret").unwrap();
        assert!(store.user_exists("alice"));

        let removed = store.remove_user("alice").unwrap();
        assert!(removed);
        assert!(!store.user_exists("alice"));

        let not_removed = store.remove_user("bob").unwrap();
        assert!(!not_removed);
    }

    #[test]
    fn test_in_memory_store_list_users() {
        let mut store = InMemoryPasswordStore::new();

        store.add_user("alice", "secret1").unwrap();
        store.add_user("bob", "secret2").unwrap();
        store.add_user("charlie", "secret3").unwrap();

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

        // Test cloning
        let store2 = store.clone();
        assert!(store2.user_exists("alice"));
    }

    #[test]
    fn test_constant_time_compare() {
        let a = vec![1, 2, 3, 4];
        let b = vec![1, 2, 3, 4];
        let c = vec![1, 2, 3, 5];

        assert!(constant_time_compare(&a, &b));
        assert!(!constant_time_compare(&a, &c));
        assert!(!constant_time_compare(&a, &[1, 2, 3]));
    }

    #[test]
    fn test_with_test_users() {
        let store = InMemoryPasswordStore::with_test_users();

        assert!(store.user_exists("postgres"));
        assert!(store.user_exists("admin"));
        assert!(store.user_exists("test"));

        let creds = store.get_credentials("postgres").unwrap();
        assert!(creds.verify_password("postgres"));
    }
}
