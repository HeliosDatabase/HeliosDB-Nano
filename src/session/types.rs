//! Session types and definitions
//!
//! This module contains the core types for multi-user session management:
//!
//! - [`SessionId`] - Unique identifier for database sessions
//! - [`Session`] - Active session state with isolation level and statistics
//! - [`User`] - User credentials with Argon2 password hashing
//! - [`IsolationLevel`] - Transaction isolation levels (ReadCommitted, RepeatableRead, Serializable)
//!
//! # Example
//!
//! ```rust,no_run
//! use heliosdb_nano::session::{User, Session, IsolationLevel};
//!
//! // Create a user with password
//! let user = User::new("alice", "secure_password");
//!
//! // Verify password
//! assert!(user.verify_password("secure_password"));
//!
//! // Create a session with snapshot isolation
//! let session = Session::new(user.id, IsolationLevel::RepeatableRead);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};

/// Unique identifier for a database session
///
/// Each session gets a unique ID that is used to track the session's
/// state, transactions, and resource usage. Session IDs are monotonically
/// increasing and never reused within a database instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

impl SessionId {
    /// Generate a new unique session ID
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// User identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(pub u64);

/// User credentials
#[derive(Debug, Clone)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub password_hash: Option<String>,
}

impl User {
    /// Create a new user with Argon2-hashed password
    pub fn new(name: impl Into<String>, password: impl Into<String>) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let password_str = password.into();

        // Hash password with Argon2id (recommended variant)
        let password_hash = if password_str.is_empty() {
            None
        } else {
            let salt = SaltString::generate(&mut OsRng);
            let argon2 = Argon2::default();
            argon2
                .hash_password(password_str.as_bytes(), &salt)
                .ok()
                .map(|hash| hash.to_string())
        };

        Self {
            id: UserId(COUNTER.fetch_add(1, Ordering::SeqCst)),
            name: name.into(),
            password_hash,
        }
    }

    /// Create a user without a password (for internal/system use)
    pub fn new_passwordless(name: impl Into<String>) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self {
            id: UserId(COUNTER.fetch_add(1, Ordering::SeqCst)),
            name: name.into(),
            password_hash: None,
        }
    }

    /// Verify a password against the stored hash
    /// Returns false if no password hash is set
    pub fn verify_password(&self, password: &str) -> bool {
        match &self.password_hash {
            Some(hash_str) => {
                if let Ok(parsed_hash) = PasswordHash::new(hash_str) {
                    Argon2::default()
                        .verify_password(password.as_bytes(), &parsed_hash)
                        .is_ok()
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Check if the user has a password set
    pub fn has_password(&self) -> bool {
        self.password_hash.is_some()
    }

    /// Update the user's password
    pub fn set_password(&mut self, password: &str) {
        if password.is_empty() {
            self.password_hash = None;
        } else {
            let salt = SaltString::generate(&mut OsRng);
            let argon2 = Argon2::default();
            self.password_hash = argon2
                .hash_password(password.as_bytes(), &salt)
                .ok()
                .map(|hash| hash.to_string());
        }
    }
}

/// Isolation level for transactions
///
/// Controls how concurrent transactions interact with each other.
/// Higher isolation levels provide stronger consistency guarantees
/// at the cost of reduced concurrency.
///
/// # Isolation Level Comparison
///
/// | Level | Dirty Reads | Non-Repeatable Reads | Phantom Reads |
/// |-------|-------------|---------------------|---------------|
/// | ReadCommitted | No | Yes | Yes |
/// | RepeatableRead | No | No | Possible |
/// | Serializable | No | No | No |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// Read Committed isolation
    ///
    /// Each statement sees a fresh snapshot of committed data.
    /// Two identical queries in the same transaction may return
    /// different results if another transaction commits between them.
    ReadCommitted,
    /// Repeatable Read isolation (also known as Snapshot Isolation)
    ///
    /// The transaction sees a consistent snapshot taken at the start.
    /// All statements in the transaction see the same data, regardless
    /// of concurrent commits.
    RepeatableRead,
    /// Serializable isolation
    ///
    /// Provides full serializability with conflict detection.
    /// Transactions appear to execute one at a time, even when running
    /// concurrently. May abort transactions to prevent anomalies.
    Serializable,
}

impl IsolationLevel {
    /// Alias for `RepeatableRead` - commonly called Snapshot Isolation
    #[allow(non_upper_case_globals)]
    pub const Snapshot: Self = Self::RepeatableRead;
}

impl Default for IsolationLevel {
    fn default() -> Self {
        Self::ReadCommitted
    }
}

/// Active database session state
///
/// Represents an active connection to the database with its associated
/// transaction state, isolation level, and usage statistics.
///
/// # Lifecycle
///
/// 1. Session is created via [`EmbeddedDatabase::create_session`]
/// 2. Commands are executed within the session context
/// 3. Session is destroyed via [`EmbeddedDatabase::destroy_session`]
///
/// [`EmbeddedDatabase::create_session`]: crate::EmbeddedDatabase::create_session
/// [`EmbeddedDatabase::destroy_session`]: crate::EmbeddedDatabase::destroy_session
#[derive(Debug, Clone)]
pub struct Session {
    /// Unique session identifier
    pub id: SessionId,
    /// User who owns this session
    pub user_id: UserId,
    /// Transaction isolation level for this session
    pub isolation_level: IsolationLevel,
    /// Active transaction ID (None if no transaction in progress)
    pub active_txn: Option<u64>,
    /// Session creation timestamp (Unix epoch seconds)
    pub created_at: u64,
    /// Last activity timestamp (Unix epoch seconds)
    pub last_activity: u64,
    /// Cumulative session statistics
    pub stats: SessionStats,
}

impl Session {
    /// Create a new session
    pub fn new(user_id: UserId, isolation_level: IsolationLevel) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            id: SessionId::new(),
            user_id,
            isolation_level,
            active_txn: None,
            created_at: now,
            last_activity: now,
            stats: SessionStats::default(),
        }
    }

    /// Update last activity timestamp
    pub fn touch(&mut self) {
        self.last_activity = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }
}

