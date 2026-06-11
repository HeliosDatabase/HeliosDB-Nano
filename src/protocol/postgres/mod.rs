//! PostgreSQL wire protocol implementation
//!
//! This module implements the PostgreSQL wire protocol, allowing HeliosDB-Lite
//! to accept connections from PostgreSQL clients (psql, pgAdmin, etc.).
//!
//! ## Features
//!
//! - **Simple Query Protocol**: Basic query execution (SELECT, INSERT, UPDATE, DELETE)
//! - **Extended Query Protocol**: Prepared statements (Parse, Bind, Execute)
//! - **Authentication**: Clear-text password, MD5, and SCRAM-SHA-256 support
//! - **System Catalogs**: Minimal pg_catalog emulation for client compatibility
//! - **Transaction Support**: BEGIN, COMMIT, ROLLBACK
//!
//! ## Usage
//!
//! ```rust,no_run
//! use heliosdb_nano::{EmbeddedDatabase, protocol::postgres::{PgServerBuilder, AuthMethod}};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create database
//! let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
//!
//! // Build PostgreSQL server
//! let server = PgServerBuilder::new()
//!     .address("127.0.0.1:5432".parse()?)
//!     .auth_method(AuthMethod::Trust)
//!     .build(db)?;
//!
//! // Start server (runs until error or shutdown)
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Protocol Reference
//!
//! This implementation follows the PostgreSQL Frontend/Backend Protocol:
//! <https://www.postgresql.org/docs/current/protocol.html>
//!
//! ## Supported Message Types
//!
//! ### Frontend (Client → Server)
//! - `Query` (Q): Simple query protocol
//! - `Parse` (P): Prepare a statement
//! - `Bind` (B): Bind parameters to a statement
//! - `Execute` (E): Execute a portal
//! - `Describe` (D): Describe a statement or portal
//! - `Sync` (S): Complete extended protocol sequence
//! - `Terminate` (X): Close connection
//!
//! ### Backend (Server → Client)
//! - `Authentication` (R): Authentication request/response
//! - `RowDescription` (T): Result set metadata
//! - `DataRow` (D): Result row data
//! - `CommandComplete` (C): Command execution complete
//! - `ReadyForQuery` (Z): Ready for next query
//! - `ErrorResponse` (E): Error message
//! - `ParameterStatus` (S): Server parameter value

pub mod auth;
pub mod catalog;
pub mod certs;
pub mod handler;
mod handler_extended;
#[cfg(test)]
mod wire_tests;
pub mod messages;
pub mod password_store;
pub mod prepared;
pub mod server;
pub mod ssl;

// Re-exports
pub use auth::{AuthManager, AuthMethod, ScramAuthState};
pub use certs::CertificateManager;
pub use messages::{BackendMessage, FrontendMessage, TransactionStatus};
pub use password_store::{InMemoryPasswordStore, PasswordStore, ScramCredentials, SharedPasswordStore};
pub use prepared::{Portal, PortalState, PreparedStatement, PreparedStatementManager};
pub use server::{PgServer, PgServerBuilder, PgServerConfig};
pub use ssl::{SecureConnection, SslConfig, SslMode, SslNegotiator};
