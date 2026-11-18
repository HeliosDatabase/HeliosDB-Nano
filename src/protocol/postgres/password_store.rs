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

use crate::{Error, Result};
use parking_lot::RwLock;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use super::auth::{prepare_scram_credentials, scram_client_key, scram_salted_password, scram_stored_key};

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

    /// The credential in PostgreSQL's stored form,
    /// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>` (base64).
    /// This is what a role's catalog record keeps instead of its password.
    pub fn to_verifier(&self) -> String {
        use base64::Engine;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        format!(
            "{SCRAM_VERIFIER_PREFIX}{}:{}${}:{}",
            self.iterations,
            b64(&self.salt),
            b64(&self.stored_key),
            b64(&self.server_key)
        )
    }

    /// Read a credential back from its stored form. `None` for anything that is
    /// not exactly one: a text that merely starts like a verifier is not a key.
    pub fn from_verifier(username: &str, verifier: &str) -> Option<Self> {
        use base64::Engine;
        let b64 = |text: &str| base64::engine::general_purpose::STANDARD.decode(text).ok();
        let (head, keys) = verifier.strip_prefix(SCRAM_VERIFIER_PREFIX)?.split_once('$')?;
        let (iterations, salt) = head.split_once(':')?;
        let (stored_key, server_key) = keys.split_once(':')?;
        let iterations: u32 = iterations.parse().ok()?;
        let (salt, stored_key, server_key) = (b64(salt)?, b64(stored_key)?, b64(server_key)?);
        let well_formed = (1..=SCRAM_VERIFIER_MAX_ITERATIONS).contains(&iterations)
            && (8..=64).contains(&salt.len())
            && stored_key.len() == 32
            && server_key.len() == 32;
        well_formed.then(|| Self {
            username: username.to_owned(),
            salt,
            iterations,
            stored_key,
            server_key,
        })
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
    bool::from(a.ct_eq(b))
}

/// How a stored SCRAM-SHA-256 verifier begins (PostgreSQL's `pg_authid` form).
const SCRAM_VERIFIER_PREFIX: &str = "SCRAM-SHA-256$";

/// More iterations than this in a stored verifier is a record this server did
/// not write and will not spend a connection's time on.
const SCRAM_VERIFIER_MAX_ITERATIONS: u32 = 1_000_000;

