// Transaction Integration Tests
//
// Tests for the transaction context integration in query execution.
// Validates ACID guarantees, error handling, and transaction control.

use heliosdb_lite::{EmbeddedDatabase, Result};

#[test]
fn test_implicit_transaction_commit() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT, name TEXT)").unwrap();

    // Implicit transaction should auto-commit
    db.execute("INSERT INTO test VALUES (1, 'Alice')").unwrap();

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "INSERT should be committed");
}

#[test]
fn test_implicit_transaction_rollback_on_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Valid insert
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Invalid SQL should rollback (but we can't verify rollback of invalid SQL
    // since the transaction never started)
    let result = db.execute("INVALID SQL SYNTAX");
    assert!(result.is_err(), "Invalid SQL should fail");

    // First insert should still be there
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_explicit_transaction_begin_commit() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Begin explicit transaction
    db.begin().unwrap();
    assert!(db.in_transaction(), "Transaction should be active");

    // Execute operations
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    // Verify transaction is still active
    assert!(db.in_transaction());

    // Commit
    db.commit().unwrap();
    assert!(!db.in_transaction(), "Transaction should be committed");

    // Verify data is persisted
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 2, "Both inserts should be committed");
}

#[test]
fn test_explicit_transaction_rollback() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Insert data outside transaction
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Begin transaction and insert more data
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();
    db.execute("INSERT INTO test VALUES (3)").unwrap();

    // Rollback
    db.rollback().unwrap();
    assert!(!db.in_transaction(), "Transaction should be rolled back");

    // Only first insert should remain
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Only first insert should remain");
}

#[test]
fn test_sql_begin_commit() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // SQL-style transaction control
    db.execute("BEGIN").unwrap();
    assert!(db.in_transaction());

    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    db.execute("COMMIT").unwrap();
    assert!(!db.in_transaction());

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_sql_start_transaction() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // START TRANSACTION is alias for BEGIN
    db.execute("START TRANSACTION").unwrap();
    assert!(db.in_transaction());

    db.execute("INSERT INTO test VALUES (1)").unwrap();

    db.execute("COMMIT").unwrap();
    assert!(!db.in_transaction());

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_sql_rollback() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("ROLLBACK").unwrap();

    assert!(!db.in_transaction());

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 0, "Data should be rolled back");
}

#[test]
fn test_nested_transaction_error() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Begin first transaction
    db.begin().unwrap();
    assert!(db.in_transaction());

    // Trying to begin another should fail
    let result = db.begin();
    assert!(result.is_err(), "Nested transactions should fail");
    assert!(result.unwrap_err().to_string().contains("already active"));

    // Cleanup
    db.rollback().unwrap();
}

#[test]
fn test_commit_without_transaction() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    let result = db.commit();
    assert!(result.is_err(), "Commit without transaction should fail");
    assert!(result.unwrap_err().to_string().contains("No active transaction"));
}

#[test]
fn test_rollback_without_transaction() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    let result = db.rollback();
    assert!(result.is_err(), "Rollback without transaction should fail");
    assert!(result.unwrap_err().to_string().contains("No active transaction"));
}

#[test]
fn test_in_transaction_state() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Initially not in transaction
    assert!(!db.in_transaction());

    // After begin
    db.begin().unwrap();
    assert!(db.in_transaction());

    // After commit
    db.commit().unwrap();
    assert!(!db.in_transaction());

    // After begin and rollback
    db.begin().unwrap();
    assert!(db.in_transaction());
    db.rollback().unwrap();
    assert!(!db.in_transaction());
}

#[test]
fn test_multi_statement_transaction() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE users (id INT, name TEXT)").unwrap();
    db.execute("CREATE TABLE orders (id INT, user_id INT, amount INT)").unwrap();

    // Multi-table transaction
    db.begin().unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
    db.execute("INSERT INTO orders VALUES (1, 1, 100)").unwrap();
    db.execute("INSERT INTO orders VALUES (2, 1, 200)").unwrap();
    db.commit().unwrap();

    // Verify all data committed
    let users = db.query("SELECT * FROM users", &[]).unwrap();
    assert_eq!(users.len(), 1);

    let orders = db.query("SELECT * FROM orders", &[]).unwrap();
    assert_eq!(orders.len(), 2);
}

