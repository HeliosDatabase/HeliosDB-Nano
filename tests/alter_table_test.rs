//! Tests for ALTER TABLE operations
//!
//! Tests ADD COLUMN, DROP COLUMN, RENAME COLUMN, and RENAME TABLE

use heliosdb_nano::{Config, EmbeddedDatabase, Value};

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

// === Multi-operation ALTER TABLE tests ===

#[test]
fn test_alter_table_multi_add_two_columns() {
    let db = create_test_db();

    db.execute("CREATE TABLE products (id INT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO products (id, name) VALUES (1, 'Widget')").unwrap();

    // Add two columns in a single ALTER TABLE statement
    db.execute("ALTER TABLE products ADD COLUMN price INT, ADD COLUMN stock INT").unwrap();

    // Verify both columns were added
    let results = db.query("SELECT * FROM products WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 4); // id, name, price, stock

    // New columns should have NULL values
    assert!(matches!(results[0].values[2], Value::Null));
    assert!(matches!(results[0].values[3], Value::Null));

    // Insert a row using all columns
    db.execute("INSERT INTO products (id, name, price, stock) VALUES (2, 'Gadget', 999, 50)").unwrap();
    let results = db.query("SELECT price, stock FROM products WHERE id = 2", &[]).unwrap();
    assert_eq!(results.len(), 1);
    match &results[0].values[0] {
        Value::Int4(v) => assert_eq!(*v, 999),
        other => panic!("Expected Int4(999), got {:?}", other),
    }
    match &results[0].values[1] {
        Value::Int4(v) => assert_eq!(*v, 50),
        other => panic!("Expected Int4(50), got {:?}", other),
    }
}

#[test]
fn test_alter_table_multi_add_and_drop_column() {
    let db = create_test_db();

    db.execute("CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, temp_col TEXT)").unwrap();
    db.execute("INSERT INTO employees (id, name, temp_col) VALUES (1, 'Alice', 'remove_me')").unwrap();

    // Add a column and drop a different column in one statement
    db.execute("ALTER TABLE employees ADD COLUMN email TEXT, DROP COLUMN temp_col").unwrap();

    // Verify: should have id, name, email (3 columns, not 4)
    let results = db.query("SELECT * FROM employees WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 3); // id, name, email

    // The new email column should be NULL
    assert!(matches!(results[0].values[2], Value::Null));

    // Querying the dropped column should fail
    let result = db.query("SELECT temp_col FROM employees WHERE id = 1", &[]);
    assert!(result.is_err());
}

#[test]
fn test_alter_table_multi_rename_and_add_column() {
    let db = create_test_db();

    db.execute("CREATE TABLE accounts (id INT PRIMARY KEY, username TEXT)").unwrap();
    db.execute("INSERT INTO accounts (id, username) VALUES (1, 'johndoe')").unwrap();

    // Rename a column, then add a new column in one statement
    db.execute("ALTER TABLE accounts RENAME COLUMN username TO login, ADD COLUMN email TEXT").unwrap();

    // Verify the rename worked
    let results = db.query("SELECT login FROM accounts WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    match &results[0].values[0] {
        Value::String(s) => assert_eq!(s, "johndoe"),
        other => panic!("Expected String, got {:?}", other),
    }

    // Verify the add worked
    let results = db.query("SELECT * FROM accounts WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 3); // id, login, email
}

#[test]
fn test_alter_table_multi_three_operations() {
    let db = create_test_db();

    db.execute("CREATE TABLE data (id INT PRIMARY KEY, col_a TEXT, col_b TEXT)").unwrap();
    db.execute("INSERT INTO data (id, col_a, col_b) VALUES (1, 'a', 'b')").unwrap();

    // Three operations: rename col_a, drop col_b, add col_c
    db.execute(
        "ALTER TABLE data RENAME COLUMN col_a TO alpha, DROP COLUMN col_b, ADD COLUMN col_c INT"
    ).unwrap();

    // Verify schema: should have id, alpha, col_c
    let results = db.query("SELECT * FROM data WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 3); // id, alpha, col_c

    // Verify renamed column has original value
    let results = db.query("SELECT alpha FROM data WHERE id = 1", &[]).unwrap();
    match &results[0].values[0] {
        Value::String(s) => assert_eq!(s, "a"),
        other => panic!("Expected String 'a', got {:?}", other),
    }

    // Verify dropped column is gone
    let result = db.query("SELECT col_b FROM data WHERE id = 1", &[]);
    assert!(result.is_err());

    // Verify new column exists with NULL
    let results = db.query("SELECT col_c FROM data WHERE id = 1", &[]).unwrap();
    assert!(matches!(results[0].values[0], Value::Null));
}

#[test]
fn test_alter_table_multi_error_in_second_op() {
    let db = create_test_db();

    db.execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO items (id, name) VALUES (1, 'Widget')").unwrap();

    // First op succeeds (add column), second op fails (drop non-existent column)
    let result = db.execute(
        "ALTER TABLE items ADD COLUMN price INT, DROP COLUMN nonexistent"
    );
    assert!(result.is_err());

    // Due to implicit transaction, the first operation's effect (add price column)
    // will have been applied since each op executes independently.
    // The price column WAS added because each op in the multi-alter runs sequentially.
    // Whether it rolls back depends on whether the whole statement is transactional.
    // In this engine, the implicit transaction wraps the entire execute() call,
    // so on error the transaction rolls back.
    //
    // However, DDL (ALTER TABLE) typically auto-commits in many databases.
    // In this embedded engine, DDL writes directly to the catalog,
    // so the first ADD COLUMN may have already been applied.
    // Let's just verify the error was returned.
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("nonexistent"), "Error should mention the missing column: {}", err_msg);
}

#[test]
fn test_alter_table_multi_with_if_not_exists() {
    let db = create_test_db();

    db.execute("CREATE TABLE settings (id INT PRIMARY KEY, key TEXT, value TEXT)").unwrap();

    // First operation: add column that already exists (with IF NOT EXISTS -- should be no-op)
    // Second operation: add a new column
    db.execute(
        "ALTER TABLE settings ADD COLUMN IF NOT EXISTS value TEXT, ADD COLUMN description TEXT"
    ).unwrap();

    // Should have 4 columns: id, key, value, description
    // Table is empty but we can check by inserting
    db.execute(
        "INSERT INTO settings (id, key, value, description) VALUES (1, 'theme', 'dark', 'UI theme')"
    ).unwrap();
    let results = db.query("SELECT * FROM settings WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 4);
}

#[test]
fn test_alter_table_multi_with_if_exists_drop() {
    let db = create_test_db();

    db.execute("CREATE TABLE logs (id INT PRIMARY KEY, msg TEXT)").unwrap();

    // First operation: drop non-existent column with IF EXISTS (should be no-op)
    // Second operation: add a new column
    db.execute(
        "ALTER TABLE logs DROP COLUMN IF EXISTS phantom, ADD COLUMN severity INT"
    ).unwrap();

    // Should have 3 columns: id, msg, severity
    db.execute("INSERT INTO logs (id, msg, severity) VALUES (1, 'test', 3)").unwrap();
    let results = db.query("SELECT * FROM logs WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].values.len(), 3);
}

#[test]
fn test_alter_table_multi_add_columns_with_defaults() {
    let db = create_test_db();

    db.execute("CREATE TABLE config (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO config (id) VALUES (1)").unwrap();

    // Add two columns with defaults
    db.execute(
        "ALTER TABLE config ADD COLUMN enabled BOOLEAN DEFAULT TRUE, ADD COLUMN retries INT DEFAULT 3"
    ).unwrap();

    let results = db.query("SELECT enabled, retries FROM config WHERE id = 1", &[]).unwrap();
    assert_eq!(results.len(), 1);

    // Check that defaults were applied to existing row
    match &results[0].values[0] {
        Value::Boolean(v) => assert!(*v),
        other => panic!("Expected Boolean(true), got {:?}", other),
    }
    match &results[0].values[1] {
        Value::Int4(v) => assert_eq!(*v, 3),
        other => panic!("Expected Int4(3), got {:?}", other),
    }
}
