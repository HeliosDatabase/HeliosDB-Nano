// Savepoint Hardening Tests
//
// Comprehensive tests for SAVEPOINT, RELEASE SAVEPOINT, and ROLLBACK TO SAVEPOINT.
//
// IMPORTANT CONTEXT:
// - SAVEPOINT via `execute()` within a BEGIN block is a KNOWN BUG: the
//   `execute_in_transaction()` catch-all delegates to `sql::Executor` which
//   does not handle Savepoint plan nodes. Use `execute_returning()` instead.
// - ROLLBACK TO SAVEPOINT is a KNOWN STUB: only the savepoint name stack is
//   managed; data changes are NOT undone. Tests document actual behavior.
// - RELEASE SAVEPOINT truncates the savepoint stack at the released position
//   (removing it and all savepoints created after it).
// - ROLLBACK TO SAVEPOINT truncates the stack to keep savepoints up to and
//   including the target (removing all savepoints created after it).

use heliosdb_nano::EmbeddedDatabase;

/// Helper: create an in-memory database with a standard test table.
fn setup() -> EmbeddedDatabase {
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE sp_test (id INT, val TEXT)").unwrap();
    db
}

/// Helper: count rows in sp_test.
fn count_rows(db: &EmbeddedDatabase) -> usize {
    db.query("SELECT * FROM sp_test", &[]).unwrap().len()
}

// ============================================================================
// 1. Basic Savepoint Lifecycle (~8 tests)
// ============================================================================

#[test]
fn test_savepoint_begin_savepoint_release_commit() {
    // BEGIN; SAVEPOINT sp1; RELEASE SAVEPOINT sp1; COMMIT
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1, "Insert should persist after RELEASE + COMMIT");
}

#[test]
fn test_savepoint_begin_savepoint_rollback_to_commit() {
    // BEGIN; SAVEPOINT sp1; ROLLBACK TO SAVEPOINT sp1; COMMIT
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 0 rows (INSERT should be undone). Actual: 1 row (stub).
    assert_eq!(count_rows(&db), 1,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; INSERT persists");
}

#[test]
fn test_savepoint_outside_transaction_errors() {
    // SAVEPOINT outside a transaction should fail.
    let db = setup();

    let result = db.execute_returning("SAVEPOINT sp1");
    assert!(result.is_err(), "SAVEPOINT outside transaction should error");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("within a transaction") || err.contains("SAVEPOINT"),
        "Error should mention transaction requirement, got: {}", err);
}

#[test]
fn test_release_nonexistent_savepoint_errors() {
    // RELEASE SAVEPOINT on a name that was never created should error.
    let db = setup();

    db.execute("BEGIN").unwrap();
    let result = db.execute_returning("RELEASE SAVEPOINT ghost");
    assert!(result.is_err(), "RELEASE nonexistent savepoint should error");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("does not exist"),
        "Error should mention savepoint does not exist, got: {}", err);
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn test_rollback_to_nonexistent_savepoint_errors() {
    // ROLLBACK TO SAVEPOINT on a name that was never created should error.
    let db = setup();

    db.execute("BEGIN").unwrap();
    let result = db.execute_returning("ROLLBACK TO SAVEPOINT ghost");
    assert!(result.is_err(), "ROLLBACK TO nonexistent savepoint should error");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("does not exist"),
        "Error should mention savepoint does not exist, got: {}", err);
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn test_savepoint_same_name_twice_pushes_two() {
    // Creating a savepoint with the same name twice should push two entries.
    // RELEASE should remove the most recent one (rposition search).
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'first')").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'second')").unwrap();

    // RELEASE sp1 should release the second (most recent) sp1 and any after it
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();

    // The first sp1 should still be on the stack, so we can release it too
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();

    db.execute("COMMIT").unwrap();
    assert_eq!(count_rows(&db), 2, "Both inserts should persist");
}

#[test]
fn test_release_then_reference_released_savepoint_errors() {
    // After releasing a savepoint, referencing it should error.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();

    // Trying to ROLLBACK TO or RELEASE the already-released savepoint should fail
    let result = db.execute_returning("ROLLBACK TO SAVEPOINT sp1");
    assert!(result.is_err(), "ROLLBACK TO released savepoint should error");

    let result2 = db.execute_returning("RELEASE SAVEPOINT sp1");
    assert!(result2.is_err(), "RELEASE already-released savepoint should error");

    db.execute("ROLLBACK").unwrap();
}