/// Cumulative session statistics for monitoring and quota enforcement
///
/// Tracks all activity within a session including transaction counts,
/// query counts, and I/O statistics. Used for resource monitoring,
/// quota enforcement, and debugging.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    /// Total transactions started in this session
    pub transactions_started: u64,
    /// Total transactions successfully committed
    pub transactions_committed: u64,
    /// Total transactions rolled back (explicit or due to error)
    pub transactions_aborted: u64,
    /// Total SQL statements executed
    pub queries_executed: u64,
    /// Total bytes read from storage
    pub bytes_read: u64,
    /// Total bytes written to storage
    pub bytes_written: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_user_password_hashing() {
        let user = User::new("alice", "secret123");
        assert!(user.has_password());
        assert!(user.verify_password("secret123"));
        assert!(!user.verify_password("wrongpassword"));
        assert!(!user.verify_password(""));
    }

    #[test]
    fn test_user_empty_password() {
        let user = User::new("bob", "");
        assert!(!user.has_password());
        assert!(!user.verify_password(""));
        assert!(!user.verify_password("anypassword"));
    }

    #[test]
    fn test_user_passwordless() {
        let user = User::new_passwordless("system");
        assert!(!user.has_password());
        assert!(!user.verify_password("anypassword"));
    }

    #[test]
    fn test_user_set_password() {
        let mut user = User::new_passwordless("charlie");
        assert!(!user.has_password());

        user.set_password("newpassword");
        assert!(user.has_password());
        assert!(user.verify_password("newpassword"));

        user.set_password("");
        assert!(!user.has_password());
    }

    #[test]
    fn test_user_unique_ids() {
        let user1 = User::new("user1", "pass1");
        let user2 = User::new("user2", "pass2");
        assert_ne!(user1.id, user2.id);
    }

    #[test]
    fn test_session_creation() {
        let user = User::new("testuser", "testpass");
        let session = Session::new(user.id, IsolationLevel::ReadCommitted);
        assert_eq!(session.user_id, user.id);
        assert_eq!(session.isolation_level, IsolationLevel::ReadCommitted);
        assert!(session.active_txn.is_none());
    }
}