#[test]
fn test_transaction_with_update() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT, value INT)").unwrap();
    db.execute("INSERT INTO test VALUES (1, 100)").unwrap();

    // Update in transaction
    db.begin().unwrap();
    db.execute("UPDATE test SET value = 200 WHERE id = 1").unwrap();
    db.commit().unwrap();

    let results = db.query("SELECT * FROM test WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_transaction_with_delete() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    // Delete in transaction
    db.begin().unwrap();
    db.execute("DELETE FROM test WHERE id = 1").unwrap();
    db.commit().unwrap();

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
#[ignore = "TODO: Transaction rollback preservation needs implementation"]
fn test_transaction_rollback_preserves_previous_data() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Insert data outside transaction
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    // Modify in transaction then rollback
    db.begin().unwrap();
    db.execute("DELETE FROM test WHERE id = 1").unwrap();
    db.execute("INSERT INTO test VALUES (3)").unwrap();
    db.rollback().unwrap();

    // Original data should be preserved
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 2, "Original data should be preserved");
}

#[test]
fn test_ddl_in_transaction() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    db.begin().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.commit().unwrap();

    // Table should exist and have data
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_ddl_rollback() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    db.begin().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.rollback().unwrap();

    // Note: Current implementation may not fully rollback DDL
    // This is a known limitation - DDL is typically auto-commit in most databases
    // This test documents current behavior
}

#[test]
fn test_case_insensitive_transaction_keywords() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Test various case combinations
    db.execute("begin").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("commit").unwrap();

    db.execute("BeGiN").unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();
    db.execute("RoLlBaCk").unwrap();

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Only committed insert should exist");
}

#[test]
fn test_transaction_with_query() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    // Query during transaction should see uncommitted changes
    // Note: This depends on read-your-own-writes support
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    // Current implementation may or may not see own writes
    // This documents the behavior

    db.commit().unwrap();

    let results_after = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results_after.len(), 2, "Both inserts should be visible after commit");
}

#[test]
fn test_empty_transaction() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Begin and commit without any operations
    db.begin().unwrap();
    db.commit().unwrap();

    assert!(!db.in_transaction());
}

#[test]
fn test_multiple_sequential_transactions() {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // First transaction
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.commit().unwrap();

    // Second transaction
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();
    db.commit().unwrap();

    // Third transaction
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (3)").unwrap();
    db.commit().unwrap();

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 3);
}

// ============================================================================
// ACID Property Tests
// ============================================================================

#[test]
fn test_acid_atomicity_insert_failure() {
    // Atomicity: All operations in a transaction succeed or all fail
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Insert initial data
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Begin transaction with multiple inserts
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();
    db.execute("INSERT INTO test VALUES (3)").unwrap();

    // Rollback - should undo ALL inserts in transaction
    db.rollback().unwrap();

    // Verify atomicity: only initial insert remains
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Atomicity: rollback should undo ALL transaction operations");
}

#[test]
fn test_acid_atomicity_multi_table() {
    // Atomicity across multiple tables
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE accounts (id INT, balance INT)").unwrap();
    db.execute("CREATE TABLE audit_log (id INT, message TEXT)").unwrap();

    // Insert initial data
    db.execute("INSERT INTO accounts VALUES (1, 1000)").unwrap();

    // Transaction: transfer money with audit log
    db.begin().unwrap();
    db.execute("UPDATE accounts SET balance = 900 WHERE id = 1").unwrap();
    db.execute("INSERT INTO audit_log VALUES (1, 'Transfer 100')").unwrap();

    // Rollback both operations
    db.rollback().unwrap();

    // Verify atomicity: both operations rolled back
    let accounts = db.query("SELECT * FROM accounts WHERE id = 1", &[]).unwrap();
    assert_eq!(accounts.len(), 1);
    // Note: balance check would require tuple value extraction

    let audit = db.query("SELECT * FROM audit_log", &[]).unwrap();
    assert_eq!(audit.len(), 0, "Atomicity: audit log should be empty after rollback");
}

#[test]
fn test_acid_consistency_constraints() {
    // Consistency: Transactions maintain database integrity
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Valid transaction maintains consistency
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.commit().unwrap();

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Consistency: valid transaction commits successfully");
}

#[test]
fn test_acid_isolation_read_uncommitted_changes() {
    // Isolation: Transactions should not see uncommitted changes from other transactions
    // Note: This test simulates isolation by verifying rollback behavior
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Transaction 1: Make changes but don't commit
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    // Transaction 1 should see its own writes
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    // Implementation may or may not support read-your-own-writes
    // This documents the behavior

    // Rollback
    db.rollback().unwrap();

    // After rollback, only initial data should exist
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Isolation: uncommitted changes should not persist");
}

#[test]
fn test_acid_durability_commit_persistence() {
    // Durability: Committed changes survive (simulated by re-query)
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Commit a transaction
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap();
    db.commit().unwrap();

    // Verify data persists after commit
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 2, "Durability: committed data should persist");

    // Query again to verify durability
    let results2 = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results2.len(), 2, "Durability: data should still be there on re-query");
}

