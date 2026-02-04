//! Oracle TNS/TTC protocol implementation for HeliosDB-Lite
//!
//! This module provides Oracle database compatibility by implementing:
//! - TNS (Transparent Network Substrate) protocol for connection management
//! - TTC (Two-Task Common) protocol for SQL execution
//! - Oracle SQL dialect to PostgreSQL translation
//!
//! Reference: Oracle Database Net Services Reference

pub mod tns;
pub mod ttc;
pub mod translator;
pub mod handler;
pub mod server;

pub use server::{OracleServer, OracleServerConfig};
pub use translator::OracleTranslator;
pub use handler::OracleProtocolHandler;

/// Oracle protocol version
pub const ORACLE_PROTOCOL_VERSION: u16 = 319; // Oracle 19c

/// Default Oracle listener port
pub const DEFAULT_ORACLE_PORT: u16 = 1521;

/// Oracle error codes
pub mod error_codes {
    pub const ORA_00900_INVALID_SQL: &str = "ORA-00900";
    pub const ORA_00904_INVALID_IDENTIFIER: &str = "ORA-00904";
    pub const ORA_00942_TABLE_NOT_EXISTS: &str = "ORA-00942";
    pub const ORA_01017_INVALID_CREDENTIALS: &str = "ORA-01017";
    pub const ORA_12541_TNS_NO_LISTENER: &str = "ORA-12541";
    pub const ORA_12514_TNS_LISTENER_NOT_RESOLVE: &str = "ORA-12514";
}
