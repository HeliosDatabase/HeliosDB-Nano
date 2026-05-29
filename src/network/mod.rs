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

mod auth;
pub mod protocol;
mod server;
mod session;

// Re-exports
pub use protocol::{BackendMessage, FrontendMessage, TransactionStatus};
pub use server::PgServer;
