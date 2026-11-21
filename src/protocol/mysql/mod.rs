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
//! - **SET Commands**: a per-session variable store (`vars`): every variable
//!   takes effect, is stored with a documented "no effect", or is refused
//! - **Multi-statement queries**: split and answered one by one (`batch`)
//! - **Authentication**: `trust`, or `caching_sha2_password` against the
//!   server's accounts and roles (`auth`)

pub(crate) mod accounts;
pub mod auth;
pub(crate) mod batch;
pub mod compatibility;
pub mod extended;
pub mod features;
pub mod handler;
pub(crate) mod kill;
pub mod server;
pub mod ssl;
pub mod translator;
pub(crate) mod vars;

#[cfg(unix)]
pub use handler::handle_mysql_connection_unix;
pub use auth::MysqlAuth;
#[cfg(unix)]
pub use handler::handle_mysql_connection_unix_with_auth;
pub use handler::{handle_mysql_connection, MySqlError, MySqlHandler};
pub use server::{MysqlServer, MysqlServerConfig};
pub use ssl::{MysqlSslConfig, MysqlSslNegotiator};

/// When this process first served a MySQL connection: what `SHOW STATUS LIKE
/// 'Uptime'` counts from.
fn listener_start() -> &'static std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

pub(crate) fn listener_started() {
    let _ = listener_start();
}

pub(crate) fn listener_uptime_secs() -> u64 {
    listener_start().elapsed().as_secs()
}
