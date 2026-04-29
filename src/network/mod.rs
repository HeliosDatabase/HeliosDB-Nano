//! PostgreSQL wire protocol network server
//!
//! Provides a PostgreSQL-compatible network interface for HeliosDB Lite.
//!
//! # Example
//!
//! ```rust,no_run
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

pub mod protocol;
mod auth;
mod session;
mod server;

// Re-exports
pub use server::PgServer;
pub use protocol::{BackendMessage, FrontendMessage, TransactionStatus};