/// What `CREATE ROLE … PASSWORD` and `ALTER ROLE … PASSWORD` keep for a role:
/// a SCRAM-SHA-256 verifier, never the password (GitHub #49).
///
/// A value that already is a verifier (a dump of roles writes them) is kept as
/// it is. A PostgreSQL MD5 hash (`md5` and 32 hex digits) is kept too: it is
/// not a password, and it cannot answer a SCRAM exchange, so such a role cannot
/// log in until it is given a password. Anything else is a password: it is
/// hashed with a fresh salt at the server's iteration count, the shape of
/// every other credential this server challenges with.
pub fn role_password_for_storage(role: &str, password: &str) -> String {
    let md5_hash = password
        .strip_prefix("md5")
        .is_some_and(|digest| digest.len() == 32 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if md5_hash || ScramCredentials::from_verifier(role, password).is_some() {
        return password.to_owned();
    }
    ScramCredentials::from_password(role.to_owned(), password, super::auth::DEFAULT_SCRAM_ITERATIONS).to_verifier()
}

/// The credential a SQL role logs in with, when it may log in at all (GitHub #49).
///
/// `None`, and with it the uniform rejection every failed login gets, for a
/// role without `LOGIN`, a role past its `VALID UNTIL` (or with one that is not
/// a time), a role without a password, and a role whose password was stored by
/// a release before 4.42.0. Those kept the password as written and never used
/// it: an upgrade must not turn such a record into an account. Setting the
/// password again (`ALTER ROLE … PASSWORD`) stores a verifier and enables the
/// login.
pub(crate) fn role_login_credentials(role: &crate::storage::RoleRecord) -> Option<ScramCredentials> {
    if !role.rolcanlogin {
        return None;
    }
    if let Some(until) = role.rolvaliduntil.as_deref() {
        match role_valid_until(until) {
            ValidUntil::At(end) if end <= chrono::Utc::now() => return None,
            ValidUntil::At(_) | ValidUntil::Forever => {}
            ValidUntil::NotATime => {
                tracing::warn!(
                    role = role.name.as_str(),
                    "VALID UNTIL of this role is not a time; the role cannot log in"
                );
                return None;
            }
        }
    }
    let stored = role.password.as_deref()?;
    let credentials = ScramCredentials::from_verifier(&role.name, stored);
    if credentials.is_none() {
        tracing::warn!(
            role = role.name.as_str(),
            "this role's password was not stored as a SCRAM verifier (set before 4.42.0, or an MD5 hash); \
             ALTER ROLE … PASSWORD enables its login"
        );
    }
    credentials
}

/// A role's `VALID UNTIL` text, read.
enum ValidUntil {
    /// `infinity`.
    Forever,
    At(chrono::DateTime<chrono::Utc>),
    NotATime,
}

/// Read a `VALID UNTIL` text. A time without a zone is UTC, a date is its
/// midnight.
fn role_valid_until(text: &str) -> ValidUntil {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
    let text = text.trim();
    if text.eq_ignore_ascii_case("infinity") {
        return ValidUntil::Forever;
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return ValidUntil::At(time.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"] {
        if let Ok(time) = DateTime::parse_from_str(text, format) {
            return ValidUntil::At(time.with_timezone(&Utc));
        }
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(time) = NaiveDateTime::parse_from_str(text, format) {
            return ValidUntil::At(time.and_utc());
        }
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map_or(ValidUntil::NotATime, |midnight| ValidUntil::At(midnight.and_utc()))
}

/// Password storage trait
///
/// Implement this trait to provide custom password storage backends
/// (e.g., database, file, LDAP, etc.)
pub trait PasswordStore: Send + Sync {
    /// Get SCRAM credentials for a user
    fn get_credentials(&self, username: &str) -> Option<ScramCredentials>;

    /// Iteration count advertised for the synthetic challenge served to an
    /// ABSENT user (HDB-001).
    ///
    /// # Homogeneity contract — read this before returning imported verifiers
    ///
    /// The synthetic challenge for a name that does not exist advertises
    /// EXACTLY this iteration count and a 16-byte salt (the shape
    /// [`ScramCredentials::from_password`] produces). Uniformity therefore
    /// requires that *every* verifier this backend hands back from
    /// [`PasswordStore::get_credentials`] use that same iteration count and a
    /// 16-byte salt.
    ///
    /// A backend holding verifiers of other shapes — typically ones imported
    /// from another system, PostgreSQL's own `pg_authid.rolpassword` included,
    /// which may carry 4096 iterations and a salt of some other length — leaks
    /// account existence through the `i=` and `s=` fields of the
    /// server-first-message: an attacker who sees a non-default shape knows the
    /// name is real, and one who sees the default shape for a name whose
    /// neighbours are all non-default knows it is not. Such a backend MUST
    /// normalise: re-derive each verifier at the account's next successful
    /// login (with [`ScramCredentials::from_password`] at this count), or
    /// re-create the accounts, before it can claim the HDB-001 property.
    ///
    /// Returning a count that this backend does not actually derive new
    /// credentials with has the same effect and is equally unsafe.
    fn default_scram_iterations(&self) -> u32 {
        super::auth::DEFAULT_SCRAM_ITERATIONS
    }

    /// Stable secret used to derive the synthetic credentials absent users are
    /// challenged with (HDB-001).
    ///
    /// A PERSISTENT backend should keep a cryptographically random 32-byte
    /// secret beside its credential data and return it here, so the synthetic
    /// salt for a given name survives a restart — a salt that changes per
    /// process is itself an account oracle. Ephemeral backends can keep the
    /// default and receive a random secret for the wrapper's lifetime. An owner
    /// that stores the secret elsewhere can pass it to
    /// [`SharedPasswordStore::with_mock_authentication_secret`] instead. Never
    /// derive it from a public identifier.
    ///
    /// In short: a PERSISTENT backend must supply the secret; an EPHEMERAL one
    /// (whose credentials die with the process anyway) needs nothing here.
    /// There is deliberately no way to ask a `dyn PasswordStore` which it is —
    /// the backend is the only thing that knows, so this is a documented
    /// contract, not a runtime check.
    fn scram_mock_authentication_secret(&self) -> Option<[u8; 32]> {
        None
    }

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

    fn default_scram_iterations(&self) -> u32 {
        self.default_iterations
    }

    fn add_user(&mut self, username: &str, password: &str) -> Result<()> {
        let credentials = ScramCredentials::from_password(username.to_string(), password, self.default_iterations);

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
    /// Secret behind the synthetic credentials absent users are challenged
    /// with. Taken from the backend when it persists one, otherwise random for
    /// this wrapper's lifetime.
    mock_authentication_secret: Arc<Zeroizing<[u8; 32]>>,
}

impl SharedPasswordStore {
    /// Create a new shared password store
    pub fn new<T: PasswordStore + 'static>(store: T) -> Self {
        let secret = store
            .scram_mock_authentication_secret()
            .unwrap_or_else(|| rand::thread_rng().gen());
        Self::with_mock_authentication_secret(store, secret)
    }

    /// Share a password backend with an explicit mock-authentication secret.
    ///
    /// Prefer [`Self::new`] when the backend implements
    /// [`PasswordStore::scram_mock_authentication_secret`]. This form is for an
    /// owner that persists the secret separately. Keep it private; never derive
    /// it from a public identifier.
    pub fn with_mock_authentication_secret<T: PasswordStore + 'static>(store: T, secret: [u8; 32]) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Box::new(store))),
            mock_authentication_secret: Arc::new(Zeroizing::new(secret)),
        }
    }

    /// Pick the SCRAM credential to challenge `username` with, WITHOUT an early
    /// "user not found" failure (HDB-001).
    ///
    /// An absent account is challenged with a DETERMINISTIC synthetic
    /// credential derived from this store's mock-authentication secret, so the
    /// exchange has the same shape as a real account's: the same message
    /// sequence, the same salt length and iteration count, and the same salt on
    /// every attempt (a fresh random salt per attempt would itself be an
    /// oracle). The returned boolean is a MANDATORY-REJECTION flag, not proof
    /// validity: when it is set the caller must fail the login even if the
    /// client's proof verifies against the synthetic credential.
    pub(crate) fn credentials_for_scram(&self, username: &str) -> (ScramCredentials, bool) {
        self.credentials_for_scram_with(username, None)
    }

    /// [`Self::credentials_for_scram`] for a name that may be a SQL role
    /// (GitHub #49): `role` is the credential of the role's catalog record
    /// ([`role_login_credentials`]), used when this store has no account of
    /// that name. The caller reads the catalog for every name the store does
    /// not hold, whether a role of that name exists or not, and the synthetic
    /// credential is still built before anything is chosen: a name with a role
    /// behind it does the work a name without one does.
    pub(crate) fn credentials_for_scram_with(
        &self,
        username: &str,
        role: Option<ScramCredentials>,
    ) -> (ScramCredentials, bool) {
        let (lookup, policy_iterations) = {
            let store = self.inner.read();
            (store.get_credentials(username), store.default_scram_iterations())
        };

        let derive = |domain: &[u8], output_len: usize| -> Vec<u8> {
            let mut output = Vec::with_capacity(output_len);
            let mut counter = 0u32;
            while output.len() < output_len {
                let mut input = Vec::with_capacity(domain.len() + username.len() + 6);
                input.extend_from_slice(domain);
                input.push(0);
                input.extend_from_slice(username.as_bytes());
                input.push(0);
                input.extend_from_slice(&counter.to_be_bytes());
                let block = Zeroizing::new(super::auth::scram_hmac_sha256(
                    self.mock_authentication_secret.as_slice(),
                    &input,
                ));
                let needed = output_len - output.len();
                output.extend(block.iter().take(needed).copied());
                // 32 bytes of output need one block, so the counter cannot
                // overflow for any length this function is called with.
                counter += 1;
            }
            output
        };

        // Built UNCONDITIONALLY, before the lookup result is consulted: the
        // absent-user path must not be distinguishable by the work it skips.
        // The 16-byte salt is the same length `ScramCredentials::from_password`
        // generates, so real and synthetic advertise the same shape.
        let client_key = Zeroizing::new(derive(b"client", 32));
        let synthetic = ScramCredentials {
            username: username.to_owned(),
            salt: derive(b"salt", 16),
            iterations: if policy_iterations == 0 {
                super::auth::DEFAULT_SCRAM_ITERATIONS
            } else {
                policy_iterations
            },
            stored_key: super::auth::scram_h(&client_key),
            server_key: derive(b"server", 32),
        };

        match lookup.or(role) {
            Some(real) => (real, false),
            None => (synthetic, true),
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

    /// A backend that persists its own mock-authentication secret, overriding
    /// ONLY the two new defaulted trait methods.
    struct PersistentTestStore {
        inner: InMemoryPasswordStore,
        secret: [u8; 32],
        policy_iterations: u32,
    }

    impl PersistentTestStore {
        fn new(secret: [u8; 32], policy_iterations: u32) -> Self {
            Self {
                inner: InMemoryPasswordStore::with_iterations(policy_iterations),
                secret,
                policy_iterations,
            }
        }
    }

    impl PasswordStore for PersistentTestStore {
        fn get_credentials(&self, username: &str) -> Option<ScramCredentials> {
            self.inner.get_credentials(username)
        }

        fn default_scram_iterations(&self) -> u32 {
            self.policy_iterations
        }

        fn scram_mock_authentication_secret(&self) -> Option<[u8; 32]> {
            Some(self.secret)
        }

        fn add_user(&mut self, username: &str, password: &str) -> Result<()> {
            self.inner.add_user(username, password)
        }

        fn remove_user(&mut self, username: &str) -> Result<bool> {
            self.inner.remove_user(username)
        }

        fn update_password(&mut self, username: &str, new_password: &str) -> Result<()> {
            self.inner.update_password(username, new_password)
        }

        fn user_exists(&self, username: &str) -> bool {
            self.inner.user_exists(username)
        }

        fn list_users(&self) -> Vec<String> {
            self.inner.list_users()
        }
    }

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
    fn synthetic_scram_credentials_are_stable_and_always_rejected() {
        let secret = [0x5a; 32];
        let first =
            SharedPasswordStore::with_mock_authentication_secret(InMemoryPasswordStore::with_iterations(8192), secret);
        // A second wrapper over the same persisted secret stands in for a restart.
        let second =
            SharedPasswordStore::with_mock_authentication_secret(InMemoryPasswordStore::with_iterations(8192), secret);

        let (missing_a, must_fail_a) = first.credentials_for_scram("missing");
        let (missing_b, must_fail_b) = first.credentials_for_scram("missing");
        let (missing_after_restart, must_fail_after_restart) = second.credentials_for_scram("missing");
        let (other, must_fail_other) = first.credentials_for_scram("other");

        assert!(must_fail_a && must_fail_b && must_fail_after_restart && must_fail_other);
        assert_eq!(missing_a.salt, missing_b.salt, "a per-attempt salt is itself an oracle");
        assert_eq!(
            missing_a.salt, missing_after_restart.salt,
            "the salt must survive a restart"
        );
        assert_ne!(
            missing_a.salt, other.salt,
            "one shared salt identifies every unknown account"
        );
        assert_eq!(
            missing_a.salt.len(),
            16,
            "synthetic salts must have the real salt's length"
        );
        assert_eq!(
            missing_a.iterations, 8192,
            "the store's iteration policy must be advertised"
        );
        assert_eq!(missing_a.stored_key.len(), 32);
        assert_eq!(missing_a.server_key.len(), 32);
        assert_eq!(missing_a.username, "missing");

        // stored_key == H(HMAC(secret, "client\0missing\0" || 0u32))
        let mut client_input = b"client\0missing\0".to_vec();
        client_input.extend_from_slice(&0_u32.to_be_bytes());
        let expected_client_key = super::super::auth::scram_hmac_sha256(&secret, &client_input);
        assert_eq!(missing_a.stored_key, super::super::auth::scram_h(&expected_client_key));
    }

    #[test]
    fn real_scram_credentials_keep_their_stored_values() {
        let store = SharedPasswordStore::with_mock_authentication_secret(InMemoryPasswordStore::new(), [0x33; 32]);
        store.add_user("alice", "correct-password").unwrap();
        let stored = store.get_credentials("alice").unwrap();

        let (selected, must_fail) = store.credentials_for_scram("alice");

        assert!(!must_fail);
        assert_eq!(selected.username, stored.username);
        assert_eq!(selected.salt, stored.salt);
        assert_eq!(selected.iterations, stored.iterations);
        assert_eq!(selected.stored_key, stored.stored_key);
        assert_eq!(selected.server_key, stored.server_key);
    }

    #[test]
    fn persistent_backend_secret_is_used_by_the_default_constructor() {
        let secret = [0x91; 32];
        let before_restart = SharedPasswordStore::new(PersistentTestStore::new(secret, 8192));
        let after_restart = SharedPasswordStore::new(PersistentTestStore::new(secret, 8192));

        let (before, before_must_fail) = before_restart.credentials_for_scram("missing");
        let (after, after_must_fail) = after_restart.credentials_for_scram("missing");

        assert!(before_must_fail && after_must_fail);
        assert_eq!(before.salt, after.salt);
        assert_eq!(before.stored_key, after.stored_key);
        assert_eq!(before.server_key, after.server_key);
        assert_eq!(before.iterations, 8192);

        // Without a persisted secret the wrapper picks a random one, so two
        // wrappers over the same ephemeral backend must NOT agree.
        let ephemeral_a = SharedPasswordStore::new(InMemoryPasswordStore::new());
        let ephemeral_b = SharedPasswordStore::new(InMemoryPasswordStore::new());
        assert_ne!(
            ephemeral_a.credentials_for_scram("missing").0.salt,
            ephemeral_b.credentials_for_scram("missing").0.salt
        );
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