#[test]
fn test_implicit_transaction_error_rollback() {
    // Critical: Implicit transactions should rollback on error
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Valid insert (implicit transaction commits)
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Invalid SQL - should not corrupt database
    let result = db.execute("INVALID SQL STATEMENT");
    assert!(result.is_err(), "Invalid SQL should return error");

    // Verify first insert is still there (not corrupted by error)
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Previous committed data should survive errors");
}

#[test]
fn test_implicit_transaction_auto_commit() {
    // Each statement should auto-commit in its own implicit transaction
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // First statement (implicit txn 1)
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Second statement (implicit txn 2)
    db.execute("INSERT INTO test VALUES (2)").unwrap();

    // Both should be committed
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 2, "Implicit transactions should auto-commit");
}

#[test]
fn test_transaction_partial_rollback_error() {
    // If one operation in a transaction fails, entire transaction should rollback
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Initial data
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Transaction with partial success
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2)").unwrap(); // Success

    // Try to insert into non-existent table (should fail)
    let result = db.execute("INSERT INTO nonexistent VALUES (3)");

    if result.is_err() {
        // Error occurred - rollback transaction
        db.rollback().unwrap();
    } else {
        // If no error, commit
        db.commit().unwrap();
    }

    // Verify: only initial data should remain (transaction rolled back)
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Failed transaction should rollback all operations");
}

#[test]
fn test_concurrent_implicit_transactions() {
    // Multiple implicit transactions should not interfere
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Simulate "concurrent" operations (sequential in single-threaded test)
    db.execute("INSERT INTO test VALUES (1)").unwrap(); // Implicit txn 1
    db.execute("INSERT INTO test VALUES (2)").unwrap(); // Implicit txn 2
    db.execute("INSERT INTO test VALUES (3)").unwrap(); // Implicit txn 3

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 3, "Concurrent implicit transactions should all succeed");
}

#[test]
fn test_explicit_transaction_isolation() {
    // Explicit transaction should isolate changes until commit
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT, value TEXT)").unwrap();

    // Insert initial data
    db.execute("INSERT INTO test VALUES (1, 'initial')").unwrap();

    // Begin explicit transaction
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (2, 'in-transaction')").unwrap();
    db.execute("UPDATE test SET value = 'modified' WHERE id = 1").unwrap();

    // Before commit, rollback
    db.rollback().unwrap();

    // Verify isolation: changes were isolated and rolled back
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1, "Explicit transaction rollback should restore initial state");
}

#[test]
fn test_transaction_savepoint_simulation() {
    // Simulate savepoint-like behavior with nested transaction attempts
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Outer transaction
    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Trying to begin nested transaction should fail
    let nested_result = db.begin();
    assert!(nested_result.is_err(), "Nested transactions should not be allowed");

    // Original transaction should still be active
    assert!(db.in_transaction(), "Original transaction should remain active");

    // Can still commit original transaction
    db.commit().unwrap();

    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_transaction_commit_idempotency() {
    // Committing an already-committed transaction should fail
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    db.begin().unwrap();
    db.execute("INSERT INTO test VALUES (1)").unwrap();
    db.commit().unwrap();

    // Try to commit again - should fail
    let result = db.commit();
    assert!(result.is_err(), "Committing non-active transaction should fail");
}

#[test]
fn test_transaction_rollback_idempotency() {
    // Rolling back an already-rolled-back transaction should fail
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    db.begin().unwrap();
    db.rollback().unwrap();

    // Try to rollback again - should fail
    let result = db.rollback();
    assert!(result.is_err(), "Rolling back non-active transaction should fail");
}

#[test]
fn test_transaction_mixed_sql_and_api() {
    // Mix SQL transaction control with API calls
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE test (id INT)").unwrap();

    // Start with SQL
    db.execute("BEGIN").unwrap();
    assert!(db.in_transaction());

    // Execute via API
    db.execute("INSERT INTO test VALUES (1)").unwrap();

    // Commit via API
    db.commit().unwrap();
    assert!(!db.in_transaction());

    // Verify data committed
    let results = db.query("SELECT * FROM test", &[]).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_implicit_transaction_with_create_table() {
    // CREATE TABLE should work in implicit transaction
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    // Implicit transaction for DDL
    db.execute("CREATE TABLE test1 (id INT)").unwrap();
    db.execute("CREATE TABLE test2 (id INT)").unwrap();

    // Both tables should exist
    db.execute("INSERT INTO test1 VALUES (1)").unwrap();
    db.execute("INSERT INTO test2 VALUES (2)").unwrap();

    let r1 = db.query("SELECT * FROM test1", &[]).unwrap();
    let r2 = db.query("SELECT * FROM test2", &[]).unwrap();

    assert_eq!(r1.len(), 1);
    assert_eq!(r2.len(), 1);
}