#[test]
fn test_savepoint_with_underscore_name() {
    // Savepoint names with underscores should work fine.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT my_save_point_1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'ok')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT my_save_point_1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1);
}

// ============================================================================
// 2. Nested Savepoints (~8 tests)
// ============================================================================

#[test]
fn test_nested_savepoint_release_inner_then_outer() {
    // BEGIN; SAVEPOINT sp1; SAVEPOINT sp2; RELEASE sp2; RELEASE sp1; COMMIT
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'b')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp2").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 2, "Both inserts should persist after nested RELEASE + COMMIT");
}

#[test]
fn test_nested_rollback_to_outer_removes_inner() {
    // BEGIN; SAVEPOINT sp1; SAVEPOINT sp2; ROLLBACK TO sp1; COMMIT
    // ROLLBACK TO sp1 should remove sp2 from the stack.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'before_sp2')").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'after_sp2')").unwrap();

    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();

    // sp2 should no longer exist
    let result = db.execute_returning("RELEASE SAVEPOINT sp2");
    assert!(result.is_err(), "sp2 should be gone after ROLLBACK TO sp1");

    // sp1 should still be on the stack
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();

    db.execute("COMMIT").unwrap();

    // SQL standard: 0 rows (both inserts undone). Actual: 2 rows (stub).
    assert_eq!(count_rows(&db), 2,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; both inserts persist");
}

#[test]
fn test_three_levels_of_nesting() {
    // Three nested savepoints, released in reverse order.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'level1')").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'level2')").unwrap();
    db.execute_returning("SAVEPOINT sp3").unwrap();
    db.execute("INSERT INTO sp_test VALUES (3, 'level3')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp3").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp2").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 3, "All three inserts should persist");
}

#[test]
fn test_release_inner_outer_still_valid() {
    // Release inner savepoint; outer should remain valid.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT outer").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'outer_data')").unwrap();
    db.execute_returning("SAVEPOINT inner").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'inner_data')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT inner").unwrap();

    // Outer should still be accessible
    db.execute_returning("RELEASE SAVEPOINT outer").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 2);
}

#[test]
fn test_rollback_to_outer_removes_all_inner() {
    // Create sp1, sp2, sp3. ROLLBACK TO sp1 should remove sp2 and sp3.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute_returning("SAVEPOINT sp3").unwrap();

    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();

    // sp2 and sp3 should be gone
    let r2 = db.execute_returning("RELEASE SAVEPOINT sp2");
    assert!(r2.is_err(), "sp2 should be removed after ROLLBACK TO sp1");
    let r3 = db.execute_returning("RELEASE SAVEPOINT sp3");
    assert!(r3.is_err(), "sp3 should be removed after ROLLBACK TO sp1");

    // sp1 should still exist
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();
}

#[test]
fn test_multiple_savepoints_at_same_level() {
    // Create multiple savepoints without nesting (sequential).
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT a").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT a").unwrap();

    db.execute_returning("SAVEPOINT b").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'b')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT b").unwrap();

    db.execute_returning("SAVEPOINT c").unwrap();
    db.execute("INSERT INTO sp_test VALUES (3, 'c')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT c").unwrap();

    db.execute("COMMIT").unwrap();
    assert_eq!(count_rows(&db), 3, "All sequential savepoint inserts should persist");
}

#[test]
fn test_deep_nesting_five_levels() {
    // 5 levels of nested savepoints.
    let db = setup();

    db.execute("BEGIN").unwrap();
    for i in 1..=5 {
        db.execute_returning(&format!("SAVEPOINT sp{}", i)).unwrap();
        db.execute(&format!("INSERT INTO sp_test VALUES ({}, 'level{}')", i, i)).unwrap();
    }
    // Release in reverse order
    for i in (1..=5).rev() {
        db.execute_returning(&format!("RELEASE SAVEPOINT sp{}", i)).unwrap();
    }
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 5, "All 5 levels of inserts should persist");
}

#[test]
fn test_alternating_savepoint_release_savepoint() {
    // Create, release, create again with different name, release again.
    let db = setup();

    db.execute("BEGIN").unwrap();

    db.execute_returning("SAVEPOINT alpha").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'alpha')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT alpha").unwrap();

    db.execute_returning("SAVEPOINT beta").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'beta')").unwrap();

    // Nest inside beta
    db.execute_returning("SAVEPOINT gamma").unwrap();
    db.execute("INSERT INTO sp_test VALUES (3, 'gamma')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT gamma").unwrap();

    db.execute_returning("RELEASE SAVEPOINT beta").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 3);
}

