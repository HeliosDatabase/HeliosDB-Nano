//! Audit logging system for HeliosDB Lite
//!
//! Provides comprehensive audit trails for all DDL and DML operations.
//! Supports compliance requirements (SOC2, HIPAA, GDPR) with tamper-proof logging.
//!
//! # Features
//!
//! - DDL operation logging (CREATE, DROP, ALTER)
//! - DML operation logging (INSERT, UPDATE, DELETE, SELECT)
//! - Tamper-proof append-only log
//! - Cryptographic checksums
//! - Async logging for performance
//! - Configurable log retention
//! - Query audit log via SQL
//!
//! # Example
//!
//! ```rust,ignore
//! use heliosdb_nano::audit::{AuditLogger, AuditConfig};
//! use heliosdb_nano::storage::StorageEngine;
//! use std::sync::Arc;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize storage engine first
//! let storage = Arc::new(StorageEngine::open_in_memory(&Default::default())?);
//! let config = AuditConfig::default();
//! let logger = AuditLogger::new(storage, config)?;
//!
//! // Log a DDL operation
//! logger.log_ddl("CREATE TABLE", "users", "CREATE TABLE users (...)", true, None)?;
//!
//! // Log a DML operation
//! logger.log_dml("INSERT", "users", "INSERT INTO users ...", 1, true, None)?;
//! # Ok(())
//! # }
//! ```

mod logger;
mod events;
mod config;
mod query;

pub use logger::AuditLogger;
pub use events::{AuditEvent, OperationType, AuditMetadata};
pub use config::AuditConfig;
pub use query::{AuditQuery, AuditFilter};

use crate::Result;

/// Initialize the audit system tables
///
/// Creates the internal audit log table structure in the storage engine.
pub fn initialize_audit_tables(storage: &crate::storage::StorageEngine) -> Result<()> {
    // Create audit log table metadata
    let catalog = storage.catalog();

    // Check if __audit_log table already exists
    let table_exists = catalog.get_table_schema("__audit_log").is_ok();

    if !table_exists {
        // Create __audit_log table
        use crate::{Schema, Column, DataType};
        let schema = Schema::new(vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Int8,
                nullable: false,
                primary_key: true,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "timestamp".to_string(),
                data_type: DataType::Timestamp,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "session_id".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "user".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "operation".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "target".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "query".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "affected_rows".to_string(),
                data_type: DataType::Int8,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "success".to_string(),
                data_type: DataType::Boolean,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "error".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
            Column {
                name: "checksum".to_string(),
                data_type: DataType::Text,
                nullable: false,
                primary_key: false,
                source_table: None,
                source_table_name: None,
            default_expr: None,
            unique: false,
            storage_mode: crate::ColumnStorageMode::Default,
            },
        ]);

        catalog.create_table("__audit_log", schema)?;
    }

    // Always ensure compression is disabled for audit log table
    // Audit logs contain nullable columns (target, error) which are incompatible with FSST compression
    // For compliance/security, we prioritize reliability over storage efficiency
    use crate::storage::compression::CompressionConfig;
    let mut compression_config = CompressionConfig::default();
    compression_config.enabled = false;
    catalog.set_compression_config("__audit_log", &compression_config)?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Config, storage::StorageEngine};

    #[test]
    fn test_initialize_audit_tables() {
        let config = Config::in_memory();
        let storage = StorageEngine::open_in_memory(&config).unwrap();

        let result = initialize_audit_tables(&storage);
        assert!(result.is_ok());

        // Verify table was created
        let catalog = storage.catalog();
        let schema = catalog.get_table_schema("__audit_log");
        assert!(schema.is_ok());
    }
}
