//! Integration tests for audit logging system
//!
//! Tests cover:
//! - Audit event creation and checksum
//! - Operation type classification
//! - Audit table initialization
//! - Event metadata handling
//! - Tamper detection via checksums

use heliosdb_nano::audit::{AuditEvent, OperationType, AuditMetadata};

#[test]
fn test_audit_event_creation() {
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

    assert_eq!(event.id, 1);
    assert_eq!(event.session_id, "session-123");
    assert_eq!(event.user, "alice");
    assert_eq!(event.operation, OperationType::Insert);
    assert_eq!(event.target, Some("users".to_string()));
    assert_eq!(event.affected_rows, 1);
    assert_eq!(event.success, true);
    assert!(event.error.is_none());
    assert!(!event.checksum.is_empty());
}

#[test]
fn test_audit_event_checksum_verification() {
    let metadata = AuditMetadata::default();
    let event = AuditEvent::new(
        42,
        "sess-001".to_string(),
        "bob".to_string(),
        OperationType::Update,
        Some("products".to_string()),
        "UPDATE products SET price = 100 WHERE id = 1".to_string(),
        1,
        true,
        None,
        metadata,
    );

    // Checksum should be valid
    assert!(event.verify_checksum(), "Checksum verification should pass");
}

#[test]
fn test_audit_event_checksum_tamper_detection() {
    let metadata = AuditMetadata::default();
    let mut event = AuditEvent::new(
        1,
        "session-456".to_string(),
        "eve".to_string(),
        OperationType::Delete,
        Some("users".to_string()),
        "DELETE FROM users WHERE id = 99".to_string(),
        1,
        true,
        None,
        metadata,
    );

    // Original checksum should be valid
    assert!(event.verify_checksum());

    // Tamper with the event
    event.affected_rows = 999;

    // Checksum should now be invalid
    assert!(
        !event.verify_checksum(),
        "Checksum should detect tampering"
    );
}

#[test]
fn test_audit_event_with_error() {
    let metadata = AuditMetadata::default();
    let event = AuditEvent::new(
        10,
        "session-error".to_string(),
        "user123".to_string(),
        OperationType::CreateTable,
        Some("duplicate_table".to_string()),
        "CREATE TABLE duplicate_table (id INT)".to_string(),
        0,
        false,
        Some("Table already exists".to_string()),
        metadata,
    );

    assert_eq!(event.success, false);
    assert_eq!(event.error, Some("Table already exists".to_string()));
    assert_eq!(event.affected_rows, 0);
    assert!(event.verify_checksum());
}

#[test]
fn test_operation_type_from_sql_ddl() {
    assert_eq!(
        OperationType::from_sql_statement("CREATE TABLE users (id INT)"),
        OperationType::CreateTable
    );
    assert_eq!(
        OperationType::from_sql_statement("DROP TABLE old_table"),
        OperationType::DropTable
    );
    assert_eq!(
        OperationType::from_sql_statement("ALTER TABLE users ADD COLUMN email TEXT"),
        OperationType::AlterTable
    );
    assert_eq!(
        OperationType::from_sql_statement("CREATE INDEX idx_name ON users(name)"),
        OperationType::CreateIndex
    );
    assert_eq!(
        OperationType::from_sql_statement("DROP INDEX idx_name"),
        OperationType::DropIndex
    );
}

#[test]
fn test_operation_type_from_sql_dml() {
    assert_eq!(
        OperationType::from_sql_statement("INSERT INTO users VALUES (1, 'Alice')"),
        OperationType::Insert
    );
    assert_eq!(
        OperationType::from_sql_statement("UPDATE users SET name = 'Bob' WHERE id = 1"),
        OperationType::Update
    );
    assert_eq!(
        OperationType::from_sql_statement("DELETE FROM users WHERE id = 1"),
        OperationType::Delete
    );
    assert_eq!(
        OperationType::from_sql_statement("SELECT * FROM users"),
        OperationType::Select
    );
}

#[test]
fn test_operation_type_from_sql_transaction() {
    assert_eq!(
        OperationType::from_sql_statement("BEGIN TRANSACTION"),
        OperationType::Begin
    );
    assert_eq!(
        OperationType::from_sql_statement("COMMIT"),
        OperationType::Commit
    );
    assert_eq!(
        OperationType::from_sql_statement("ROLLBACK"),
        OperationType::Rollback
    );
}

#[test]
fn test_operation_type_from_sql_case_insensitive() {
    assert_eq!(
        OperationType::from_sql_statement("insert into users values (1)"),
        OperationType::Insert
    );
    assert_eq!(
        OperationType::from_sql_statement("SeLeCt * FrOm users"),
        OperationType::Select
    );
    assert_eq!(
        OperationType::from_sql_statement("CREATE table USERS (ID int)"),
        OperationType::CreateTable
    );
}