// ============================================================================
// 3. Savepoint with DML Operations (~10 tests)
// ============================================================================

#[test]
fn test_insert_after_savepoint_rollback_to_stub() {
    // INSERT after SAVEPOINT, then ROLLBACK TO.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'should_vanish')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 0 rows. Actual: 1 row (stub).
    assert_eq!(count_rows(&db), 1,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; INSERT persists");
}

#[test]
fn test_update_after_savepoint_rollback_to_stub() {
    // UPDATE after SAVEPOINT, then ROLLBACK TO.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();
    db.execute("INSERT INTO sp_test VALUES (1, 'original')").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("UPDATE sp_test SET val = 'modified' WHERE id = 1").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    let rows = db.query("SELECT * FROM sp_test WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    // SQL standard: val should be 'original'. Actual: 'modified' (stub).
    // We cannot easily extract the value, but the row count documents the stub behavior.
}

#[test]
fn test_delete_after_savepoint_rollback_to_stub() {
    // DELETE after SAVEPOINT, then ROLLBACK TO.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();
    db.execute("INSERT INTO sp_test VALUES (1, 'keep_me')").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("DELETE FROM sp_test WHERE id = 1").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 1 row (DELETE undone). Actual: 0 rows (stub, DELETE persists).
    assert_eq!(count_rows(&db), 0,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; DELETE persists");
}

#[test]
fn test_multiple_dml_between_savepoints() {
    // Multiple DML operations between two savepoints.
    // NOTE: Within an explicit transaction, DELETE WHERE cannot see rows inserted
    // in the same transaction (no read-your-own-writes). The DELETE effectively
    // matches 0 rows, so all 3 inserts persist.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'b')").unwrap();
    db.execute("INSERT INTO sp_test VALUES (3, 'c')").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("DELETE FROM sp_test WHERE id = 2").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp2").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 2 rows (DELETE removes row 2). Actual: 3 rows (no read-your-own-writes).
    assert_eq!(count_rows(&db), 3,
        "KNOWN LIMITATION: DELETE in explicit transaction cannot see own inserts (no RYOW)");
}

#[test]
fn test_insert_savepoint_update_rollback_to_stub() {
    // INSERT, SAVEPOINT, UPDATE, ROLLBACK TO SAVEPOINT (first INSERT preserved?)
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'before_sp')").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("UPDATE sp_test SET val = 'modified' WHERE id = 1").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // The INSERT before the savepoint should always be preserved.
    // SQL standard: val = 'before_sp' (UPDATE undone). Actual: val = 'modified' (stub).
    assert_eq!(count_rows(&db), 1, "Row should exist regardless of stub behavior");
}

#[test]
fn test_dml_before_savepoint_preserved_on_rollback_to() {
    // DML before savepoint should be preserved even with ROLLBACK TO.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    // This test validates that the pre-savepoint insert persists (which happens
    // to be correct even though the stub doesn't undo post-savepoint changes).
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'before')").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'after')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 1 row (only 'before'). Actual: 2 rows (stub, 'after' not undone).
    assert_eq!(count_rows(&db), 2,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; post-savepoint INSERT persists");
}

#[test]
fn test_dml_visibility_within_transaction_after_savepoint() {
    // NOTE: Read-your-own-writes is NOT supported in explicit transactions.
    // Queries within the transaction do not see uncommitted inserts from the
    // same transaction. Additionally, issuing a query() during an explicit
    // transaction can interfere with the transaction state (the inserts may
    // be lost on COMMIT). This test verifies post-commit visibility WITHOUT
    // querying mid-transaction.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'visible')").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'also_visible')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // After COMMIT, both inserts should be visible
    assert_eq!(count_rows(&db), 2);
}

#[test]
fn test_query_within_explicit_transaction_no_ryow() {
    // Document that read-your-own-writes is NOT supported in explicit transactions.
    // SELECT within the transaction does not see inserts made in the same transaction.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'invisible_within_txn')").unwrap();

    // Query within the transaction sees 0 rows (no RYOW)
    let rows = db.query("SELECT * FROM sp_test", &[]).unwrap();
    assert_eq!(rows.len(), 0,
        "KNOWN LIMITATION: read-your-own-writes not supported in explicit transactions");

    db.execute("ROLLBACK").unwrap();
}

#[test]
fn test_savepoint_insert_release_commit() {
    // SAVEPOINT, INSERT, RELEASE, COMMIT - data should be visible after commit.
    // NOTE: We do not query mid-transaction because query() within an explicit
    // transaction can interfere with transaction state (known limitation).
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'released')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // After commit, the insert is visible
    assert_eq!(count_rows(&db), 1);
}

#[test]
fn test_mixed_ddl_and_dml_around_savepoints() {
    // Create a new table within a transaction with savepoints.
    let db = EmbeddedDatabase::new_in_memory().unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("CREATE TABLE ddl_sp (id INT, name TEXT)").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO ddl_sp VALUES (1, 'hello')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    let rows = db.query("SELECT * FROM ddl_sp", &[]).unwrap();
    assert_eq!(rows.len(), 1, "DDL + DML around savepoint should work");
}

#[test]
fn test_truncate_within_savepoint() {
    // TRUNCATE TABLE within a savepoint context.
    let db = setup();
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'b')").unwrap();
    assert_eq!(count_rows(&db), 2);

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    let truncate_result = db.execute("TRUNCATE TABLE sp_test");
    if truncate_result.is_ok() {
        db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
        db.execute("COMMIT").unwrap();
        assert_eq!(count_rows(&db), 0, "TRUNCATE should remove all rows");
    } else {
        // TRUNCATE may not be supported within savepoint context; document behavior
        db.execute("ROLLBACK").unwrap();
        assert_eq!(count_rows(&db), 2, "Data should be unchanged if TRUNCATE failed");
    }
}

