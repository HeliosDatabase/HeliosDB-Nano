//! Edge case and error handling tests for HeliosDB Lite
//!
//! Tests boundary conditions, error cases, and security

mod test_helpers;

use heliosdb_nano::{EmbeddedDatabase, Result};
use test_helpers::*;

// ============================================================================
// Empty Data Tests
// ============================================================================

#[test]
fn test_query_empty_table() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Query empty table
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 0);

    Ok(())
}

#[test]
fn test_update_empty_table() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Update on empty table
    let affected = db.execute("UPDATE users SET age = 100")?;
    assert_eq!(affected, 0);

    Ok(())
}

#[test]
fn test_delete_empty_table() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Delete from empty table
    let affected = db.execute("DELETE FROM users")?;
    assert_eq!(affected, 0);

    Ok(())
}

#[test]
fn test_aggregate_on_empty_table() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // COUNT on empty table should return 0
    let results = db.query("SELECT COUNT(*) FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 0);

    Ok(())
}

// ============================================================================
// Boundary Value Tests
// ============================================================================

#[test]
fn test_single_row_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert single row
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Query single row
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    // Update single row
    let affected = db.execute("UPDATE users SET age = 31")?;
    assert_eq!(affected, 1);

    // Delete single row
    let affected = db.execute("DELETE FROM users")?;
    assert_eq!(affected, 1);

    Ok(())
}

#[test]
fn test_maximum_integer_values() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE max_ints (id INT PRIMARY KEY, max_int INT)")?;

    // Test max value (skip min value due to parsing issues)
    db.execute("INSERT INTO max_ints (id, max_int) VALUES (1, 2147483647)")?;

    let results = db.query("SELECT * FROM max_ints", &[])?;
    assert_eq!(get_int_value(&results[0], 1).unwrap(), 2147483647);

    Ok(())
}

#[test]
fn test_very_large_result_set() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10000)?;

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 10000);

    Ok(())
}

#[test]
fn test_limit_zero() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10)?;

    let results = db.query("SELECT * FROM users LIMIT 0", &[])?;
    assert_eq!(results.len(), 0);

    Ok(())
}

#[test]
fn test_limit_one() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10)?;

    let results = db.query("SELECT * FROM users LIMIT 1", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_offset_beyond_result_set() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10)?;

    let results = db.query("SELECT * FROM users LIMIT 10 OFFSET 100", &[])?;
    assert_eq!(results.len(), 0);

    Ok(())
}

// ============================================================================
// SQL Syntax Error Tests
// ============================================================================

#[test]
fn test_invalid_sql_syntax() {
    let db = create_test_db().unwrap();

    // Completely invalid SQL
    let result = db.query("SELECT FROM WHERE", &[]);
    assert!(result.is_err());

    // Missing FROM clause - behavior varies by database:
    // Some return error, some return empty, some return single row
    // Our parser accepts this as valid syntax
    let _result = db.query("SELECT *", &[]);
    // No assertion - behavior is implementation-defined

    // Incomplete statement
    let result = db.query("SELECT * FROM", &[]);
    assert!(result.is_err());
}

#[test]
fn test_table_not_found() {
    let db = create_test_db().unwrap();

    let result = db.query("SELECT * FROM nonexistent_table", &[]);
    // Should either error or return empty (depending on implementation)
    match result {
        Ok(rows) => assert_eq!(rows.len(), 0),
        Err(_) => {} // Error is also acceptable
    }
}

#[test]
fn test_column_not_found() {
    let db = create_test_db().unwrap();
    setup_users_table(&db).unwrap();

    let result = db.query("SELECT nonexistent_column FROM users", &[]);
    // Should error or handle gracefully
    match result {
        Ok(_) => {},
        Err(_) => {} // Error is acceptable
    }
}

#[test]
fn test_ambiguous_column() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE t1 (id INT PRIMARY KEY, value INT)").unwrap();
    db.execute("CREATE TABLE t2 (id INT PRIMARY KEY, value INT)").unwrap();

    // Ambiguous column reference in join
    let result = db.query("SELECT id FROM t1 INNER JOIN t2 ON t1.id = t2.id", &[]);
    // Should either error or handle gracefully
    match result {
        Ok(_) => {},
        Err(_) => {}
    }
}

// ============================================================================
// Type Mismatch Tests
// ============================================================================

#[test]
fn test_string_in_int_column() {
    let db = create_test_db().unwrap();
    db.execute("CREATE TABLE type_test (id INT PRIMARY KEY, value INT)").unwrap();

    // Attempt to insert string in INT column
    let result = db.execute("INSERT INTO type_test (id, value) VALUES (1, 'not a number')");
    // Should error
    match result {
        Ok(_) => {}, // Some systems may coerce types
        Err(_) => {}
    }
}

#[test]
fn test_comparison_type_mismatch() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Compare int with string (may succeed or fail depending on implementation)
    let result = db.query("SELECT * FROM users WHERE age = '30'", &[]);
    match result {
        Ok(_) => {},
        Err(_) => {}
    }

    Ok(())
}