#[test]
fn test_operation_type_classification_ddl() {
    assert!(OperationType::CreateTable.is_ddl());
    assert!(OperationType::DropTable.is_ddl());
    assert!(OperationType::AlterTable.is_ddl());
    assert!(OperationType::CreateIndex.is_ddl());
    assert!(OperationType::DropIndex.is_ddl());

    assert!(!OperationType::Insert.is_ddl());
    assert!(!OperationType::Select.is_ddl());
}

#[test]
fn test_operation_type_classification_dml() {
    assert!(OperationType::Insert.is_dml());
    assert!(OperationType::Update.is_dml());
    assert!(OperationType::Delete.is_dml());
    assert!(OperationType::Select.is_dml());

    assert!(!OperationType::CreateTable.is_dml());
    assert!(!OperationType::Begin.is_dml());
}

#[test]
fn test_operation_type_classification_transaction() {
    assert!(OperationType::Begin.is_transaction());
    assert!(OperationType::Commit.is_transaction());
    assert!(OperationType::Rollback.is_transaction());

    assert!(!OperationType::Insert.is_transaction());
    assert!(!OperationType::CreateTable.is_transaction());
}

#[test]
fn test_operation_type_classification_auth() {
    assert!(OperationType::Login.is_auth());
    assert!(OperationType::Logout.is_auth());
    assert!(OperationType::GrantPermission.is_auth());
    assert!(OperationType::RevokePermission.is_auth());

    assert!(!OperationType::Select.is_auth());
    assert!(!OperationType::CreateTable.is_auth());
}

#[test]
fn test_operation_type_display() {
    assert_eq!(OperationType::CreateTable.to_string(), "CREATE_TABLE");
    assert_eq!(OperationType::Insert.to_string(), "INSERT");
    assert_eq!(OperationType::Select.to_string(), "SELECT");
    assert_eq!(OperationType::Begin.to_string(), "BEGIN");
    assert_eq!(OperationType::Login.to_string(), "LOGIN");
}

#[test]
fn test_audit_metadata_basic() {
    let metadata = AuditMetadata {
        client_ip: Some("192.168.1.100".to_string()),
        application_name: Some("myapp".to_string()),
        database_name: Some("testdb".to_string()),
        execution_time_ms: Some(42),
        custom_fields: std::collections::HashMap::new(),
    };

    assert_eq!(metadata.client_ip, Some("192.168.1.100".to_string()));
    assert_eq!(metadata.application_name, Some("myapp".to_string()));
    assert_eq!(metadata.database_name, Some("testdb".to_string()));
    assert_eq!(metadata.execution_time_ms, Some(42));
}

#[test]
fn test_audit_metadata_custom_fields() {
    let mut custom_fields = std::collections::HashMap::new();
    custom_fields.insert("request_id".to_string(), "req-12345".to_string());
    custom_fields.insert("api_version".to_string(), "v2".to_string());

    let metadata = AuditMetadata {
        client_ip: None,
        application_name: None,
        database_name: None,
        execution_time_ms: None,
        custom_fields,
    };

    assert_eq!(
        metadata.custom_fields.get("request_id"),
        Some(&"req-12345".to_string())
    );
    assert_eq!(
        metadata.custom_fields.get("api_version"),
        Some(&"v2".to_string())
    );
}

#[test]
fn test_audit_event_checksum_consistency() {
    let metadata = AuditMetadata::default();

    // Create same event twice
    let event1 = AuditEvent::new(
        100,
        "session-100".to_string(),
        "testuser".to_string(),
        OperationType::Select,
        Some("products".to_string()),
        "SELECT * FROM products".to_string(),
        10,
        true,
        None,
        metadata.clone(),
    );

    // Sleep to ensure different timestamps
    std::thread::sleep(std::time::Duration::from_millis(10));

    let event2 = AuditEvent::new(
        100,
        "session-100".to_string(),
        "testuser".to_string(),
        OperationType::Select,
        Some("products".to_string()),
        "SELECT * FROM products".to_string(),
        10,
        true,
        None,
        metadata,
    );

    // Checksums should be different due to different timestamps
    assert_ne!(
        event1.checksum, event2.checksum,
        "Checksums should differ with different timestamps"
    );
}

#[test]
fn test_audit_event_multiple_operations() {
    // Test a sequence of different operations
    let operations = vec![
        (OperationType::CreateTable, "CREATE TABLE test (id INT)"),
        (OperationType::Insert, "INSERT INTO test VALUES (1)"),
        (OperationType::Select, "SELECT * FROM test"),
        (OperationType::Update, "UPDATE test SET id = 2"),
        (OperationType::Delete, "DELETE FROM test WHERE id = 2"),
        (OperationType::DropTable, "DROP TABLE test"),
    ];

    for (i, (op_type, query)) in operations.iter().enumerate() {
        let metadata = AuditMetadata::default();
        let event = AuditEvent::new(
            i as u64,
            format!("session-{}", i),
            "testuser".to_string(),
            op_type.clone(),
            Some("test".to_string()),
            query.to_string(),
            1,
            true,
            None,
            metadata,
        );

        assert_eq!(event.operation, op_type.clone());
        assert!(event.verify_checksum());
    }
}

