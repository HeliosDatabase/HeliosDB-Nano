//! MySQL wire protocol implementation
//!
//! This module implements the MySQL wire protocol (v10), allowing HeliosDB Nano
//! to accept connections from MySQL clients (mysql CLI, PyMySQL,
//! mysql-connector-python, etc.).
//!
//! ## Features
//!
//! - **HandshakeV10**: Full MySQL 8.0 compatible handshake
//! - **COM_QUERY**: Text protocol query execution via `EmbeddedDatabase`
//! - **COM_STMT_PREPARE / EXECUTE / CLOSE**: Prepared statement support
//! - **Transaction Control**: BEGIN, COMMIT, ROLLBACK
//! - **SHOW Commands**: DATABASES, TABLES, COLUMNS, VARIABLES, WARNINGS
//! - **SET Commands**: Silently acknowledged for client compatibility
//! - **Authentication**: Trust-based (mysql_native_password wire format)

pub mod handler;
pub mod compatibility;
pub mod extended;
pub mod features;

pub use handler::{handle_mysql_connection, MySqlHandler, MySqlError};