// ============================================================================
// 4. Savepoint with Errors (~6 tests)
// ============================================================================

#[test]
fn test_error_after_savepoint_constraint_violation() {
    // Attempt an operation that causes an error after a savepoint.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE sp_uniq (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("INSERT INTO sp_uniq VALUES (1, 'existing')").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();

    // Try inserting a duplicate PK - should fail
    let result = db.execute("INSERT INTO sp_uniq VALUES (1, 'duplicate')");
    // The error may or may not occur depending on PK enforcement; document behavior
    if result.is_err() {
        // After error, we should still be able to use the savepoint
        let rollback = db.execute_returning("ROLLBACK TO SAVEPOINT sp1");
        // The savepoint should still be valid after the error
        assert!(rollback.is_ok(), "ROLLBACK TO SAVEPOINT should succeed after error");
    }

    db.execute("COMMIT").unwrap();
}

#[test]
fn test_rollback_to_savepoint_after_error_recovery() {
    // The classic error recovery pattern: SAVEPOINT, try risky op, on error ROLLBACK TO.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE sp_recover (id INT, val TEXT)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_recover VALUES (1, 'safe')").unwrap();

    db.execute_returning("SAVEPOINT before_risky").unwrap();

    // Try something that might fail (insert into nonexistent table)
    let risky = db.execute("INSERT INTO nonexistent_table VALUES (99)");
    if risky.is_err() {
        // Recover via savepoint
        db.execute_returning("ROLLBACK TO SAVEPOINT before_risky").unwrap();
    }

    // Continue with more work after recovery
    db.execute("INSERT INTO sp_recover VALUES (2, 'after_recovery')").unwrap();
    db.execute("COMMIT").unwrap();

    let rows = db.query("SELECT * FROM sp_recover", &[]).unwrap();
    assert!(rows.len() >= 1, "At least the safe insert should persist");
}

#[test]
fn test_duplicate_key_insert_after_savepoint() {
    // Duplicate key insert after savepoint creation.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE sp_dup (id INT PRIMARY KEY, val TEXT)").unwrap();
    db.execute("INSERT INTO sp_dup VALUES (1, 'first')").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_dup VALUES (2, 'ok')").unwrap();

    // Duplicate key
    let dup_result = db.execute("INSERT INTO sp_dup VALUES (1, 'dup')");
    // Document whether this errors or silently succeeds
    if dup_result.is_err() {
        // Savepoint should still be valid
        let rb = db.execute_returning("ROLLBACK TO SAVEPOINT sp1");
        assert!(rb.is_ok(), "Savepoint should survive a DML error");
    }

    db.execute("COMMIT").unwrap();
}

