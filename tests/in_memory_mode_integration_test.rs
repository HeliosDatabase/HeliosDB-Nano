//! Comprehensive in-memory mode integration tests
//!
//! Tests for in-memory deployment mode with:
//! - Pure RAM storage (no persistence)
//! - ACID compliance
//! - Transaction isolation
//! - Multi-threaded concurrent access

use heliosdb_lite::{EmbeddedDatabase, Result, Value};
use std::sync::Arc;
use std::thread;

// Run with: cargo test --test in_memory_mode_integration_test --lib

#[test]
fn test_in_memory_database_creation() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    // Should be able to execute basic query
    db.execute("CREATE TABLE test (id INT)")?;
    Ok(())
}

#[test]
fn test_in_memory_crud_operations() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    
    // Create
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    
    // Insert
    db.execute("INSERT INTO users VALUES (1, 'Alice')")?;
    db.execute("INSERT INTO users VALUES (2, 'Bob')")?;
    
    // Read
    let results = db.query("SELECT * FROM users ORDER BY id", &[])?;
    assert_eq!(results.len(), 2);
    
    // Update
    db.execute("UPDATE users SET name = 'Alicia' WHERE id = 1")?;
    let alice = db.query("SELECT name FROM users WHERE id = 1", &[])?;
    assert_eq!(alice[0].get(0).unwrap(), &Value::String("Alicia".to_string()));
    
    // Delete
    db.execute("DELETE FROM users WHERE id = 2")?;
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    
    Ok(())
}

#[test]
fn test_in_memory_transaction_commit() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE data (id INT)")?;
    
    // Use the session API for transactions
    let session_id = db.create_session("test_user", heliosdb_lite::session::IsolationLevel::ReadCommitted)?;
    
    db.begin_transaction_for_session(session_id)?;
    db.execute_in_session(session_id, "INSERT INTO data VALUES (1)")?;
    db.commit_transaction_for_session(session_id)?;
    
    // Verify changes are visible
    let results = db.query("SELECT * FROM data", &[])?;
    assert_eq!(results.len(), 1);
    
    Ok(())
}

#[test]
fn test_in_memory_transaction_rollback() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;
    db.execute("CREATE TABLE data (id INT)")?;
    db.execute("INSERT INTO data VALUES (1)")?;
    
    let session_id = db.create_session("test_user", heliosdb_lite::session::IsolationLevel::ReadCommitted)?;
    
    db.begin_transaction_for_session(session_id)?;
    db.execute_in_session(session_id, "INSERT INTO data VALUES (2)")?;
    db.rollback_transaction_for_session(session_id)?;
    
    // Verify only original data remains
    let results = db.query("SELECT * FROM data", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &Value::Int4(1));
    
    Ok(())
}

#[test]
fn test_in_memory_no_persistence() -> Result<()> {
    // Create first instance
    {
        let db = EmbeddedDatabase::new_in_memory()?;
        db.execute("CREATE TABLE persistence_check (id INT)")?;
        db.execute("INSERT INTO persistence_check VALUES (1)")?;
    } 
    // db is dropped here
    
    // Create new instance
    let db2 = EmbeddedDatabase::new_in_memory()?;
    
    // Should NOT have the table
    // Note: catalog check or query failure expects table not found
    let result = db2.query("SELECT * FROM persistence_check", &[]);
    assert!(result.is_err(), "Table from previous in-memory instance should not exist");
    
    Ok(())
}

#[test]
fn test_in_memory_concurrent_writes() -> Result<()> {
    let db = Arc::new(EmbeddedDatabase::new_in_memory()?);
    db.execute("CREATE TABLE concurrent (id INT PRIMARY KEY)")?;
    
    let mut handles = vec![];
    
    for i in 0..10 {
        let db_clone = db.clone();
        handles.push(thread::spawn(move || {
            // Each thread inserts a unique row
            let session = db_clone.create_session(&format!("user_{}", i), heliosdb_lite::session::IsolationLevel::ReadCommitted).unwrap();
            db_clone.execute_in_session(session, &format!("INSERT INTO concurrent VALUES ({})", i)).unwrap();
        }));
    }
    
    for handle in handles {
        handle.join().unwrap();
    }
    
    let results = db.query("SELECT * FROM concurrent", &[])?;
    assert_eq!(results.len(), 10);
    
    Ok(())
}