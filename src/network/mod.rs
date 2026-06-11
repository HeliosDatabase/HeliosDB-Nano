//! Legacy PostgreSQL wire protocol network server — **deprecated**.
//!
//! # Deprecation (R5.W6 / audit discovery C14)
//!
//! This is the original PG-wire stack, superseded by
//! [`crate::protocol::postgres`] (which the `heliosdb-nano start` server
//! and all current deployments use). The legacy listener here has two
//! known, unfixed hazards:
//!
//! - **Authentication is decorative**: the session handler accepts *any*
//!   password for the built-in `postgres` / `helios` users.
//! - **Extended-protocol DML drops bound parameters** — prepared INSERT /
//!   UPDATE / DELETE statements execute without their values.
//!
//! The server/session/auth layers are therefore gated behind the
//! non-default `legacy-network` cargo feature and kept only for API
//! compatibility with existing embedders. Do not use them in new code —
//! use [`crate::protocol::postgres::PgServer`] instead.
//!
//! The [`protocol`] submodule (pure message encode/decode helpers and
//! SQLSTATE constants) has no such hazards, is shared with the modern
//! stack, and remains available unconditionally.
//!
//! # Example (requires `--features legacy-network`)
//!
//! ```rust,ignore
//! use heliosdb_nano::{EmbeddedDatabase, network::PgServer};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create database
//! let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
//!
//! // Create and run server
//! let server = PgServer::new("127.0.0.1:5432", db);
//! server.run().await?;
//! # Ok(())
//! # }
//! ```

#[cfg(feature = "legacy-network")]
mod auth;
pub mod protocol;
#[cfg(feature = "legacy-network")]
mod server;
#[cfg(feature = "legacy-network")]
mod session;

// Re-exports. The protocol-level types are hazard-free and stay available;
// the listener stack is opt-in (see module docs).
pub use protocol::{BackendMessage, FrontendMessage, TransactionStatus};
#[cfg(feature = "legacy-network")]
pub use server::PgServer;
