//! Audit logging integration tests

use heliosdb_lite::{EmbeddedDatabase, Config, audit::{AuditLogger, AuditConfig, AuditQuery, OperationType}};
use std::sync::Arc;

#[tokio::test]
async fn test_audit_logger_basic() {
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Get storage reference
    let storage = Arc::new(
        heliosdb_lite::storage::StorageEngine::open_in_memory(&config).unwrap()
    );

    let audit_config = AuditConfig::default();
    let logger = AuditLogger::new(storage.clone(), audit_config).unwrap();

    // Log DDL operation
    logger.log_ddl(
        "CREATE TABLE",
        "users",
        "CREATE TABLE users (id INT, name TEXT)",
        true,
        None,
    ).unwrap();

    // Log DML operation
    logger.log_dml(
        "INSERT",
        "users",
        "INSERT INTO users VALUES (1, 'Alice')",
        1,
        true,
        None,
    ).unwrap();

    // Force async flush for testing (allows tokio runtime to process background task)
    logger.flush_async().await.unwrap();

    // Query audit log
    let events = storage.scan_table("__audit_log").unwrap();
    assert!(!events.is_empty());
    assert!(events.len() >= 2);
}

#[tokio::test]
async fn test_audit_query_builder() {
    let config = Config::in_memory();
    let storage = Arc::new(
        heliosdb_lite::storage::StorageEngine::open_in_memory(&config).unwrap()
    );

    let audit_config = AuditConfig::default();
    let logger = AuditLogger::new(storage.clone(), audit_config).unwrap();

    // Log multiple operations
    logger.log_ddl("CREATE TABLE", "users", "CREATE TABLE users (id INT)", true, None).unwrap();
    logger.log_dml("INSERT", "users", "INSERT INTO users VALUES (1)", 1, true, None).unwrap();
    logger.log_dml("UPDATE", "users", "UPDATE users SET name='Bob'", 1, true, None).unwrap();

    // Force synchronous flush for testing
    logger.flush_async().await.unwrap();

    // Build query
    let query = AuditQuery::new()
        .with_operation(OperationType::Insert)
        .limit(10);

    let sql = query.build_sql();
    assert!(sql.contains("operation = 'INSERT'"));
    assert!(sql.contains("LIMIT 10"));
}

#[tokio::test]
async fn test_audit_config_filtering() {
    let config = Config::in_memory();
    let storage = Arc::new(
        heliosdb_lite::storage::StorageEngine::open_in_memory(&config).unwrap()
    );

    // Config that only logs DDL
    let audit_config = AuditConfig {
        log_ddl: true,
        log_dml: false,
        log_select: false,
        ..Default::default()
    };

    let logger = AuditLogger::new(storage.clone(), audit_config).unwrap();

    // Log DDL (should be logged)
    logger.log_ddl("CREATE TABLE", "users", "CREATE TABLE users (id INT)", true, None).unwrap();

    // Log DML (should NOT be logged)
    logger.log_dml("INSERT", "users", "INSERT INTO users VALUES (1)", 1, true, None).unwrap();

    // Force synchronous flush for testing
    logger.flush_async().await.unwrap();

    // Verify only DDL was logged
    let events = storage.scan_table("__audit_log").unwrap();
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn test_audit_checksum_verification() {
    use heliosdb_lite::audit::{AuditEvent, AuditMetadata};

    let metadata = AuditMetadata::default();
    let event = AuditEvent::new(
        1,
        "session-123".to_string(),
        "alice".to_string(),
        OperationType::Insert,
        Some("users".to_string()),
        "INSERT INTO users VALUES (1, 'Alice')".to_string(),
        1,
        true,
        None,
        metadata,
    );

    // Verify checksum
    assert!(event.verify_checksum());
    assert!(!event.checksum.is_empty());
}

#[tokio::test]
async fn test_audit_retention_config() {
    let minimal = AuditConfig::minimal();
    assert!(minimal.log_ddl);
    assert!(!minimal.log_dml);
    assert_eq!(minimal.retention_days, 30);

    let verbose = AuditConfig::verbose();
    assert!(verbose.log_ddl);
    assert!(verbose.log_dml);
    assert!(verbose.log_select);
    assert_eq!(verbose.retention_days, 365);

    let compliance = AuditConfig::compliance();
    assert!(compliance.log_ddl);
    assert!(compliance.log_dml);
    assert!(compliance.enable_checksums);
    assert_eq!(compliance.retention_days, 2555); // 7 years
}

#[tokio::test]
async fn test_query_truncation() {
    let config = AuditConfig {
        max_query_length: 20,
        ..Default::default()
    };

    let short_query = "SELECT * FROM users";
    let truncated = config.truncate_query(short_query);
    assert_eq!(truncated, short_query);

    let long_query = "SELECT * FROM users WHERE name = 'very long name that exceeds the limit'";
    let truncated = config.truncate_query(long_query);
    assert!(truncated.len() < long_query.len() + 50);
    assert!(truncated.contains("truncated"));
}

#[tokio::test]
async fn test_error_logging() {
    let config = Config::in_memory();
    let storage = Arc::new(
        heliosdb_lite::storage::StorageEngine::open_in_memory(&config).unwrap()
    );

    let audit_config = AuditConfig::default();
    let logger = AuditLogger::new(storage.clone(), audit_config).unwrap();

    // Log failed operation
    logger.log_dml(
        "INSERT",
        "nonexistent_table",
        "INSERT INTO nonexistent_table VALUES (1)",
        0,
        false,
        Some("Table does not exist"),
    ).unwrap();

    // Force synchronous flush for testing
    logger.flush_async().await.unwrap();

    // Verify error was logged
    let events = storage.scan_table("__audit_log").unwrap();
    assert!(!events.is_empty());
}
