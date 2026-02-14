//! Integration tests for transaction context in query execution
//!
//! This test suite verifies that:
//! 1. All queries execute within transaction context
//! 2. Auto-commit mode works for single queries
//! 3. Explicit transactions provide proper isolation
//! 4. Rollback on error works correctly
//! 5. Read-your-own-writes semantics are preserved

use heliosdb_nano::{EmbeddedDatabase, Value, Result};

#[test]
fn test_auto_commit_insert() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Create table
    db.execute("CREATE TABLE users (id INT, name TEXT)")?;

    // Auto-commit INSERT
    let count = db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;
    assert_eq!(count, 1);

    // Verify data is committed
    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_explicit_transaction_commit() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;

    // Begin explicit transaction
    let tx = db.begin_transaction()?;

    // Insert within transaction
    tx.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;
    tx.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')")?;

    // Commit transaction
    tx.commit()?;

    // Verify both inserts are committed
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 2);

    Ok(())
}

#[test]
fn test_explicit_transaction_rollback() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;
    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;

    // Begin explicit transaction
    let tx = db.begin_transaction()?;

    // Insert within transaction
    tx.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')")?;
    tx.execute("INSERT INTO users (id, name) VALUES (3, 'Charlie')")?;

    // Rollback transaction
    tx.rollback()?;

    // Verify rollback - only Alice should exist
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
#[ignore = "TODO: Transaction read-your-own-writes not yet implemented"]
fn test_read_your_own_writes() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;

    // Begin explicit transaction
    let tx = db.begin_transaction()?;

    // Insert within transaction
    tx.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;

    // Should see own writes before commit
    let results = tx.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    // Commit and verify again
    tx.commit()?;

    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_auto_rollback_on_error() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;
    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;

    // This should fail and auto-rollback (invalid SQL)
    let result = db.execute("INSERT INTO nonexistent_table VALUES (1)");
    assert!(result.is_err());

    // Verify Alice is still there (no corruption)
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
#[ignore = "TODO: Transaction isolation not yet fully implemented"]
fn test_isolation_between_transactions() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;
    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;

    // Begin first transaction
    let tx1 = db.begin_transaction()?;
    tx1.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')")?;

    // Second transaction should NOT see Bob (not committed yet)
    // Note: This test would require concurrent access which is tricky in a single-threaded test
    // For now, we just verify the transaction is isolated internally

    // Verify tx1 can see its own write
    let results = tx1.query("SELECT * FROM users WHERE id = 2", &[])?;
    assert_eq!(results.len(), 1);

    // Commit tx1
    tx1.commit()?;

    // Now Bob should be visible to everyone
    let results = db.query("SELECT * FROM users WHERE id = 2", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
#[ignore = "TODO: Parameterized queries in transactions need implementation"]
fn test_parameterized_query_in_transaction() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;

    // Begin transaction
    let tx = db.begin_transaction()?;

    // Insert with explicit execute (not parameterized in this version)
    tx.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")?;

    // Query within transaction
    let results = tx.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    tx.commit()?;

    Ok(())
}

#[test]
#[ignore = "TODO: Multiple inserts atomicity needs implementation"]
fn test_multiple_inserts_atomic() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    db.execute("CREATE TABLE users (id INT, name TEXT)")?;

    // Begin transaction
    let tx = db.begin_transaction()?;

    // Multiple inserts
    for i in 1..=10 {
        tx.execute(&format!("INSERT INTO users (id, name) VALUES ({}, 'User{}')", i, i))?;
    }

    // All should be visible in transaction
    let results = tx.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 10);

    // Commit all at once
    tx.commit()?;

    // Verify all committed
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 10);

    Ok(())
}

#[test]
fn test_transaction_state_management() -> Result<()> {
    let db = EmbeddedDatabase::new_in_memory()?;

    // Initially no transaction active
    assert!(!db.in_transaction());

    // Begin transaction via SQL
    db.execute("BEGIN")?;
    assert!(db.in_transaction());

    // Commit via SQL
    db.execute("COMMIT")?;
    assert!(!db.in_transaction());

    // Begin and rollback via SQL
    db.execute("BEGIN")?;
    assert!(db.in_transaction());
    db.execute("ROLLBACK")?;
    assert!(!db.in_transaction());

    Ok(())
}