#[test]
fn test_audit_event_serialization() {
    let metadata = AuditMetadata::default();
    let event = AuditEvent::new(
        999,
        "session-ser".to_string(),
        "sertest".to_string(),
        OperationType::Insert,
        Some("audit_test".to_string()),
        "INSERT INTO audit_test VALUES (1)".to_string(),
        1,
        true,
        None,
        metadata,
    );

    // Serialize to JSON
    let json = serde_json::to_string(&event).expect("Serialization failed");
    assert!(json.contains("\"id\":999"));
    assert!(json.contains("session-ser"));

    // Deserialize back
    let deserialized: AuditEvent = serde_json::from_str(&json)
        .expect("Deserialization failed");

    assert_eq!(deserialized.id, event.id);
    assert_eq!(deserialized.session_id, event.session_id);
    assert_eq!(deserialized.operation, event.operation);
    assert!(deserialized.verify_checksum());
}

#[test]
fn test_audit_event_with_long_query() {
    let metadata = AuditMetadata::default();
    let long_query = "SELECT * FROM users WHERE ".to_string() + &"id = 1 OR ".repeat(1000) + "id = 2";

    let event = AuditEvent::new(
        1,
        "session-long".to_string(),
        "user".to_string(),
        OperationType::Select,
        Some("users".to_string()),
        long_query.clone(),
        0,
        true,
        None,
        metadata,
    );

    assert_eq!(event.query, long_query);
    assert!(event.verify_checksum());
}

#[test]
fn test_audit_event_special_characters() {
    let metadata = AuditMetadata::default();
    let query_with_special = "INSERT INTO users VALUES (1, 'Test \\' \" \n\t User')";

    let event = AuditEvent::new(
        1,
        "session-special".to_string(),
        "test\nuser".to_string(), // User with newline
        OperationType::Insert,
        Some("users".to_string()),
        query_with_special.to_string(),
        1,
        true,
        None,
        metadata,
    );

    assert!(event.verify_checksum());
}

#[test]
fn test_audit_initialization() {
    use heliosdb_nano::{Config, storage::StorageEngine};

    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    // Initialize audit tables
    let result = heliosdb_nano::audit::initialize_audit_tables(&storage);
    assert!(result.is_ok(), "Audit table initialization should succeed");

    // Verify audit log table exists
    let catalog = storage.catalog();
    let schema_result = catalog.get_table_schema("__audit_log");
    assert!(schema_result.is_ok(), "Audit log table should exist");

    let schema = schema_result.unwrap();
    assert!(schema.columns.iter().any(|c| c.name == "id"));
    assert!(schema.columns.iter().any(|c| c.name == "timestamp"));
    assert!(schema.columns.iter().any(|c| c.name == "operation"));
    assert!(schema.columns.iter().any(|c| c.name == "query"));
    assert!(schema.columns.iter().any(|c| c.name == "checksum"));
}

#[test]
fn test_audit_initialization_idempotent() {
    use heliosdb_nano::{Config, storage::StorageEngine};

    let config = Config::in_memory();
    let storage = StorageEngine::open_in_memory(&config)
        .expect("Failed to create storage engine");

    // Initialize twice
    let result1 = heliosdb_nano::audit::initialize_audit_tables(&storage);
    let result2 = heliosdb_nano::audit::initialize_audit_tables(&storage);

    assert!(result1.is_ok());
    assert!(result2.is_ok(), "Second initialization should be idempotent");
}

#[test]
fn test_operation_type_coverage() {
    // Ensure all operation types can be created and displayed
    let all_ops = vec![
        OperationType::CreateTable,
        OperationType::DropTable,
        OperationType::AlterTable,
        OperationType::CreateIndex,
        OperationType::DropIndex,
        OperationType::Insert,
        OperationType::Update,
        OperationType::Delete,
        OperationType::Select,
        OperationType::Begin,
        OperationType::Commit,
        OperationType::Rollback,
        OperationType::Login,
        OperationType::Logout,
        OperationType::GrantPermission,
        OperationType::RevokePermission,
        OperationType::Backup,
        OperationType::Restore,
        OperationType::Vacuum,
    ];

    for op in all_ops {
        let display = op.to_string();
        assert!(!display.is_empty());

        let metadata = AuditMetadata::default();
        let event = AuditEvent::new(
            1,
            "session".to_string(),
            "user".to_string(),
            op,
            None,
            "TEST".to_string(),
            0,
            true,
            None,
            metadata,
        );

        assert!(event.verify_checksum());
    }
}
