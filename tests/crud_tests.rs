//! Comprehensive CRUD operation tests for HeliosDB Lite
//!
//! Tests CREATE, INSERT, UPDATE, DELETE operations with various scenarios

mod test_helpers;

use heliosdb_nano::{EmbeddedDatabase, Result, Value};
use test_helpers::*;

// ============================================================================
// CREATE TABLE Tests
// ============================================================================

#[test]
fn test_create_table_simple() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE simple (id INT PRIMARY KEY, name TEXT)")?;

    // Verify table exists by inserting data
    db.execute("INSERT INTO simple (id, name) VALUES (1, 'test')")?;
    let results = db.query("SELECT * FROM simple", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_create_table_all_types() -> Result<()> {
    let db = create_test_db()?;

    db.execute(
        "CREATE TABLE all_types (
            id INT PRIMARY KEY,
            bool_col BOOLEAN,
            int_col INT,
            bigint_col BIGINT,
            text_col TEXT,
            varchar_col VARCHAR(100)
        )"
    )?;

    // Verify table creation by inserting data
    db.execute(
        "INSERT INTO all_types (id, bool_col, int_col, bigint_col, text_col, varchar_col)
         VALUES (1, TRUE, 42, 9223372036854775807, 'text', 'varchar')"
    )?;

    let results = db.query("SELECT * FROM all_types", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_create_multiple_tables() -> Result<()> {
    let db = create_test_db()?;

    db.execute("CREATE TABLE table1 (id INT PRIMARY KEY)")?;
    db.execute("CREATE TABLE table2 (id INT PRIMARY KEY)")?;
    db.execute("CREATE TABLE table3 (id INT PRIMARY KEY)")?;

    // Verify all tables exist
    db.execute("INSERT INTO table1 (id) VALUES (1)")?;
    db.execute("INSERT INTO table2 (id) VALUES (2)")?;
    db.execute("INSERT INTO table3 (id) VALUES (3)")?;

    assert_row_count(&db, "SELECT * FROM table1", 1)?;
    assert_row_count(&db, "SELECT * FROM table2", 1)?;
    assert_row_count(&db, "SELECT * FROM table3", 1)?;

    Ok(())
}

#[test]
fn test_create_table_with_constraints() -> Result<()> {
    let db = create_test_db()?;

    db.execute(
        "CREATE TABLE constrained (
            id INT PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT
        )"
    )?;

    // Insert valid data
    db.execute("INSERT INTO constrained (id, name, email) VALUES (1, 'Alice', 'alice@example.com')")?;

    let results = db.query("SELECT * FROM constrained", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

// ============================================================================
// INSERT Tests
// ============================================================================

#[test]
fn test_insert_single_row() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    let affected = db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    assert_eq!(affected, 1);

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(0).unwrap(), &Value::Int4(1));
    assert_eq!(results[0].get(1).unwrap(), &Value::String("Alice".to_string()));

    Ok(())
}

#[test]
fn test_insert_multiple_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 35)")?;

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 3);

    Ok(())
}

#[test]
fn test_insert_bulk_data() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 100)?;

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 100);

    Ok(())
}

#[test]
fn test_insert_with_special_characters() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Test special characters in strings
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'O''Brien', 'test@example.com', 30)")?;

    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_insert_null_values() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE nullable (id INT PRIMARY KEY, optional_value INT)")?;

    // Insert with NULL
    db.execute("INSERT INTO nullable (id, optional_value) VALUES (1, NULL)")?;

    let results = db.query("SELECT * FROM nullable", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get(1).unwrap(), &Value::Null);

    Ok(())
}

#[test]
fn test_insert_very_long_strings() -> Result<()> {
    let db = create_test_db()?;
    db.execute("CREATE TABLE long_strings (id INT PRIMARY KEY, text_col TEXT)")?;

    // Insert very long string (10KB)
    let long_string = "a".repeat(10_000);
    db.execute(&format!("INSERT INTO long_strings (id, text_col) VALUES (1, '{}')", long_string))?;

    let results = db.query("SELECT * FROM long_strings", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(
        get_string_value(&results[0], 1).unwrap().len(),
        10_000
    );

    Ok(())
}

// ============================================================================
// UPDATE Tests
// ============================================================================

#[test]
fn test_update_single_row() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    let affected = db.execute("UPDATE users SET age = 31 WHERE id = 1")?;
    assert_eq!(affected, 1);

    let results = db.query("SELECT age FROM users WHERE id = 1", &[])?;
    assert_eq!(results[0].get(0).unwrap(), &Value::Int4(31));

    Ok(())
}

#[test]
fn test_update_multiple_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 25)")?;

    let affected = db.execute("UPDATE users SET age = 35 WHERE age = 30")?;
    assert_eq!(affected, 2);

    let results = db.query("SELECT * FROM users WHERE age = 35", &[])?;
    assert_eq!(results.len(), 2);

    Ok(())
}