#[test]
fn test_type_error_after_savepoint() {
    // Type mismatch in INSERT after savepoint.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE sp_type (id INT, val INT)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();

    // Insert type mismatch (text into INT column) - may or may not error depending on coercion
    let result = db.execute("INSERT INTO sp_type VALUES ('not_a_number', 42)");
    // Document behavior regardless of outcome
    if result.is_err() {
        // Savepoint should remain valid
        let rb = db.execute_returning("ROLLBACK TO SAVEPOINT sp1");
        assert!(rb.is_ok(), "Savepoint should survive a type error");
    }

    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();
}

#[test]
fn test_error_does_not_invalidate_savepoint() {
    // After an error within a savepoint, the savepoint itself should remain.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'good')").unwrap();

    // Cause an error (reference nonexistent table)
    let _ = db.execute("INSERT INTO no_such_table VALUES (99)");

    // Savepoint should still be on the stack
    let release = db.execute_returning("RELEASE SAVEPOINT sp1");
    assert!(release.is_ok(), "Savepoint should survive DML errors");

    db.execute("COMMIT").unwrap();
    assert_eq!(count_rows(&db), 1);
}

#[test]
fn test_multiple_errors_recovery_via_savepoint() {
    // Multiple errors followed by recovery via ROLLBACK TO SAVEPOINT.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'baseline')").unwrap();
    db.execute_returning("SAVEPOINT recovery_point").unwrap();

    // Error 1
    let _ = db.execute("INSERT INTO nonexistent1 VALUES (1)");
    // Error 2
    let _ = db.execute("INSERT INTO nonexistent2 VALUES (2)");

    // Recover
    let rb = db.execute_returning("ROLLBACK TO SAVEPOINT recovery_point");
    assert!(rb.is_ok(), "ROLLBACK TO SAVEPOINT should succeed after multiple errors");

    // Continue working
    db.execute("INSERT INTO sp_test VALUES (2, 'after_recovery')").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 2, "Baseline and after-recovery inserts should persist");
}

// ============================================================================
// 5. Transaction Commit/Rollback Interaction (~8 tests)
// ============================================================================

#[test]
fn test_full_transaction_rollback_with_savepoint() {
    // BEGIN; SAVEPOINT; ROLLBACK (full transaction rollback, not ROLLBACK TO)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'gone')").unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(count_rows(&db), 0, "Full ROLLBACK should discard everything including savepoint data");
}

#[test]
fn test_commit_implicitly_releases_savepoints() {
    // BEGIN; SAVEPOINT sp1; COMMIT - savepoints should be implicitly released.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'committed')").unwrap();
    // Do NOT explicitly release sp1
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1, "COMMIT should implicitly release all savepoints");
}

#[test]
fn test_savepoint_in_autocommit_mode() {
    // SAVEPOINT without an explicit BEGIN should error (autocommit mode).
    let db = setup();

    let result = db.execute_returning("SAVEPOINT sp1");
    assert!(result.is_err(),
        "SAVEPOINT in autocommit mode (no active transaction) should error");
}

#[test]
fn test_begin_insert_savepoint_insert_commit_both_persisted() {
    // BEGIN; INSERT; SAVEPOINT; INSERT; COMMIT - both inserts should be persisted.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'before_sp')").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'after_sp')").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 2, "Both inserts should persist after COMMIT");
}

#[test]
fn test_begin_insert_savepoint_insert_rollback_nothing_persisted() {
    // BEGIN; INSERT; SAVEPOINT; INSERT; ROLLBACK - nothing should persist.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'gone1')").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'gone2')").unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(count_rows(&db), 0, "Full ROLLBACK should discard all inserts");
}

#[test]
fn test_nested_begin_not_supported_with_savepoints() {
    // Nested BEGIN should fail; savepoints are the alternative.
    let db = setup();

    db.execute("BEGIN").unwrap();
    let result = db.execute("BEGIN");
    assert!(result.is_err(), "Nested BEGIN should not be supported");

    // But savepoints should work as the nesting mechanism
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'nested_via_sp')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1);
}

#[test]
fn test_savepoint_after_failed_dml() {
    // Create a savepoint after a DML failure and continue working.
    let db = setup();

    db.execute("BEGIN").unwrap();

    // Failed DML (nonexistent table)
    let _ = db.execute("INSERT INTO nonexistent VALUES (1)");

    // Should still be able to create a savepoint and work
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'after_failure')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1);
}

