//! Protocol implementations
//!
//! This module provides wire protocol implementations for client connectivity.

pub mod mysql;
pub mod postgres;
pub mod tls_provider;
pub mod tls_stream;

// Re-export commonly used items
pub use postgres::{AuthManager, AuthMethod, PgServer, PgServerBuilder, PgServerConfig};