#[test]
fn test_update_all_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10)?;

    let affected = db.execute("UPDATE users SET age = 40")?;
    assert_eq!(affected, 10);

    let results = db.query("SELECT * FROM users WHERE age = 40", &[])?;
    assert_eq!(results.len(), 10);

    Ok(())
}

#[test]
fn test_update_multiple_columns() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    let affected = db.execute("UPDATE users SET name = 'Alicia', age = 31, email = 'alicia@example.com' WHERE id = 1")?;
    assert_eq!(affected, 1);

    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(get_string_value(&results[0], 1).unwrap(), "Alicia");
    assert_eq!(get_string_value(&results[0], 2).unwrap(), "alicia@example.com");
    assert_eq!(get_int_value(&results[0], 3).unwrap(), 31);

    Ok(())
}

#[test]
fn test_update_no_matching_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    let affected = db.execute("UPDATE users SET age = 40 WHERE id = 999")?;
    assert_eq!(affected, 0);

    Ok(())
}

#[test]
fn test_update_with_complex_where() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 35)")?;

    let affected = db.execute("UPDATE users SET age = 50 WHERE age > 25 AND age < 35")?;
    assert_eq!(affected, 1); // Only Alice (age 30)

    let results = db.query("SELECT * FROM users WHERE age = 50", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 1); // Alice's ID

    Ok(())
}

// ============================================================================
// DELETE Tests
// ============================================================================

#[test]
fn test_delete_single_row() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;

    let affected = db.execute("DELETE FROM users WHERE id = 1")?;
    assert_eq!(affected, 1);

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 2); // Bob remains

    Ok(())
}

#[test]
fn test_delete_multiple_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 25)")?;

    let affected = db.execute("DELETE FROM users WHERE age = 30")?;
    assert_eq!(affected, 2);

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 3); // Charlie remains

    Ok(())
}

#[test]
fn test_delete_all_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 10)?;

    let affected = db.execute("DELETE FROM users")?;
    assert_eq!(affected, 10);

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 0);

    Ok(())
}

#[test]
fn test_delete_no_matching_rows() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;

    let affected = db.execute("DELETE FROM users WHERE id = 999")?;
    assert_eq!(affected, 0);

    // Original row still exists
    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 1);

    Ok(())
}

#[test]
fn test_delete_with_complex_where() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (2, 'Bob', 'bob@example.com', 25)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (3, 'Charlie', 'charlie@example.com', 35)")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (4, 'Dave', 'dave@example.com', 40)")?;

    let affected = db.execute("DELETE FROM users WHERE age >= 30 AND age < 40")?;
    assert_eq!(affected, 2); // Alice and Charlie

    let results = db.query("SELECT * FROM users", &[])?;
    assert_eq!(results.len(), 2); // Bob and Dave remain

    Ok(())
}

#[test]
fn test_delete_and_reinsert() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // Insert, delete, then reinsert with same ID
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    db.execute("DELETE FROM users WHERE id = 1")?;
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Bob', 'bob@example.com', 25)")?;

    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(results.len(), 1);
    assert_eq!(get_string_value(&results[0], 1).unwrap(), "Bob");

    Ok(())
}

// ============================================================================
// Combined CRUD Operations
// ============================================================================

#[test]
fn test_crud_lifecycle() -> Result<()> {
    let db = create_test_db()?;
    setup_users_table(&db)?;

    // CREATE (table already created in setup)

    // INSERT
    db.execute("INSERT INTO users (id, name, email, age) VALUES (1, 'Alice', 'alice@example.com', 30)")?;
    assert_row_count(&db, "SELECT * FROM users", 1)?;

    // READ
    let results = db.query("SELECT * FROM users WHERE id = 1", &[])?;
    assert_eq!(get_string_value(&results[0], 1).unwrap(), "Alice");

    // UPDATE
    db.execute("UPDATE users SET age = 31 WHERE id = 1")?;
    let results = db.query("SELECT age FROM users WHERE id = 1", &[])?;
    assert_eq!(get_int_value(&results[0], 0).unwrap(), 31);

    // DELETE
    db.execute("DELETE FROM users WHERE id = 1")?;
    assert_row_count(&db, "SELECT * FROM users", 0)?;

    Ok(())
}

#[test]
fn test_large_dataset_operations() -> Result<()> {
    let db = create_test_db()?;
    setup_with_test_data(&db, 1000)?;

    // Verify insert
    assert_row_count(&db, "SELECT * FROM users", 1000)?;

    // Update subset
    let affected = db.execute("UPDATE users SET age = 100 WHERE id <= 100")?;
    assert_eq!(affected, 100);

    // Delete subset
    let affected = db.execute("DELETE FROM users WHERE age = 100")?;
    assert_eq!(affected, 100);

    // Verify final count
    assert_row_count(&db, "SELECT * FROM users", 900)?;

    Ok(())
}