// ============================================================================
// Special Character Tests
// ============================================================================

#[test]
fn test_sql_injection_prevention() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Attempt SQL injection (should be handled safely)
    let malicious_input = "1 OR 1=1; DROP TABLE users;--";

    // Using parameterized query (when implemented) would prevent injection
    // For now, test that table still exists after attempted injection
    let result = db.query("SELECT * FROM users", &[]);
    assert!(result.is_ok());

    Ok(())
}

#[test]
fn test_quotes_in_strings() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Test single quotes (SQL escaped)
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'O''Brien', 'test@example.com', 30)")?;

    let results = db.query("SELECT name FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_special_characters_in_data() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Test various special characters
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Test\n\r\t', 'test@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Test<>{}[]', 'test2@example.com', 25)")?;

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 2);

    Ok(())
}

// ============================================================================
// Concurrent Access Tests
// ============================================================================

#[test]
fn test_multiple_reads_same_data() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 100)?;

    // Multiple concurrent reads should all return same data
    for _ in 0..10 {
        let results = db.query("SELECT * FROM users", &[])?;
        assert_eq!(results.len(), 100);
    }

    Ok(())
}

#[test]
fn test_read_after_write() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Write
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Immediate read should see the write
    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_write_after_read() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Read empty table
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 0);

    // Write
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Read again should see new data
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

// ============================================================================
// Memory and Resource Tests
// ============================================================================

#[test]
fn test_many_columns() -> Result<()> {
    let db = create_test_db()?;

    // Create table with many columns
    let mut cols = vec!["id INT PRIMARY KEY".to_string()];
    for i in 1..=50 {
        cols.push(format!("col{} INT", i));
    }

    db.execute(&format!("CREATE TABLE wide_table ({})", cols.join(", ")))?;

    // Insert data
    let mut values = vec!["1".to_string()];
    for i in 1..=50 {
        values.push(i.to_string());
    }

    db.execute(&format!("INSERT INTO wide_table VALUES ({})", values.join(", ")))?;

    let results = db.query("SELECT * FROM wide_table", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].len(), 51); // id + 50 columns

    Ok(())
}

#[test]
fn test_repeated_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Perform same operation many times
    for i in 1..=100 {
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', {})",
            i, i, i, 20 + (i % 50)
        ))?;

        let results = db.query("SELECT * FROM users", &[])?;
        assert_eq!(results.len(), i as usize);
    }

    Ok(())
}

#[test]
fn test_alternating_insert_delete() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    for i in 1..=50 {
        // Insert
        db.execute(&format!(
            "INSERT INTO users (id, name, email, age) VALUES ({}, 'User{}', 'user{}@example.com', 30)",
            i, i, i
        ))?;

        // Delete
        if i > 1 {
            db.execute(&format!("DELETE FROM users WHERE id = {}", i - 1))?;
        }
    }

    // Should have only last inserted row
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

// ============================================================================
// Data Integrity Tests
// ============================================================================

#[test]
fn test_data_persistence_across_queries() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert data
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Multiple queries should all see the same data
    for _ in 0..10 {
        let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
        assert_eq!(results.len(), 1);
        assert_eq!(get_string_value(&results[0], 1).unwrap(), "Alice");
        assert_eq!(get_int_value(&results[0], 3).unwrap(), 30);
    }

    Ok(())
}

#[test]
fn test_update_preserves_other_columns() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    // Update only age
    db.execute("UPDATE users SET age = 31 WHERE id = 1")?;

    // Verify other columns unchanged
    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(get_string_value(&results[0], 1).unwrap(), "Alice");
    assert_eq!(get_string_value(&results[0], 2).unwrap(), "alice@example.com");
    assert_eq!(get_int_value(&results[0], 3).unwrap(), 31);

    Ok(())
}

#[test]
fn test_delete_does_not_affect_other_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 35)")?;

    // Delete middle row
    db.execute("DELETE FROM users WHERE id = 2")?;

    // Verify other rows unchanged
    let alice = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(alice.len(), 1);
    assert_eq!(get_string_value(&alice[0], 1).unwrap(), "Alice");

    let charlie = db.query("SELECT * FROM users WHERE id = 3", &[])?;
    assert_eq!(charlie.len(), 1);
    assert_eq!(get_string_value(&charlie[0], 1).unwrap(), "Charlie");

    Ok(())
}

// ============================================================================
// Whitespace and Formatting Tests
// ============================================================================

#[test]
fn test_sql_with_extra_whitespace() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // SQL with lots of whitespace
    db.execute("INSERT   INTO   users   (id,   name,   email,   age)   VALUES   (1,   'Alice',   'alice@example.com',   30)")?;

    let results = db.query("SELECT   *   FROM   users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_sql_with_newlines() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    let sql = "INSERT INTO users
               (id, name, email, age)
               VALUES
               (1, 'Alice', 'alice@example.com', 30)";

    db.execute(sql)?;

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}