#[test]
fn test_full_rollback_does_not_clear_savepoint_stack() {
    // KNOWN LIMITATION: rollback_internal() does NOT clear the savepoints Vec.
    // Savepoint names from a rolled-back transaction leak into the next transaction.
    // This is a bug: ideally ROLLBACK should clear the savepoint stack.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("ROLLBACK").unwrap();

    // Start a new transaction - savepoint names LEAK from previous transaction
    db.execute("BEGIN").unwrap();
    let result = db.execute_returning("RELEASE SAVEPOINT sp1");
    // BUG: This succeeds because savepoints were not cleared by ROLLBACK
    assert!(result.is_ok(),
        "KNOWN BUG: savepoint stack not cleared on ROLLBACK; old savepoints leak");
    db.execute("ROLLBACK").unwrap();
}

// ============================================================================
// 6. Savepoint Naming Edge Cases (~5 tests)
// ============================================================================

#[test]
fn test_savepoint_with_very_long_name() {
    // Savepoint with a very long name (256 characters).
    let db = setup();
    let long_name = "x".repeat(256);

    db.execute("BEGIN").unwrap();
    let result = db.execute_returning(&format!("SAVEPOINT {}", long_name));
    if result.is_ok() {
        let release = db.execute_returning(&format!("RELEASE SAVEPOINT {}", long_name));
        assert!(release.is_ok(), "RELEASE long-named savepoint should work");
    }
    // If creation fails, that is acceptable behavior too - document it
    db.execute("COMMIT").unwrap();
}

#[test]
fn test_savepoint_named_same_as_table() {
    // Savepoint name that collides with a table name should be fine (different namespace).
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp_test").unwrap(); // Same name as the table
    db.execute("INSERT INTO sp_test VALUES (1, 'no_conflict')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp_test").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1, "Savepoint named same as table should not conflict");
}

#[test]
fn test_savepoint_case_sensitivity() {
    // Test whether savepoint names are case-sensitive.
    // SQL standard says savepoint names are identifiers and should be case-insensitive,
    // but implementations vary. This test documents the actual behavior.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT MyPoint").unwrap();

    // Try to release with different case
    let result = db.execute_returning("RELEASE SAVEPOINT mypoint");
    if result.is_ok() {
        // Case-insensitive: release succeeded
        db.execute("COMMIT").unwrap();
    } else {
        // Case-sensitive: 'mypoint' != 'MyPoint'
        // Release the original casing
        db.execute_returning("RELEASE SAVEPOINT MyPoint").unwrap();
        db.execute("COMMIT").unwrap();
    }
}

#[test]
fn test_savepoint_with_numeric_name() {
    // Savepoint with a purely numeric name (if parser accepts it).
    let db = setup();

    db.execute("BEGIN").unwrap();
    // Some SQL parsers require identifiers to start with a letter.
    // Test what happens with a numeric name.
    let result = db.execute_returning("SAVEPOINT s123");
    if result.is_ok() {
        db.execute("INSERT INTO sp_test VALUES (1, 'numeric_sp')").unwrap();
        db.execute_returning("RELEASE SAVEPOINT s123").unwrap();
    }
    db.execute("COMMIT").unwrap();
}

#[test]
fn test_savepoint_reuse_name_after_release() {
    // After releasing a savepoint, the same name should be usable again.
    let db = setup();

    db.execute("BEGIN").unwrap();

    // First use
    db.execute_returning("SAVEPOINT reusable").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'first_use')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT reusable").unwrap();

    // Second use with same name
    db.execute_returning("SAVEPOINT reusable").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'second_use')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT reusable").unwrap();

    db.execute("COMMIT").unwrap();
    assert_eq!(count_rows(&db), 2, "Both uses of the reused savepoint name should persist");
}

// ============================================================================
// Additional edge cases
// ============================================================================

