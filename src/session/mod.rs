//! Session Management Module
//!
//! Provides multi-user session management, isolation levels, and resource quotas
//! for concurrent ACID transactions in HeliosDB-Lite.
//!
//! # Overview
//!
//! Sessions provide isolated execution contexts for database clients. Each session:
//!
//! - Has its own transaction isolation level
//! - Tracks usage statistics (queries, bytes read/written)
//! - Can be limited by resource quotas
//! - Times out after inactivity
//!
//! # Key Types
//!
//! - [`SessionId`] - Unique session identifier
//! - [`Session`] - Session state and statistics
//! - [`SessionManager`] - Manages active sessions
//! - [`IsolationLevel`] - Transaction isolation level
//! - [`User`] - User credentials with Argon2 password hashing
//!
//! # Example
//!
//! ```rust,no_run
//! use heliosdb_nano::EmbeddedDatabase;
//! use heliosdb_nano::session::IsolationLevel;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let db = EmbeddedDatabase::new_in_memory()?;
//!
//! // Create a session for user
//! let session = db.create_session("alice", IsolationLevel::RepeatableRead)?;
//!
//! // Execute queries in session context
//! db.execute_in_session(session, "CREATE TABLE users (id INT)")?;
//! db.execute_in_session(session, "INSERT INTO users VALUES (1)")?;
//!
//! // Clean up
//! db.destroy_session(session)?;
//! # Ok(())
//! # }
//! ```

mod manager;
mod types;

pub use manager::{SessionManager, ResourceQuota};
pub use types::{
    Session, SessionId, SessionStats, IsolationLevel, UserId, User,
};
