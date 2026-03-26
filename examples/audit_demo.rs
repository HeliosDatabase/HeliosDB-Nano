//! Audit logging demonstration
//!
//! This example shows how to use the audit logging system.
//!
//! Run with: cargo run --example audit_demo

use heliosdb_nano::{EmbeddedDatabase, Config};
use heliosdb_nano::audit::{AuditLogger, AuditConfig, AuditQuery, OperationType};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== HeliosDB Nano Audit Logging Demo ===\n");

    // 1. Create database
    println!("1. Creating database...");
    let config = Config::in_memory();
    let db = EmbeddedDatabase::new_in_memory()?;
    let storage = Arc::new(
        heliosdb_nano::storage::StorageEngine::open_in_memory(&config)?
    );
    println!("   Database created\n");

    // 2. Initialize audit logger with default configuration
    println!("2. Initializing audit logger...");
    let audit_config = AuditConfig::default();
    println!("   Config: log_ddl={}, log_dml={}, log_select={}",
        audit_config.log_ddl,
        audit_config.log_dml,
        audit_config.log_select
    );
    let mut logger = AuditLogger::new(storage.clone(), audit_config)?;
    logger.set_user("demo_user".to_string());
    println!("   Audit logger ready\n");

    // 3. Log DDL operation
    println!("3. Logging DDL operation...");
    logger.log_ddl(
        "CREATE TABLE",
        "users",
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, email TEXT)",
        true,
        None,
    )?;
    println!("   Logged: CREATE TABLE users\n");

    // 4. Log successful DML operations
    println!("4. Logging DML operations...");
    logger.log_dml(
        "INSERT",
        "users",
        "INSERT INTO users VALUES (1, 'Alice', 'alice@example.com')",
        1,
        true,
        None,
    )?;
    println!("   Logged: INSERT (1 row)");

    logger.log_dml(
        "INSERT",
        "users",
        "INSERT INTO users VALUES (2, 'Bob', 'bob@example.com')",
        1,
        true,
        None,
    )?;
    println!("   Logged: INSERT (1 row)");

    logger.log_dml(
        "UPDATE",
        "users",
        "UPDATE users SET email='alice.smith@example.com' WHERE id=1",
        1,
        true,
        None,
    )?;
    println!("   Logged: UPDATE (1 row)\n");

    // 5. Log failed operation
    println!("5. Logging failed operation...");
    logger.log_dml(
        "DELETE",
        "users",
        "DELETE FROM users WHERE id=999",
        0,
        false,
        Some("Record not found"),
    )?;
    println!("   Logged: DELETE (failed)\n");

    // 6. Flush audit events
    println!("6. Flushing audit events...");
    logger.flush()?;
    println!("   Events flushed\n");

    // 7. Query audit log
    println!("7. Querying audit log...");
    let events = storage.scan_table("__audit_log")?;
    println!("   Found {} audit events:\n", events.len());

    // Parse and display events
    let parsed_events = AuditQuery::parse_events(events)?;
    for (i, event) in parsed_events.iter().enumerate() {
        println!("   Event {}:", i + 1);
        println!("     ID: {}", event.id);
        println!("     Time: {}", event.timestamp);
        println!("     User: {}", event.user);
        println!("     Operation: {}", event.operation);
        println!("     Target: {}", event.target.as_ref().unwrap_or(&"N/A".to_string()));
        println!("     Query: {}", event.query);
        println!("     Affected Rows: {}", event.affected_rows);
        println!("     Success: {}", event.success);
        if let Some(error) = &event.error {
            println!("     Error: {}", error);
        }
        println!("     Checksum: {}...", &event.checksum[..16]);
        println!("     Checksum Valid: {}", event.verify_checksum());
        println!();
    }

    // 8. Query with filter
    println!("8. Querying INSERT operations only...");
    let query = AuditQuery::new()
        .with_operation(OperationType::Insert)
        .limit(10);
    let sql = query.build_sql();
    println!("   SQL: {}", sql);
    let tuples = logger.query_audit_log(&format!(
        "operation = 'INSERT'"
    ))?;
    let insert_events = AuditQuery::parse_events(tuples)?;
    println!("   Found {} INSERT events\n", insert_events.len());

    // 9. Demonstrate different configurations
    println!("9. Configuration examples:");

    let minimal = AuditConfig::minimal();
    println!("   Minimal: log_ddl={}, log_dml={}, retention={}d",
        minimal.log_ddl, minimal.log_dml, minimal.retention_days);

    let verbose = AuditConfig::verbose();
    println!("   Verbose: log_ddl={}, log_dml={}, log_select={}, retention={}d",
        verbose.log_ddl, verbose.log_dml, verbose.log_select, verbose.retention_days);

    let compliance = AuditConfig::compliance();
    println!("   Compliance: log_ddl={}, log_dml={}, retention={}d, checksums={}",
        compliance.log_ddl, compliance.log_dml, compliance.retention_days, compliance.enable_checksums);

    println!("\n=== Demo Complete ===");

    Ok(())
}