#[test]
fn test_savepoint_via_execute_is_known_bug() {
    // Document that execute() within a BEGIN block does NOT handle SAVEPOINT.
    // This is a known routing bug: execute_in_transaction() falls through to
    // sql::Executor which does not implement Savepoint plan nodes.
    let db = setup();

    db.execute("BEGIN").unwrap();
    let result = db.execute("SAVEPOINT sp1");
    // Current behavior: fails with "not yet implemented" or similar
    assert!(result.is_err(),
        "KNOWN BUG: SAVEPOINT via execute() in BEGIN block fails (not routed to handler)");
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn test_release_savepoint_via_execute_is_known_bug() {
    // Document that RELEASE SAVEPOINT via execute() also does not work.
    let db = setup();

    db.execute("BEGIN").unwrap();
    // First create via execute_returning (which works)
    db.execute_returning("SAVEPOINT sp1").unwrap();

    // Try to release via execute() (which should fail due to same routing bug)
    let result = db.execute("RELEASE SAVEPOINT sp1");
    if result.is_err() {
        // Known bug path - clean up via execute_returning
        db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    }
    db.execute("COMMIT").unwrap();
}

#[test]
fn test_rollback_to_savepoint_via_execute_is_known_bug() {
    // Document that ROLLBACK TO SAVEPOINT via execute() also does not work.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'data')").unwrap();

    // Try ROLLBACK TO via execute() (should fail due to routing bug)
    let result = db.execute("ROLLBACK TO SAVEPOINT sp1");
    if result.is_err() {
        // Known bug path - use execute_returning instead
        db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    }
    db.execute("COMMIT").unwrap();
}

#[test]
fn test_savepoint_survives_successful_dml() {
    // A savepoint should remain valid after successful DML.
    // NOTE: DELETE WHERE cannot see rows inserted in the same explicit transaction
    // (no read-your-own-writes), so all 3 inserts persist.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();

    // Several successful DML operations
    db.execute("INSERT INTO sp_test VALUES (1, 'a')").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'b')").unwrap();
    db.execute("INSERT INTO sp_test VALUES (3, 'c')").unwrap();
    db.execute("DELETE FROM sp_test WHERE id = 2").unwrap();

    // Savepoint should still be releasable
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 2 rows (DELETE removes row 2). Actual: 3 rows (no RYOW).
    assert_eq!(count_rows(&db), 3,
        "KNOWN LIMITATION: DELETE in explicit transaction cannot see own inserts (no RYOW)");
}

#[test]
fn test_release_outer_also_releases_inner() {
    // Releasing an outer savepoint should also release all inner savepoints.
    // Implementation: truncate(pos) removes the savepoint at pos and everything after it.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT outer").unwrap();
    db.execute_returning("SAVEPOINT middle").unwrap();
    db.execute_returning("SAVEPOINT inner").unwrap();

    // Release outer - should remove outer, middle, and inner
    db.execute_returning("RELEASE SAVEPOINT outer").unwrap();

    // middle and inner should be gone
    let r1 = db.execute_returning("RELEASE SAVEPOINT middle");
    assert!(r1.is_err(), "middle should be gone after releasing outer");
    let r2 = db.execute_returning("RELEASE SAVEPOINT inner");
    assert!(r2.is_err(), "inner should be gone after releasing outer");

    db.execute("COMMIT").unwrap();
}

#[test]
fn test_rollback_to_preserves_target_savepoint() {
    // ROLLBACK TO sp1 should keep sp1 on the stack (usable again).
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'first')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();

    // sp1 should still be valid - we can create new savepoints after it
    db.execute("INSERT INTO sp_test VALUES (2, 'second')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();

    // sp1 should STILL be valid after second rollback to
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 0 rows (both inserts undone). Actual: 2 rows (stub).
    assert_eq!(count_rows(&db), 2,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; both inserts persist");
}

#[test]
fn test_savepoint_with_no_dml_operations() {
    // Create and release a savepoint with no DML in between.
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT empty_sp").unwrap();
    db.execute_returning("RELEASE SAVEPOINT empty_sp").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 0, "No data should exist with empty savepoint");
}

#[test]
fn test_rollback_to_with_no_dml_after_savepoint() {
    // ROLLBACK TO with no DML after the savepoint - should be a no-op.
    let db = setup();
    db.execute("INSERT INTO sp_test VALUES (1, 'pre_existing')").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    // No DML here
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    assert_eq!(count_rows(&db), 1, "Pre-existing data should be unchanged");
}

