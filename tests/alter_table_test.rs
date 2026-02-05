//! Tests for ALTER TABLE operations
//!
//! Tests ADD COLUMN, DROP COLUMN, RENAME COLUMN, and RENAME TABLE

use heliosdb_lite::{Config, EmbeddedDatabase, Value};

fn create_test_db() -> EmbeddedDatabase {
    let config = Config::in_memory();
    EmbeddedDatabase::with_config(config).unwrap()
}

#[test]
fn test_alter_table_add_column() {
    let db = create_test_db();

    // Create a table
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();

    // Insert some data
    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
    db.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')").unwrap();

    // Add a new column
    db.execute("ALTER TABLE users ADD COLUMN email TEXT").unwrap();

    // Verify the column was added (check by selecting)
    let results = db.query("SELECT * FROM users WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 3); // id, name, email

    // The new column should have NULL value
    assert!(matches!(results[0].values[2], Value::Null));

    // Insert a row with the new column
    db.execute("INSERT INTO users (id, name, email) VALUES (3, 'Charlie', 'charlie@example.com')").unwrap();

    let results = db.query("SELECT email FROM users WHERE id = 3", &[]).unwrap();
    assert_eq!(results.len(), 1);
    match &results[0].values[0] {
        Value::String(s) => assert_eq!(s, "charlie@example.com"),
        _ => panic!("Expected String value"),
    }
}

#[test]
fn test_alter_table_add_column_with_default() {
    let db = create_test_db();

    // Create a table and insert data
    db.execute("CREATE TABLE products (id INT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO products (id, name) VALUES (1, 'Widget')").unwrap();

    // Add column with default value
    db.execute("ALTER TABLE products ADD COLUMN stock INT DEFAULT 0").unwrap();

    // Check that existing rows have the default value
    let results = db.query("SELECT stock FROM products WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    match &results[0].values[0] {
        Value::Int4(v) => assert_eq!(*v, 0),
        other => panic!("Expected Int4(0), got {:?}", other),
    }
}

#[test]
fn test_alter_table_add_column_if_not_exists() {
    let db = create_test_db();

    db.execute("CREATE TABLE test (id INT PRIMARY KEY)").unwrap();
    db.execute("ALTER TABLE test ADD COLUMN name TEXT").unwrap();

    // Should not fail when using IF NOT EXISTS
    let result = db.execute("ALTER TABLE test ADD COLUMN IF NOT EXISTS name TEXT");
    assert!(result.is_ok());
}

#[test]
fn test_alter_table_add_column_already_exists_error() {
    let db = create_test_db();

    db.execute("CREATE TABLE test (id INT PRIMARY KEY, name TEXT)").unwrap();

    // Should fail when column already exists without IF NOT EXISTS
    let result = db.execute("ALTER TABLE test ADD COLUMN name TEXT");
    assert!(result.is_err());
}

#[test]
fn test_alter_table_drop_column() {
    let db = create_test_db();

    // Create a table with multiple columns
    db.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, salary INT)").unwrap();
    db.execute("INSERT INTO employees (id, name, salary) VALUES (1, 'Alice', 50000)").unwrap();

    // Drop the salary column
    db.execute("ALTER TABLE employees DROP COLUMN salary").unwrap();

    // Verify the column was removed
    let results = db.query("SELECT * FROM employees WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 2); // id, name only
}

#[test]
fn test_alter_table_drop_column_if_exists() {
    let db = create_test_db();

    db.execute("CREATE TABLE test (id INT PRIMARY KEY, name TEXT)").unwrap();

    // Should not fail when using IF EXISTS for non-existent column
    let result = db.execute("ALTER TABLE test DROP COLUMN IF EXISTS nonexistent");
    assert!(result.is_ok());
}

#[test]
fn test_alter_table_drop_column_not_exists_error() {
    let db = create_test_db();

    db.execute("CREATE TABLE test (id INT PRIMARY KEY)").unwrap();

    // Should fail when column doesn't exist without IF EXISTS
    let result = db.execute("ALTER TABLE test DROP COLUMN nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_alter_table_rename_column() {
    let db = create_test_db();

    // Create table with data
    db.execute("CREATE TABLE accounts (id INT PRIMARY KEY, username TEXT)").unwrap();
    db.execute("INSERT INTO accounts (id, username) VALUES (1, 'johndoe')").unwrap();

    // Rename column
    db.execute("ALTER TABLE accounts RENAME COLUMN username TO login").unwrap();

    // Verify we can query with the new name
    let results = db.query("SELECT login FROM accounts WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    match &results[0].values[0] {
        Value::String(s) => assert_eq!(s, "johndoe"),
        _ => panic!("Expected String value"),
    }

    // Verify old name doesn't work anymore
    let result = db.query("SELECT username FROM accounts WHERE id = 1", &[]);
    assert!(result.is_err());
}

#[test]
fn test_alter_table_rename() {
    let db = create_test_db();

    // Create table with data
    db.execute("CREATE TABLE old_table (id INT PRIMARY KEY, value TEXT)").unwrap();
    db.execute("INSERT INTO old_table (id, value) VALUES (1, 'test')").unwrap();

    // Rename table
    db.execute("ALTER TABLE old_table RENAME TO new_table").unwrap();

    // Verify we can query with the new name
    let results = db.query("SELECT * FROM new_table WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);

    // Verify old name doesn't work anymore
    let result = db.query("SELECT * FROM old_table", &[]);
    assert!(result.is_err());
}

#[test]
fn test_alter_table_rename_to_existing_error() {
    let db = create_test_db();

    db.execute("CREATE TABLE table1 (id INT PRIMARY KEY)").unwrap();
    db.execute("CREATE TABLE table2 (id INT PRIMARY KEY)").unwrap();

    // Should fail when target table already exists
    let result = db.execute("ALTER TABLE table1 RENAME TO table2");
    assert!(result.is_err());
}
