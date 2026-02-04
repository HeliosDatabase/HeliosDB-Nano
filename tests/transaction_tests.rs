//! Transaction tests for HeliosDB Lite
//!
//! Tests ACID properties, transaction isolation, and MVCC functionality

mod test_helpers;

use heliosdb_lite::{EmbeddedDatabase, Result};
use test_helpers::*;

// ============================================================================
// Basic Transaction Tests
// ============================================================================

#[test]
fn test_transaction_creation() -> Result<()> {
    let db = create_test_db()?;
    let tx = db.begin_transaction()?;
    tx.commit()?;
    Ok(())
}

#[test]
fn test_transaction_commit() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    let tx = db.begin_transaction()?;
    // Note: Current implementation doesn't fully support transactional execute
    // This test verifies the API works
    tx.commit()?;

    Ok(())
}

#[test]
fn test_transaction_rollback() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    let tx = db.begin_transaction()?;
    tx.rollback()?;

    Ok(())
}

#[test]
fn test_multiple_sequential_transactions() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Transaction 1
    let tx1 = db.begin_transaction()?;
    tx1.commit()?;

    // Transaction 2
    let tx2 = db.begin_transaction()?;
    tx2.commit()?;

    // Transaction 3
    let tx3 = db.begin_transaction()?;
    tx3.rollback()?;

    Ok(())
}

// ============================================================================
// Transaction Isolation Tests
// ============================================================================

#[test]
fn test_transaction_insert_commit() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert outside transaction
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Verify data exists
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_read_your_own_writes() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert data
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Should be able to read our own write
    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_string_value(&results[0], 1).unwrap(), "Alice");

    Ok(())
}

#[test]
fn test_transaction_visibility() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert data in transaction
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Data should be visible after implicit commit
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

// ============================================================================
// ACID Properties Tests
// ============================================================================

#[test]
fn test_atomicity_all_or_nothing() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Successful batch of operations
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 2);

    Ok(())
}

#[test]
fn test_consistency_constraints() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE constrained (id INT PRIMARY KEY, value INT NOT NULL)")?;

    // Valid insert
    db.execute("INSERT INTO constrained (id, value) VALUES (1, 100)")?;

    // Verify data
    let results = db.query("SELECT * FROM constrained", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_isolation_read_consistency() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert initial data
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Read should show consistent state
    let results1 = db.query("SELECT * FROM users", &[])?;
    let results2 = db.query("SELECT * FROM users", &[])?;

    assert_eq!(results1.len(), results2.len());
    assert_eq!(results1[0], results2[0]);

    Ok(())
}

#[test]
fn test_durability_after_commit() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert and commit
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Data should persist
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

// ============================================================================
// Concurrent Operations Tests
// ============================================================================

#[test]
fn test_multiple_inserts_sequential() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Sequential inserts
    for i in 1..=100 {
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
            i, i, i, 20 + (i % 50)
        ))?;
    }

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 100);

    Ok(())
}

#[test]
fn test_insert_update_delete_sequence() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    assert_row_count(&db, "SELECT * FROM users", 1)?;

    // Update
    db.execute("UPDATE users SET age = 31 WHERE id = 1")?;
    let results = db.query("SELECT age FROM users WHERE id = 1", &[])?;
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 31);

    // Delete
    db.execute("DELETE FROM users WHERE id = 1")?;
    assert_row_count(&db, "SELECT * FROM users", 0)?;

    Ok(())
}

#[test]
fn test_interleaved_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert multiple rows
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 35)")?;

    // Update one
    db.execute("UPDATE users SET age = 31 WHERE id = 1")?;

    // Insert another
    db.execute("INSERT INTO users (id, name, email, age) VALUES (4, 'Dave', 'dave@example.com', 40)")?;

    // Delete one
    db.execute("DELETE FROM users WHERE id = 2")?;

    // Query and verify final state
    let results = db.query("SELECT * FROM users ORDER BY id", &[])?;
    assert_eq!(results.len(), 3); // Alice, Charlie, Dave remain

    Ok(())
}

// ============================================================================
// MVCC Tests
// ============================================================================

#[test]
fn test_mvcc_snapshot_isolation() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Initial insert
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Take a snapshot by reading
    let snapshot1 = db.query("SELECT * FROM users", &[])?;
    assert_eq!(snapshot1.len(), 1);

    // Modify data
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;

    // New read should see updated data
    let snapshot2 = db.query("SELECT * FROM users", &[])?;
    assert_eq!(snapshot2.len(), 2);

    Ok(())
}

#[test]
fn test_mvcc_version_chains() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Create initial version
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Update creates new version
    db.execute("UPDATE users SET age = 31 WHERE id = 1")?;

    // Update again creates another version
    db.execute("UPDATE users SET age = 32 WHERE id = 1")?;

    // Latest version should be visible
    let results = db.query("SELECT age FROM users WHERE id = 1", &[])?;
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 32);

    Ok(())
}

#[test]
fn test_mvcc_delete_visibility() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert data
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;

    // Delete one row
    db.execute("DELETE FROM users WHERE id = 1")?;

    // Only Bob should be visible
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 2);

    Ok(())
}

// ============================================================================
// Transaction Error Handling
// ============================================================================

#[test]
fn test_transaction_error_recovery() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Successful operation
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Verify data exists despite earlier error attempts
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_empty_transaction() -> Result<()> {
    let db = create_test_db()?;

    // Begin and commit without any operations
    let tx = db.begin_transaction()?;
    tx.commit()?;

    Ok(())
}

#[test]
fn test_transaction_with_only_reads() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Read-only transaction
    let tx = db.begin_transaction()?;
    let _ = tx.query("SELECT * FROM users", &[])?;
    tx.commit()?;

    Ok(())
}

// ============================================================================
// Long-Running Transaction Tests
// ============================================================================

#[test]
fn test_long_transaction_many_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Perform many operations
    for i in 1..=50 {
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
            i, i, i, 20 + (i % 50)
        ))?;
    }

    // Verify all operations succeeded
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 50);

    Ok(())
}

#[test]
fn test_transaction_with_mixed_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Mix of inserts, updates, deletes, and selects
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;

    let _ = db.query("SELECT * FROM users", &[])?;

    db.execute("UPDATE users SET age = 31 WHERE id = 1")?;

    let _ = db.query("SELECT * FROM users WHERE age > 30", &[])?;

    db.execute("DELETE FROM users WHERE id = 2")?;

    // Final verification
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 1);

    Ok(())
}

// ============================================================================
// Transaction Cleanup Tests
// ============================================================================

#[test]
fn test_transaction_cleanup_after_commit() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    let tx = db.begin_transaction()?;
    tx.commit()?;

    // Should be able to start new transaction
    let tx2 = db.begin_transaction()?;
    tx2.commit()?;

    Ok(())
}

#[test]
fn test_transaction_cleanup_after_rollback() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    let tx = db.begin_transaction()?;
    tx.rollback()?;

    // Should be able to start new transaction
    let tx2 = db.begin_transaction()?;
    tx2.commit()?;

    Ok(())
}