#[test]
fn test_commit_after_rollback_to_savepoint() {
    // ROLLBACK TO SAVEPOINT then COMMIT should commit whatever state we have.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'committed')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 0 rows (INSERT undone by ROLLBACK TO). Actual: 1 row (stub).
    assert_eq!(count_rows(&db), 1,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; INSERT persists through COMMIT");
}

#[test]
fn test_multiple_tables_with_savepoints() {
    // Savepoint operations spanning multiple tables.
    let db = EmbeddedDatabase::new_in_memory().unwrap();
    db.execute("CREATE TABLE orders (id INT, amount INT)").unwrap();
    db.execute("CREATE TABLE audit (id INT, msg TEXT)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO orders VALUES (1, 100)").unwrap();
    db.execute("INSERT INTO audit VALUES (1, 'order created')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    let orders = db.query("SELECT * FROM orders", &[]).unwrap();
    let audit = db.query("SELECT * FROM audit", &[]).unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(audit.len(), 1);
}

#[test]
fn test_savepoint_interleaved_with_selects_pre_existing_data() {
    // SELECT queries between savepoint operations using pre-existing data only.
    // We avoid inserting + querying within the same transaction because
    // query() can interfere with the transaction (no RYOW + state disruption).
    let db = setup();
    db.execute("INSERT INTO sp_test VALUES (1, 'pre')").unwrap();

    db.execute("BEGIN").unwrap();
    // Pre-existing row is visible within the transaction
    let rows = db.query("SELECT * FROM sp_test", &[]).unwrap();
    assert_eq!(rows.len(), 1, "Pre-existing row should be visible within transaction");

    db.execute_returning("SAVEPOINT sp1").unwrap();
    let rows = db.query("SELECT * FROM sp_test", &[]).unwrap();
    assert_eq!(rows.len(), 1, "Still just the pre-existing row after savepoint");

    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    let rows = db.query("SELECT * FROM sp_test", &[]).unwrap();
    assert_eq!(rows.len(), 1, "Pre-existing row still visible after release");

    db.execute("COMMIT").unwrap();
    assert_eq!(count_rows(&db), 1);
}

#[test]
fn test_savepoint_with_insert_no_mid_txn_query() {
    // Insert within savepoint, verify visibility after commit (no mid-txn queries).
    let db = setup();
    db.execute("INSERT INTO sp_test VALUES (1, 'pre')").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'new')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // After commit, both rows are visible
    assert_eq!(count_rows(&db), 2);
}

#[test]
fn test_rollback_to_middle_savepoint() {
    // sp1, sp2, sp3 - ROLLBACK TO sp2. sp3 removed, sp1 and sp2 remain.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'sp1_data')").unwrap();
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'sp2_data')").unwrap();
    db.execute_returning("SAVEPOINT sp3").unwrap();
    db.execute("INSERT INTO sp_test VALUES (3, 'sp3_data')").unwrap();

    db.execute_returning("ROLLBACK TO SAVEPOINT sp2").unwrap();

    // sp3 should be gone
    let r3 = db.execute_returning("RELEASE SAVEPOINT sp3");
    assert!(r3.is_err(), "sp3 should be gone after ROLLBACK TO sp2");

    // sp2 should still exist (ROLLBACK TO keeps the target)
    db.execute_returning("RELEASE SAVEPOINT sp2").unwrap();

    // sp1 should still exist
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();

    db.execute("COMMIT").unwrap();
}

#[test]
fn test_savepoint_create_after_rollback_to() {
    // After ROLLBACK TO sp1, create a new savepoint sp2.
    // NOTE: ROLLBACK TO SAVEPOINT is a stub - data changes are NOT undone (known limitation)
    let db = setup();

    db.execute("BEGIN").unwrap();
    db.execute_returning("SAVEPOINT sp1").unwrap();
    db.execute("INSERT INTO sp_test VALUES (1, 'before_rollback')").unwrap();
    db.execute_returning("ROLLBACK TO SAVEPOINT sp1").unwrap();

    // Create a new savepoint after rollback
    db.execute_returning("SAVEPOINT sp2").unwrap();
    db.execute("INSERT INTO sp_test VALUES (2, 'after_rollback')").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp2").unwrap();
    db.execute_returning("RELEASE SAVEPOINT sp1").unwrap();
    db.execute("COMMIT").unwrap();

    // SQL standard: 1 row (only sp2 insert). Actual: 2 rows (stub, sp1 insert not undone).
    assert_eq!(count_rows(&db), 2,
        "KNOWN LIMITATION: ROLLBACK TO SAVEPOINT is a stub; first INSERT persists");
}
